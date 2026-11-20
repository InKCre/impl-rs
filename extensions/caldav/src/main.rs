mod codec;
mod graph;
mod projection;
mod properties;
mod protocol;
mod report;
mod resolver;
mod schema;
mod temporal;

use async_trait::async_trait;
use inkcre_core::Core;
use inkcre_extension_sdk::{
    Capabilities, ContentContract, ContentKind, Descriptor, Extension, HttpAuth, HttpIngress,
    HttpPathMatch, HttpRequest, HttpResponse, HttpRoute, KnowledgeStartContext, PROTOCOL_VERSION,
    PeerHttpInbound, RelationSelection, ResolverCapability, ResolverRequest, RpcError, SDK_VERSION,
    StartContext, serve,
};
use schema::*;
use serde_json::{Value, json};

type Result<T> = std::result::Result<T, Error>;
#[derive(Debug)]
enum Error {
    Invalid(&'static str),
    Integrity(&'static str),
    Database,
    Json,
    Core,
}
impl From<inkcre_core::Error> for Error {
    fn from(_: inkcre_core::Error) -> Self {
        Self::Core
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
            Error::Integrity(message) => Self::new("graph_integrity", message),
            Error::Json => Self::new("invalid_content", "invalid DAV persisted content"),
            Error::Database | Error::Core => {
                Self::new("database_error", "DAV native database operation failed")
            }
        }
    }
}

#[derive(Default)]
struct CalDav {
    core: Option<Core>,
    peer: Option<uuid::Uuid>,
}
impl CalDav {
    fn core(&self) -> std::result::Result<&Core, RpcError> {
        self.core
            .as_ref()
            .ok_or_else(|| RpcError::new("not_started", "CalDAV extension is not started"))
    }
    async fn connection(&self) -> std::result::Result<HttpResponse, RpcError> {
        let core = self.core()?;
        let config: Option<Value> =
            sqlx::query_scalar("SELECT config FROM inkcre.peers WHERE id=$1")
                .bind(self.peer)
                .fetch_optional(core.pool())
                .await
                .map_err(Error::from)?;
        let Some(base) = config
            .as_ref()
            .and_then(|v| v["http_public_base_url"].as_str())
            .filter(|v| !v.is_empty())
        else {
            return Ok(protocol::response(
                409,
                b"Configure this Core's Public HTTP Base URL.".to_vec(),
                "text/plain; charset=utf-8",
            ));
        };
        Ok(protocol::response(200,serde_json::to_vec(&json!({"server_url":format!("{}/caldav/",base.trim_end_matches('/')),"username":"inkcre"})).map_err(Error::from)?,"application/json"))
    }
}
fn capability(id: &str) -> ResolverCapability {
    let schema = if [COLLECTION, LEGACY_COLLECTION].contains(&id) {
        json!({"type":"object","additionalProperties":false,"required":["path","props"],"properties":{"path":{"type":"string"},"props":{"type":"object","additionalProperties":{"type":"string"}},"published":{"type":"boolean","default":true}}})
    } else {
        let legacy = [LEGACY_CALENDAR, LEGACY_CONTACT].contains(&id);
        let field = if legacy { "text" } else { "remainder" };
        json!({"type":"object","additionalProperties":false,"required":["href",field],"properties":{"href":{"type":"string"},(field):{"type":"string"}}})
    };
    ResolverCapability {
        id: id.into(),
        description: "Native DAV publication and semantic property projection".into(),
        content: ContentContract {
            kind: ContentKind::Json,
            schema: Some(schema),
        },
        relations: RelationSelection {
            include_in: false,
            include_out: true,
        },
        read_methods: vec![],
        draft: None,
    }
}
#[async_trait]
impl Extension for CalDav {
    fn descriptor(&self) -> Descriptor {
        let connection = HttpRoute {
            method: "GET".into(),
            path: "/caldav/connection".into(),
            path_match: HttpPathMatch::Exact,
            auth: HttpAuth::Peer,
        };
        let mut routes = vec![connection];
        routes.extend(protocol::METHODS.into_iter().map(|method| HttpRoute {
            method: method.into(),
            path: "/caldav".into(),
            path_match: HttpPathMatch::Prefix,
            auth: HttpAuth::Protocol,
        }));
        Descriptor {
            name: NAME.into(),
            version: env!("CARGO_PKG_VERSION").parse().expect("static version"),
            sdk_version: SDK_VERSION.parse().expect("static SDK version"),
            protocol_version: PROTOCOL_VERSION,
            config_schema: json!({"title":"CalDAVConfig","type":"object","additionalProperties":false,"properties":{"password":{"anyOf":[{"type":"string","minLength":32,"maxLength":128,"format":"password"},{"type":"null"}],"default":null,"description":"DAV 专用密码；用户名为 inkcre，清空密码撤销访问。"}}}),
            methods: vec![],
            capabilities: Capabilities {
                passive_knowledge: true,
                http_ingress: Some(HttpIngress {
                    routes,
                    peer_inbounds: vec![PeerHttpInbound {
                        capability: "caldav.connection.v1".into(),
                        method: "GET".into(),
                        path: "/caldav/connection".into(),
                    }],
                }),
                resolvers: RESOLVERS.into_iter().map(capability).collect(),
                ..Default::default()
            },
        }
    }
    async fn start(&mut self, context: StartContext) -> std::result::Result<Value, RpcError> {
        let config: Config = serde_json::from_value(context.config).map_err(Error::from)?;
        config.validate()?;
        let url = std::env::var("INKCRE_DATABASE_URL").map_err(|_| {
            RpcError::new("missing_database", "native database capability is required")
        })?;
        let core = Core::connect(&url, 4).await.map_err(Error::from)?;
        graph::migrate(&core).await?;
        self.peer = uuid::Uuid::parse_str(&context.peer_id).ok();
        self.core = Some(core);
        Ok(json!({"started":true}))
    }
    async fn start_knowledge(
        &mut self,
        _context: KnowledgeStartContext,
    ) -> std::result::Result<(), RpcError> {
        let url = std::env::var("INKCRE_DATABASE_URL").map_err(|_| {
            RpcError::new("missing_database", "native database capability is required")
        })?;
        self.core = Some(Core::connect(&url, 4).await.map_err(Error::from)?);
        Ok(())
    }
    async fn stop_knowledge(&mut self) -> std::result::Result<(), RpcError> {
        if let Some(core) = self.core.take() {
            core.close().await;
        }
        Ok(())
    }
    async fn http_request(
        &mut self,
        request: HttpRequest,
    ) -> std::result::Result<HttpResponse, RpcError> {
        if request.method == "GET" && request.path == "/caldav/connection" {
            self.connection().await
        } else {
            protocol::handle(self.core()?, request)
                .await
                .map_err(Into::into)
        }
    }
    async fn resolver_read(
        &mut self,
        request: ResolverRequest,
    ) -> std::result::Result<Value, RpcError> {
        resolver::read(self.core()?, request)
            .await
            .map_err(Into::into)
    }
    async fn invoke(
        &mut self,
        _method: &str,
        _params: Value,
    ) -> std::result::Result<Value, RpcError> {
        Err(RpcError::new(
            "unknown_method",
            "CalDAV has no generic methods",
        ))
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
    match serve(CalDav::default()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => {
            eprintln!("CalDAV extension stopped with a protocol or I/O failure");
            std::process::ExitCode::FAILURE
        }
    }
}
