//! MCP's per-item correlation and preview contracts; Core owns the business reads.

use super::{
    Result,
    delivery::{ToolOutput, deliver},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use inkcre_core::{
    Block, Core, Direction, HydratedContent, PathOptions, PathOutcome, Relation,
    knowledge::{self, ReadOptions, ReadValue},
};
use serde_json::{Value, json};
use std::collections::HashSet;

fn error(mut correlation: Value, code: &str, message: &str) -> Value {
    correlation["error"] = json!({"code":code,"message":message});
    correlation
}
fn knowledge_error(correlation: Value, failure: knowledge::Error, invoking: bool) -> Value {
    let code = match (invoking, failure) {
        (
            _,
            knowledge::Error::UnknownResolver(_)
            | knowledge::Error::UnknownMethod(_)
            | knowledge::Error::Unsupported(_),
        ) => "unavailable",
        (
            true,
            knowledge::Error::InvalidInput(_)
            | knowledge::Error::Core(inkcre_core::Error::InvalidInput(_)),
        ) => "invalid_arguments",
        _ => "failed",
    };
    error(
        correlation,
        code,
        match code {
            "unavailable" => "Requested Resolver capability is unavailable",
            "invalid_arguments" => "Resolver method arguments are invalid",
            _ => "Requested content could not be read",
        },
    )
}
pub(super) fn block_preview(block: &Block) -> Value {
    json!({"id":block.id,"resolver":block.resolver,"storage":block.storage,"created_at":block.created_at,"updated_at":block.updated_at})
}
pub(super) fn relation_preview(relation: &Relation) -> Value {
    json!({"id":relation.id,"from_block_id":relation.from_,"to_block_id":relation.to_,"content":relation.content,"updated_at":relation.updated_at})
}
fn graph(blocks: &[Block], relations: &[Relation]) -> Value {
    json!({"blocks":blocks.iter().map(block_preview).collect::<Vec<_>>(),"relations":relations.iter().map(relation_preview).collect::<Vec<_>>()})
}
fn entity(reference: &str) -> Option<(&str, i32)> {
    let (kind, id) = reference.split_once(':')?;
    if !matches!(kind, "block" | "relation") {
        return None;
    }
    Some((kind, id.trim().parse().ok()?))
}
fn block_id(value: &Value) -> Option<i32> {
    value.as_i64().and_then(|id| i32::try_from(id).ok())
}

pub(super) async fn read_value(
    core: &Core,
    block: &Block,
    mode: &str,
) -> knowledge::Result<ReadValue> {
    match mode {
        "raw" => Ok(ReadValue::Json(json!(block.content))),
        "solved" => {
            core.knowledge()
                .from_block(block.clone())
                .get_solved_value(ReadOptions::default())
                .await
        }
        "hydrated" => Ok(match core.knowledge().actual_content(block).await? {
            HydratedContent::Text(text) => ReadValue::Json(json!(text)),
            HydratedContent::Bytes(bytes) => ReadValue::Bytes(bytes),
        }),
        _ => Err(knowledge::Error::InvalidInput("Invalid content mode")),
    }
}

pub(super) async fn call(core: &Core, name: &str, args: Value) -> Result<ToolOutput> {
    match name {
        "open_entities" | "expand_entities" => {
            let direction: Direction =
                super::parse(args.get("direction").cloned().unwrap_or(json!("both")))?;
            let mut results = Vec::new();
            for raw in args["entities"].as_array().ok_or("Invalid entities")? {
                let correlation = json!({"entity":raw});
                let Some((kind, id)) = raw.as_str().and_then(entity) else {
                    results.push(error(
                        correlation,
                        "invalid_arguments",
                        "Invalid entity reference",
                    ));
                    continue;
                };
                let result: inkcre_core::Result<Option<Value>> = async {
                    if name == "expand_entities" {
                        if kind == "block" {
                            Ok(core
                                .block_neighborhood(
                                    id,
                                    direction,
                                    &[],
                                    args["context_limit"].as_i64().unwrap_or(20),
                                    None,
                                )
                                .await?
                                .map(|n| json!({"graph":graph(&n.blocks,&n.relations)})))
                        } else {
                            Ok(core
                                .relation_neighborhood(id)
                                .await?
                                .map(|n| json!({"graph":graph(&n.blocks,&n.relations)})))
                        }
                    } else if kind == "block" {
                        Ok(core
                            .block_get(id)
                            .await?
                            .map(|b| json!({"block":block_preview(&b)})))
                    } else {
                        Ok(core
                            .relation_get(id)
                            .await?
                            .map(|r| json!({"relation":relation_preview(&r)})))
                    }
                }
                .await;
                results.push(match result {
                    Ok(Some(mut value)) => {
                        value["entity"] = raw.clone();
                        value
                    }
                    Ok(None) => error(correlation, "not_found", "Entity does not exist"),
                    Err(_) => error(correlation, "failed", "Entity could not be read"),
                });
            }
            Ok(ToolOutput::plain(json!({"results":results})))
        }
        "read_blocks" => {
            let mode = args["content"].as_str().unwrap_or("solved");
            let mut results = Vec::new();
            let mut content = Vec::new();
            for id in args["blocks"].as_array().ok_or("Invalid Blocks")? {
                let correlation = json!({"block":id});
                let Some(id) = block_id(id) else {
                    results.push(error(correlation, "invalid_arguments", "Invalid Block ID"));
                    continue;
                };
                let block = match core.block_get(id).await {
                    Ok(Some(block)) => block,
                    Ok(None) => {
                        results.push(error(correlation, "not_found", "Block does not exist"));
                        continue;
                    }
                    Err(_) => {
                        results.push(error(correlation, "failed", "Block could not be read"));
                        continue;
                    }
                };
                match read_value(core, &block, mode).await {
                    Ok(value) => {
                        let (projected, blocks, uri) = deliver(
                            value,
                            &format!("inkcre://blocks/{id}/content/{mode}"),
                            &format!("Block {id} {mode} content"),
                        );
                        content.extend(blocks);
                        results.push(json!({"block":id,"resolver":block.resolver,"storage":block.storage,"content":projected,"resource":uri}));
                    }
                    Err(failure) => results.push(knowledge_error(correlation, failure, false)),
                }
            }
            Ok(ToolOutput {
                payload: json!({"results":results}),
                content,
            })
        }
        "invoke_resolver_methods" => {
            let mut results = Vec::new();
            let mut content = Vec::new();
            for (index, call) in args["calls"]
                .as_array()
                .ok_or("Invalid calls")?
                .iter()
                .enumerate()
            {
                let correlation =
                    json!({"index":index,"block":call["block"],"method":call["method"]});
                let Some(id) = block_id(&call["block"]) else {
                    results.push(error(correlation, "invalid_arguments", "Invalid Block ID"));
                    continue;
                };
                let method = call["method"].as_str().ok_or("Invalid method")?;
                let block = match core.block_get(id).await {
                    Ok(Some(block)) => block,
                    Ok(None) => {
                        results.push(error(correlation, "not_found", "Block does not exist"));
                        continue;
                    }
                    Err(_) => {
                        results.push(error(correlation, "failed", "Block could not be read"));
                        continue;
                    }
                };
                let methods = core.knowledge().read_methods(&block.resolver);
                if !methods
                    .as_ref()
                    .is_ok_and(|methods| methods.iter().any(|m| m.name == method))
                {
                    results.push(error(
                        correlation,
                        "unavailable",
                        "Resolver method is not available",
                    ));
                    continue;
                }
                let arguments = call.get("arguments").cloned().unwrap_or(json!({}));
                match core
                    .knowledge()
                    .invoke_value(id, method, arguments.clone())
                    .await
                {
                    Ok(value) => {
                        let stem = format!(
                            "inkcre://blocks/{id}/resolver/{method}/{}",
                            URL_SAFE_NO_PAD.encode(arguments.to_string())
                        );
                        let (projected, blocks, uri) = deliver(
                            value,
                            &stem,
                            &format!("Block {id} Resolver method {method}"),
                        );
                        content.extend(blocks);
                        let mut result = correlation;
                        result["result"] = projected;
                        result["resource"] = json!(uri);
                        results.push(result);
                    }
                    Err(failure) => results.push(knowledge_error(correlation, failure, true)),
                }
            }
            Ok(ToolOutput {
                payload: json!({"results":results}),
                content,
            })
        }
        "resolver_methods" => {
            let blocks = args["blocks"].as_array().cloned().unwrap_or_default();
            let mut groups: Vec<(String, Vec<i32>)> = Vec::new();
            let mut errors = Vec::new();
            let mut seen = HashSet::new();
            if !blocks.is_empty() {
                for value in &blocks {
                    let Some(id) = block_id(value) else {
                        errors.push(error(
                            json!({"block":value}),
                            "invalid_arguments",
                            "Invalid Block ID",
                        ));
                        continue;
                    };
                    if !seen.insert(id) {
                        continue;
                    }
                    match core.block_get(id).await {
                        Ok(Some(block)) => {
                            if let Some((_, ids)) =
                                groups.iter_mut().find(|(name, _)| *name == block.resolver)
                            {
                                ids.push(id);
                            } else {
                                groups.push((block.resolver, vec![id]));
                            }
                        }
                        Ok(None) => errors.push(error(
                            json!({"block":id}),
                            "not_found",
                            "Block does not exist",
                        )),
                        Err(_) => errors.push(error(
                            json!({"block":id}),
                            "failed",
                            "Block could not be read",
                        )),
                    }
                }
            } else {
                for value in args["resolvers"].as_array().into_iter().flatten() {
                    let resolver = value.as_str().ok_or("Invalid Resolver")?;
                    if !groups.iter().any(|(name, _)| name == resolver) {
                        groups.push((resolver.into(), vec![]));
                    }
                }
            }
            if blocks.is_empty() && groups.is_empty() {
                return Err("At least one Block or Resolver is required".into());
            }
            let mut results = Vec::new();
            for (resolver, ids) in groups {
                match core.knowledge().read_methods(&resolver) {
                    Ok(methods) => {
                        let mut item = json!({"resolver":resolver,"methods":methods});
                        if !blocks.is_empty() {
                            item["blocks"] = json!(ids);
                        }
                        results.push(item);
                    }
                    Err(_) => errors.push(error(
                        json!({"resolver":resolver}),
                        "unavailable",
                        "Resolver is not registered",
                    )),
                }
            }
            Ok(ToolOutput::plain(
                json!({"results":results,"errors":errors}),
            ))
        }
        "find_path" => {
            let path = core
                .find_path(
                    super::number(&args, "from_block")?,
                    super::number(&args, "to_block")?,
                    PathOptions {
                        direction: super::parse(
                            args.get("direction").cloned().unwrap_or(json!("both")),
                        )?,
                        contents: vec![],
                        max_hops: args["max_hops"].as_u64().unwrap_or(4) as u8,
                        max_explored_blocks: 1000,
                    },
                )
                .await
                .map_err(|_| "Path unavailable")?;
            Ok(ToolOutput::plain(match path.outcome {
                PathOutcome::Found => {
                    json!({"status":"found","graph":graph(&path.graph.blocks,&path.graph.relations),"block_path":path.block_ids,"relation_path":path.relation_ids})
                }
                PathOutcome::NotFound => json!({"status":"not_found"}),
                PathOutcome::LimitReached => json!({"status":"limit_reached"}),
            }))
        }
        _ => Err("Unknown tool".into()),
    }
}
