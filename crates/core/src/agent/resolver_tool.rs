use super::{Tool, ToolFailure};
use crate::{Core, ai::FunctionTool};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
pub(super) struct ResolverTool(pub Core);
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Call {
    block_id: i32,
    method: String,
    #[serde(default = "object")]
    arguments: Value,
}
fn object() -> Value {
    json!({})
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    action: String,
    #[serde(default)]
    resolver_types: Vec<String>,
    #[serde(default)]
    block_ids: Vec<i32>,
    #[serde(default)]
    calls: Vec<Call>,
}
fn failure(_: impl std::fmt::Display) -> ToolFailure {
    ToolFailure::Failed
}
fn invalid() -> ToolFailure {
    ToolFailure::Actionable(json!({"error":"invalid_arguments"}))
}
#[async_trait]
impl Tool for ResolverTool {
    fn definition(&self) -> FunctionTool {
        FunctionTool{id:"resolver".into(),description:"Describe or invoke public typed read methods on exact Block Resolvers.".into(),input_schema:json!({"type":"object","additionalProperties":false,"required":["action"],"properties":{"action":{"enum":["describe","invoke"]},"resolver_types":{"type":"array","items":{"type":"string"},"default":[]},"block_ids":{"type":"array","items":{"type":"integer"},"default":[]},"calls":{"type":"array","maxItems":20,"default":[],"items":{"type":"object","additionalProperties":false,"required":["block_id","method"],"properties":{"block_id":{"type":"integer"},"method":{"type":"string"},"arguments":{"type":"object","default":{}}}}}}}).as_object().unwrap().clone()}
    }
    async fn invoke(&self, input: Value) -> std::result::Result<Value, ToolFailure> {
        let input: Input = serde_json::from_value(input).map_err(|_| invalid())?;
        let knowledge = self.0.knowledge();
        if input.action == "describe" {
            if !input.calls.is_empty() {
                return Err(invalid());
            }
            let records = self
                .0
                .entity_records(&input.block_ids, &[])
                .await
                .map_err(failure)?;
            let mut ids = input.resolver_types.into_iter().collect::<BTreeSet<_>>();
            ids.extend(records.blocks.values().map(|b| b.resolver.clone()));
            if input.block_ids.is_empty() && ids.is_empty() {
                ids.extend(
                    knowledge
                        .registry()
                        .resolvers()
                        .map_err(failure)?
                        .into_iter()
                        .map(|r| r.id),
                );
            }
            let mut results = Vec::new();
            let mut missing = Vec::new();
            for id in ids {
                match knowledge.read_methods(&id) {
                    Ok(methods) => results.push(json!({"resolver":id,"methods":methods})),
                    Err(_) => missing.push(id),
                }
            }
            let absent = input
                .block_ids
                .into_iter()
                .filter(|id| !records.blocks.contains_key(id))
                .collect::<BTreeSet<_>>();
            return Ok(
                json!({"results":results,"missing_blocks":absent,"missing_resolvers":missing}),
            );
        }
        if input.action != "invoke"
            || !input.resolver_types.is_empty()
            || !input.block_ids.is_empty()
            || input.calls.is_empty()
            || input.calls.len() > 20
        {
            return Err(invalid());
        }
        let mut results = Vec::new();
        for (index, call) in input.calls.into_iter().enumerate() {
            let mut row = json!({"index":index,"block_id":call.block_id,"method":call.method});
            match self.0.block_get(call.block_id).await.map_err(failure)? {
                None => row["error"] = json!("not_found"),
                Some(block) => match knowledge.read_methods(&block.resolver) {
                    Err(_) => row["error"] = json!("resolver_unavailable"),
                    Ok(methods) => match methods.iter().find(|m| m.name == call.method) {
                        None => {
                            row["error"] = json!("method_unavailable");
                            row["available_methods"] =
                                json!(methods.iter().map(|m| &m.name).collect::<Vec<_>>());
                        }
                        Some(contract) => {
                            if !call.arguments.is_object()
                                || !jsonschema::is_valid(&contract.input_schema, &call.arguments)
                            {
                                row["error"] = json!("invalid_arguments");
                                row["input_schema"] = contract.input_schema.clone();
                            } else {
                                match knowledge
                                    .invoke(call.block_id, &call.method, call.arguments)
                                    .await
                                {
                                    Ok(value) => {
                                        self.0.record_event(
                                            "inkcre.entity.read",
                                            json!({
                                                "inkcre.entity.read.kind": "resolved",
                                                "inkcre.entity.block_ids": [call.block_id],
                                                "inkcre.entity.read.count": 1,
                                            })
                                            .as_object()
                                            .expect("metadata object")
                                            .clone(),
                                        );
                                        row["result"] = value;
                                    }
                                    Err(error) => {
                                        row["error"] = json!("resolver_read_failed");
                                        row["message"] = json!(error.to_string());
                                    }
                                }
                            }
                        }
                    },
                },
            }
            results.push(row);
        }
        Ok(json!({"results":results}))
    }
}
