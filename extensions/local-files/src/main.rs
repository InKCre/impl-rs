use async_trait::async_trait;
use chrono::{DateTime, Utc};
use inkcre_core::{
    Core,
    knowledge::{TextOptions, decode_unicode_bytes, format_label},
};
use inkcre_extension_sdk::{
    Capabilities, Content, ContentContract, ContentKind, Descriptor, Extension, PROTOCOL_VERSION,
    RelationSelection, ResolverCapability, ResolverOperation, ResolverRequest, RpcError,
    SDK_VERSION, StartContext, serve,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

const V1: &str = "extensions.local_files.file.v1";
const V2: &str = "extensions.local_files.file.v2";
type Result<T> = std::result::Result<T, RpcError>;
fn invalid() -> RpcError {
    RpcError::new(
        "invalid_content",
        "Local file metadata or body graph is invalid",
    )
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    volume: String,
    number: u64,
    created: DateTime<Utc>,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Scope {
    Text,
    TextLayer,
    MainDocument,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Revision {
    sha256: String,
    extractor: String,
    extracted_at: DateTime<Utc>,
    scope: Scope,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Format {
    Pdf,
    Docx,
    Markdown,
    Org,
    Html,
    Text,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    location: String,
    format: Format,
    identity: Option<Identity>,
    available: bool,
    observed_at: DateTime<Utc>,
    revision: Option<Revision>,
    issue: Option<String>,
}
fn location(value: &str) -> Result<String> {
    let url = url::Url::parse(value).map_err(|_| invalid())?;
    if url.scheme() != "inkcre-peer-file"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || value.contains(['?', '#'])
    {
        return Err(invalid());
    }
    Uuid::parse_str(url.host_str().ok_or_else(invalid)?).map_err(|_| invalid())?;
    // Decode percent escapes as UTF-8 without treating '+' as a space.
    let mut bytes = Vec::new();
    let raw = url.path().as_bytes();
    let mut index = 0;
    while index < raw.len() {
        if raw[index] == b'%'
            && index + 2 < raw.len()
            && let (Some(high), Some(low)) = (
                (raw[index + 1] as char).to_digit(16),
                (raw[index + 2] as char).to_digit(16),
            )
        {
            let byte = (high * 16 + low) as u8;
            bytes.push(byte);
            index += 3;
            continue;
        }
        bytes.push(raw[index]);
        index += 1;
    }
    let path = String::from_utf8(bytes).map_err(|_| invalid())?;
    if !path.starts_with('/') || path.contains('\0') {
        return Err(invalid());
    }
    Ok(path)
}
fn metadata(content: &Content) -> Result<Metadata> {
    let text = match content {
        Content::Text(text) => text.clone(),
        Content::Bytes(bytes) => decode_unicode_bytes(bytes).map_err(|_| invalid())?,
    };
    let metadata: Metadata = serde_json::from_str(&text).map_err(|_| invalid())?;
    location(&metadata.location)?;
    if metadata.revision.as_ref().is_some_and(|revision| {
        revision.sha256.len() != 64
            || !revision
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }) {
        return Err(invalid());
    }
    Ok(metadata)
}
fn schema() -> Value {
    let timestamp = json!({"type":"string","format":"date-time"});
    json!({"type":"object","additionalProperties":false,"required":["location","format","available","observed_at"],"properties":{
        "location":{"type":"string"},"format":{"enum":["pdf","docx","markdown","org","html","text"]},"available":{"type":"boolean"},"observed_at":timestamp,
        "identity":{"anyOf":[{"type":"null"},{"type":"object","additionalProperties":false,"required":["volume","number","created"],"properties":{"volume":{"type":"string"},"number":{"type":"integer","minimum":0},"created":timestamp}}]},
        "revision":{"anyOf":[{"type":"null"},{"type":"object","additionalProperties":false,"required":["sha256","extractor","extracted_at","scope"],"properties":{"sha256":{"type":"string","pattern":"^[0-9a-f]{64}$"},"extractor":{"type":"string"},"extracted_at":timestamp,"scope":{"enum":["text","text_layer","main_document"]}}}]},"issue":{"type":["string","null"]}
    }})
}
async fn read(core: &Core, request: ResolverRequest) -> Result<Value> {
    if ![V1, V2].contains(&request.resolver.as_str()) {
        return Err(invalid());
    }
    let metadata = metadata(&request.input.content)?;
    if matches!(request.operation, ResolverOperation::Label { .. }) {
        let path = location(&metadata.location)?;
        let name = path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty());
        return Ok(json!(format_label("本地文件", name, false)));
    }
    let bodies: Vec<_> = request
        .input
        .relations
        .iter()
        .filter(|edge| edge.from_ == Some(request.input.block.id) && edge.content == "body")
        .collect();
    if bodies.len() > 1 || (metadata.revision.is_some() && bodies.is_empty()) {
        return Err(invalid());
    }
    let (body_id, text) = if let Some(body) = bodies.first() {
        let id = body.to_.ok_or_else(invalid)?;
        let block = core
            .block_get(id)
            .await
            .map_err(|_| invalid())?
            .ok_or_else(invalid)?;
        let allowed = block.resolver == "core.text.v1"
            || (request.resolver == V2
                && ["core.html.v1", "core.markdown.v1"].contains(&block.resolver.as_str()));
        if block.storage.is_some() || !allowed {
            return Err(invalid());
        }
        let text = if block.resolver == "core.text.v1" {
            Some(block.content)
        } else {
            let (refresh, materialize_missing) = match request.operation {
                ResolverOperation::Solve {
                    refresh,
                    materialize_missing,
                } => (refresh, materialize_missing),
                ResolverOperation::Text { refresh, .. } => (refresh, false),
                _ => (false, false),
            };
            core.knowledge()
                .text(
                    id,
                    TextOptions {
                        refresh,
                        materialize_missing,
                        ..Default::default()
                    },
                )
                .await
                .map_err(|_| invalid())?
        };
        (Some(id), text)
    } else {
        (None, None)
    };
    match request.operation {
        ResolverOperation::Solve { .. } => {
            Ok(json!({"metadata":metadata,"body_id":body_id,"text":text}))
        }
        ResolverOperation::Text { .. } => {
            let mut parts = vec![format!("原件位置：{}", metadata.location)];
            if !metadata.available {
                parts.push("原件已失效；保留最后收入的正文。".into());
            }
            if let Some(issue) = metadata.issue.filter(|issue| !issue.is_empty()) {
                parts.push(format!("当前提取失败：{issue}。正文对应上次成功修订。"));
            }
            if metadata
                .revision
                .is_some_and(|revision| matches!(revision.scope, Scope::MainDocument))
            {
                parts.push("DOCX 主文档文字，不含脚注、批注。".into());
            }
            if let Some(text) = text {
                parts.push(text);
            }
            Ok(json!(parts.join("\n\n")))
        }
        _ => Err(RpcError::new(
            "unknown_method",
            "Local Files has no custom methods",
        )),
    }
}
#[derive(Default)]
struct LocalFiles {
    core: Option<Core>,
}
#[async_trait]
impl Extension for LocalFiles {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            name: "inkcre/local-files".into(),
            version: env!("CARGO_PKG_VERSION").parse().expect("static version"),
            sdk_version: SDK_VERSION.parse().expect("static SDK"),
            protocol_version: PROTOCOL_VERSION,
            config_schema: json!({"type":"object","additionalProperties":false,"properties":{}}),
            methods: vec![],
            capabilities: Capabilities {
                resolvers: [V1, V2]
                    .into_iter()
                    .map(|id| ResolverCapability {
                        id: id.into(),
                        description: "Interpret persisted Local Files body and location".into(),
                        content: ContentContract {
                            kind: ContentKind::Json,
                            schema: Some(schema()),
                        },
                        relations: RelationSelection {
                            include_in: false,
                            include_out: true,
                        },
                        read_methods: vec![],
                        draft: None,
                    })
                    .collect(),
                ..Default::default()
            },
        }
    }
    async fn invoke(&mut self, _method: &str, _params: Value) -> Result<Value> {
        Err(RpcError::new(
            "unknown_method",
            "Local Files has no generic commands",
        ))
    }
    async fn start(&mut self, _context: StartContext) -> Result<Value> {
        let url = std::env::var("INKCRE_DATABASE_URL").map_err(|_| invalid())?;
        self.core = Some(Core::connect(&url, 4).await.map_err(|_| invalid())?);
        Ok(json!({"started":true}))
    }
    async fn resolver_read(&mut self, request: ResolverRequest) -> Result<Value> {
        read(self.core.as_ref().ok_or_else(invalid)?, request).await
    }
    async fn stop(&mut self) -> Result<()> {
        if let Some(core) = self.core.take() {
            core.close().await;
        }
        Ok(())
    }
}
#[tokio::main]
async fn main() -> std::process::ExitCode {
    if serve(LocalFiles::default()).await.is_err() {
        eprintln!("Local Files protocol failed");
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}
