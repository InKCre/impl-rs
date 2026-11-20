//! Stateless MCP JSON responses over live, PAT-authorized Sink instances.
mod skills;
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
use inkcre_core::{
    Core, Direction, HydratedContent, PathOptions,
    knowledge::{ReadOptions, ReadValue},
};

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
        "resources/templates/list" => Ok(
            json!({"resourceTemplates":[{"uriTemplate":"inkcre://blocks/{block_id}/content/{mode}/{kind}/{selector}","name":"Live Block content"}]}),
        ),
        "resources/read" if params["uri"] == skills::URI => Ok(skills::resource()),
        "resources/read" => resource(&state.core, params["uri"].as_str().unwrap_or_default()).await,
        "tools/call" => {
            let payload = call(
                &state.core,
                params["name"].as_str().unwrap_or_default(),
                params.get("arguments").cloned().unwrap_or(json!({})),
            )
            .await;
            let (mut value, failed) = match payload {
                Ok(value) => (value, false),
                Err(_) => (
                    json!({"error":{"code":"invalid_arguments","message":"The requested evidence operation could not be completed"}}),
                    true,
                ),
            };
            if params["name"] == "meta_discover"
                && params["arguments"]["collection"] == "source_types"
                && let Some(rows) = value["results"].as_array_mut()
            {
                for row in rows {
                    row["registered_locally"] = json!(row["id"].as_str().is_some_and(|id| {
                        state.source_registry.get(id).ok().flatten().is_some()
                    }));
                }
            }
            Ok(
                json!({"content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":failed}),
            )
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
        ("query",json!({"query":{"type":["string","null"]},"modes":{"type":"array","items":{"enum":["lexical","semantic"]}},"filters":{"type":"object"},"limit":{"type":"integer","minimum":1,"maximum":100},"cursor":{"type":["string","null"]}}),vec![]),
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
async fn call(core: &Core, name: &str, mut args: Value) -> Result<Value> {
    let contract = tools()
        .into_iter()
        .find(|t| t["name"] == name)
        .ok_or("Unknown tool")?;
    inkcre_core::models::validate_json(&contract["inputSchema"], &args)
        .map_err(|_| "Invalid arguments")?;
    let item_key = match name {
        "open_entities" | "expand_entities" => Some("entities"),
        "read_blocks" => Some("blocks"),
        "invoke_resolver_methods" => Some("calls"),
        _ => None,
    };
    if let Some(key) = item_key {
        let Value::Array(items) = args[key].take() else {
            return Err("Invalid tool items".into());
        };
        let mut results = Vec::new();
        for (index, item) in items.into_iter().enumerate() {
            let mut input = args.clone();
            input[key] = json!([item]);
            match call_tool(core,name,input).await {
                    Ok(mut value)=>{
                        for mut row in value["results"].as_array_mut().ok_or("Invalid result")?.drain(..) {
                            if name=="invoke_resolver_methods" { row["index"]=json!(index); }
                            results.push(row);
                        }
                    },
                    Err(_)=>results.push(json!({"index":index,"input":item,"error":{"code":"unavailable","message":"Requested item is unavailable"}})),
                }
        }
        return Ok(json!({"results":results}));
    }
    call_tool(core, name, args).await
}

async fn call_tool(core: &Core, name: &str, args: Value) -> Result<Value> {
    match name {
        "meta_discover" => discover(core, args).await,
        "query" => query(core, args).await,
        "open_entities" | "expand_entities" => {
            let mut rows = Vec::new();
            let direction: Direction =
                parse(args.get("direction").cloned().unwrap_or(json!("both")))?;
            for reference in args["entities"].as_array().ok_or("Invalid entities")? {
                let raw = reference.as_str().ok_or("Invalid reference")?;
                let (kind, id) = raw.split_once(':').ok_or("Invalid reference")?;
                let id: i32 = id.parse().map_err(|_| "Invalid ID")?;
                let value = if name == "expand_entities" {
                    if kind == "block" {
                        serde_json::to_value(
                            core.block_neighborhood(
                                id,
                                direction,
                                &[],
                                args["context_limit"].as_i64().unwrap_or(20),
                                None,
                            )
                            .await
                            .map_err(|_| "Graph read failed")?,
                        )
                        .map_err(|_| "Graph serialization failed")?
                    } else {
                        json!(
                            core.relation_neighborhood(id)
                                .await
                                .map_err(|_| "Graph read failed")?
                        )
                    }
                } else if kind == "block" {
                    core.block_get(id).await.map_err(|_|"Block read failed")?.map(|b|json!({"block":{"id":b.id,"resolver":b.resolver,"storage":b.storage,"created_at":b.created_at,"updated_at":b.updated_at}})).unwrap_or(Value::Null)
                } else if kind == "relation" {
                    core.relation_get(id).await.map_err(|_|"Relation read failed")?.map(|r|json!({"relation":{"id":r.id,"from_block_id":r.from_,"to_block_id":r.to_,"content":r.content,"updated_at":r.updated_at}})).unwrap_or(Value::Null)
                } else {
                    return Err("Invalid entity kind".into());
                };
                let mut projected = if name == "expand_entities" {
                    json!({"graph":value})
                } else {
                    value
                };
                if projected.is_null() {
                    projected =
                        json!({"error":{"code":"not_found","message":"Entity does not exist"}})
                }
                projected["entity"] = json!(raw);
                rows.push(projected);
            }
            Ok(json!({"results":rows}))
        }
        "find_path" => Ok(json!(
            core.find_path(
                number(&args, "from_block")?,
                number(&args, "to_block")?,
                PathOptions {
                    direction: parse(args.get("direction").cloned().unwrap_or(json!("both")))?,
                    contents: vec![],
                    max_hops: args["max_hops"].as_u64().unwrap_or(4) as u8,
                    max_explored_blocks: 1000
                }
            )
            .await
            .map_err(|_| "Path unavailable")?
        )),
        "resolver_methods" => {
            let mut ids: Vec<String> = parse(args.get("resolvers").cloned().unwrap_or(json!([])))?;
            for id in args["blocks"].as_array().into_iter().flatten() {
                if let Some(b) = core
                    .block_get(id.as_i64().ok_or("Invalid ID")? as i32)
                    .await
                    .map_err(|_| "Block read failed")?
                {
                    ids.push(b.resolver)
                }
            }
            ids.sort();
            ids.dedup();
            let mut results = vec![];
            for id in ids {
                results.push(json!({"resolver":id,"methods":core.knowledge().read_methods(&id).map_err(|_|"Resolver unavailable")?}))
            }
            Ok(json!({"results":results,"errors":[]}))
        }
        "read_blocks" => {
            let mode = args["content"].as_str().unwrap_or("solved");
            let mut rows = vec![];
            for id in args["blocks"].as_array().ok_or("Invalid Blocks")? {
                let id = id.as_i64().ok_or("Invalid Block")? as i32;
                let value = block_value(core, id, mode).await?;
                let stem = format!("inkcre://blocks/{id}/content/{mode}");
                let (projected, uri) = project_inline(value, &stem);
                rows.push(json!({"block":id,"content":projected,"resource":uri}))
            }
            Ok(json!({"results":rows}))
        }
        "invoke_resolver_methods" => {
            let mut rows = vec![];
            for (index, c) in args["calls"]
                .as_array()
                .ok_or("Invalid calls")?
                .iter()
                .enumerate()
            {
                let id = number(c, "block")?;
                let method = c["method"].as_str().ok_or("Invalid method")?;
                let arguments = c.get("arguments").cloned().unwrap_or(json!({}));
                let value = core
                    .knowledge()
                    .invoke_value(id, method, arguments.clone())
                    .await
                    .map_err(|_| "Resolver read failed")?;
                let stem = format!(
                    "inkcre://blocks/{id}/resolver/{method}/{}",
                    URL_SAFE_NO_PAD.encode(arguments.to_string())
                );
                let (projected, uri) = project_inline(value, &stem);
                rows.push(json!({"index":index,"block":id,"method":method,"result":projected,"resource":uri}))
            }
            Ok(json!({"results":rows}))
        }
        _ => Err("Unknown tool".into()),
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
        let mut results = std::collections::BTreeMap::<String, Value>::new();
        let mut errors = Vec::new();
        for mode in modes {
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
                        let reference=if mode=="lexical" { format!("block:{}",row["block"]["id"]) } else { format!("{}:{}",row["type"].as_str().unwrap_or("block"),row["entity"]["id"]) };
                        let item=results.entry(reference.clone()).or_insert_with(||json!({"entity":reference,"label":null,"evidence":[]}));
                        let evidence=if mode=="lexical" { item["label"]=row["label"].clone();json!({"mode":mode,"rank":row["rank"],"kind":row["evidence"],"excerpt":row["excerpt"]}) } else { json!({"mode":mode,"score":row["score"]}) };
                        item["evidence"].as_array_mut().ok_or("Invalid evidence")?.push(evidence);
                    }
                }
                Err(_)=>errors.push(json!({"mode":mode,"error":{"code":"unavailable","message":"Retrieval unavailable"}})),
            }
        }
        return Ok(
            json!({"results":results.into_values().collect::<Vec<_>>(),"mode_errors":errors}),
        );
    }
    if !args["modes"].is_null() {
        return Err("Condition query does not accept modes".into());
    }
    let filters: inkcre_core::lexical::BlockFilters =
        parse(args.get("filters").cloned().ok_or("Filters required")?)?;
    if filters.resolver.is_none() && filters.storage.is_none() {
        return Err("Filters required".into());
    }
    let scope = json!({"tool":"query","branch":"conditions","filters":filters});
    let key = after(&args, &scope)?
        .map(|k| k.as_i64().ok_or("Invalid cursor"))
        .transpose()?;
    let rows = core
        .discover_blocks(
            filters.resolver.as_ref().map(|p| p.eq.as_str()),
            filters.storage.as_ref().map(|p| p.eq),
            key,
            count as i64,
        )
        .await
        .map_err(|_| "Metadata query failed")?;
    Ok(page(rows, count, scope))
}
async fn block_value(core: &Core, id: i32, mode: &str) -> Result<ReadValue> {
    let b = core
        .block_get(id)
        .await
        .map_err(|_| "Block unavailable")?
        .ok_or("Block missing")?;
    match mode {
        "raw" => Ok(ReadValue::Json(json!(b.content))),
        "solved" => core
            .knowledge()
            .from_block(b)
            .get_solved_value(ReadOptions::default())
            .await
            .map_err(|_| "Resolver unavailable".into()),
        "hydrated" => match core
            .knowledge()
            .actual_content(&b)
            .await
            .map_err(|_| "Storage unavailable")?
        {
            HydratedContent::Text(t) => Ok(ReadValue::Json(json!(t))),
            HydratedContent::Bytes(bytes) => Ok(ReadValue::Bytes(bytes)),
        },
        _ => Err("Invalid content mode".into()),
    }
}

fn project_inline(value: ReadValue, stem: &str) -> (Value, String) {
    let root_kind = if matches!(value, ReadValue::Bytes(_)) {
        "blob"
    } else {
        "text"
    };
    let uri = format!("{stem}/{root_kind}/root");
    let value = project_links(value, stem);
    let size = value.to_string().len();
    if size > 65536 {
        (json!({"resource":uri,"size":size}), uri)
    } else {
        (value, uri)
    }
}

fn project_links(value: ReadValue, stem: &str) -> Value {
    let (mut value, parts) = value.into_parts();
    for (pointer, bytes) in parts {
        let keys = pointer_keys(&value, &pointer);
        let selector = if keys.is_empty() {
            "root".into()
        } else {
            URL_SAFE_NO_PAD.encode(json!(keys).to_string())
        };
        if let Some(field) = value.pointer_mut(&pointer) {
            *field = json!({"resource":format!("{stem}/blob/{selector}"),"size":bytes.len()});
            if let Some(mime_type) = inkcre_core::knowledge::detect_media_type(&bytes) {
                field["mime_type"] = json!(mime_type);
            }
        }
    }
    value
}

fn pointer_keys(value: &Value, pointer: &str) -> Vec<Value> {
    let mut keys = Vec::new();
    let mut current = value;
    for token in pointer.split('/').skip(1) {
        let key = token.replace("~1", "/").replace("~0", "~");
        if let (true, Ok(index)) = (current.is_array(), token.parse::<usize>()) {
            keys.push(json!(index));
            current = &current[index];
        } else {
            current = &current[&key];
            keys.push(json!(key));
        }
    }
    keys
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
            let mime_type = inkcre_core::knowledge::detect_media_type(&bytes)
                .unwrap_or("application/octet-stream");
            Ok(json!({"contents":[{"uri":uri,"mimeType":mime_type,"blob":STANDARD.encode(bytes)}]}))
        }
        "text" => {
            let projected = project_links(value, stem);
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
                None => ("application/json", value.to_string()),
            };
            Ok(json!({"contents":[{"uri":uri,"mimeType":mime_type,"text":text}]}))
        }
        _ => Err("Unsupported resource kind".into()),
    }
}
