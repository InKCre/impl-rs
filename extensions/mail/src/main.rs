mod adapter;
mod collect;
mod materialize;
mod newsletter;
mod reconcile;
mod resolver;
mod schema;
mod session;
use async_trait::async_trait;
use inkcre_core::Core;
use inkcre_extension_sdk::{
    Capabilities, ContentContract, ContentKind, Descriptor, Extension, HttpAuth, HttpBody,
    HttpHeader, HttpIngress, HttpPathMatch, HttpRequest, HttpResponse, HttpRoute, PROTOCOL_VERSION,
    PeerHttpInbound, RelationSelection, ResolverCapability, ResolverRequest, RpcError, SDK_VERSION,
    SourceCapability, SourceOutcome, SourceRequest, StartContext, serve,
};
use schema::*;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
type Result<T> = std::result::Result<T, RpcError>;
fn fail(message: &str) -> RpcError {
    RpcError::new("mail_error", message)
}
fn schema<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).expect("generated schema")
}
#[derive(Default)]
struct Mail {
    core: Option<Core>,
    config: ExtensionConfig,
}
impl Mail {
    fn core(&self) -> Result<&Core> {
        self.core
            .as_ref()
            .ok_or_else(|| fail("Mail extension is not started"))
    }
}
#[async_trait]
impl Extension for Mail {
    fn descriptor(&self) -> Descriptor {
        let mut config_schema = schema::<Config>();
        config_schema["$defs"]["Parameters"]["properties"]["password"]["format"] =
            json!("password");
        let mut resolvers = vec![];
        for (id, schema) in [
            (EMAIL, schema::<Email>()),
            (MAILBOX, schema::<Mailbox>()),
            (ADDRESS, schema::<Address>()),
            (FLAG, schema::<Flag>()),
            (PART, schema::<MimePart>()),
            (LIST, schema::<MailingList>()),
            (NEWS_HTML, json!({"type":"string"})),
            (NEWS_TEXT, json!({"type":"string"})),
        ] {
            resolvers.push(ResolverCapability {
                id: id.into(),
                description: format!("Canonical Mail {id}"),
                content: ContentContract {
                    kind: if [NEWS_HTML, NEWS_TEXT].contains(&id) {
                        ContentKind::Text
                    } else {
                        ContentKind::Json
                    },
                    schema: Some(schema),
                },
                relations: RelationSelection {
                    include_in: [EMAIL, PART].contains(&id),
                    include_out: [EMAIL, PART, LIST].contains(&id),
                },
                read_methods: vec![],
                draft: None,
            });
        }
        Descriptor {
            name: "inkcre/mail".into(),
            version: env!("CARGO_PKG_VERSION").parse().expect("static version"),
            sdk_version: SDK_VERSION.parse().expect("static SDK"),
            protocol_version: PROTOCOL_VERSION,
            config_schema: schema::<ExtensionConfig>(),
            methods: vec!["materialize_mime_part".into()],
            capabilities: Capabilities {
                passive_knowledge: true,
                resolvers,
                sources: vec![SourceCapability {
                    id: SOURCE.into(),
                    description: Some("Collect ordinary Mail since Source creation or an explicit historical interval over IMAP.".into()),
                    config_schema,
                    collect_config_schema: schema::<Collect>(),
                    backfill_config_schema: Some(schema::<Backfill>()),
                }],
                http_ingress: Some(HttpIngress {
                    routes: vec![HttpRoute {
                        method: "POST".into(), path: MATERIALIZE_PATH.into(),
                        path_match: HttpPathMatch::Exact, auth: HttpAuth::Peer,
                    }],
                    peer_inbounds: vec![PeerHttpInbound {
                        capability: MATERIALIZE.into(), method: "POST".into(),
                        path: MATERIALIZE_PATH.into(),
                    }],
                }),
                ..Default::default()
            },
        }
    }
    async fn start_knowledge(
        &mut self,
        context: inkcre_extension_sdk::KnowledgeStartContext,
    ) -> Result<()> {
        self.start(StartContext {
            peer_id: context.peer_id,
            config: json!({}),
            state: json!({}),
        })
        .await
        .map(|_| ())
    }
    async fn stop_knowledge(&mut self) -> Result<()> {
        self.stop().await
    }
    async fn start(&mut self, context: StartContext) -> Result<Value> {
        self.config = serde_json::from_value(context.config)
            .map_err(|_| fail("Invalid Mail extension configuration"))?;
        let url =
            std::env::var("INKCRE_DATABASE_URL").map_err(|_| fail("Native database required"))?;
        self.core = Some(
            Core::connect(&url, 4)
                .await
                .map_err(|_| fail("Mail database admission failed"))?,
        );
        Ok(json!({"started":true}))
    }
    async fn stop(&mut self) -> Result<()> {
        if let Some(core) = self.core.take() {
            core.close().await
        }
        Ok(())
    }
    async fn source_collect(&mut self, request: SourceRequest) -> Result<SourceOutcome> {
        if request.type_id != SOURCE {
            return Err(fail("Source type changed"));
        }
        Ok(SourceOutcome {
            state: collect::run(
                self.core()?,
                request.source,
                request.config,
                false,
                &self.config.default_excluded_mailboxes,
            )
            .await?,
        })
    }
    async fn source_backfill(&mut self, request: SourceRequest) -> Result<SourceOutcome> {
        if request.type_id != SOURCE {
            return Err(fail("Source type changed"));
        }
        Ok(SourceOutcome {
            state: collect::run(
                self.core()?,
                request.source,
                request.config,
                true,
                &self.config.default_excluded_mailboxes,
            )
            .await?,
        })
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
    async fn invoke(&mut self, method: &str, input: Value) -> Result<Value> {
        if method != "materialize_mime_part" {
            return Err(fail("Unknown Mail method"));
        }
        let input: Materialize =
            serde_json::from_value(input).map_err(|_| fail("Invalid MIME part request"))?;
        serde_json::to_value(materialize::run(self.core()?, input.block).await?)
            .map_err(|_| fail("Cannot represent MIME part content"))
    }
    async fn http_request(&mut self, request: HttpRequest) -> Result<HttpResponse> {
        if request.method != "POST" || request.path != MATERIALIZE_PATH {
            return response(404, json!({"detail":"Mail route not found"}));
        }
        let input: Materialize = match serde_json::from_slice(&request.body.0) {
            Ok(input) => input,
            Err(_) => return response(422, json!({"detail":"Invalid MIME part request"})),
        };
        let core = self.core()?;
        let block = core
            .block_get(input.block)
            .await
            .map_err(|_| fail("Mail database read failed"))?;
        match block {
            None => response(404, json!({"detail":"MIME part Block not found"})),
            Some(block) if block.resolver != PART => {
                response(422, json!({"detail":"Block is not a Mail MIME part"}))
            }
            Some(_) => match materialize::run(core, input.block).await {
                Ok(child) => response(200, json!(child)),
                Err(_) => response(409, json!({"detail":"MIME part is unavailable"})),
            },
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Materialize {
    block: i32,
}
fn response(status: u16, value: Value) -> Result<HttpResponse> {
    Ok(HttpResponse {
        status,
        headers: vec![HttpHeader {
            name: "content-type".into(),
            value: b"application/json".to_vec(),
        }],
        body: HttpBody(
            serde_json::to_vec(&value).map_err(|_| fail("Cannot represent Mail response"))?,
        ),
    })
}
#[tokio::main]
async fn main() {
    if serve(Mail::default()).await.is_err() {
        eprintln!("Mail extension protocol failed");
        std::process::exit(1)
    }
}
