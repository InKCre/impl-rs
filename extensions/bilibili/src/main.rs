use async_trait::async_trait;
use inkcre_core::knowledge::decode_unicode_bytes;
use inkcre_extension_sdk::{
    Capabilities, Content, ContentContract, ContentKind, Descriptor, Extension, PROTOCOL_VERSION,
    RelationSelection, ResolverCapability, ResolverOperation, ResolverRequest, RpcError,
    SDK_VERSION, serve,
};
use serde_json::{Value, json};

const PREFIX: &str = "extensions.bilibili.";
const KINDS: [&str; 4] = ["user", "folder", "video", "membership"];
type Result<T> = std::result::Result<T, RpcError>;

fn invalid() -> RpcError {
    RpcError::new("invalid_content", "Bilibili v1 record is invalid")
}

fn schema(kind: &str) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    let mut add = |name: &str, value: Value| {
        properties.insert(name.into(), value);
    };
    match kind {
        "user" => {
            add("mid", json!({"type":"string","pattern":"^[1-9][0-9]*$"}));
            add("name", json!({"type":"string"}));
            add("avatar_url", json!({"type":"string","format":"uri"}));
            required.push("mid");
        }
        "folder" => {
            add("id", json!({"type":"string","pattern":"^[1-9][0-9]*$"}));
            for key in ["title", "description"] {
                add(key, json!({"type":"string"}));
            }
            add("is_private", json!({"type":"boolean"}));
            add("reported_count", json!({"type":"integer","minimum":0}));
            required.push("id");
        }
        "video" => {
            add("aid", json!({"type":"string","pattern":"^[1-9][0-9]*$"}));
            add(
                "bvid",
                json!({"type":"string","pattern":"^BV[0-9A-Za-z]{10}$"}),
            );
            for key in ["title", "description"] {
                add(key, json!({"type":"string"}));
            }
            add("duration_seconds", json!({"type":"integer","minimum":0}));
            add(
                "published_at",
                json!({"type":"string","format":"date-time","pattern":"Z$"}),
            );
            add("cover_url", json!({"type":"string","format":"uri"}));
            required.push("aid");
        }
        "membership" => {
            add(
                "availability",
                json!({"enum":["available","unavailable","unknown"]}),
            );
            add("page_number", json!({"type":"integer","minimum":1}));
            add("position_in_page", json!({"type":"integer","minimum":0}));
            for key in ["favored_at", "observed_at"] {
                add(
                    key,
                    json!({"type":"string","format":"date-time","pattern":"Z$"}),
                );
            }
            required.extend(["availability", "observed_at"]);
        }
        _ => {}
    }
    json!({"type":"object","additionalProperties":false,"properties":properties,"required":required})
}

fn validate(kind: &str, value: &Value) -> Result<()> {
    let object = value.as_object().ok_or_else(invalid)?;
    let contract = schema(kind);
    let properties = contract["properties"].as_object().ok_or_else(invalid)?;
    for required in contract["required"].as_array().ok_or_else(invalid)? {
        if !object.contains_key(required.as_str().ok_or_else(invalid)?) {
            return Err(invalid());
        }
    }
    for (key, value) in object {
        if !properties.contains_key(key) || value.is_null() {
            return Err(invalid());
        }
        match key.as_str() {
            "mid" | "id" | "aid" => {
                let id = value.as_str().ok_or_else(invalid)?;
                if !id.starts_with(|c: char| c.is_ascii_digit() && c != '0')
                    || !id.bytes().all(|c| c.is_ascii_digit())
                {
                    return Err(invalid());
                }
            }
            "bvid" => {
                let id = value.as_str().ok_or_else(invalid)?;
                if id.len() != 12
                    || !id.starts_with("BV")
                    || !id.bytes().all(|c| c.is_ascii_alphanumeric())
                {
                    return Err(invalid());
                }
            }
            "is_private" => {
                if !value.is_boolean() {
                    return Err(invalid());
                }
            }
            "reported_count" | "duration_seconds" | "page_number" | "position_in_page" => {
                let count = value.as_u64().ok_or_else(invalid)?;
                if key == "page_number" && count == 0 {
                    return Err(invalid());
                }
            }
            "published_at" | "favored_at" | "observed_at" => {
                let instant = value.as_str().ok_or_else(invalid)?;
                if !instant.ends_with('Z') || chrono::DateTime::parse_from_rfc3339(instant).is_err()
                {
                    return Err(invalid());
                }
            }
            "avatar_url" | "cover_url" => {
                if url::Url::parse(value.as_str().ok_or_else(invalid)?).is_err() {
                    return Err(invalid());
                }
            }
            "availability" => {
                if !matches!(
                    value.as_str(),
                    Some("available" | "unavailable" | "unknown")
                ) {
                    return Err(invalid());
                }
            }
            _ => {
                if !value.is_string() {
                    return Err(invalid());
                }
            }
        }
    }
    Ok(())
}

fn title(kind: &str, value: &Value) -> String {
    let text = |key| {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    match kind {
        "user" => text("name")
            .map(str::to_owned)
            .unwrap_or_else(|| format!("UID {}", value["mid"].as_str().unwrap_or_default())),
        "folder" => text("title")
            .map(str::to_owned)
            .unwrap_or_else(|| format!("收藏夹 {}", value["id"].as_str().unwrap_or_default())),
        "video" => text("title")
            .or_else(|| text("bvid"))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("av{}", value["aid"].as_str().unwrap_or_default())),
        _ if value["availability"] == "unavailable" => "已失效的收藏条目".into(),
        _ => "收藏条目".into(),
    }
}

struct Bilibili;
#[async_trait]
impl Extension for Bilibili {
    async fn start(&mut self, _context: inkcre_extension_sdk::StartContext) -> Result<Value> {
        Ok(json!({"started":true}))
    }
    async fn invoke(&mut self, _method: &str, _params: Value) -> Result<Value> {
        Err(RpcError::new(
            "unsupported",
            "Bilibili exposes typed readers",
        ))
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            name: "inkcre/bilibili".into(),
            version: env!("CARGO_PKG_VERSION").parse().expect("static version"),
            sdk_version: SDK_VERSION.parse().expect("static SDK version"),
            protocol_version: PROTOCOL_VERSION,
            config_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            methods: vec![],
            capabilities: Capabilities {
                resolvers: KINDS
                    .iter()
                    .map(|kind| ResolverCapability {
                        id: format!("{PREFIX}{kind}.v1"),
                        description: "Interpret shared Bilibili favorites".into(),
                        content: ContentContract {
                            kind: ContentKind::Json,
                            schema: Some(schema(kind)),
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
    async fn resolver_read(&mut self, request: ResolverRequest) -> Result<Value> {
        let kind = request
            .resolver
            .strip_prefix(PREFIX)
            .and_then(|s| s.strip_suffix(".v1"))
            .filter(|kind| KINDS.contains(kind))
            .ok_or_else(invalid)?;
        let content = match request.input.content {
            Content::Text(text) => text,
            Content::Bytes(bytes) => decode_unicode_bytes(&bytes).map_err(|_| invalid())?,
        };
        let record: Value = serde_json::from_str(&content).map_err(|_| invalid())?;
        validate(kind, &record)?;
        match request.operation {
            ResolverOperation::Solve { .. } => {
                let mut solved = record;
                if let Some(object) = solved.as_object_mut()
                    && let Some(properties) = schema(kind)["properties"].as_object()
                {
                    for key in properties.keys() {
                        object.entry(key.clone()).or_insert(Value::Null);
                    }
                }
                Ok(solved)
            }
            ResolverOperation::Label { .. } => Ok(json!(title(kind, &record))),
            ResolverOperation::Text { .. } => {
                let mut text = title(kind, &record);
                if matches!(kind, "folder" | "video")
                    && let Some(description) =
                        record["description"].as_str().filter(|s| !s.is_empty())
                {
                    text.push('\n');
                    text.push_str(description);
                }
                Ok(json!(text))
            }
            _ => Err(RpcError::new(
                "unsupported",
                "Bilibili has no custom read methods",
            )),
        }
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match serve(Bilibili).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => {
            eprintln!("Bilibili protocol failed");
            std::process::ExitCode::FAILURE
        }
    }
}
