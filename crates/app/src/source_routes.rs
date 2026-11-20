use axum::{
    Json, Router,
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use inkcre_core::{
    Job,
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

#[derive(Deserialize)]
struct TypePage {
    limit: Option<i64>,
    cursor: Option<String>,
}

async fn source_types(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    ApiQuery(page): ApiQuery<TypePage>,
) -> ApiResult<Json<Value>> {
    crate::routes::resource_page(
        &uri,
        "source_types",
        page.limit,
        page.cursor,
        |limit, cursor| {
            let core = state.core.clone();
            async move { core.source_types(limit, cursor.as_deref()).await }
        },
    )
    .await
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
    limit: Option<i64>,
    cursor: Option<i32>,
}

async fn sources(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    ApiQuery(page): ApiQuery<SourcePage>,
) -> ApiResult<Json<Value>> {
    crate::routes::resource_page(&uri, "sources", page.limit, page.cursor, |limit, cursor| {
        state.core.sources(limit, cursor)
    })
    .await
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

fn resource_location(uri: &axum::http::Uri, path: String) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if !crate::routes::legacy_uri(uri) {
        headers.insert(
            header::LOCATION,
            path.parse().expect("generated resource URI"),
        );
    }
    headers
}
fn write_error(uri: &axum::http::Uri, error: inkcre_core::Error) -> ApiError {
    if crate::routes::legacy_uri(uri) {
        error.into()
    } else {
        crate::error::database_write(error)
    }
}
fn admission_error(uri: &axum::http::Uri, error: inkcre_core::Error) -> ApiError {
    if crate::routes::legacy_uri(uri) {
        error.into()
    } else {
        crate::error::command_admission(error)
    }
}

async fn source_create(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    ApiJson(input): ApiJson<SourceInput>,
) -> ApiResult<(StatusCode, HeaderMap, Json<Source>)> {
    let source = state
        .core
        .source_create(input)
        .await
        .map_err(|error| write_error(&uri, error))?;
    Ok((
        StatusCode::CREATED,
        resource_location(&uri, format!("/sources/{}", source.id)),
        Json(source),
    ))
}

async fn source_patch(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    ApiPath(id): ApiPath<i32>,
    ApiJson(input): ApiJson<SourcePatch>,
) -> ApiResult<Json<Source>> {
    Ok(Json(
        state
            .core
            .source_patch(id, input)
            .await
            .map_err(|error| write_error(&uri, error))?,
    ))
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

fn source_command_config(headers: &HeaderMap, bytes: &[u8], required: bool) -> ApiResult<Value> {
    if bytes.is_empty() {
        return if required {
            Err(ApiError::invalid("backfill config must be a JSON object"))
        } else {
            Ok(inkcre_core::models::empty_object())
        };
    }
    if let Some(content_type) = headers.get(header::CONTENT_TYPE) {
        let media = content_type
            .to_str()
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if media != "application/json"
            && !(media.starts_with("application/") && media.ends_with("+json"))
        {
            return Err(ApiError::invalid("Source config must be a JSON object"));
        }
    }
    let config: Value = serde_json::from_slice(bytes)
        .map_err(|_| ApiError::invalid("Source config must be a JSON object"))?;
    if config.is_null() && !required {
        return Ok(inkcre_core::models::empty_object());
    }
    if !config.is_object() {
        return Err(ApiError::invalid("Source config must be a JSON object"));
    }
    Ok(config)
}
async fn collect(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
    ApiQuery(query): ApiQuery<CommandQuery>,
    headers: HeaderMap,
    ApiBytes(bytes): ApiBytes,
) -> ApiResult<Response> {
    let config = source_command_config(&headers, &bytes, false)?;
    source_command(&state, id, SourceCommand::Collect, config, query).await
}
async fn backfill(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
    ApiQuery(query): ApiQuery<CommandQuery>,
    headers: HeaderMap,
    ApiBytes(bytes): ApiBytes,
) -> ApiResult<Response> {
    let config = source_command_config(&headers, &bytes, true)?;
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
    limit: Option<i64>,
    cursor: Option<i64>,
}

async fn crons(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    ApiQuery(page): ApiQuery<CronPage>,
) -> ApiResult<Json<Value>> {
    crate::routes::resource_page(&uri, "crons", page.limit, page.cursor, |limit, cursor| {
        state.core.crons(limit, cursor)
    })
    .await
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
    OriginalUri(uri): OriginalUri,
    ApiJson(input): ApiJson<CronInput>,
) -> ApiResult<(StatusCode, HeaderMap, Json<Cron>)> {
    let cron = state
        .core
        .cron_create(input)
        .await
        .map_err(|error| admission_error(&uri, error))?;
    Ok((
        StatusCode::CREATED,
        resource_location(&uri, format!("/crons/{}", cron.id)),
        Json(cron),
    ))
}

async fn cron_patch(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    ApiPath(id): ApiPath<i64>,
    ApiJson(input): ApiJson<CronPatch>,
) -> ApiResult<Json<Cron>> {
    Ok(Json(
        state
            .core
            .cron_patch(id, input)
            .await
            .map_err(|error| admission_error(&uri, error))?,
    ))
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
    OriginalUri(uri): OriginalUri,
    ApiPath(id): ApiPath<i64>,
) -> ApiResult<(StatusCode, HeaderMap, Json<Job>)> {
    let job = state
        .core
        .cron_run_now(id)
        .await
        .map_err(|error| admission_error(&uri, error))?;
    notify(&state, &job);
    Ok((
        if crate::routes::legacy_uri(&uri) {
            StatusCode::ACCEPTED
        } else {
            StatusCode::CREATED
        },
        resource_location(&uri, format!("/jobs/{}", job.id)),
        Json(job),
    ))
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
