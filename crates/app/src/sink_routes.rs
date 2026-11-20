use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    routing::{get, post, put},
};
use inkcre_core::{
    Job,
    sink::{QueryInput, Sink, SinkInput, SinkType},
};
use serde_json::Value;

use crate::{
    error::{ApiError, ApiJson, ApiPath, ApiResult},
    routes::AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/sink-types", get(types))
        .route("/sink-types/{id}", get(sink_type))
        .route("/sinks", get(list).post(create))
        .route("/sinks/{id}", get(read).delete(delete))
        .route("/sinks/{id}/config", put(config))
        .route("/sinks/{id}/enable", post(enable))
        .route("/sinks/{id}/disable", post(disable))
        .route("/sinks/{id}/query", post(query))
}

async fn types(State(state): State<AppState>) -> ApiResult<Json<Vec<SinkType>>> {
    Ok(Json(state.core.sink_types().await?))
}

async fn sink_type(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<SinkType>> {
    Ok(Json(state.core.sink_type(&id).await?.ok_or_else(|| {
        ApiError::not_found("Sink type not found")
    })?))
}

async fn list(State(state): State<AppState>) -> ApiResult<Json<Vec<Sink>>> {
    Ok(Json(state.core.sinks().await?))
}

async fn read(State(state): State<AppState>, ApiPath(id): ApiPath<i64>) -> ApiResult<Json<Sink>> {
    Ok(Json(
        state
            .core
            .sink(id)
            .await?
            .ok_or_else(|| ApiError::not_found("Sink not found"))?,
    ))
}

async fn create(
    State(state): State<AppState>,
    ApiJson(input): ApiJson<SinkInput>,
) -> ApiResult<(StatusCode, Json<Sink>)> {
    Ok((
        StatusCode::CREATED,
        Json(state.core.sink_create(input).await?),
    ))
}

async fn config(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i64>,
    ApiJson(config): ApiJson<Value>,
) -> ApiResult<Json<Sink>> {
    Ok(Json(state.core.sink_replace_config(id, config).await?))
}

async fn enable(State(state): State<AppState>, ApiPath(id): ApiPath<i64>) -> ApiResult<Json<Sink>> {
    Ok(Json(state.core.sink_enable(id, state.peer_id, true).await?))
}

async fn disable(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i64>,
) -> ApiResult<Json<Sink>> {
    Ok(Json(
        state.core.sink_enable(id, state.peer_id, false).await?,
    ))
}

async fn delete(State(state): State<AppState>, ApiPath(id): ApiPath<i64>) -> ApiResult<StatusCode> {
    state.core.sink_delete(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn query(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i64>,
    ApiJson(input): ApiJson<QueryInput>,
) -> ApiResult<(StatusCode, [(header::HeaderName, String); 1], Json<Job>)> {
    let job = state.core.sink_query(id, state.peer_id, input).await?;
    if let Some(worker) = &state.worker {
        worker.notify_job(job.id);
    }
    Ok((
        StatusCode::ACCEPTED,
        [(header::LOCATION, format!("/jobs/{}", job.id))],
        Json(job),
    ))
}
