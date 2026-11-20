use crate::{
    Core, Error, GraphInput, Result,
    agent::{Runtime, Tool, ToolFailure},
    ai::FunctionTool,
    knowledge::{DraftCapability, Resolver},
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone)]
struct DraftRegistration {
    contract: DraftCapability,
    resolver: Arc<dyn Resolver>,
}
#[derive(Clone, Copy)]
enum Operation {
    Schemas,
    Draft,
    Submit,
}
struct DraftTool {
    core: Core,
    operation: Operation,
    registrations: BTreeMap<String, DraftRegistration>,
}
impl DraftTool {
    fn new(core: Core, operation: Operation) -> Result<Self> {
        let registry = core.knowledge();
        let mut registrations = BTreeMap::new();
        for descriptor in registry
            .registry()
            .resolvers()
            .map_err(|_| Error::Unavailable("Resolver registry"))?
        {
            if let Some(contract) = descriptor.draft {
                let resolver = registry
                    .registry()
                    .resolver(&descriptor.id)
                    .map_err(|_| Error::Unavailable("draft Resolver"))?;
                registrations.insert(descriptor.id, DraftRegistration { contract, resolver });
            }
        }
        Ok(Self {
            core,
            operation,
            registrations,
        })
    }
    fn id(&self) -> &'static str {
        match self.operation {
            Operation::Schemas => "get_draft_graph_schema",
            Operation::Draft => "draft_graph",
            Operation::Submit => "submit_graph",
        }
    }
}
pub(super) fn register(runtime: &mut Runtime, core: &Core) -> Result<()> {
    for operation in [Operation::Schemas, Operation::Draft, Operation::Submit] {
        runtime.register(Arc::new(DraftTool::new(core.clone(), operation)?))?;
    }
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemaRequest {
    resolver_types: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftRequest {
    resolver_type: String,
    input: serde_json::Map<String, Value>,
    #[serde(default = "first_local")]
    local_block_id_start: i32,
}
fn first_local() -> i32 {
    -1
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitRequest {
    graph: GraphInput,
}
fn invalid() -> Error {
    Error::InvalidInput("arguments must match the declared graph Tool schema".into())
}

#[async_trait]
impl Tool for DraftTool {
    fn snapshot(&self) -> Option<Arc<dyn Tool>> {
        let bound = Self::new(self.core.clone(), self.operation).unwrap_or_else(|_| Self {
            core: self.core.clone(),
            operation: self.operation,
            registrations: BTreeMap::new(),
        });
        Some(Arc::new(bound))
    }
    fn definition(&self) -> FunctionTool {
        let ids = self.registrations.keys().collect::<Vec<_>>();
        let schema = match self.operation {
            Operation::Schemas => {
                json!({"type":"object","additionalProperties":false,"required":["resolver_types"],"properties":{"resolver_types":{"type":"array","items":{"type":"string","enum":ids}}}})
            }
            Operation::Draft => {
                json!({"type":"object","additionalProperties":false,"required":["resolver_type","input"],"properties":{"resolver_type":{"type":"string","enum":ids},"input":{"type":"object"},"local_block_id_start":{"type":"integer","maximum":-1,"default":-1}}})
            }
            Operation::Submit => {
                json!({"type":"object","additionalProperties":false,"required":["graph"],"properties":{"graph":{"type":"object","additionalProperties":false,"properties":{"blocks":{"type":"array","default":[],"items":{"type":"object","additionalProperties":false,"required":["id","resolver","content"],"properties":{"id":{"type":"integer","maximum":-1},"resolver":{"type":"string"},"content":{"type":"string"},"storage":{"type":["integer","null"],"default":null}}}},"relations":{"type":"array","default":[],"items":{"type":"object","additionalProperties":false,"required":["from_","to_","content"],"properties":{"from_":{"type":"integer","not":{"const":0}},"to_":{"type":"integer","not":{"const":0}},"content":{"type":"string"}}}}}}}})
            }
        };
        let descriptions: BTreeMap<String, String> =
            serde_json::from_str(include_str!("tool-descriptions.json"))
                .expect("canonical Tool descriptions");
        FunctionTool {
            id: self.id().into(),
            description: descriptions[self.id()].clone(),
            input_schema: schema.as_object().expect("object schema").clone(),
        }
    }
    async fn invoke(&self, input: Value) -> std::result::Result<Value, ToolFailure> {
        self.execute(input).await.map_err(|error| match error {
            Error::Database(_) => ToolFailure::Failed,
            _ => ToolFailure::Actionable(
                json!({"error":"graph_command_failed","message":error.to_string()}),
            ),
        })
    }
}
impl DraftTool {
    async fn execute(&self, input: Value) -> Result<Value> {
        match self.operation {
            Operation::Schemas => {
                let request: SchemaRequest =
                    serde_json::from_value(input).map_err(|_| invalid())?;
                let mut resolvers = Vec::with_capacity(request.resolver_types.len());
                for id in request.resolver_types {
                    let registration = self.registrations.get(&id).ok_or_else(invalid)?;
                    resolvers.push(json!({"resolver_type":id,"description":registration.contract.description,"input_schema":registration.contract.input_schema}));
                }
                Ok(json!({"resolvers":resolvers}))
            }
            Operation::Draft => {
                let request: DraftRequest = serde_json::from_value(input).map_err(|_| invalid())?;
                if request.local_block_id_start >= 0 {
                    return Err(invalid());
                }
                let registration = self
                    .registrations
                    .get(&request.resolver_type)
                    .ok_or_else(invalid)?;
                let payload = Value::Object(request.input);
                crate::models::validate_json(&registration.contract.input_schema, &payload)?;
                let mut graph = registration
                    .resolver
                    .draft(&self.core, payload)
                    .await
                    .map_err(Error::from)?;
                graph.validate().map_err(Error::from)?;
                let mut ids = BTreeMap::new();
                for (offset, block) in graph.blocks.iter_mut().enumerate() {
                    let offset = i32::try_from(offset).map_err(|_| invalid())?;
                    let id = request
                        .local_block_id_start
                        .checked_sub(offset)
                        .ok_or_else(invalid)?;
                    ids.insert(block.id, id);
                    block.id = id;
                }
                for relation in &mut graph.relations {
                    relation.from_ = ids.get(&relation.from_).copied().unwrap_or(relation.from_);
                    relation.to_ = ids.get(&relation.to_).copied().unwrap_or(relation.to_);
                }
                Ok(json!({"blocks":graph.blocks,"relations":graph.relations}))
            }
            Operation::Submit => {
                let request: SubmitRequest =
                    serde_json::from_value(input).map_err(|_| invalid())?;
                Ok(json!(self.core.submit_graph(request.graph).await?))
            }
        }
    }
}
