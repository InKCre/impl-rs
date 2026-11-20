use axum::{
    Json, Router,
    extract::State,
    http::header,
    response::{IntoResponse, Response},
    routing::post,
};
use inkcre_core::{GraphInput, GraphSubmission, HydratedContent};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::{
    error::{ApiError, ApiJson, ApiResult},
    routes::AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/graph", post(submit_graph))
        .route("/entities/get", post(entities))
}

async fn submit_graph(
    State(state): State<AppState>,
    ApiJson(form): ApiJson<GraphInput>,
) -> ApiResult<Json<GraphSubmission>> {
    Ok(Json(state.core.submit_graph(form).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum EntityKind {
    Block,
    Relation,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntityRef {
    #[serde(rename = "type")]
    kind: EntityKind,
    id: i32,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ContentMode {
    None,
    #[default]
    Raw,
    Hydrated,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntityForm {
    entities: Vec<EntityRef>,
    #[serde(default)]
    content: ContentMode,
}

async fn entities(
    State(state): State<AppState>,
    ApiJson(form): ApiJson<EntityForm>,
) -> ApiResult<Response> {
    let mut blocks = Vec::new();
    let mut relations = Vec::new();
    for entity in &form.entities {
        match entity.kind {
            EntityKind::Block => blocks.push(entity.id),
            EntityKind::Relation => relations.push(entity.id),
        }
    }
    let records = state.core.entity_records(&blocks, &relations).await?;
    let knowledge = state.core.knowledge();
    let mut results = Vec::with_capacity(form.entities.len());
    let mut parts = BTreeMap::new();
    let mut blobs = Vec::new();
    for (index, entity) in form.entities.into_iter().enumerate() {
        let id = entity.id;
        let result = match entity.kind {
            EntityKind::Block => match records.blocks.get(&id) {
                None => missing("block", id),
                Some(block) => {
                    let mut value = json!(block);
                    value["type"] = json!("block");
                    if !matches!(form.content, ContentMode::Raw) {
                        omit_content(&mut value);
                    }
                    if matches!(form.content, ContentMode::Hydrated) {
                        match knowledge.actual_content(block).await {
                            Ok(HydratedContent::Text(text)) => {
                                value["hydrated_content"] = json!(text)
                            }
                            Ok(HydratedContent::Bytes(bytes)) => {
                                let content_id = format!("body-{}@inkcre", blobs.len() + 1);
                                parts.insert(
                                    format!("/{index}/hydrated_content"),
                                    content_id.clone(),
                                );
                                blobs.push((content_id, bytes));
                                value["hydrated_content"] = Value::Null;
                            }
                            Err(_) => {
                                value = json!({"type":"block","id":id,"error":{
                                    "code":"content_read_failed","message":"Block content is unavailable"
                                }});
                            }
                        }
                    }
                    value
                }
            },
            EntityKind::Relation => match records.relations.get(&id) {
                None => missing("relation", id),
                Some(relation) => {
                    let mut value = relation_record(json!(relation));
                    value["type"] = json!("relation");
                    if matches!(form.content, ContentMode::None) {
                        omit_content(&mut value);
                    }
                    value
                }
            },
        };
        results.push(result);
    }
    if blobs.is_empty() {
        return Ok(Json(results).into_response());
    }
    multipart(json!({"value":results,"parts":parts}), blobs)
}

fn omit_content(value: &mut Value) {
    if let Some(record) = value.as_object_mut() {
        record.remove("content");
    }
}

pub(crate) fn relation_record(mut value: Value) -> Value {
    if let Some(record) = value.as_object_mut() {
        for (source, target) in [("from_", "from_block_id"), ("to_", "to_block_id")] {
            if let Some(field) = record.remove(source) {
                record.insert(target.into(), field);
            }
        }
    }
    value
}

fn missing(kind: &str, id: i32) -> Value {
    json!({"type":kind,"id":id,"error":{"code":"not_found","message":format!("{kind} {id} not found")}})
}

fn multipart(metadata: Value, blobs: Vec<(String, Vec<u8>)>) -> ApiResult<Response> {
    let boundary = Uuid::new_v4().simple().to_string();
    let mut bytes = format!("--{boundary}\r\nContent-Type: application/json\r\n\r\n").into_bytes();
    serde_json::to_writer(&mut bytes, &metadata)
        .map_err(|_| ApiError::invalid("unable to represent entities"))?;
    for (id, data) in blobs {
        let marker = format!(
            "\r\n--{boundary}\r\nContent-Type: application/octet-stream\r\nContent-ID: <{id}>\r\n\r\n"
        );
        bytes.extend_from_slice(marker.as_bytes());
        bytes.extend(data);
    }
    bytes.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Ok((
        [(
            header::CONTENT_TYPE,
            format!("multipart/related; boundary={boundary}; type=\"application/json\""),
        )],
        bytes,
    )
        .into_response())
}
