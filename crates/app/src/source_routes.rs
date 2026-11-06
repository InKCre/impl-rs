use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use inkcre_core::{
    Job, Page,
    cron::{Cron, CronConfig, CronInput, CronPatch},
    source::{Source, SourceCommand, SourceInput, SourcePatch, SourceType},
};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
    routes::AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/source-types", get(source_types))
        .route("/source-types/{id}", get(source_type))
        .route("/sources", get(sources).post(source_create))
        .route(
            "/sources/{id}",
            get(source_get).patch(source_patch).delete(source_delete),
        )
        .route("/sources/{id}/anchor", post(source_anchor))
        .route("/sources/{id}/collect", post(source_collect))
        .route("/sources/{id}/backfill", post(source_backfill))
        .route("/crons", get(crons).post(cron_create))
        .route(
            "/crons/{id}",
            get(cron_get).patch(cron_patch).delete(cron_delete),
        )
        .route("/crons/{id}/run", post(cron_run))
        .route("/crons/{id}/materialize", post(cron_materialize))
        .route("/cron/config", get(cron_config).put(cron_config_set))
}

fn default_limit() -> i64 {
    50
}

#[derive(Deserialize)]
struct TypePage {
    #[serde(default = "default_limit")]
    limit: i64,
    cursor: Option<String>,
}

async fn source_types(
    State(state): State<AppState>,
    ApiQuery(page): ApiQuery<TypePage>,
) -> ApiResult<Json<Page<SourceType, String>>> {
    Ok(Json(
        state
            .core
            .source_types(page.limit, page.cursor.as_deref())
            .await?,
    ))
}

async fn source_type(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<SourceType>> {
    Ok(Json(state.core.source_type_get(&id).await?.ok_or_else(
        || ApiError::not_found("source type not found"),
    )?))
}

#[derive(Deserialize)]
struct SourcePage {
    #[serde(default = "default_limit")]
    limit: i64,
    cursor: Option<i32>,
}

async fn sources(
    State(state): State<AppState>,
    ApiQuery(page): ApiQuery<SourcePage>,
) -> ApiResult<Json<Page<Source, i32>>> {
    Ok(Json(state.core.sources(page.limit, page.cursor).await?))
}

async fn source_get(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
) -> ApiResult<Json<Source>> {
    Ok(Json(
        state
            .core
            .source_get(id)
            .await?
            .ok_or_else(|| ApiError::not_found("source not found"))?,
    ))
}

async fn source_create(
    State(state): State<AppState>,
    ApiJson(input): ApiJson<SourceInput>,
) -> ApiResult<(StatusCode, Json<Source>)> {
    Ok((
        StatusCode::CREATED,
        Json(state.core.source_create(input).await?),
    ))
}

async fn source_patch(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
    ApiJson(input): ApiJson<SourcePatch>,
) -> ApiResult<Json<Source>> {
    Ok(Json(state.core.source_patch(id, input).await?))
}

async fn source_delete(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
) -> ApiResult<StatusCode> {
    if !state.core.source_delete(id).await? {
        return Err(ApiError::not_found("source not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn source_anchor(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
) -> ApiResult<Json<inkcre_core::Block>> {
    Ok(Json(state.core.source_ensure_anchor(id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandInput {
    #[serde(default = "inkcre_core::models::empty_object")]
    config: Value,
    timeout_seconds: Option<i32>,
}

fn notify(state: &AppState, job: &Job) {
    if let Some(worker) = &state.worker {
        worker.notify_job(job.id);
    }
}

async fn source_collect(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
    ApiJson(input): ApiJson<CommandInput>,
) -> ApiResult<(StatusCode, Json<Job>)> {
    let job = state
        .core
        .source_submit(
            id,
            SourceCommand::Collect,
            input.config,
            input.timeout_seconds,
        )
        .await?;
    notify(&state, &job);
    Ok((StatusCode::ACCEPTED, Json(job)))
}

async fn source_backfill(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
    ApiJson(input): ApiJson<CommandInput>,
) -> ApiResult<(StatusCode, Json<Job>)> {
    let job = state
        .core
        .source_submit(
            id,
            SourceCommand::Backfill,
            input.config,
            input.timeout_seconds,
        )
        .await?;
    notify(&state, &job);
    Ok((StatusCode::ACCEPTED, Json(job)))
}

#[derive(Deserialize)]
struct CronPage {
    #[serde(default = "default_limit")]
    limit: i64,
    cursor: Option<i64>,
}

async fn crons(
    State(state): State<AppState>,
    ApiQuery(page): ApiQuery<CronPage>,
) -> ApiResult<Json<Page<Cron, i64>>> {
    Ok(Json(state.core.crons(page.limit, page.cursor).await?))
}

async fn cron_get(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i64>,
) -> ApiResult<Json<Cron>> {
    Ok(Json(
        state
            .core
            .cron_get(id)
            .await?
            .ok_or_else(|| ApiError::not_found("cron not found"))?,
    ))
}

async fn cron_create(
    State(state): State<AppState>,
    ApiJson(input): ApiJson<CronInput>,
) -> ApiResult<(StatusCode, Json<Cron>)> {
    Ok((
        StatusCode::CREATED,
        Json(state.core.cron_create(input).await?),
    ))
}

async fn cron_patch(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i64>,
    ApiJson(input): ApiJson<CronPatch>,
) -> ApiResult<Json<Cron>> {
    Ok(Json(state.core.cron_patch(id, input).await?))
}

async fn cron_delete(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i64>,
) -> ApiResult<StatusCode> {
    if !state.core.cron_delete(id).await? {
        return Err(ApiError::not_found("cron not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn cron_run(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i64>,
) -> ApiResult<(StatusCode, Json<Job>)> {
    let job = state.core.cron_run_now(id).await?;
    notify(&state, &job);
    Ok((StatusCode::ACCEPTED, Json(job)))
}

async fn cron_materialize(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i64>,
) -> ApiResult<Json<Option<Job>>> {
    let job = state.core.cron_materialize(id).await?;
    if let Some(job) = &job {
        notify(&state, job);
    }
    Ok(Json(job))
}

async fn cron_config(State(state): State<AppState>) -> ApiResult<Json<CronConfig>> {
    Ok(Json(state.core.cron_config_get().await?))
}

async fn cron_config_set(
    State(state): State<AppState>,
    ApiJson(config): ApiJson<CronConfig>,
) -> ApiResult<Json<CronConfig>> {
    state.core.cron_config_set(&config).await?;
    Ok(Json(config))
}
