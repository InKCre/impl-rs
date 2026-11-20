use crate::{Result, conflict, invalid, schema::*};
use inkcre_core::{
    Core,
    knowledge::{ReadOptions, ReadValue, decode_unicode_bytes, format_label},
};
use inkcre_extension_sdk::{Content, ResolverOperation, ResolverRequest};
use serde_json::{Value, json};
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_default()
}
pub async fn read(core: &Core, request: ResolverRequest) -> Result<Value> {
    let raw = match &request.input.content {
        Content::Text(s) => s.clone(),
        Content::Bytes(bytes) => decode_unicode_bytes(bytes)
            .map_err(|_| invalid("Twitter content must be Unicode JSON"))?,
    };
    let root: Value =
        serde_json::from_str(&raw).map_err(|_| invalid("Twitter content must be JSON"))?;
    if request.resolver == LEGACY {
        return legacy(core, request, root).await;
    }
    let kind = match request.resolver.as_str() {
        POST => "posts",
        USER => "users",
        MEDIA => "media",
        _ => return Err(invalid("Unknown Twitter Resolver")),
    };
    native(kind, &root)?;
    let id = text(&root, "id");
    let identity = root.as_object().is_some_and(|o| o.len() == 1);
    match request.operation {
        ResolverOperation::Solve { .. } => solved(kind, &root),
        ResolverOperation::Label { .. } => {
            let (prefix, label, first) = match kind {
                "posts" => (
                    "tweet",
                    root["text"]["text"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .unwrap_or(id)
                        .to_owned(),
                    true,
                ),
                "users" => (
                    "twitter user",
                    if identity {
                        id.into()
                    } else {
                        format!("@{}", text(&root, "handle"))
                    },
                    false,
                ),
                _ => (
                    "twitter media",
                    root["title"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .or(root["alt"].as_str().filter(|s| !s.is_empty()))
                        .unwrap_or(id)
                        .to_owned(),
                    true,
                ),
            };
            Ok(json!(format_label(prefix, Some(&label), first)))
        }
        ResolverOperation::Text { .. } => {
            let mut parts = Vec::new();
            match kind {
                "posts" => {
                    parts.extend([format!("https://x.com/i/status/{id}"), id.into()]);
                    if !identity {
                        parts.push(format!("author: {}", text(&root, "authorId")));
                        for edge in &request.input.relations {
                            if edge.from_ != Some(request.input.block.id) {
                                continue;
                            }
                            let Ok(value) = serde_json::from_str::<Value>(&edge.content) else {
                                continue;
                            };
                            if value != json!({"schema":REFERENCE,"type":"author"}) {
                                continue;
                            }
                            if let Some(target) = edge.to_
                                && let Some(author) = core
                                    .block_get(target)
                                    .await
                                    .map_err(|_| conflict("Twitter author read failed"))?
                                && author.resolver == USER
                            {
                                let author: Value = serde_json::from_str(&author.content)
                                    .map_err(|_| invalid("Stored Twitter author invalid"))?;
                                if author.as_object().is_some_and(|o| o.len() != 1) {
                                    validate("TwitterUser", &author)?;
                                    parts.push(format!(
                                        "{} @{}",
                                        text(&author, "name"),
                                        text(&author, "handle")
                                    ));
                                }
                            }
                            break;
                        }
                        if let Some(value) = root["text"]["text"].as_str() {
                            parts.push(value.into())
                        }
                        if let Some(article) = root.get("article") {
                            parts.push(text(article, "title").into());
                            if let Some(blocks) = article["blocks"].as_array() {
                                parts.extend(blocks.iter().map(|b| text(b, "text").to_owned()))
                            } else if let Some(preview) = article["preview"].as_str() {
                                parts.push(preview.into());
                            }
                        }
                    }
                }
                "users" => {
                    parts.push(id.into());
                    if !identity {
                        parts.push(format!(
                            "{} @{}",
                            text(&root, "name"),
                            text(&root, "handle")
                        ));
                        parts.push(format!("https://x.com/{}", text(&root, "handle")));
                        for value in [&root["bio"]["text"], &root["location"], &root["website"]] {
                            if let Some(value) = value.as_str() {
                                parts.push(value.into())
                            }
                        }
                    }
                }
                _ => {
                    parts.push(id.into());
                    if !identity {
                        for key in ["alt", "title", "description"] {
                            if let Some(value) = root[key].as_str() {
                                parts.push(value.into())
                            }
                        }
                    }
                }
            }
            Ok(json!(parts.join("\n")))
        }
        _ => Err(invalid("Unknown Twitter Resolver read method")),
    }
}
fn legacy_root(mut root: Value) -> Result<Value> {
    validate("Tweet", &root)?;
    if let Some(object) = root.as_object_mut() {
        object.retain(|key, _| schemas()["Tweet"]["properties"].get(key).is_some());
    }
    for key in ["user_id", "conversation_id", "quote"] {
        if root.get(key).is_none() {
            root[key] = Value::Null
        }
    }
    Ok(root)
}
async fn legacy_solved(
    core: &Core,
    request: &ResolverRequest,
    root: Value,
    refresh: bool,
    materialize_missing: bool,
) -> Result<ReadValue> {
    let root = legacy_root(root)?;
    let mut attachments = Vec::new();
    let mut links = Vec::new();
    for edge in &request.input.relations {
        if edge.from_ != Some(request.input.block.id)
            || (!edge.content.starts_with("attachment:") && edge.content != "entities:url")
        {
            continue;
        }
        let Some(id) = edge.to_ else { continue };
        if core
            .block_get(id)
            .await
            .map_err(|_| conflict("Legacy Twitter attachment lookup failed"))?
            .is_none()
        {
            continue;
        }
        let value = core
            .knowledge()
            .solved_value(
                id,
                ReadOptions {
                    refresh,
                    materialize_missing,
                },
            )
            .await
            .map_err(|_| conflict("Legacy Twitter attachment resolution failed"))?;
        if edge.content.starts_with("attachment:") {
            attachments.push(value)
        } else if let ReadValue::Json(Value::String(link)) = value {
            links.push(link)
        }
    }
    let mut fields: std::collections::BTreeMap<_, _> = root
        .as_object()
        .ok_or_else(|| invalid("Invalid legacy Twitter record"))?
        .iter()
        .map(|(key, value)| (key.clone(), ReadValue::Json(value.clone())))
        .collect();
    fields.insert("attachments".into(), ReadValue::Array(attachments));
    fields.insert("links".into(), ReadValue::Json(json!(links)));
    Ok(ReadValue::Object(fields))
}
pub async fn read_value(core: &Core, request: ResolverRequest) -> Result<ReadValue> {
    if request.resolver == LEGACY
        && let ResolverOperation::Solve {
            refresh,
            materialize_missing,
        } = request.operation
    {
        let raw = match &request.input.content {
            Content::Text(text) => text.clone(),
            Content::Bytes(bytes) => decode_unicode_bytes(bytes)
                .map_err(|_| invalid("Twitter content must be Unicode JSON"))?,
        };
        let root =
            serde_json::from_str(&raw).map_err(|_| invalid("Twitter content must be JSON"))?;
        return legacy_solved(core, &request, root, refresh, materialize_missing).await;
    }
    read(core, request).await.map(ReadValue::Json)
}
async fn legacy(core: &Core, request: ResolverRequest, root: Value) -> Result<Value> {
    let root = legacy_root(root)?;
    let source = text(&root, "text").to_owned();
    match request.operation {
        ResolverOperation::Label { .. } => Ok(json!(format_label(
            "tweet",
            Some(
                if source.is_empty() {
                    root["id"].to_string()
                } else {
                    source
                }
                .as_str()
            ),
            true
        ))),
        ResolverOperation::Solve {
            refresh,
            materialize_missing,
        } => legacy_solved(core, &request, root, refresh, materialize_missing)
            .await
            .map(ReadValue::into_json),
        ResolverOperation::Text {
            refresh,
            materialize_missing,
            ..
        } => {
            legacy_solved(core, &request, root, refresh, materialize_missing).await?;
            Ok(json!(source))
        }
        _ => Err(invalid("Unknown legacy Twitter read method")),
    }
}
