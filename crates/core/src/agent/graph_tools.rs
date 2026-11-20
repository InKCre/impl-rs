use super::{Tool, ToolFailure};
use crate::{Core, Direction, PathOptions, ai::FunctionTool};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
pub(super) struct GraphTool {
    pub core: Core,
    pub path: bool,
}
fn both() -> Direction {
    Direction::Both
}
fn limit() -> i64 {
    20
}
fn hops() -> u8 {
    4
}
fn budget() -> u32 {
    1000
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Path {
    from_block_id: i32,
    to_block_id: i32,
    #[serde(default = "both")]
    direction: Direction,
    #[serde(default)]
    contents: Vec<String>,
    #[serde(default = "hops")]
    max_hops: u8,
    #[serde(default = "budget")]
    max_explored_blocks: u32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Neighborhood {
    entity_type: String,
    entity_id: i32,
    #[serde(default = "both")]
    direction: Direction,
    #[serde(default)]
    contents: Vec<String>,
    #[serde(default = "limit")]
    limit: i64,
    cursor: Option<i32>,
}
fn failed(error: impl std::fmt::Display) -> ToolFailure {
    ToolFailure::Actionable(json!({"error":"graph_read_failed","message":error.to_string()}))
}
fn invalid() -> ToolFailure {
    ToolFailure::Actionable(json!({"error":"invalid_arguments"}))
}
#[async_trait]
impl Tool for GraphTool {
    fn definition(&self) -> FunctionTool {
        let mut properties = json!({"direction":{"enum":["in","out","both"],"default":"both"},"contents":{"type":"array","items":{"type":"string"},"default":[]}});
        let required = if self.path {
            properties["from_block_id"] = json!({"type":"integer"});
            properties["to_block_id"] = json!({"type":"integer"});
            properties["max_hops"] = json!({"type":"integer","minimum":0,"maximum":8,"default":4});
            properties["max_explored_blocks"] =
                json!({"type":"integer","minimum":1,"maximum":10000,"default":1000});
            vec!["from_block_id", "to_block_id"]
        } else {
            properties["entity_type"] = json!({"enum":["block","relation"]});
            properties["entity_id"] = json!({"type":"integer"});
            properties["limit"] = json!({"type":"integer","minimum":1,"maximum":100,"default":20});
            properties["cursor"] = json!({"type":["integer","null"]});
            vec!["entity_type", "entity_id"]
        };
        FunctionTool{id:if self.path{"find_path"}else{"get_entity_neighborhood"}.into(),description:if self.path{"Find a bounded graph path; an exploration limit is not proof of absence."}else{"Read a Block's direct neighborhood or a Relation with its endpoints."}.into(),input_schema:json!({"type":"object","additionalProperties":false,"required":required,"properties":properties}).as_object().unwrap().clone()}
    }
    async fn invoke(&self, input: Value) -> std::result::Result<Value, ToolFailure> {
        if self.path {
            let p: Path = serde_json::from_value(input).map_err(|_| invalid())?;
            if p.max_hops > 8 {
                return Err(invalid());
            }
            let result = self
                .core
                .find_path(
                    p.from_block_id,
                    p.to_block_id,
                    PathOptions {
                        direction: p.direction,
                        contents: p.contents,
                        max_hops: p.max_hops,
                        max_explored_blocks: p.max_explored_blocks,
                    },
                )
                .await
                .map_err(failed)?;
            let value = serde_json::to_value(result).map_err(failed)?;
            return Ok(if value["outcome"] == "found" {
                json!({"status":"found","graph":value["graph"],"block_path":value["block_ids"],"relation_path":value["relation_ids"]})
            } else {
                json!({"status":value["outcome"]})
            });
        }
        let p: Neighborhood = serde_json::from_value(input.clone()).map_err(|_| invalid())?;
        if p.entity_type == "relation"
            && input.as_object().is_some_and(|object| {
                object
                    .keys()
                    .any(|key| key != "entity_type" && key != "entity_id")
            })
        {
            return Err(invalid());
        }
        match p.entity_type.as_str() {
            "block" => {
                if !(1..=100).contains(&p.limit) {
                    return Err(invalid());
                }
                let result = self
                    .core
                    .block_neighborhood(p.entity_id, p.direction, &p.contents, p.limit, p.cursor)
                    .await
                    .map_err(failed)?;
                Ok(result.map(|n|json!({"focal_block":p.entity_id,"graph":{"blocks":n.blocks,"relations":n.relations},"next_cursor":n.next_cursor})).unwrap_or(Value::Null))
            }
            "relation" => Ok(self
                .core
                .relation_neighborhood(p.entity_id)
                .await
                .map_err(failed)?
                .map(|graph| json!({"focal_relation":p.entity_id,"graph":graph}))
                .unwrap_or(Value::Null)),
            _ => Err(invalid()),
        }
    }
}
