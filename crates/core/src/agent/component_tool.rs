use super::{Tool, ToolFailure};
use crate::{Core, ai::FunctionTool, graph_components::ComponentsRequest};
use async_trait::async_trait;
use serde_json::{Value, json};
pub(super) struct ComponentTool(pub Core);
#[async_trait]
impl Tool for ComponentTool {
    fn definition(&self) -> FunctionTool {
        FunctionTool{id:"get_connected_components".into(),description:"Partition seeds by bounded undirected reachability through exact Relation contents.".into(),input_schema:json!({"type":"object","additionalProperties":false,"required":["seed_block_ids","contents"],"properties":{"seed_block_ids":{"type":"array","items":{"type":"integer"}},"contents":{"type":"array","minItems":1,"items":{"type":"string"}},"max_explored_blocks":{"type":"integer","minimum":1,"default":1000},"max_explored_relations":{"type":"integer","minimum":1,"default":10000}}}).as_object().unwrap().clone()}
    }
    async fn invoke(&self, input: Value) -> std::result::Result<Value, ToolFailure> {
        let input: ComponentsRequest = serde_json::from_value(input)
            .map_err(|_| ToolFailure::Actionable(json!({"error":"invalid_arguments"})))?;
        let result = self.0.connected_components(input).await.map_err(|error| {
            ToolFailure::Actionable(
                json!({"error":"invalid_arguments","message":error.to_string()}),
            )
        })?;
        serde_json::to_value(result).map_err(|_| ToolFailure::Failed)
    }
}
