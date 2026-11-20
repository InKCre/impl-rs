mod adapter;
mod reconcile;
mod schema;
use async_trait::async_trait;
use inkcre_core::{
    Core,
    knowledge::{decode_unicode_bytes, format_label},
};
use inkcre_extension_sdk::{
    Capabilities, Content, ContentContract, ContentKind, Descriptor, Extension, PROTOCOL_VERSION,
    RelationSelection, ResolverCapability, ResolverOperation, ResolverRequest, RpcError,
    SDK_VERSION, SourceCapability, SourceOutcome, SourceRequest, StartContext, serve,
};
use schema::*;
use serde_json::{Value, json};
type Result<T> = std::result::Result<T, RpcError>;
fn fail(message: &str) -> RpcError {
    RpcError::new("github_error", message)
}
fn decode(kind: &str, raw: &str) -> Result<Value> {
    let value = match kind {
        ACCOUNT => {
            let v: Account =
                serde_json::from_str(raw).map_err(|_| fail("Invalid GitHub Account"))?;
            if !matches!(v.kind.as_str(), "user" | "organization") {
                return Err(fail("Invalid GitHub Account kind"));
            }
            serde_json::to_value(v)
        }
        REPOSITORY => serde_json::from_str::<Repository>(raw).and_then(serde_json::to_value),
        LIST => serde_json::from_str::<List>(raw).and_then(serde_json::to_value),
        _ => return Err(fail("Unknown GitHub Resolver")),
    };
    value.map_err(|_| fail("Invalid canonical GitHub content"))
}
#[derive(Default)]
struct Github {
    core: Option<Core>,
}
#[async_trait]
impl Extension for Github {
    fn descriptor(&self) -> Descriptor {
        let schemas = schemas();
        Descriptor {
            name: "inkcre/github".into(),
            version: env!("CARGO_PKG_VERSION").parse().expect("static version"),
            sdk_version: SDK_VERSION.parse().expect("static SDK"),
            protocol_version: PROTOCOL_VERSION,
            config_schema: json!({"type":"object","properties":{}}),
            methods: vec![],
            capabilities: Capabilities {
                sources: vec![SourceCapability {
                    id: SOURCE.into(),
                    description: Some(
                        "Synchronize authenticated GitHub Account Stars and Lists.".into(),
                    ),
                    config_schema: schemas["GitHubSourceConfig"].clone(),
                    collect_config_schema: json!({"type":"object","properties":{}}),
                    backfill_config_schema: None,
                }],
                resolvers: [
                    (ACCOUNT, "GitHubAccount"),
                    (REPOSITORY, "GitHubRepository"),
                    (LIST, "GitHubList"),
                ]
                .into_iter()
                .map(|(id, name)| ResolverCapability {
                    id: id.into(),
                    description: name.into(),
                    content: ContentContract {
                        kind: ContentKind::Json,
                        schema: Some(schemas[name].clone()),
                    },
                    relations: RelationSelection {
                        include_in: false,
                        include_out: false,
                    },
                    read_methods: vec![],
                    draft: None,
                })
                .collect(),
                ..Default::default()
            },
        }
    }
    async fn start(&mut self, _: StartContext) -> Result<Value> {
        let url =
            std::env::var("INKCRE_DATABASE_URL").map_err(|_| fail("Native database required"))?;
        self.core = Some(
            Core::connect(&url, 4)
                .await
                .map_err(|_| fail("Database admission failed"))?,
        );
        Ok(json!({"started":true}))
    }
    async fn stop(&mut self) -> Result<()> {
        if let Some(core) = self.core.take() {
            core.close().await
        }
        Ok(())
    }
    async fn invoke(&mut self, _: &str, _: Value) -> Result<Value> {
        Err(fail("Unknown method"))
    }
    async fn source_collect(&mut self, request: SourceRequest) -> Result<SourceOutcome> {
        let core = self
            .core
            .as_ref()
            .ok_or_else(|| fail("Extension not started"))?;
        let source = core
            .source_get(request.source)
            .await
            .map_err(|_| fail("Source read failed"))?
            .ok_or_else(|| fail("Source missing"))?;
        if request.type_id != SOURCE || source.type_ != SOURCE {
            return Err(fail("Source type changed"));
        }
        let token = source.config["github_token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| fail("GitHub token missing"))?;
        let snapshot = adapter::Adapter::new(token.into())?.snapshot().await?;
        let state = reconcile::run(core, source.id, snapshot, source.config).await?;
        Ok(SourceOutcome { state })
    }
    async fn resolver_read(&mut self, request: ResolverRequest) -> Result<Value> {
        let raw = match request.input.content {
            Content::Text(s) => s,
            Content::Bytes(b) => {
                decode_unicode_bytes(&b).map_err(|_| fail("Invalid text encoding"))?
            }
        };
        let value = decode(&request.resolver, &raw)?;
        let text = |key: &str| value[key].as_str().unwrap_or_default();
        match request.operation {
            ResolverOperation::Solve { .. } => Ok(value),
            ResolverOperation::Label { .. } => {
                let (prefix, name) = match request.resolver.as_str() {
                    ACCOUNT => (format!("github {}", text("kind")), text("login")),
                    REPOSITORY => ("github repository".into(), text("name_with_owner")),
                    _ => ("github list".into(), text("name")),
                };
                Ok(json!(format_label(&prefix, Some(name), false)))
            }
            ResolverOperation::Text { .. } => {
                let rendered = match request.resolver.as_str() {
                    ACCOUNT => {
                        if text("name").is_empty() {
                            format!("@{}", text("login"))
                        } else {
                            format!("{} (@{})", text("name"), text("login"))
                        }
                    }
                    REPOSITORY => {
                        let mut parts = vec![text("name_with_owner").to_owned()];
                        if !text("description").is_empty() {
                            parts.push(text("description").into())
                        }
                        if !text("primary_language").is_empty() {
                            parts.push(format!("Language: {}", text("primary_language")))
                        }
                        if let Some(topics) = value["topics"].as_array().filter(|a| !a.is_empty()) {
                            parts.push(format!(
                                "Topics: {}",
                                topics
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ))
                        }
                        parts.join("\n")
                    }
                    _ => [text("name"), text("description")]
                        .into_iter()
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                Ok(json!(rendered))
            }
            _ => Err(fail("Unknown Resolver method")),
        }
    }
}
#[tokio::main]
async fn main() {
    if serve(Github::default()).await.is_err() {
        eprintln!("GitHub extension protocol stopped");
        std::process::exit(1)
    }
}
