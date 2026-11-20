mod archive;
mod bookmark;
mod capture;
mod graph;
mod merge;
mod official;
mod resolver;
mod schema;
mod setup;
mod twikit;
use async_trait::async_trait;
use inkcre_core::Core;
use inkcre_extension_sdk::{
    Capabilities, ContentContract, ContentKind, Descriptor, Extension, HttpRequest, HttpResponse,
    PROTOCOL_VERSION, RelationSelection, ResolverCapability, ResolverRequest, RpcError,
    SDK_VERSION, SourceCapability, SourceOutcome, SourceRequest, StartContext, serve,
};
use schema::*;
use serde_json::{Value, json};
type Result<T> = std::result::Result<T, RpcError>;
fn invalid(message: &str) -> RpcError {
    RpcError::new("invalid_input", message)
}
fn conflict(message: &str) -> RpcError {
    RpcError::new("twitter_conflict", message)
}
fn provider(message: &str) -> RpcError {
    RpcError::new("provider_error", message)
}
fn db(_: sqlx::Error) -> RpcError {
    RpcError::new("database_error", "Twitter native graph operation failed")
}
#[derive(Default)]
struct Twitter {
    core: Option<Core>,
    peer: uuid::Uuid,
}
impl Twitter {
    fn core(&self) -> Result<&Core> {
        self.core
            .as_ref()
            .ok_or_else(|| conflict("Twitter extension not started"))
    }
    async fn connect(&mut self, peer: &str) -> Result<()> {
        let url = std::env::var("INKCRE_DATABASE_URL")
            .map_err(|_| conflict("Native database required"))?;
        self.core = Some(
            Core::connect(&url, 4)
                .await
                .map_err(|_| conflict("Database admission failed"))?,
        );
        self.peer = peer.parse().map_err(|_| invalid("Invalid Peer identity"))?;
        Ok(())
    }
}
#[async_trait]
impl Extension for Twitter {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            name: EXTENSION.into(),
            version: env!("CARGO_PKG_VERSION").parse().expect("static version"),
            sdk_version: SDK_VERSION.parse().expect("static SDK"),
            protocol_version: PROTOCOL_VERSION,
            config_schema: json!({"type":"object","properties":{
             "backend":{"type":"string","enum":["official","twikit"],"default":"official"},"api_language":{"type":"string","default":"en-US"},"proxy":{"type":["string","null"],"default":null},"client_id":{"type":"string","default":""},"client_secret":{"type":"string","default":""},"email":{"type":"string","default":""},"username":{"type":"string","default":""},"password":{"type":"string","default":""},"totp_secret":{"type":["string","null"],"default":null}
            }}),
            methods: vec!["archive_write".into(), "archive_read".into()],
            capabilities: Capabilities {
                http_ingress: Some(setup::ingress()),
                passive_knowledge: true,
                resolvers: [
                    (LEGACY, "Tweet", true),
                    (POST, "NativeTwitterPost", true),
                    (USER, "NativeTwitterUser", false),
                    (MEDIA, "NativeTwitterMedia", false),
                ]
                .into_iter()
                .map(|(id, model, out)| ResolverCapability {
                    id: id.into(),
                    description: model.into(),
                    content: ContentContract {
                        kind: ContentKind::Json,
                        schema: Some(schemas()[model].clone()),
                    },
                    relations: RelationSelection {
                        include_in: false,
                        include_out: out,
                    },
                    read_methods: vec![],
                    draft: None,
                })
                .collect(),
                sources: vec![SourceCapability {
                    id: SOURCE.into(),
                    description: Some("Twitter Bookmarks over the shared native archive".into()),
                    config_schema: json!({"type":"object","properties":{}}),
                    collect_config_schema: collect_schema(),
                    backfill_config_schema: Some(backfill_schema()),
                }],
                ..Default::default()
            },
        }
    }
    async fn start(&mut self, context: StartContext) -> Result<Value> {
        self.connect(&context.peer_id).await?;
        setup::reconcile(self.core()?).await?;
        Ok(json!({"started":true}))
    }
    async fn start_knowledge(
        &mut self,
        context: inkcre_extension_sdk::KnowledgeStartContext,
    ) -> Result<()> {
        self.connect(&context.peer_id).await
    }
    async fn stop_knowledge(&mut self) -> Result<()> {
        self.stop().await
    }
    async fn stop(&mut self) -> Result<()> {
        if let Some(core) = self.core.take() {
            core.close().await
        }
        Ok(())
    }
    async fn invoke(&mut self, method: &str, parameters: Value) -> Result<Value> {
        match method {
            "archive_write" => {
                let object = parameters
                    .as_object()
                    .ok_or_else(|| invalid("Invalid archive write request"))?;
                if object
                    .keys()
                    .any(|k| !matches!(k.as_str(), "batch" | "text_policy"))
                {
                    return Err(invalid("Unknown archive write field"));
                }
                let policy = parameters["text_policy"].as_str().unwrap_or("replace");
                if !matches!(policy, "replace" | "supplement") {
                    return Err(invalid("Invalid Twitter text policy"));
                }
                let batch = Batch::parse(parameters["batch"].clone())?;
                serde_json::to_value(
                    graph::write(self.core()?, &batch, policy == "supplement").await?,
                )
                .map_err(|_| invalid("Twitter result encoding failed"))
            }
            "archive_read" => graph::read(self.core()?, &parameters).await,
            _ => Err(invalid("Unknown Twitter method")),
        }
    }
    async fn http_request(&mut self, request: HttpRequest) -> Result<HttpResponse> {
        setup::http(self.core()?, self.peer, request).await
    }
    async fn resolver_read(&mut self, request: ResolverRequest) -> Result<Value> {
        resolver::read(self.core()?, request).await
    }
    async fn resolver_read_value(
        &mut self,
        request: ResolverRequest,
    ) -> Result<inkcre_extension_sdk::ResolverValue> {
        resolver::read_value(self.core()?, request)
            .await
            .map(Into::into)
    }
    async fn source_collect(&mut self, request: SourceRequest) -> Result<SourceOutcome> {
        self.source(request, false).await
    }
    async fn source_backfill(&mut self, request: SourceRequest) -> Result<SourceOutcome> {
        self.source(request, true).await
    }
}
impl Twitter {
    async fn source(&self, request: SourceRequest, backfill: bool) -> Result<SourceOutcome> {
        let core = self.core()?;
        let source = core
            .source_get(request.source)
            .await
            .map_err(|_| conflict("Twitter Source read failed"))?
            .ok_or_else(|| conflict("Twitter Source missing"))?;
        if request.type_id != SOURCE || source.type_ != SOURCE {
            return Err(conflict("Twitter Source type changed"));
        }
        Ok(SourceOutcome {
            state: bookmark::run(core, source, request.job, request.config, backfill).await?,
        })
    }
}
#[tokio::main]
async fn main() {
    if serve(Twitter::default()).await.is_err() {
        eprintln!("Twitter extension protocol stopped");
        std::process::exit(1)
    }
}
