use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use inkcre_core::extension::ExtensionHost;
use inkcre_core::{
    Block, BlockInput, BlockPatch, Core, Page, Relation, RelationInput, RelationPatch,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::{
    auth,
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
};

#[derive(Clone)]
pub struct AppState {
    pub core: Core,
    pub host: ExtensionHost,
    pub peer_id: Uuid,
    pub secret: Arc<str>,
    pub lease_valid: Arc<AtomicBool>,
    pub catalog: Arc<BTreeMap<(String, String), PathBuf>>,
    pub timeout: Duration,
}

pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/self", get(self_info))
        .route("/blocks", get(blocks).post(block_create))
        .route(
            "/blocks/{id}",
            get(block_get).patch(block_patch).delete(block_delete),
        )
        .route("/blocks/{id}/relations", get(block_relations))
        .route("/blocks/{id}/content", get(block_content))
        .route("/relations", post(relation_create))
        .route(
            "/relations/{id}",
            get(relation_get)
                .patch(relation_patch)
                .delete(relation_delete),
        )
        .route("/relations/{id}/neighborhood", get(relation_neighborhood))
        .route("/graph/neighborhood", post(graph_neighborhood))
        .route("/graph/path", get(graph_path).post(graph_path_post))
        .route("/storages", get(storages))
        .route("/storage-blobs", post(blob_create))
        .route(
            "/storage-blobs/{id}",
            get(blob_read).put(blob_update).delete(blob_delete),
        )
        .route("/job-types", get(job_types).post(job_type_upsert))
        .route("/job-types/{id}", get(job_type_get).delete(job_type_delete))
        .route("/jobs", get(jobs).post(job_submit))
        .route("/jobs/{id}", get(job_get).delete(job_delete))
        .route("/jobs/{id}/claim", post(job_claim))
        .route("/jobs/{id}/finish", post(job_finish))
        .route("/jobs/{id}/abort", post(job_abort))
        .route("/peers", get(peers))
        .route("/peers/{id}", get(peer_get).delete(peer_delete))
        .route("/extensions", get(extensions))
        .route("/extensions/catalog", get(extension_catalog))
        .route("/extensions/install", post(extension_install))
        .route("/extensions/{name}/enable", post(extension_enable))
        .route("/extensions/{name}/disable", post(extension_disable))
        .route("/extensions/{name}/invoke", post(extension_invoke))
        .route(
            "/extensions/{name}/config",
            get(extension_config).patch(extension_patch_config),
        )
        .route(
            "/extensions/{name}/participation",
            axum::routing::delete(extension_unselect),
        )
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    Router::new()
        .route(
            "/health/live",
            get(|| async { Json(json!({"status":"live"})) }),
        )
        .route("/health/ready", get(ready))
        .nest("/api/v1", protected)
        .fallback(|| async { ApiError::not_found("route not found") })
        .method_not_allowed_fallback(|| async {
            ApiError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "method_not_allowed",
                "method not allowed",
            )
        })
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            request_timeout,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn authenticate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if !token.is_some_and(|token| auth::verify(token, &state.secret)) {
        let mut response = ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "valid Peer bearer token required",
        )
        .into_response();
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            axum::http::HeaderValue::from_static("Bearer"),
        );
        return response;
    }
    next.run(request).await
}

async fn request_timeout(State(state): State<AppState>, request: Request, next: Next) -> Response {
    match tokio::time::timeout(state.timeout, next.run(request)).await {
        Ok(response) => response,
        Err(_) => ApiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "request_timeout",
            "request timed out; the operation outcome may be unknown",
        )
        .into_response(),
    }
}

async fn ready(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    if !state.lease_valid.load(Ordering::Relaxed) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "peer_unavailable",
            "Peer lease is not current",
        ));
    }
    state.core.admission().await?;
    Ok(Json(json!({"status":"ready"})))
}

async fn self_info(State(state): State<AppState>) -> Json<Value> {
    Json(
        json!({"peer_id":state.peer_id,"application_version":env!("CARGO_PKG_VERSION"),"supported_distributions":["rust_stdio"]}),
    )
}

#[derive(Deserialize)]
struct PageQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    cursor: Option<i32>,
}

fn default_limit() -> i64 {
    50
}

async fn blocks(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<PageQuery>,
) -> ApiResult<Json<Page<Block, i32>>> {
    Ok(Json(
        state.core.blocks_recent(query.limit, query.cursor).await?,
    ))
}

async fn block_create(
    State(state): State<AppState>,
    ApiJson(input): ApiJson<BlockInput>,
) -> ApiResult<(StatusCode, Json<Block>)> {
    Ok((
        StatusCode::CREATED,
        Json(state.core.block_create(input).await?),
    ))
}

async fn block_get(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
) -> ApiResult<Json<Block>> {
    Ok(Json(
        state
            .core
            .block_get(id)
            .await?
            .ok_or_else(|| ApiError::not_found("block not found"))?,
    ))
}

async fn block_patch(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
    ApiJson(input): ApiJson<BlockPatch>,
) -> ApiResult<Json<Block>> {
    Ok(Json(state.core.block_patch(id, input).await?))
}

async fn block_delete(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
) -> ApiResult<StatusCode> {
    if !state.core.block_delete(id).await? {
        return Err(ApiError::not_found("block not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn block_relations(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
    ApiQuery(query): ApiQuery<PageQuery>,
) -> ApiResult<Json<Page<Relation, i32>>> {
    Ok(Json(
        state
            .core
            .relations_for_block(id, query.limit, query.cursor)
            .await?,
    ))
}

async fn relation_create(
    State(state): State<AppState>,
    ApiJson(input): ApiJson<RelationInput>,
) -> ApiResult<(StatusCode, Json<Relation>)> {
    Ok((
        StatusCode::CREATED,
        Json(state.core.relation_create(input).await?),
    ))
}

async fn relation_get(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
) -> ApiResult<Json<Relation>> {
    Ok(Json(state.core.relation_get(id).await?.ok_or_else(
        || ApiError::not_found("relation not found"),
    )?))
}

async fn relation_patch(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
    ApiJson(input): ApiJson<RelationPatch>,
) -> ApiResult<Json<Relation>> {
    Ok(Json(state.core.relation_patch(id, input).await?))
}

async fn relation_delete(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
) -> ApiResult<StatusCode> {
    if !state.core.relation_delete(id).await? {
        return Err(ApiError::not_found("relation not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct PeerPageQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    cursor: Option<Uuid>,
}

async fn peers(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<PeerPageQuery>,
) -> ApiResult<Json<Page<inkcre_core::Peer, Uuid>>> {
    Ok(Json(state.core.peer_list(query.limit, query.cursor).await?))
}

async fn peer_get(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<inkcre_core::Peer>> {
    Ok(Json(
        state
            .core
            .peer_get(id)
            .await?
            .ok_or_else(|| ApiError::not_found("peer not found"))?,
    ))
}

async fn peer_delete(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<StatusCode> {
    if !state.core.peer_delete(id).await? {
        return Err(ApiError::not_found("peer not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn extensions(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let status = state.host.list().await.map_err(extension_error)?;
    Ok(Json(
        serde_json::to_value(status).map_err(|_| extension_response_error())?,
    ))
}

#[derive(Serialize)]
struct CatalogEntry {
    name: String,
    version: String,
    distribution: &'static str,
}

async fn extension_catalog(State(state): State<AppState>) -> Json<Vec<CatalogEntry>> {
    Json(
        state
            .catalog
            .keys()
            .map(|(name, version)| CatalogEntry {
                name: name.clone(),
                version: version.clone(),
                distribution: "rust_stdio",
            })
            .collect(),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallInput {
    name: String,
    version: String,
}

async fn extension_install(
    State(state): State<AppState>,
    ApiJson(input): ApiJson<InstallInput>,
) -> ApiResult<Json<Value>> {
    let path = state
        .catalog
        .get(&(input.name.clone(), input.version.clone()))
        .ok_or_else(|| ApiError::invalid("extension is not in the trusted startup catalog"))?;
    let result = state
        .host
        .install(path.clone(), &input.name, &input.version)
        .await
        .map_err(extension_error)?;
    Ok(Json(
        serde_json::to_value(result).map_err(|_| extension_response_error())?,
    ))
}

async fn extension_enable(
    State(state): State<AppState>,
    ApiPath(name): ApiPath<String>,
) -> ApiResult<Json<Value>> {
    let result = state.host.enable(&name).await.map_err(extension_error)?;
    Ok(Json(
        serde_json::to_value(result).map_err(|_| extension_response_error())?,
    ))
}

async fn extension_disable(
    State(state): State<AppState>,
    ApiPath(name): ApiPath<String>,
) -> ApiResult<Json<Value>> {
    let result = state.host.disable(&name).await.map_err(extension_error)?;
    Ok(Json(
        serde_json::to_value(result).map_err(|_| extension_response_error())?,
    ))
}

async fn extension_unselect(
    State(state): State<AppState>,
    ApiPath(name): ApiPath<String>,
) -> ApiResult<Json<Value>> {
    let result = state.host.unselect(&name).await.map_err(extension_error)?;
    Ok(Json(
        serde_json::to_value(result).map_err(|_| extension_response_error())?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InvokeInput {
    method: String,
    #[serde(default = "inkcre_core::models::empty_object")]
    parameters: Value,
}

async fn extension_invoke(
    State(state): State<AppState>,
    ApiPath(name): ApiPath<String>,
    ApiJson(input): ApiJson<InvokeInput>,
) -> ApiResult<Json<Value>> {
    Ok(Json(
        state
            .host
            .invoke(&name, &input.method, input.parameters)
            .await
            .map_err(extension_error)?,
    ))
}

async fn extension_config(
    State(state): State<AppState>,
    ApiPath(name): ApiPath<String>,
) -> ApiResult<Json<Value>> {
    let shared = state
        .core
        .extension_get(&name)
        .await?
        .ok_or_else(|| ApiError::not_found("extension not found"))?;
    let selected = state
        .core
        .participation_get(&name, state.peer_id)
        .await?
        .ok_or_else(|| ApiError::not_found("extension participation not found"))?;
    Ok(Json(
        json!({"config":shared.config,"schema":selected.config_schema,"distribution":selected.distribution,"version":selected.version}),
    ))
}

async fn extension_patch_config(
    State(state): State<AppState>,
    ApiPath(name): ApiPath<String>,
    ApiJson(patch): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    let updated = state
        .core
        .extension_patch_config_for_peer(&name, state.peer_id, &patch)
        .await?;
    Ok(Json(
        json!({"config":updated.config,"applies_on_next_start":true}),
    ))
}

fn extension_error(error: inkcre_core::extension::Error) -> ApiError {
    // 插件可能把凭据放入错误文本；HTTP 保持固定错误，不能转发原始子进程输出。
    use inkcre_core::extension::Error;
    let (status, code, message) = match error {
        Error::Core(error) => return error.into(),
        Error::NotFound => (
            StatusCode::NOT_FOUND,
            "extension_not_found",
            "extension or its exact local artifact was not found",
        ),
        Error::Invalid(_) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_extension",
            "invalid extension manifest or invocation",
        ),
        Error::Conflict(_) => (
            StatusCode::CONFLICT,
            "extension_conflict",
            "extension selection or runtime state conflicts with this operation",
        ),
        _ => (
            StatusCode::BAD_GATEWAY,
            "extension_operation_failed",
            "extension process or protocol operation failed; inspect its runtime status",
        ),
    };
    ApiError::new(status, code, message)
}

fn extension_response_error() -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "extension_response_invalid",
        "extension returned an invalid response",
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NeighborhoodInput {
    block_id: i32,
    #[serde(default = "default_direction")]
    direction: inkcre_core::Direction,
    #[serde(default)]
    contents: Vec<String>,
    #[serde(default = "default_limit")]
    limit: i64,
    cursor: Option<i32>,
}

fn default_direction() -> inkcre_core::Direction {
    inkcre_core::Direction::Both
}

async fn graph_neighborhood(
    State(state): State<AppState>,
    ApiJson(input): ApiJson<NeighborhoodInput>,
) -> ApiResult<Json<inkcre_core::Neighborhood>> {
    Ok(Json(
        state
            .core
            .block_neighborhood(
                input.block_id,
                input.direction,
                &input.contents,
                input.limit,
                input.cursor,
            )
            .await?
            .ok_or_else(|| ApiError::not_found("block not found"))?,
    ))
}

async fn relation_neighborhood(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
) -> ApiResult<Json<inkcre_core::GraphModel>> {
    Ok(Json(
        state
            .core
            .relation_neighborhood(id)
            .await?
            .ok_or_else(|| ApiError::not_found("relation not found"))?,
    ))
}

#[derive(Deserialize)]
struct PathQuery {
    source: i32,
    target: i32,
    #[serde(default = "default_direction")]
    direction: inkcre_core::Direction,
    #[serde(default)]
    contents: Option<String>,
    #[serde(default = "default_depth")]
    max_hops: u8,
    #[serde(default = "default_edges")]
    max_explored_blocks: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathInput {
    source: i32,
    target: i32,
    #[serde(default = "default_direction")]
    direction: inkcre_core::Direction,
    #[serde(default)]
    contents: Vec<String>,
    #[serde(default = "default_depth")]
    max_hops: u8,
    #[serde(default = "default_edges")]
    max_explored_blocks: u32,
}
fn default_depth() -> u8 {
    4
}
fn default_edges() -> u32 {
    1000
}

async fn graph_path(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<PathQuery>,
) -> ApiResult<Json<inkcre_core::GraphPath>> {
    let contents = query
        .contents
        .map(|value| serde_json::from_str::<Vec<String>>(&value))
        .transpose()
        .map_err(|_| ApiError::invalid("contents query parameter must be a JSON array of strings"))?
        .unwrap_or_default();
    graph_path_response(
        state,
        PathInput {
            source: query.source,
            target: query.target,
            direction: query.direction,
            contents,
            max_hops: query.max_hops,
            max_explored_blocks: query.max_explored_blocks,
        },
    )
    .await
}

async fn graph_path_post(
    State(state): State<AppState>,
    ApiJson(input): ApiJson<PathInput>,
) -> ApiResult<Json<inkcre_core::GraphPath>> {
    graph_path_response(state, input).await
}

async fn graph_path_response(
    state: AppState,
    input: PathInput,
) -> ApiResult<Json<inkcre_core::GraphPath>> {
    Ok(Json(
        state
            .core
            .find_path(
                input.source,
                input.target,
                inkcre_core::PathOptions {
                    direction: input.direction,
                    contents: input.contents,
                    max_hops: input.max_hops,
                    max_explored_blocks: input.max_explored_blocks,
                },
            )
            .await?,
    ))
}

async fn storages(State(state): State<AppState>) -> ApiResult<Json<Vec<inkcre_core::Storage>>> {
    Ok(Json(state.core.storages().await?))
}

type BinaryBody = Result<axum::body::Bytes, axum::extract::rejection::BytesRejection>;
fn binary_body(body: BinaryBody) -> ApiResult<axum::body::Bytes> {
    body.map_err(|error| {
        ApiError::new(
            error.status(),
            "invalid_body",
            "unable to read request bytes",
        )
    })
}

async fn blob_create(
    State(state): State<AppState>,
    body: BinaryBody,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let bytes = binary_body(body)?;
    let id = state.core.blob_create(&bytes).await?;
    Ok((StatusCode::CREATED, Json(json!({"id":id}))))
}

async fn blob_read(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Response> {
    let bytes = state
        .core
        .blob_read(id)
        .await?
        .ok_or_else(|| ApiError::not_found("storage blob not found"))?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

async fn blob_update(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    body: BinaryBody,
) -> ApiResult<StatusCode> {
    let bytes = binary_body(body)?;
    if !state.core.blob_update(id, &bytes).await? {
        return Err(ApiError::not_found("storage blob not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn blob_delete(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<StatusCode> {
    if !state.core.blob_delete(id).await? {
        return Err(ApiError::not_found("storage blob not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn block_content(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
) -> ApiResult<Response> {
    use inkcre_core::HydratedContent;
    Ok(match state.core.block_content(id).await? {
        HydratedContent::Text(text) => {
            ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response()
        }
        HydratedContent::Bytes(bytes) => {
            ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response()
        }
    })
}

#[derive(Deserialize)]
struct TypePageQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    cursor: Option<String>,
}

async fn job_types(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<TypePageQuery>,
) -> ApiResult<Json<Page<inkcre_core::JobType, String>>> {
    Ok(Json(
        state
            .core
            .job_types(query.limit, query.cursor.as_deref())
            .await?,
    ))
}

async fn job_type_get(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<inkcre_core::JobType>> {
    Ok(Json(state.core.job_type_get(&id).await?.ok_or_else(
        || ApiError::not_found("job type not found"),
    )?))
}

async fn job_type_upsert(
    State(state): State<AppState>,
    ApiJson(input): ApiJson<inkcre_core::JobType>,
) -> ApiResult<Json<inkcre_core::JobType>> {
    state.core.job_type_upsert(&input).await?;
    Ok(Json(input))
}

async fn job_type_delete(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<StatusCode> {
    if !state.core.job_type_delete(&id).await? {
        return Err(ApiError::not_found("job type not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct JobPageQuery {
    #[serde(default = "default_limit")]
    limit: i64,
    cursor: Option<i64>,
    #[serde(rename = "type")]
    type_: Option<String>,
    status: Option<inkcre_core::JobStatus>,
}

async fn jobs(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<JobPageQuery>,
) -> ApiResult<Json<Page<inkcre_core::Job, i64>>> {
    Ok(Json(
        state
            .core
            .jobs(
                query.limit,
                query.cursor,
                query.type_.as_deref(),
                query.status,
            )
            .await?,
    ))
}

async fn job_get(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i64>,
) -> ApiResult<Json<inkcre_core::Job>> {
    Ok(Json(
        state
            .core
            .job_get(id)
            .await?
            .ok_or_else(|| ApiError::not_found("job not found"))?,
    ))
}

async fn job_submit(
    State(state): State<AppState>,
    ApiJson(input): ApiJson<inkcre_core::JobInput>,
) -> ApiResult<(StatusCode, Json<inkcre_core::Job>)> {
    Ok((
        StatusCode::CREATED,
        Json(state.core.job_submit(input).await?),
    ))
}

async fn job_delete(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i64>,
) -> ApiResult<StatusCode> {
    if !state.core.job_delete(id).await? {
        return Err(ApiError::not_found("job not found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn job_claim(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i64>,
) -> ApiResult<Json<inkcre_core::Job>> {
    let job = state.core.job_claim(id).await?.ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "job_not_pending",
            "job is not pending or does not exist",
        )
    })?;
    Ok(Json(job))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FinishInput {
    status: inkcre_core::JobStatus,
    #[serde(default = "inkcre_core::models::empty_object")]
    state: Value,
}

async fn job_finish(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i64>,
    ApiJson(input): ApiJson<FinishInput>,
) -> ApiResult<Json<inkcre_core::Job>> {
    if !state
        .core
        .job_finish(id, input.status, &input.state)
        .await?
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "job_not_running",
            "job is not running or does not exist",
        ));
    }
    Ok(Json(
        state
            .core
            .job_get(id)
            .await?
            .ok_or_else(|| ApiError::not_found("job not found"))?,
    ))
}

async fn job_abort(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i64>,
) -> ApiResult<Json<inkcre_core::Job>> {
    Ok(Json(state.core.job_abort(id).await?))
}
