//! Stateless MCP JSON responses over live, PAT-authorized Sink instances.
mod delivery;
mod skills;
mod tool_calls;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD},
};
use delivery::{ToolOutput, pointer_keys, project_links, python_json};
use inkcre_core::{Core, knowledge::ReadValue};

use crate::routes::AppState;
use serde_json::{Value, json};
type Result<T> = std::result::Result<T, String>;

pub fn router() -> Router<AppState> {
    Router::new().route("/sinks/{id}/mcp", post(endpoint))
}
async fn endpoint(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    let sink = match state.core.sink(id).await {
        Ok(Some(s)) if s.type_ == "core.mcp.v1" && s.enabled.contains(&state.peer_id) => s,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let expected = sink
        .config
        .get("pat")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if expected.is_empty()
        || headers.get("authorization").and_then(|h| h.to_str().ok())
            != Some(format!("Bearer {expected}").as_str())
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if headers
        .get("mcp-protocol-version")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| !matches!(v, "2025-03-26" | "2025-06-18" | "2025-11-25"))
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if request["jsonrpc"] != "2.0" || !request.is_object() {
        return Json(
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid Request"}}),
        )
        .into_response();
    }
    if request.get("id").is_none() {
        return StatusCode::ACCEPTED.into_response();
    }
    let id = request["id"].clone();
    let params = &request["params"];
    let result = match request["method"].as_str().unwrap_or_default() {
        "initialize" => Ok(
            json!({"protocolVersion":"2025-11-25","serverInfo":{"name":"inkcre","version":"1"},"capabilities":{"tools":{},"resources":{},"extensions":{(skills::CAPABILITY):{}}},"instructions":"Use InKCre as an evidence environment. Retrieve Blocks and directed Relations, then use that evidence in the caller's current work."}),
        ),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools":tools()})),
        "skills/list" => Ok(skills::list(params)),
        "skills/get" => Ok(skills::get(params)),
        "resources/list" => Ok(
            json!({"resources":[{"uri":skills::URI,"name":"Use InKCre","mimeType":"text/markdown"}]}),
        ),
        "resources/templates/list" => Ok(json!({"resourceTemplates":[
            {"uriTemplate":"inkcre://blocks/{block_id}/content/{mode}/text/{selector}","name":"Block text content","mimeType":"text/plain"},
            {"uriTemplate":"inkcre://blocks/{block_id}/content/{mode}/blob/{selector}","name":"Block binary content","mimeType":"application/octet-stream"},
            {"uriTemplate":"inkcre://blocks/{block_id}/resolver/{method}/{arguments}/text/{selector}","name":"Resolver method text result","mimeType":"text/plain"},
            {"uriTemplate":"inkcre://blocks/{block_id}/resolver/{method}/{arguments}/blob/{selector}","name":"Resolver method binary result","mimeType":"application/octet-stream"}
        ]})),
        "resources/read" if params["uri"] == skills::URI => Ok(skills::resource()),
        "resources/read" => resource(&state.core, params["uri"].as_str().unwrap_or_default()).await,
        "tools/call" => {
            let payload = call(
                &state.core,
                params["name"].as_str().unwrap_or_default(),
                params.get("arguments").cloned().unwrap_or(json!({})),
            )
            .await;
            let (mut output, failed) = match payload {
                Ok(output) => (output, false),
                Err(_) => (
                    ToolOutput::plain(
                        json!({"error":{"code":"invalid_arguments","message":"The requested evidence operation could not be completed"}}),
                    ),
                    true,
                ),
            };
            if params["name"] == "meta_discover"
                && params["arguments"]["collection"] == "source_types"
                && let Some(rows) = output.payload["results"].as_array_mut()
            {
                for row in rows {
                    row["registered_locally"] = json!(row["id"].as_str().is_some_and(|id| {
                        state.source_registry.get(id).ok().flatten().is_some()
                    }));
                }
            }
            Ok(output.into_result(failed))
        }
        _ => Err("Unknown MCP method".into()),
    };
    Json(match result {
        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
        Err(message) => json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":message}}),
    })
    .into_response()
}
fn tools() -> Vec<Value> {
    [
        ("meta_discover",json!({"collection":{"enum":["resolvers","source_types","storage_types","storages"]},"limit":{"type":"integer","minimum":1,"maximum":100},"cursor":{"type":["string","null"]}}),vec!["collection"]),
        ("query",json!({"query":{"type":["string","null"]},"modes":{"type":["array","null"],"items":{"enum":["lexical","semantic"]}},"filters":{"type":["object","null"]},"limit":{"type":"integer","minimum":1,"maximum":100},"cursor":{"type":["string","null"]}}),vec![]),
        ("open_entities",json!({"entities":{"type":"array","items":{"type":"string"}}}),vec!["entities"]),
        ("read_blocks",json!({"blocks":{"type":"array","items":{"type":"integer"}},"content":{"enum":["raw","hydrated","solved"]}}),vec!["blocks"]),
        ("expand_entities",json!({"entities":{"type":"array","items":{"type":"string"}},"context_limit":{"type":"integer","minimum":1,"maximum":100},"direction":{"enum":["in","out","both"]}}),vec!["entities"]),
        ("find_path",json!({"from_block":{"type":"integer"},"to_block":{"type":"integer"},"max_hops":{"type":"integer","minimum":0,"maximum":8},"direction":{"enum":["in","out","both"]}}),vec!["from_block","to_block"]),
        ("resolver_methods",json!({"blocks":{"type":"array","items":{"type":"integer"}},"resolvers":{"type":"array","items":{"type":"string"}}}),vec![]),
        ("invoke_resolver_methods",json!({"calls":{"type":"array","items":{"type":"object","properties":{"block":{"type":"integer"},"method":{"type":"string"},"arguments":{"type":"object"}},"required":["block","method"],"additionalProperties":false}}}),vec!["calls"]),
    ].into_iter().map(|(name,properties,required)|json!({"name":name,"description":name.replace('_'," "),"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},"annotations":{"readOnlyHint":!matches!(name,"read_blocks"|"invoke_resolver_methods"),"destructiveHint":false,"openWorldHint":false}})).collect()
}
fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|_| "Invalid arguments".into())
}
fn number(v: &Value, key: &str) -> Result<i32> {
    v[key]
        .as_i64()
        .and_then(|n| i32::try_from(n).ok())
        .ok_or("Invalid entity ID".into())
}
fn page(rows: Vec<Value>, limit: usize, scope: Value) -> Value {
    let more = rows.len() > limit;
    let rows: Vec<_> = rows.into_iter().take(limit).collect();
    let cursor = if more {
        rows.last().map(|r| {
            URL_SAFE.encode(
                json!({"version":1,"scope":scope,"order":"id_asc","key":r["id"]}).to_string(),
            )
        })
    } else {
        None
    };
    json!({"results":rows,"next_cursor":cursor})
}
fn after(args: &Value, scope: &Value) -> Result<Option<Value>> {
    let Some(cursor) = args["cursor"].as_str() else {
        return Ok(None);
    };
    if cursor.len() > 16384 {
        return Err("Invalid cursor".into());
    }
    let bytes = URL_SAFE.decode(cursor).map_err(|_| "Invalid cursor")?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|_| "Invalid cursor")?;
    if v["version"] != 1 || v["order"] != "id_asc" || v["scope"] != *scope {
        return Err("Cursor scope mismatch".into());
    }
    let numeric = scope["collection"] == "storages" || scope["branch"] == "conditions";
    if v.as_object().is_none_or(|o| o.len() != 4)
        || (numeric
            && v["key"]
                .as_i64()
                .is_none_or(|id| i32::try_from(id).is_err()))
        || (!numeric && !v["key"].is_string())
    {
        return Err("Invalid cursor key".into());
    }
    Ok(Some(v["key"].clone()))
}
fn limit(args: &Value, max: usize) -> Result<usize> {
    let n = args
        .get("limit")
        .map_or(Some(20), Value::as_u64)
        .ok_or("Invalid limit")? as usize;
    if n == 0 || n > max {
        return Err("Invalid limit".into());
    }
    Ok(n)
}
async fn call(core: &Core, name: &str, args: Value) -> Result<ToolOutput> {
    let contract = tools()
        .into_iter()
        .find(|t| t["name"] == name)
        .ok_or("Unknown tool")?;
    inkcre_core::models::validate_json(&contract["inputSchema"], &args)
        .map_err(|_| "Invalid arguments")?;
    match name {
        "meta_discover" => discover(core, args).await.map(ToolOutput::plain),
        "query" => query(core, args).await.map(ToolOutput::plain),
        _ => tool_calls::call(core, name, args).await,
    }
}

async fn discover(core: &Core, args: Value) -> Result<Value> {
    let collection = args["collection"].as_str().ok_or("Invalid collection")?;
    let scope = json!({"tool":"meta_discover","collection":collection});
    let after = after(&args, &scope)?;
    let count = limit(&args, 100)?;
    let rows=match collection {
        "resolvers"=>core.knowledge().registry().resolvers().map_err(|_|"Registry unavailable")?.into_iter().filter(|r|after.as_ref().is_none_or(|a|a.as_str().is_some_and(|a|r.id.as_str()>a))).take(count+1).map(|r|json!({"id":r.id,"description":r.description.chars().take(512).collect::<String>(),"registered_locally":true})).collect(),
        "storages"=>{let key=after.map(|a|a.as_i64().ok_or("Invalid cursor")).transpose()?;let rows:Vec<(i32,String,Option<String>)>=sqlx_query_storages(core,key,count).await?;rows.into_iter().map(|(id,kind,name)|json!({"id":id,"type":kind,"nickname":name})).collect()},
        "source_types"|"storage_types"=>metadata_types(core,collection,after,count).await?,
        _=>return Err("Invalid collection".into())};
    let mut output = page(rows, count, scope);
    output["collection"] = json!(collection);
    Ok(output)
}
async fn sqlx_query_storages(
    core: &Core,
    after: Option<i64>,
    limit: usize,
) -> Result<Vec<(i32, String, Option<String>)>> {
    core.discover_storages(after, limit as i64)
        .await
        .map_err(|_| "Metadata unavailable".into())
}
async fn metadata_types(
    core: &Core,
    kind: &str,
    after: Option<Value>,
    limit: usize,
) -> Result<Vec<Value>> {
    let key = after
        .map(|a| a.as_str().map(str::to_owned).ok_or("Invalid cursor"))
        .transpose()?;
    let rows = core
        .discover_types(kind, key.as_deref(), limit as i64)
        .await
        .map_err(|_| "Metadata unavailable")?;
    Ok(rows.into_iter().map(|(id,desc)|json!({"id":id,"description":desc.map(|s|s.chars().take(512).collect::<String>()),"registered_locally": kind == "storage_types" && core.knowledge().registry().storage(&id).is_ok()})).collect())
}
async fn query(core: &Core, args: Value) -> Result<Value> {
    let count = limit(&args, 100)?;
    if let Some(text) = args["query"].as_str().filter(|q| !q.trim().is_empty()) {
        if count > 20 || !args["cursor"].is_null() {
            return Err("Text query does not accept cursor".into());
        }
        let modes = args["modes"]
            .as_array()
            .filter(|m| {
                !m.is_empty()
                    && m.iter()
                        .all(|v| matches!(v.as_str(), Some("lexical" | "semantic")))
            })
            .ok_or("Text query requires modes")?;
        let filters = args.get("filters").cloned();
        let mut results: Vec<Value> = Vec::new();
        let mut seen_modes = std::collections::HashSet::new();
        let mut errors = Vec::new();
        for mode in modes {
            if !seen_modes.insert(mode.as_str()) {
                continue;
            }
            let result = if mode == "lexical" {
                core.lexical_retrieve(parse(
                    json!({"query":text,"limit":count,"filters":filters}),
                )?)
                .await
                .map(|v| json!(v))
            } else {
                core.semantic_retrieve(parse(
                    json!({"query":text,"options":{"limit":count,"filters":filters}}),
                )?)
                .await
                .map(|v| json!(v))
            };
            match result {
                Ok(value) => {
                    for row in value["matches"].as_array().into_iter().flatten() {
                        let reference = if mode == "lexical" {
                            format!("block:{}", row["block"]["id"])
                        } else {
                            format!(
                                "{}:{}",
                                row["type"].as_str().unwrap_or("block"),
                                row["entity"]["id"]
                            )
                        };
                        let index = if let Some(index) =
                            results.iter().position(|item| item["entity"] == reference)
                        {
                            index
                        } else {
                            results.push(json!({"entity":reference,"label":null,"evidence":[]}));
                            results.len() - 1
                        };
                        let item = &mut results[index];
                        let evidence = if mode == "lexical" {
                            item["label"] = row["label"].clone();
                            json!({"mode":mode,"rank":row["rank"],"kind":row["evidence"],"excerpt":row["excerpt"]})
                        } else {
                            if item["label"].is_null() {
                                if row["type"] == "block" {
                                    if let Some(id) = row["entity"]["id"]
                                        .as_i64()
                                        .and_then(|id| i32::try_from(id).ok())
                                        && let Ok(label) = core.knowledge().label(id, false).await
                                    {
                                        item["label"] = json!(label);
                                    }
                                } else {
                                    item["label"] = json!(format!(
                                        "block:{} → {} → block:{}",
                                        row["entity"]["from_"],
                                        row["entity"]["content"]
                                            .as_str()
                                            .unwrap_or_default()
                                            .chars()
                                            .take(160)
                                            .collect::<String>(),
                                        row["entity"]["to_"]
                                    ));
                                }
                            }
                            json!({"mode":mode,"score":row["score"]})
                        };
                        item["evidence"]
                            .as_array_mut()
                            .ok_or("Invalid evidence")?
                            .push(evidence);
                    }
                }
                Err(_) => errors.push(json!({"mode":mode,"message":"Retrieval unavailable"})),
            }
        }
        return Ok(json!({"results":results,"mode_errors":errors}));
    }
    if !args["modes"].is_null() {
        return Err("Condition query does not accept modes".into());
    }
    let filters: inkcre_core::lexical::BlockFilters =
        parse(args.get("filters").cloned().ok_or("Filters required")?)?;
    if filters.resolver.is_none() && filters.storage.is_none() {
        return Err("Filters required".into());
    }
    let mut projected_filters = json!(filters);
    if let Some(fields) = projected_filters.as_object_mut() {
        fields.retain(|_, value| !value.is_null());
    }
    let scope = json!({"tool":"query","branch":"conditions","filters":projected_filters});
    let key = after(&args, &scope)?
        .map(|k| k.as_i64().ok_or("Invalid cursor"))
        .transpose()?;
    let mut rows = core
        .discover_blocks(
            filters.resolver.as_ref().map(|p| p.eq.as_str()),
            filters.storage.as_ref().map(|p| p.eq),
            key,
            count as i64,
        )
        .await
        .map_err(|_| "Metadata query failed")?;
    for row in &mut rows {
        row["entity"] = json!(format!("block:{}", row["id"]));
    }
    Ok(page(rows, count, scope))
}
async fn block_value(core: &Core, id: i32, mode: &str) -> Result<ReadValue> {
    let block = core
        .block_get(id)
        .await
        .map_err(|_| "Block unavailable")?
        .ok_or("Block missing")?;
    tool_calls::read_value(core, &block, mode)
        .await
        .map_err(|_| "Content unavailable".into())
}

fn decode_selector(selector: &str) -> Result<Vec<Value>> {
    if selector == "root" {
        return Ok(vec![]);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(selector)
        .map_err(|_| "Invalid selector")?;
    let keys: Vec<Value> = serde_json::from_slice(&bytes).map_err(|_| "Invalid selector")?;
    if keys.iter().any(|key| !key.is_string() && !key.is_u64()) {
        return Err("Invalid selector".into());
    }
    Ok(keys)
}
async fn resource(core: &Core, uri: &str) -> Result<Value> {
    let path = uri
        .strip_prefix("inkcre://blocks/")
        .ok_or("Unknown resource")?;
    let parts: Vec<_> = path.split('/').collect();
    if parts.len() == 6 && parts[1] == "resolver" {
        let id = parts[0].parse().map_err(|_| "Invalid Block")?;
        let arguments = URL_SAFE_NO_PAD
            .decode(parts[3])
            .map_err(|_| "Invalid arguments")?;
        let arguments: Value =
            serde_json::from_slice(&arguments).map_err(|_| "Invalid arguments")?;
        if !arguments.is_object() {
            return Err("Arguments must be an object".into());
        }
        let value = core
            .knowledge()
            .invoke_value(id, parts[2], arguments)
            .await
            .map_err(|_| "Resolver unavailable")?;
        return resource_value(uri, value, parts[4], parts[5]);
    }
    if parts.len() != 5 || parts[1] != "content" || !matches!(parts[3], "text" | "blob") {
        return Err("Unsupported resource URI".into());
    }
    let id: i32 = parts[0].parse().map_err(|_| "Invalid Block")?;
    resource_value(
        uri,
        block_value(core, id, parts[2]).await?,
        parts[3],
        parts[4],
    )
}

fn resource_value(uri: &str, value: ReadValue, kind: &str, selector: &str) -> Result<Value> {
    let keys = decode_selector(selector)?;
    let stem = uri.rsplitn(3, '/').nth(2).ok_or("Invalid resource")?;
    match kind {
        "blob" => {
            let (json, parts) = value.into_parts();
            let (_, bytes) = parts
                .into_iter()
                .find(|(pointer, _)| pointer_keys(&json, pointer) == keys)
                .ok_or("Stale binary selector")?;
            let mime_type = "application/octet-stream";
            Ok(json!({"contents":[{"uri":uri,"mimeType":mime_type,"blob":STANDARD.encode(bytes)}]}))
        }
        "text" => {
            let (projected, _) = project_links(value, stem);
            let mut value = &projected;
            for key in keys {
                value = if let Some(index) = key.as_u64() {
                    value.get(index as usize)
                } else {
                    key.as_str().and_then(|k| value.get(k))
                }
                .ok_or("Stale selector")?;
            }
            let (mime_type, text) = match value.as_str() {
                Some(text) => ("text/plain", text.to_owned()),
                None => ("text/plain", python_json(value)),
            };
            Ok(json!({"contents":[{"uri":uri,"mimeType":mime_type,"text":text}]}))
        }
        _ => Err("Unsupported resource kind".into()),
    }
}
