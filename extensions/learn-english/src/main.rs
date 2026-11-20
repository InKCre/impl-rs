use async_trait::async_trait;
use inkcre_core::knowledge::{decode_unicode_bytes, format_label};
use inkcre_extension_sdk::{
    Capabilities, Content, ContentContract, ContentKind, Descriptor, DraftBlock, DraftCapability,
    Extension, GraphDraft, PROTOCOL_VERSION, ResolverCapability, ResolverDraftRequest,
    ResolverOperation, ResolverRequest, RpcError, SDK_VERSION, StartContext, serve,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const RESOLVER: &str = "extensions.learn_english.lexical.v1";
type Result<T> = std::result::Result<T, RpcError>;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum PartOfSpeech {
    Noun,
    Verb,
    Adjective,
    Adverb,
    Pronoun,
    Preposition,
    Conjunction,
    Interjection,
    Determiner,
    Article,
    Numeral,
    Particle,
}

#[derive(Deserialize, Serialize)]
struct LexicalItem {
    text: String,
    #[serde(default)]
    pos: Vec<PartOfSpeech>,
}

fn schema() -> Value {
    json!({"type":"object","required":["text"],"properties":{
        "text":{"type":"string"},
        "pos":{"type":"array","default":[],"items":{"enum":["noun","verb","adjective",
            "adverb","pronoun","preposition","conjunction","interjection","determiner",
            "article","numeral","particle"]}}
    }})
}

fn invalid() -> RpcError {
    RpcError::new("invalid_content", "Invalid lexical item")
}

struct LearnEnglish;

#[async_trait]
impl Extension for LearnEnglish {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            name: "inkcre/learn-english".into(),
            version: env!("CARGO_PKG_VERSION").parse().expect("static version"),
            sdk_version: SDK_VERSION.parse().expect("static SDK version"),
            protocol_version: PROTOCOL_VERSION,
            config_schema: json!({"type":"object","properties":{}}),
            methods: vec![],
            capabilities: Capabilities {
                resolvers: vec![ResolverCapability {
                    id: RESOLVER.into(),
                    description: "An English word, phrase, or idiom".into(),
                    content: ContentContract {
                        kind: ContentKind::Json,
                        schema: Some(schema()),
                    },
                    relations: Default::default(),
                    read_methods: vec![],
                    draft: Some(DraftCapability {
                        description:
                            "Create one English lexical item with optional parts of speech".into(),
                        input_schema: schema(),
                    }),
                }],
                ..Default::default()
            },
        }
    }

    async fn start(&mut self, _context: StartContext) -> Result<Value> {
        Ok(json!({"started":true}))
    }

    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }

    async fn invoke(&mut self, _method: &str, _params: Value) -> Result<Value> {
        Err(RpcError::new(
            "unknown_method",
            "Learn English uses a typed Resolver",
        ))
    }

    async fn resolver_read(&mut self, request: ResolverRequest) -> Result<Value> {
        let raw = match request.input.content {
            Content::Text(text) => text,
            Content::Bytes(bytes) => decode_unicode_bytes(&bytes).map_err(|_| invalid())?,
        };
        let item: LexicalItem = serde_json::from_str(&raw).map_err(|_| invalid())?;
        match request.operation {
            ResolverOperation::Solve { .. } => serde_json::to_value(item).map_err(|_| invalid()),
            ResolverOperation::Text { .. } => Ok(Value::String(item.text)),
            ResolverOperation::Label { .. } => Ok(Value::String(format_label(
                "lexical item",
                Some(&item.text),
                false,
            ))),
            ResolverOperation::Read { .. } => Err(RpcError::new(
                "unknown_method",
                "Unknown lexical read method",
            )),
        }
    }

    async fn resolver_draft(&mut self, request: ResolverDraftRequest) -> Result<GraphDraft> {
        let item: LexicalItem = serde_json::from_value(request.input).map_err(|_| invalid())?;
        Ok(GraphDraft {
            root: -1,
            blocks: vec![DraftBlock {
                id: -1,
                resolver: RESOLVER.into(),
                storage: None,
                content: serde_json::to_string(&item).map_err(|_| invalid())?,
            }],
            relations: vec![],
        })
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    if serve(LearnEnglish).await.is_err() {
        eprintln!("Learn English protocol failed");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
