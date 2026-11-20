use axum::{
    Json, Router,
    extract::State,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
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
    error::{ApiBytes, ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
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
        .route("/crons", get(crons).post(cron_create))
        .route(
            "/crons/{id}",
            get(cron_get).patch(cron_patch).delete(cron_delete),
        )
        .route("/crons/{id}/run", post(cron_run))
        .route("/crons/{id}/materialize", post(cron_materialize))
        .route("/cron/config", get(cron_config).put(cron_config_set))
}

pub fn commands(legacy: bool) -> Router<AppState> {
    if legacy {
        Router::new()
            .route("/sources/{id}/collect", post(legacy_collect))
            .route("/sources/{id}/backfill", post(legacy_backfill))
    } else {
        Router::new()
            .route("/sources/{id}/collect", post(collect))
            .route("/sources/{id}/backfill", post(backfill))
    }
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

async fn legacy_collect(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
    ApiJson(input): ApiJson<CommandInput>,
) -> ApiResult<(StatusCode, Json<Job>)> {
    Ok((
        StatusCode::ACCEPTED,
        Json(submit_source(&state, id, SourceCommand::Collect, input).await?),
    ))
}

async fn legacy_backfill(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
    ApiJson(input): ApiJson<CommandInput>,
) -> ApiResult<(StatusCode, Json<Job>)> {
    Ok((
        StatusCode::ACCEPTED,
        Json(submit_source(&state, id, SourceCommand::Backfill, input).await?),
    ))
}

#[derive(Deserialize)]
struct CommandQuery {
    timeout_seconds: Option<i32>,
}

async fn collect(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
    ApiQuery(query): ApiQuery<CommandQuery>,
    ApiBytes(bytes): ApiBytes,
) -> ApiResult<Response> {
    let config = if bytes.is_empty() {
        inkcre_core::models::empty_object()
    } else {
        serde_json::from_slice(&bytes)
            .map_err(|_| ApiError::invalid("collect config must be a JSON object"))?
    };
    source_command(&state, id, SourceCommand::Collect, config, query).await
}

async fn backfill(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
    ApiQuery(query): ApiQuery<CommandQuery>,
    ApiJson(config): ApiJson<Value>,
) -> ApiResult<Response> {
    source_command(&state, id, SourceCommand::Backfill, config, query).await
}

async fn source_command(
    state: &AppState,
    id: i32,
    command: SourceCommand,
    config: Value,
    query: CommandQuery,
) -> ApiResult<Response> {
    let job = submit_source(
        state,
        id,
        command,
        CommandInput {
            config,
            timeout_seconds: query.timeout_seconds,
        },
    )
    .await
    .map_err(|error| {
        if error.status == StatusCode::NOT_IMPLEMENTED {
            ApiError::invalid("Source type does not support this command")
        } else {
            error
        }
    })?;
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, format!("/jobs/{}", job.id))],
        Json(job),
    )
        .into_response())
}

async fn submit_source(
    state: &AppState,
    id: i32,
    command: SourceCommand,
    input: CommandInput,
) -> ApiResult<Job> {
    let job = state
        .core
        .source_submit(id, command, input.config, input.timeout_seconds)
        .await?;
    notify(state, &job);
    Ok(job)
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
