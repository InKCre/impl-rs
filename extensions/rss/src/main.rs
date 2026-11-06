mod collect;
mod enrichment;
mod parser;
mod reconcile;
mod resolver;
mod schema;

use async_trait::async_trait;
use inkcre_core::Core;
use inkcre_extension_sdk::{
    Capabilities, ContentContract, ContentKind, Descriptor, Extension, PROTOCOL_VERSION,
    RelationSelection, ResolverCapability, ResolverRequest, RpcError, SDK_VERSION,
    SourceCapability, SourceOutcome, SourceRequest, StartContext, serve,
};
use serde::Deserialize;
use serde_json::{Value, json};

use schema::*;

type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
enum Error {
    Invalid(&'static str),
    Http(&'static str),
    Integrity(&'static str),
    Conflict(&'static str),
    Core,
    Database,
    Json,
    Knowledge,
}

impl From<inkcre_core::Error> for Error {
    fn from(_: inkcre_core::Error) -> Self {
        Self::Core
    }
}
impl From<inkcre_core::knowledge::Error> for Error {
    fn from(_: inkcre_core::knowledge::Error) -> Self {
        Self::Knowledge
    }
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
            Error::Invalid(message) => Self::new("invalid_input", message),
            Error::Http(message) => Self::new("http_error", message),
            Error::Integrity(message) => Self::new("graph_integrity", message),
            Error::Conflict(message) => Self::new("conflict", message),
            Error::Core | Error::Database => {
                Self::new("database_error", "RSS database operation failed")
            }
            Error::Json => Self::new("representation_error", "RSS content cannot be represented"),
            Error::Knowledge => Self::new("knowledge_error", "RSS knowledge operation failed"),
        }
    }
}

#[derive(Default)]
struct Rss {
    core: Option<Core>,
}

impl Rss {
    fn core(&self) -> std::result::Result<&Core, RpcError> {
        self.core
            .as_ref()
            .ok_or_else(|| RpcError::new("not_started", "RSS extension is not started"))
    }
}

fn resolver_capability(
    id: &str,
    description: &str,
    schema: Value,
    include_out: bool,
) -> ResolverCapability {
    ResolverCapability {
        id: id.into(),
        description: description.into(),
        content: ContentContract {
            kind: ContentKind::Json,
            schema: Some(schema),
        },
        relations: RelationSelection {
            include_in: false,
            include_out,
        },
        read_methods: vec![],
        draft: None,
    }
}

#[async_trait]
impl Extension for Rss {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            name: EXTENSION_NAME.into(),
            version: env!("CARGO_PKG_VERSION")
                .parse()
                .expect("static extension version"),
            sdk_version: SDK_VERSION.parse().expect("static SDK version"),
            protocol_version: PROTOCOL_VERSION,
            config_schema: json!({"type":"object","properties":{}}),
            methods: vec![
                "materialize_full_text".into(),
                "materialize_enclosure".into(),
            ],
            capabilities: Capabilities {
                sources: [
                    (
                        RSS_SOURCE,
                        "Collect one configured RSS 2.0 feed through the shared feed service.",
                    ),
                    (
                        ATOM_SOURCE,
                        "Collect one configured Atom feed through the shared feed service.",
                    ),
                ]
                .into_iter()
                .map(|(id, description)| SourceCapability {
                    id: id.into(),
                    description: Some(description.into()),
                    config_schema: FeedConfig::schema(),
                    collect_config_schema: CollectConfig::schema(),
                    backfill_config_schema: None,
                })
                .collect(),
                resolvers: vec![
                    resolver_capability(
                        FEED_RESOLVER,
                        "Canonical feed metadata",
                        CanonicalFeed::schema(),
                        false,
                    ),
                    resolver_capability(
                        ITEM_RESOLVER,
                        "Feed item and graph-owned content projections",
                        CanonicalItem::schema(),
                        true,
                    ),
                    resolver_capability(
                        ENCLOSURE_RESOLVER,
                        "Protocol-authored enclosure metadata",
                        CanonicalEnclosure::schema(),
                        true,
                    ),
                ],
                ..Default::default()
            },
        }
    }
    async fn start(&mut self, _context: StartContext) -> std::result::Result<Value, RpcError> {
        let url = std::env::var("INKCRE_DATABASE_URL").map_err(|_| {
            RpcError::new("missing_database", "native database capability is required")
        })?;
        self.core = Some(
            Core::connect(&url, 4)
                .await
                .map_err(|error| RpcError::from(Error::from(error)))?,
        );
        Ok(json!({"started":true}))
    }
    async fn source_collect(
        &mut self,
        request: SourceRequest,
    ) -> std::result::Result<SourceOutcome, RpcError> {
        let core = self.core()?;
        let source = core
            .source_get(request.source)
            .await
            .map_err(Error::from)?
            .ok_or_else(|| RpcError::new("source_not_found", "Source no longer exists"))?;
        if source.type_ != request.type_id {
            return Err(RpcError::new(
                "source_type_changed",
                "Source type no longer matches the claimed command",
            ));
        }
        let family = Family::from_type(&source.type_).map_err(RpcError::from)?;
        let config: CollectConfig = serde_json::from_value(request.config)
            .map_err(|_| RpcError::new("invalid_config", "RSS collect configuration is invalid"))?;
        let state = collect::run(core, source, family, request.job, config)
            .await
            .map_err(RpcError::from)?;
        Ok(SourceOutcome { state })
    }
    async fn resolver_read(
        &mut self,
        request: ResolverRequest,
    ) -> std::result::Result<Value, RpcError> {
        resolver::read(self.core()?, request)
            .await
            .map_err(RpcError::from)
    }
    async fn invoke(
        &mut self,
        method: &str,
        params: Value,
    ) -> std::result::Result<Value, RpcError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Item {
            item_block_id: i32,
            #[serde(default)]
            refresh: bool,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Enclosure {
            enclosure_block_id: i32,
            target_storage_id: i32,
        }
        match method {
            "materialize_full_text" => {
                let input: Item = serde_json::from_value(params)
                    .map_err(|_| RpcError::new("invalid_params", "invalid full-text command"))?;
                Ok(serde_json::to_value(
                    enrichment::full_text(self.core()?, input.item_block_id, input.refresh, false)
                        .await
                        .map_err(RpcError::from)?,
                )
                .map_err(Error::from)?)
            }
            "materialize_enclosure" => {
                let input: Enclosure = serde_json::from_value(params)
                    .map_err(|_| RpcError::new("invalid_params", "invalid enclosure command"))?;
                Ok(serde_json::to_value(
                    enrichment::enclosure(
                        self.core()?,
                        input.enclosure_block_id,
                        input.target_storage_id,
                    )
                    .await
                    .map_err(RpcError::from)?,
                )
                .map_err(Error::from)?)
            }
            _ => Err(RpcError::new("unknown_method", "unknown RSS method")),
        }
    }
    async fn stop(&mut self) -> std::result::Result<(), RpcError> {
        if let Some(core) = self.core.take() {
            core.close().await;
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match serve(Rss::default()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => {
            eprintln!("RSS extension process stopped with a protocol or I/O failure");
            std::process::ExitCode::FAILURE
        }
    }
}
