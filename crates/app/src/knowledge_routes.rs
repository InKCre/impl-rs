use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use inkcre_core::knowledge::{
    GraphDraft, PersistedDraft, ReadMethod, ReadOptions, ResolverDescriptor, StorageDescriptor,
    TextOptions,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    error::{ApiBytes, ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
    routes::AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/resolvers", get(resolvers))
        .route("/resolvers/{resolver}/methods", get(resolver_methods))
        .route("/blocks/{id}/read-methods", get(block_methods))
        .route("/blocks/{id}/solved", get(solved))
        .route("/blocks/{id}/text", get(text))
        .route("/blocks/{id}/label", get(label))
        .route("/blocks/{id}/invoke/{method}", post(invoke))
        .route("/graph/draft", post(draft))
        .route("/graph/submit", post(submit))
        .route("/storage-types", get(storage_types))
        .route("/storages", post(create_storage))
        .route(
            "/storages/{id}/objects",
            get(read_object)
                .post(create_object)
                .put(update_object)
                .delete(delete_object),
        )
}

async fn resolvers(State(state): State<AppState>) -> ApiResult<Json<Vec<ResolverDescriptor>>> {
    Ok(Json(state.core.knowledge().registry().resolvers()?))
}

async fn resolver_methods(
    State(state): State<AppState>,
    ApiPath(resolver): ApiPath<String>,
) -> ApiResult<Json<Vec<ReadMethod>>> {
    Ok(Json(state.core.knowledge().read_methods(&resolver)?))
}

async fn block_methods(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
) -> ApiResult<Json<Vec<ReadMethod>>> {
    let block = state
        .core
        .block_get(id)
        .await?
        .ok_or_else(|| ApiError::not_found("block not found"))?;
    Ok(Json(state.core.knowledge().read_methods(&block.resolver)?))
}

async fn solved(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
    ApiQuery(options): ApiQuery<ReadOptions>,
) -> ApiResult<Json<Value>> {
    Ok(Json(state.core.knowledge().solved(id, options).await?))
}

async fn text(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
    ApiQuery(options): ApiQuery<TextOptions>,
) -> ApiResult<Json<Option<String>>> {
    Ok(Json(state.core.knowledge().text(id, options).await?))
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Refresh {
    refresh: bool,
}

async fn label(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
    ApiQuery(options): ApiQuery<Refresh>,
) -> ApiResult<Json<String>> {
    Ok(Json(
        state.core.knowledge().label(id, options.refresh).await?,
    ))
}

async fn invoke(
    State(state): State<AppState>,
    ApiPath((id, method)): ApiPath<(i32, String)>,
    ApiJson(arguments): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        state
            .core
            .knowledge()
            .invoke(id, &method, arguments)
            .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftInput {
    resolver: String,
    input: Value,
}

async fn draft(
    State(state): State<AppState>,
    ApiJson(input): ApiJson<DraftInput>,
) -> ApiResult<Json<GraphDraft>> {
    Ok(Json(
        state
            .core
            .knowledge()
            .draft(&input.resolver, input.input)
            .await?,
    ))
}

async fn submit(
    State(state): State<AppState>,
    ApiJson(input): ApiJson<GraphDraft>,
) -> ApiResult<(StatusCode, Json<PersistedDraft>)> {
    Ok((
        StatusCode::CREATED,
        Json(state.core.knowledge().submit_draft(input).await?),
    ))
}

async fn storage_types(State(state): State<AppState>) -> ApiResult<Json<Vec<StorageDescriptor>>> {
    Ok(Json(state.core.knowledge().registry().storages()?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StorageInput {
    #[serde(rename = "type")]
    type_: String,
    nickname: Option<String>,
    #[serde(default = "inkcre_core::models::empty_object")]
    config: Value,
}

async fn create_storage(
    State(state): State<AppState>,
    ApiJson(input): ApiJson<StorageInput>,
) -> ApiResult<(StatusCode, Json<inkcre_core::Storage>)> {
    let storage = state
        .core
        .knowledge()
        .create_storage(&input.type_, input.nickname.as_deref(), input.config)
        .await?;
    Ok((StatusCode::CREATED, Json(storage)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectQuery {
    pointer: String,
}

async fn read_object(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
    ApiQuery(query): ApiQuery<ObjectQuery>,
) -> ApiResult<Response> {
    let storage = state
        .core
        .storage_get(id)
        .await?
        .ok_or_else(|| ApiError::not_found("storage not found"))?;
    let knowledge = state.core.knowledge();
    let handler = knowledge.registry().storage(&storage.type_)?;
    let bytes = handler.read(&state.core, &storage, &query.pointer).await?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

async fn create_object(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
    ApiBytes(bytes): ApiBytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let mut tx = state
        .core
        .pool()
        .begin()
        .await
        .map_err(inkcre_core::Error::from)?;
    let pointer = state
        .core
        .knowledge()
        .storage_create(&mut tx, id, &bytes)
        .await?;
    tx.commit().await.map_err(inkcre_core::Error::from)?;
    Ok((StatusCode::CREATED, Json(json!({"pointer":pointer}))))
}

async fn update_object(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
    ApiQuery(query): ApiQuery<ObjectQuery>,
    ApiBytes(bytes): ApiBytes,
) -> ApiResult<StatusCode> {
    let mut tx = state
        .core
        .pool()
        .begin()
        .await
        .map_err(inkcre_core::Error::from)?;
    if !state
        .core
        .knowledge()
        .storage_update(&mut tx, id, &query.pointer, &bytes)
        .await?
    {
        return Err(ApiError::not_found("storage object not found"));
    }
    tx.commit().await.map_err(inkcre_core::Error::from)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_object(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
    ApiQuery(query): ApiQuery<ObjectQuery>,
) -> ApiResult<StatusCode> {
    let mut tx = state
        .core
        .pool()
        .begin()
        .await
        .map_err(inkcre_core::Error::from)?;
    if !state
        .core
        .knowledge()
        .storage_delete(&mut tx, id, &query.pointer)
        .await?
    {
        return Err(ApiError::not_found("storage object not found"));
    }
    tx.commit().await.map_err(inkcre_core::Error::from)?;
    Ok(StatusCode::NO_CONTENT)
}
