mod collect;
mod markup;
mod materialize;
mod remote;
mod schema;

use async_trait::async_trait;
use inkcre_core::{
    Core,
    knowledge::{decode_unicode_bytes, format_label},
};
use inkcre_extension_sdk::{
    Capabilities, Content, ContentContract, ContentKind, Descriptor, Extension, HttpAuth, HttpBody,
    HttpHeader, HttpIngress, HttpPathMatch, HttpRequest, HttpResponse, HttpRoute, PROTOCOL_VERSION,
    PeerHttpInbound, RelationSelection, ResolverCapability, ResolverOperation, ResolverRequest,
    RpcError, SDK_VERSION, SourceCapability, SourceOutcome, SourceRequest, StartContext, serve,
};
use schema::*;
use serde_json::{Value, json};
type Result<T> = std::result::Result<T, RpcError>;
fn fail(message: &str) -> RpcError {
    RpcError::new("telegram_error", message)
}
fn db(_: sqlx::Error) -> RpcError {
    fail("Telegram database operation failed")
}
#[derive(Default)]
struct Telegram {
    core: Option<Core>,
}
impl Telegram {
    fn core(&self) -> Result<&Core> {
        self.core
            .as_ref()
            .ok_or_else(|| fail("Telegram extension not started"))
    }
}
#[async_trait]
impl Extension for Telegram {
    fn descriptor(&self) -> Descriptor {
        let schemas = schemas();
        Descriptor {
            name: "inkcre/telegram".into(),
            version: env!("CARGO_PKG_VERSION").parse().expect("static version"),
            sdk_version: SDK_VERSION.parse().expect("static SDK"),
            protocol_version: PROTOCOL_VERSION,
            config_schema: json!({"type":"object","properties":{},"additionalProperties":false}),
            methods: vec![MATERIALIZE_METHOD.into()],
            capabilities: Capabilities {
                passive_knowledge: true,
                http_ingress: Some(HttpIngress {
                    routes: vec![HttpRoute {
                        method: "POST".into(),
                        path: MATERIALIZE_PATH.into(),
                        path_match: HttpPathMatch::Exact,
                        auth: HttpAuth::Peer,
                    }],
                    peer_inbounds: vec![PeerHttpInbound {
                        capability: MATERIALIZE_CAPABILITY.into(),
                        method: "POST".into(),
                        path: MATERIALIZE_PATH.into(),
                    }],
                }),
                sources: vec![SourceCapability {
                    id: SOURCE.into(),
                    description: Some("Sender-bound private Telegram delivery inbox".into()),
                    config_schema: schemas["TelegramSourceConfig"].clone(),
                    collect_config_schema: json!({"type":"object","properties":{}}),
                    backfill_config_schema: None,
                }],
                resolvers: [
                    (MESSAGE, "TelegramMessage", false),
                    (ATTACHMENT, "TelegramAttachment", true),
                ]
                .into_iter()
                .map(|(id, name, out)| ResolverCapability {
                    id: id.into(),
                    description: name.into(),
                    content: ContentContract {
                        kind: ContentKind::Json,
                        schema: Some(schemas[name].clone()),
                    },
                    relations: RelationSelection {
                        include_in: false,
                        include_out: out,
                    },
                    read_methods: vec![],
                    draft: None,
                })
                .collect(),
                ..Default::default()
            },
        }
    }
    async fn start_knowledge(
        &mut self,
        context: inkcre_extension_sdk::KnowledgeStartContext,
    ) -> std::result::Result<(), RpcError> {
        self.start(inkcre_extension_sdk::StartContext {
            peer_id: context.peer_id,
            config: json!({}),
            state: json!({}),
        })
        .await
        .map(|_| ())
    }
    async fn stop_knowledge(&mut self) -> std::result::Result<(), RpcError> {
        self.stop().await
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
    async fn invoke(&mut self, method: &str, parameters: Value) -> Result<Value> {
        if method != MATERIALIZE_METHOD {
            return Err(fail("Unknown Telegram method"));
        }
        let request: Materialize = serde_json::from_value(parameters).map_err(|_| {
            RpcError::new(
                "invalid_input",
                "Invalid attachment materialization request",
            )
        })?;
        serde_json::to_value(materialize::run(self.core()?, request.block).await?)
            .map_err(|_| fail("Telegram materialization representation failed"))
    }
    async fn http_request(&mut self, request: HttpRequest) -> Result<HttpResponse> {
        if request.method != "POST" || request.path != MATERIALIZE_PATH {
            return Err(RpcError::new("not_found", "Telegram route not found"));
        }
        let parameters = serde_json::from_slice(&request.body.0).map_err(|_| {
            RpcError::new(
                "invalid_input",
                "Invalid attachment materialization request",
            )
        })?;
        let value = self.invoke(MATERIALIZE_METHOD, parameters).await?;
        Ok(HttpResponse {
            status: 200,
            headers: vec![HttpHeader {
                name: "content-type".into(),
                value: b"application/json".to_vec(),
            }],
            body: HttpBody(
                serde_json::to_vec(&value)
                    .map_err(|_| fail("Telegram response encoding failed"))?,
            ),
        })
    }
    async fn source_collect(&mut self, request: SourceRequest) -> Result<SourceOutcome> {
        let core = self.core()?;
        let source = core
            .source_get(request.source)
            .await
            .map_err(|_| fail("Source read failed"))?
            .ok_or_else(|| fail("Telegram Source missing"))?;
        if source.type_ != SOURCE || request.type_id != SOURCE {
            return Err(fail("Source type changed"));
        }
        Ok(SourceOutcome {
            state: collect::run(core, source, request.job).await?,
        })
    }
    async fn resolver_read(&mut self, request: ResolverRequest) -> Result<Value> {
        let raw = match &request.input.content {
            Content::Text(s) => s.clone(),
            Content::Bytes(b) => {
                decode_unicode_bytes(b).map_err(|_| fail("Invalid Telegram text encoding"))?
            }
        };
        match request.resolver.as_str() {
            MESSAGE => {
                let root: LegacyMessage = serde_json::from_str(&raw)
                    .map_err(|_| fail("Invalid legacy Telegram message"))?;
                let text = root
                    .text
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .or(root.caption.as_deref());
                match request.operation {
                    ResolverOperation::Solve { .. } => serde_json::to_value(&root)
                        .map_err(|_| fail("Legacy message encoding failed")),
                    ResolverOperation::Text { .. } => {
                        let mut parts = Vec::new();
                        if let Some(text) = text.filter(|s| !s.is_empty()) {
                            parts.push(text.to_owned())
                        }
                        if root.has_media
                            && let Some(kind) = root.media_type.filter(|s| !s.is_empty())
                        {
                            parts.push(format!("[{kind}]"))
                        }
                        if parts.is_empty() {
                            Ok(Value::Null)
                        } else {
                            Ok(json!(parts.join("\n")))
                        }
                    }
                    ResolverOperation::Label { .. } => Ok(json!(format_label(
                        "telegram message",
                        Some(
                            text.filter(|s| !s.is_empty())
                                .map(str::to_owned)
                                .unwrap_or_else(|| root.message_id.to_string())
                                .as_str()
                        ),
                        true
                    ))),
                    _ => Err(fail("Unknown Telegram Resolver read method")),
                }
            }
            ATTACHMENT => {
                let root = Attachment::parse(&raw)?;
                match request.operation {
                    ResolverOperation::Label { .. } => Ok(json!(format_label(
                        &format!("telegram {}", root.kind),
                        root.filename
                            .as_deref()
                            .filter(|s| !s.is_empty())
                            .or(root.title.as_deref().filter(|s| !s.is_empty()))
                            .or(root.emoji.as_deref()),
                        false
                    ))),
                    operation => {
                        let children: Vec<_> = request
                            .input
                            .relations
                            .iter()
                            .filter(|r| {
                                r.from_ == Some(request.input.block.id) && r.content == "content"
                            })
                            .collect();
                        if children.len() > 1 {
                            return Err(fail("Telegram attachment has multiple content children"));
                        }
                        let child = if let Some(edge) = children.first() {
                            let id = edge.to_.ok_or_else(|| {
                                fail("Telegram attachment content endpoint missing")
                            })?;
                            Some(
                                self.core()?
                                    .block_get(id)
                                    .await
                                    .map_err(|_| fail("Attachment content read failed"))?
                                    .ok_or_else(|| fail("Attachment content child missing"))?,
                            )
                        } else {
                            None
                        };
                        match operation {
                            ResolverOperation::Solve { .. } => {
                                Ok(json!({"root":root,"content":child}))
                            }
                            ResolverOperation::Text { .. } => {
                                let text = [
                                    root.filename.as_deref(),
                                    root.title.as_deref(),
                                    root.performer.as_deref(),
                                    root.mime_type.as_deref(),
                                    Some(root.kind.as_str()),
                                    root.emoji.as_deref(),
                                ]
                                .into_iter()
                                .flatten()
                                .filter(|s| !s.is_empty())
                                .collect::<Vec<_>>()
                                .join("\n");
                                if text.is_empty() {
                                    Ok(Value::Null)
                                } else {
                                    Ok(json!(text))
                                }
                            }
                            _ => Err(fail("Unknown Telegram Resolver read method")),
                        }
                    }
                }
            }
            _ => Err(fail("Unknown Telegram Resolver")),
        }
    }
}
#[tokio::main]
async fn main() {
    if serve(Telegram::default()).await.is_err() {
        eprintln!("Telegram extension protocol stopped");
        std::process::exit(1)
    }
}
