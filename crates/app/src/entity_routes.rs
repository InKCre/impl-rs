use axum::{
    Json, Router,
    extract::{OriginalUri, State},
    response::{IntoResponse, Response},
    routing::post,
};
use inkcre_core::{
    BlockInput, GraphInput, HydratedContent,
    knowledge::{DraftBlock, DraftRelation},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json, value::RawValue};
use std::collections::BTreeMap;

use crate::{
    error::{ApiError, ApiJson, ApiResult},
    routes::{AppState, legacy_uri},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/graph", post(submit_graph))
        .route("/entities/get", post(entities))
}

/// Command-local identities are arbitrary negative JSON integers, not PostgreSQL IDs.
/// RawValue preserves every digit until normalization and when echoing the result.
#[derive(schemars::JsonSchema, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CanonicalGraphForm {
    #[serde(default)]
    blocks: Vec<CanonicalGraphBlock>,
    #[serde(default)]
    relations: Vec<CanonicalGraphRelation>,
}
#[derive(schemars::JsonSchema, Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalGraphBlock {
    #[schemars(schema_with = "negative_id_schema")]
    id: Box<RawValue>,
    #[serde(default)]
    storage: Option<i32>,
    resolver: String,
    content: String,
}
#[derive(schemars::JsonSchema, Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalGraphRelation {
    #[schemars(schema_with = "nonzero_id_schema")]
    from_: Box<RawValue>,
    #[schemars(schema_with = "nonzero_id_schema")]
    to_: Box<RawValue>,
    content: String,
}
fn negative_id_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":"integer","maximum":-1})
}
fn nonzero_id_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({"type":"integer","not":{"const":0}})
}

fn invalid_graph() -> ApiError {
    ApiError::new(
        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_json",
        "request body must contain valid JSON with the required fields",
    )
}

enum GraphId<'a> {
    Local(&'a str),
    Persisted(i32),
}
fn graph_id(raw: &RawValue) -> Result<GraphId<'_>, ApiError> {
    let token = raw.get();
    let (negative, digits) = token
        .strip_prefix('-')
        .map_or((false, token), |digits| (true, digits));
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) || digits == "0" {
        return Err(ApiError::invalid(
            "GraphForm IDs must be non-zero JSON integers",
        ));
    }
    if negative {
        Ok(GraphId::Local(token))
    } else {
        let id = token
            .parse::<i32>()
            .map_err(|_| ApiError::invalid("persisted Block ID is outside the database range"))?;
        Ok(GraphId::Persisted(id))
    }
}
#[derive(Serialize)]
struct CanonicalGraphMapping {
    local_id: Box<RawValue>,
    id: i32,
}
#[derive(Serialize)]
struct CanonicalGraphSubmission {
    blocks: Vec<CanonicalGraphMapping>,
}
impl CanonicalGraphForm {
    fn normalize(self) -> Result<(GraphInput, BTreeMap<i32, Box<RawValue>>), ApiError> {
        let mut local = BTreeMap::new();
        let mut originals = BTreeMap::new();
        let mut blocks = Vec::with_capacity(self.blocks.len());
        for (index, block) in self.blocks.into_iter().enumerate() {
            let GraphId::Local(key) = graph_id(&block.id)? else {
                return Err(ApiError::invalid("new Block IDs must be negative"));
            };
            let id = i32::try_from(index)
                .ok()
                .and_then(|index| index.checked_add(1))
                .and_then(i32::checked_neg)
                .ok_or_else(|| ApiError::invalid("too many command-local Blocks"))?;
            if local.insert(key.to_owned(), id).is_some() {
                return Err(ApiError::invalid("GraphForm new Block IDs must be unique"));
            }
            originals.insert(id, block.id);
            blocks.push(DraftBlock {
                id,
                block: BlockInput {
                    storage: block.storage,
                    resolver: block.resolver,
                    content: block.content,
                },
            });
        }
        let endpoint = |raw: &RawValue| match graph_id(raw)? {
            GraphId::Local(key) => local.get(key).copied().ok_or_else(|| {
                ApiError::invalid("GraphForm contains an undeclared local Block ID")
            }),
            GraphId::Persisted(id) => Ok(id),
        };
        let relations = self
            .relations
            .into_iter()
            .map(|relation| {
                Ok(DraftRelation {
                    from_: endpoint(&relation.from_)?,
                    to_: endpoint(&relation.to_)?,
                    content: relation.content,
                })
            })
            .collect::<Result<Vec<_>, ApiError>>()?;
        Ok((GraphInput { blocks, relations }, originals))
    }
}
async fn submit_graph(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    ApiJson(raw): ApiJson<Box<RawValue>>,
) -> ApiResult<Response> {
    if legacy_uri(&uri) {
        let form: GraphInput = serde_json::from_str(raw.get()).map_err(|_| invalid_graph())?;
        return Ok(Json(state.core.submit_graph(form).await?).into_response());
    }
    let form: CanonicalGraphForm = serde_json::from_str(raw.get()).map_err(|_| invalid_graph())?;
    let (form, mut originals) = form.normalize()?;
    let result = state
        .core
        .submit_graph(form)
        .await
        .map_err(crate::error::database_write)?;
    let blocks = result
        .blocks
        .into_iter()
        .map(|mapping| {
            let local_id = originals
                .remove(&mapping.local_id)
                .expect("submitted command-local identity");
            CanonicalGraphMapping {
                local_id,
                id: mapping.id,
            }
        })
        .collect();
    // Serialize RawValue directly; converting through serde_json::Value would round large IDs.
    Ok(Json(CanonicalGraphSubmission { blocks }).into_response())
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
