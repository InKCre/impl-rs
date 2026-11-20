use axum::{Json, Router, extract::State, response::Response, routing::post};
use inkcre_core::{GraphInput, GraphSubmission, HydratedContent};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    error::{ApiJson, ApiResult},
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

#[derive(schemars::JsonSchema, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntityKind {
    Block,
    Relation,
}

#[derive(schemars::JsonSchema, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EntityRef {
    #[serde(rename = "type")]
    kind: EntityKind,
    id: i32,
}

#[derive(schemars::JsonSchema, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContentMode {
    None,
    #[default]
    Raw,
    Hydrated,
}

#[derive(schemars::JsonSchema, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EntityForm {
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
                                blobs.push((format!("/{index}/hydrated_content"), bytes));
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
    crate::content::project(json!(results), blobs)
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
