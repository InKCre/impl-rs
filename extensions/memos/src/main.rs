mod graph;
mod ingress;
mod schema;
mod wire;

use async_trait::async_trait;
use inkcre_core::{
    Core,
    knowledge::{decode_unicode_bytes, format_label},
};
use inkcre_extension_sdk::{
    Capabilities, Content, ContentContract, ContentKind, Descriptor, Extension, HttpRequest,
    HttpResponse, KnowledgeStartContext, PROTOCOL_VERSION, RelationSelection, ResolverCapability,
    ResolverOperation, ResolverRequest, RpcError, SDK_VERSION, StartContext, serve,
};
use schema::*;
use serde_json::{Value, json};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Error>;
#[derive(Debug)]
enum Error {
    BadRequest(&'static str),
    NotFound,
    Unauthorized,
    Conflict(&'static str),
    Integrity(&'static str),
    Database,
    Json,
}
impl From<sqlx::Error> for Error {
    fn from(_: sqlx::Error) -> Self {
        Self::Database
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::Json
    }
}
impl From<Error> for RpcError {
    fn from(error: Error) -> Self {
        match error {
            Error::BadRequest(message) => Self::new("invalid_input", message),
            Error::NotFound => Self::new("not_found", "Memos resource not found"),
            Error::Unauthorized => Self::new("unauthorized", "Invalid Memos personal access token"),
            Error::Conflict(message) => Self::new("conflict", message),
            Error::Integrity(message) => Self::new("graph_integrity", message),
            Error::Database => Self::new("database", "Memos database operation failed"),
            Error::Json => Self::new(
                "invalid_content",
                "Memos canonical graph content is invalid",
            ),
        }
    }
}

#[derive(Default)]
struct Memos {
    core: Option<Core>,
    peer: Option<Uuid>,
}
impl Memos {
    async fn connect(&mut self, peer: &str) -> std::result::Result<(), RpcError> {
        self.peer =
            Some(peer.parse().map_err(|_| {
                RpcError::new("invalid_peer", "Memos requires a UUID Peer identity")
            })?);
        let url = std::env::var("INKCRE_DATABASE_URL")
            .map_err(|_| RpcError::new("database", "Memos requires native database capability"))?;
        self.core = Some(
            Core::connect(&url, 4)
                .await
                .map_err(|_| RpcError::new("database", "Memos database admission failed"))?,
        );
        Ok(())
    }
}
fn schema(id: &str) -> Value {
    if id == MEMO {
        json!({"type":"object","required":["body","created_at","updated_at"],"properties":{"body":{"type":"string"},"created_at":{"type":["string","null"],"format":"date-time"},"updated_at":{"type":["string","null"],"format":"date-time"},"archived":{"type":"boolean","default":false},"visibility":{"type":"string","enum":["private","protected","public"],"default":"private"},"pinned":{"type":"boolean","default":false}},"additionalProperties":false})
    } else {
        json!({"type":"object","required":["filename","media_type","size","created_at"],"properties":{"filename":{"type":"string"},"media_type":{"type":"string"},"size":{"type":"integer","minimum":0},"created_at":{"type":"string","format":"date-time"}},"additionalProperties":false})
    }
}
#[async_trait]
impl Extension for Memos {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            name: "inkcre/memos".into(),
            version: env!("CARGO_PKG_VERSION").parse().expect("version"),
            sdk_version: SDK_VERSION.parse().expect("SDK"),
            protocol_version: PROTOCOL_VERSION,
            config_schema: json!({"type":"object","properties":{"personal_access_token":{"anyOf":[{"type":"string","pattern":"^memos_pat_[0-9A-Za-z]{32}$"},{"type":"null"}],"default":null}},"additionalProperties":false}),
            methods: vec![],
            capabilities: Capabilities {
                passive_knowledge: true,
                http_ingress: Some(ingress::descriptor()),
                resolvers: [MEMO, ATTACHMENT]
                    .into_iter()
                    .map(|id| ResolverCapability {
                        id: id.into(),
                        description: "Memos family graph".into(),
                        content: ContentContract {
                            kind: ContentKind::Json,
                            schema: Some(schema(id)),
                        },
                        relations: RelationSelection {
                            include_in: id == ATTACHMENT,
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
    async fn start(&mut self, context: StartContext) -> std::result::Result<Value, RpcError> {
        self.connect(&context.peer_id).await?;
        Ok(json!({"started":true}))
    }
    async fn start_knowledge(
        &mut self,
        context: KnowledgeStartContext,
    ) -> std::result::Result<(), RpcError> {
        self.connect(&context.peer_id).await
    }
    async fn stop_knowledge(&mut self) -> std::result::Result<(), RpcError> {
        self.stop().await
    }
    async fn invoke(&mut self, _: &str, _: Value) -> std::result::Result<Value, RpcError> {
        Err(RpcError::new(
            "unknown_method",
            "Memos has no generic command",
        ))
    }
    async fn http_request(
        &mut self,
        request: HttpRequest,
    ) -> std::result::Result<HttpResponse, RpcError> {
        let core = self
            .core
            .as_ref()
            .ok_or_else(|| RpcError::new("invalid_state", "Memos is not running"))?;
        Ok(
            match ingress::handle(core, self.peer.expect("started peer"), request).await {
                Ok(response) => response,
                Err(error) => ingress::error(error),
            },
        )
    }
    async fn resolver_read(
        &mut self,
        request: ResolverRequest,
    ) -> std::result::Result<Value, RpcError> {
        let core = self
            .core
            .as_ref()
            .ok_or_else(|| RpcError::new("invalid_state", "Memos is not running"))?;
        let text = match &request.input.content {
            Content::Text(s) => s.clone(),
            Content::Bytes(b) => decode_unicode_bytes(b).map_err(|_| {
                RpcError::new("invalid_content", "Memos content must be Unicode JSON")
            })?,
        };
        match request.resolver.as_str() {
            MEMO => {
                let canonical: CanonicalMemo = serde_json::from_str(&text)
                    .map_err(|_| RpcError::new("invalid_content", "Memo v1 JSON is invalid"))?;
                match request.operation {
                    ResolverOperation::Text { .. } => Ok(json!(canonical.body)),
                    ResolverOperation::Label { .. } => {
                        Ok(json!(format_label("memo", Some(&canonical.body), true)))
                    }
                    ResolverOperation::Solve { .. } => {
                        let mut connection =
                            core.pool().acquire().await.map_err(|_| {
                                RpcError::new("database", "Memos graph unavailable")
                            })?;
                        Ok(json!(
                            graph::solve(&mut connection, request.input.block.id, core).await?
                        ))
                    }
                    _ => Err(RpcError::new(
                        "unsupported_capability",
                        "Memos resolver has no custom read method",
                    )),
                }
            }
            ATTACHMENT => {
                let canonical: CanonicalAttachment =
                    serde_json::from_str(&request.input.block.content).map_err(|_| {
                        RpcError::new("invalid_content", "Attachment v2 JSON is invalid")
                    })?;
                match request.operation {
                    ResolverOperation::Text { .. } => Ok(json!(canonical.filename)),
                    ResolverOperation::Label { .. } => Ok(json!(format_label(
                        "memo attachment",
                        Some(&canonical.filename),
                        false
                    ))),
                    ResolverOperation::Solve { .. } => {
                        let mut connection =
                            core.pool().acquire().await.map_err(|_| {
                                RpcError::new("database", "Memos graph unavailable")
                            })?;
                        Ok(json!(
                            graph::solve_attachment(&mut connection, request.input.block.id, core)
                                .await?
                        ))
                    }
                    _ => Err(RpcError::new(
                        "unsupported_capability",
                        "Memos resolver has no custom read method",
                    )),
                }
            }
            _ => Err(RpcError::new(
                "unsupported_capability",
                "Unknown Memos resolver",
            )),
        }
    }
    async fn stop(&mut self) -> std::result::Result<(), RpcError> {
        if let Some(core) = self.core.take() {
            core.close().await;
        }
        self.peer = None;
        Ok(())
    }
}
#[tokio::main]
async fn main() -> std::process::ExitCode {
    match serve(Memos::default()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => {
            eprintln!("Memos protocol failed");
            std::process::ExitCode::FAILURE
        }
    }
}
