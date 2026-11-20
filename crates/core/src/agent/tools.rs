use super::{Runtime, Tool, ToolFailure};
use crate::{Core, ai::FunctionTool, retrieval::Mode};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub fn register_builtin_tools(runtime: &mut Runtime, core: Core) -> crate::Result<()> {
    runtime.register(Arc::new(ReadTool {
        core: core.clone(),
        retrieve: true,
    }))?;
    runtime.register(Arc::new(ReadTool {
        core: core.clone(),
        retrieve: false,
    }))?;
    runtime.register(Arc::new(super::resolver_tool::ResolverTool(core.clone())))?;
    runtime.register(Arc::new(super::graph_tools::GraphTool {
        core: core.clone(),
        path: true,
    }))?;
    runtime.register(Arc::new(super::graph_tools::GraphTool {
        core: core.clone(),
        path: false,
    }))?;
    runtime.register(Arc::new(super::component_tool::ComponentTool(core)))
}
struct ReadTool {
    core: Core,
    retrieve: bool,
}
fn failed(_: impl std::fmt::Display) -> ToolFailure {
    ToolFailure::Failed
}
fn invalid() -> ToolFailure {
    ToolFailure::Actionable(json!({"error":"invalid_arguments"}))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Retrieve {
    query: String,
    #[serde(default)]
    mode: Mode,
    #[serde(default = "twenty")]
    limit: i64,
}
fn twenty() -> i64 {
    20
}
fn one() -> i64 {
    1
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    #[serde(rename = "type")]
    kind: String,
    id: i32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    #[serde(default)]
    entities: Vec<Reference>,
    #[serde(default = "one")]
    random_count: i64,
}
#[async_trait]
impl Tool for ReadTool {
    fn definition(&self) -> FunctionTool {
        let schema = if self.retrieve {
            json!({"type":"object","additionalProperties":false,"required":["query"],"properties":{"query":{"type":"string"},"mode":{"enum":["lexical","semantic","hybrid"],"default":"hybrid"},"limit":{"type":"integer","minimum":1,"maximum":20,"default":20}}})
        } else {
            json!({"type":"object","additionalProperties":false,"properties":{"entities":{"type":"array","maxItems":20,"default":[],"items":{"type":"object","additionalProperties":false,"required":["type","id"],"properties":{"type":{"enum":["block","relation"]},"id":{"type":"integer"}}}},"random_count":{"type":"integer","minimum":1,"maximum":20,"default":1}}})
        };
        FunctionTool{id:if self.retrieve{"retrieve"}else{"get_entities"}.into(),description:if self.retrieve{"Retrieve lexical, semantic, or separate hybrid results for one query."}else{"Read persisted Blocks or Relations without resolving content. Null may indicate an incorrect entity type."}.into(),input_schema:schema.as_object().unwrap().clone()}
    }
    async fn invoke(&self, input: Value) -> std::result::Result<Value, ToolFailure> {
        if self.retrieve {
            let input: Retrieve = serde_json::from_value(input).map_err(|_| invalid())?;
            if input.query.trim().is_empty() || !(1..=20).contains(&input.limit) {
                return Err(invalid());
            }
            let branches = self
                .core
                .retrieve(&input.query, input.mode, input.limit)
                .await;
            let mut result = serde_json::Map::new();
            if let Some(branch) = branches.lexical {
                if let Ok(result) = &branch {
                    self.core.record_event("inkcre.retrieval.candidates", json!({
                        "inkcre.retrieval.method": "lexical",
                        "inkcre.entity.block_ids": result.matches.iter().map(|m|m.block.id).collect::<Vec<_>>(),
                        "inkcre.entity.relation_ids": [],
                        "inkcre.retrieval.candidate_count": result.matches.len(),
                    }).as_object().expect("metadata object").clone());
                }
                result.insert("lexical".into(),match branch{Err(error)=>json!({"error":"LexicalRetrievalError","message":error.to_string()}),Ok(branch)=>json!({"matches":branch.matches.into_iter().map(|m|json!({"entity":{"entity_type":"block","entity_id":m.block.id},"label":m.label,"excerpt":m.excerpt,"evidence":m.evidence,"rank":m.rank})).collect::<Vec<_>>()})});
            }
            if let Some(branch) = branches.semantic {
                if let Ok(result) = &branch {
                    self.core.record_event("inkcre.retrieval.candidates", json!({
                        "inkcre.retrieval.method": "semantic",
                        "inkcre.entity.block_ids": result.matches.iter().filter(|m|m.kind=="block").filter_map(|m|m.entity["id"].as_i64()).collect::<Vec<_>>(),
                        "inkcre.entity.relation_ids": result.matches.iter().filter(|m|m.kind=="relation").filter_map(|m|m.entity["id"].as_i64()).collect::<Vec<_>>(),
                        "inkcre.retrieval.candidate_count": result.matches.len(),
                    }).as_object().expect("metadata object").clone());
                }
                result.insert("semantic".into(),match branch{Err(error)=>json!({"error":"SemanticRetrievalError","message":error.to_string()}),Ok(branch)=>json!({"profile":branch.profile,"metric":branch.metric,"matches":branch.matches.into_iter().map(|m|json!({"entity":{"entity_type":m.kind,"entity_id":m.entity["id"]},"score":m.score})).collect::<Vec<_>>()})});
            }
            return Ok(Value::Object(result));
        }
        let input: Selection = serde_json::from_value(input).map_err(|_| invalid())?;
        if input.entities.len() > 20
            || !(1..=20).contains(&input.random_count)
            || (!input.entities.is_empty() && input.random_count != 1)
        {
            return Err(invalid());
        }
        if input.entities.is_empty() {
            let rows:Vec<crate::Block>=sqlx::query_as("SELECT id,storage,resolver,content,created_at,updated_at FROM inkcre.blocks ORDER BY random() LIMIT $1").bind(input.random_count).fetch_all(self.core.pool()).await.map_err(failed)?;
            self.core.record_event(
                "inkcre.entity.read",
                json!({
                    "inkcre.entity.read.kind": "persisted",
                    "inkcre.entity.block_ids": rows.iter().map(|b|b.id).collect::<Vec<_>>(),
                    "inkcre.entity.relation_ids": [],
                    "inkcre.entity.read.count": rows.len(),
                })
                .as_object()
                .expect("metadata object")
                .clone(),
            );
            return serde_json::to_value(rows).map_err(failed);
        }
        if input
            .entities
            .iter()
            .any(|r| r.kind != "block" && r.kind != "relation")
        {
            return Err(invalid());
        }
        let blocks = input
            .entities
            .iter()
            .filter(|r| r.kind == "block")
            .map(|r| r.id)
            .collect::<Vec<_>>();
        let relations = input
            .entities
            .iter()
            .filter(|r| r.kind == "relation")
            .map(|r| r.id)
            .collect::<Vec<_>>();
        let records = self
            .core
            .entity_records(&blocks, &relations)
            .await
            .map_err(failed)?;
        let block_ids = input
            .entities
            .iter()
            .filter(|r| r.kind == "block" && records.blocks.contains_key(&r.id))
            .map(|r| r.id)
            .collect::<Vec<_>>();
        let relation_ids = input
            .entities
            .iter()
            .filter(|r| r.kind == "relation" && records.relations.contains_key(&r.id))
            .map(|r| r.id)
            .collect::<Vec<_>>();
        self.core.record_event(
            "inkcre.entity.read",
            json!({
                "inkcre.entity.read.kind": "persisted",
                "inkcre.entity.block_ids": block_ids,
                "inkcre.entity.relation_ids": relation_ids,
                "inkcre.entity.read.count": block_ids.len()+relation_ids.len(),
            })
            .as_object()
            .expect("metadata object")
            .clone(),
        );
        Ok(Value::Array(
            input
                .entities
                .into_iter()
                .map(|r| {
                    if r.kind == "block" {
                        records
                            .blocks
                            .get(&r.id)
                            .map(|b| json!(b))
                            .unwrap_or(Value::Null)
                    } else {
                        records
                            .relations
                            .get(&r.id)
                            .map(|b| json!(b))
                            .unwrap_or(Value::Null)
                    }
                })
                .collect(),
        ))
    }
}
