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
    extract::{DefaultBodyLimit, MatchedPath, OriginalUri, Request, State},
    http::{HeaderMap, StatusCode, header},
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
    error::{ApiBytes, ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
};

#[derive(Clone)]
pub struct AppState {
    pub extension_http: Arc<tokio::sync::Semaphore>,
    pub core: Core,
    pub observations: Arc<inkcre_core::observability::Runtime>,
    pub telemetry: Arc<inkcre_core::observability::Relay>,
    pub source_registry: inkcre_core::source::SourceRegistry,
    pub agents: inkcre_core::agent::Runtime,
    pub configs: inkcre_core::deployment_config::ConfigRegistry,
    pub host: ExtensionHost,
    pub peer_id: Uuid,
    pub secret: Arc<str>,
    pub lease_valid: Arc<AtomicBool>,
    pub catalog: Arc<BTreeMap<(String, String), PathBuf>>,
    pub timeout: Duration,
    pub worker: Option<inkcre_core::worker::WorkerNotifier>,
}

pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/self", get(self_info))
        .route("/blocks", get(blocks).post(block_create))
        .route("/blocks/recent", get(recently_created_blocks))
        .route(
            "/blocks/{id}",
            get(block_get).patch(block_patch).delete(block_delete),
        )
        .route("/blocks/{id}/relations", get(block_relations))
        .route("/blocks/{id}/neighborhood", get(block_neighborhood))
        .route("/blocks/{id}/content", get(block_content))
        .route("/relations", post(relation_create))
        .route("/relations/by_block/{id}", get(relations_by_block))
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
        .merge(crate::agent_routes::router())
        .merge(crate::ai_routes::router())
        .merge(crate::observation_routes::router())
        .merge(crate::telemetry_routes::router())
        .merge(crate::component_routes::router())
        .merge(crate::config_routes::router())
        .merge(crate::entity_routes::router())
        .merge(crate::knowledge_routes::router())
        .merge(crate::lexical_routes::router())
        .merge(crate::organization_routes::router())
        .merge(crate::peer_routes::router())
        .merge(crate::retrieval_routes::router())
        .merge(crate::semantic_routes::router())
        .merge(crate::sink_routes::router())
        .merge(crate::source_routes::router())
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    let mcp = crate::mcp::router();
    let canonical = crate::extension_v2_routes::router()
        .merge(crate::knowledge_routes::discovery(false))
        .merge(crate::source_routes::commands(false))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    let legacy = Router::new()
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
        .merge(crate::source_routes::commands(true))
        .merge(crate::knowledge_routes::discovery(true))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    Router::new()
        .route("/openapi.json", get(crate::openapi::document))
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .route("/livez", get(live))
        .route("/readyz", get(ready))
        .merge(protected.clone())
        .merge(mcp.clone())
        .merge(canonical)
        .nest("/api/v1", protected.merge(mcp).merge(legacy))
        .fallback(crate::extension_ingress::route)
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
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &Request| {
                let route = request
                    .extensions()
                    .get::<MatchedPath>()
                    .map_or("unmatched", MatchedPath::as_str);
                tracing::debug_span!("http.request", method = %request.method(), route)
            }),
        )
        .with_state(state)
}

async fn live() -> Json<Value> {
    Json(json!({"status":"live"}))
}

async fn authenticate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if let Some(error) = peer_authentication_error(request.headers(), &state.secret) {
        return error;
    }
    next.run(request).await
}

pub(crate) fn peer_authentication_error(headers: &HeaderMap, secret: &str) -> Option<Response> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if token.is_some_and(|token| auth::verify(token, secret)) {
        return None;
    }
    Some(
        (
            [(header::WWW_AUTHENTICATE, "Bearer")],
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "valid Peer bearer token required",
            ),
        )
            .into_response(),
    )
}

async fn request_timeout(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if request
        .extensions()
        .get::<MatchedPath>()
        .is_some_and(|path| {
            matches!(
                path.as_str(),
                "/telemetry/v1/{signal}" | "/api/v1/telemetry/v1/{signal}"
            )
        })
    {
        // OTLP relay already applies the configured signal deadline to body and upstream I/O.
        return next.run(request).await;
    }
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

/// Root REST resources use their named Python contract; the versioned API keeps SDK pages.
pub(crate) fn legacy_uri(uri: &axum::http::Uri) -> bool {
    uri.path() == "/api/v1" || uri.path().starts_with("/api/v1/")
}

/// Read SDK-sized batches without imposing the SDK's page-size ceiling on REST.
/// An omitted canonical limit means all rows, matching the authoritative API.
pub(crate) async fn resource_page<T, C, F, Fut>(
    uri: &axum::http::Uri,
    name: &str,
    requested: Option<i64>,
    mut cursor: Option<C>,
    mut fetch: F,
) -> ApiResult<Json<Value>>
where
    T: Serialize,
    C: Clone + Serialize,
    F: FnMut(i64, Option<C>) -> Fut,
    Fut: std::future::Future<Output = inkcre_core::Result<Page<T, C>>>,
{
    if legacy_uri(uri) {
        return Ok(Json(json!(fetch(requested.unwrap_or(50), cursor).await?)));
    }
    if requested.is_some_and(|value| value <= 0) {
        return Err(ApiError::invalid("limit must be positive"));
    }
    let mut remaining = requested;
    let mut items = Vec::new();
    loop {
        let count = remaining.unwrap_or(100).min(100);
        let page = fetch(count, cursor.clone()).await?;
        if let Some(left) = &mut remaining {
            *left -= page.items.len() as i64;
        }
        items.extend(page.items);
        cursor = page.next_cursor;
        if cursor.is_none() || remaining == Some(0) {
            break;
        }
    }
    Ok(Json(json!({name: items, "next_cursor": cursor})))
}

async fn blocks(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<PageQuery>,
) -> ApiResult<Json<Page<Block, i32>>> {
    Ok(Json(
        state.core.blocks_recent(query.limit, query.cursor).await?,
    ))
}

#[derive(Deserialize)]
struct RecentBlocksQuery {
    num: Option<i64>,
    resolver: Option<String>,
}

async fn recently_created_blocks(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<RecentBlocksQuery>,
) -> ApiResult<Json<Vec<Block>>> {
    Ok(Json(
        state
            .core
            .recently_created_blocks(query.num.unwrap_or(10), query.resolver.as_deref())
            .await?,
    ))
}

async fn block_create(
    State(state): State<AppState>,
    ApiJson(input): ApiJson<BlockInput>,
) -> ApiResult<(StatusCode, Json<Block>)> {
    Ok((
        StatusCode::CREATED,
        Json(state.core.knowledge().create_block(input).await?),
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
    Ok(Json(state.core.knowledge().patch_block(id, input).await?))
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

async fn relations_by_block(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
) -> ApiResult<Json<Vec<Value>>> {
    Ok(Json(canonical_relations(
        state.core.incident_relations(id).await?,
    )))
}

fn relation_response(uri: &axum::http::Uri, relation: Relation) -> Value {
    let value = json!(relation);
    if legacy_uri(uri) {
        value
    } else {
        crate::entity_routes::relation_record(value)
    }
}

pub(crate) fn canonical_relations(relations: Vec<Relation>) -> Vec<Value> {
    relations
        .into_iter()
        .map(|relation| crate::entity_routes::relation_record(json!(relation)))
        .collect()
}

/// REST names are explicit endpoint IDs; legacy field names remain accepted aliases.
#[derive(schemars::JsonSchema, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RelationRequest {
    #[serde(alias = "from_")]
    from_block_id: i32,
    #[serde(alias = "to_")]
    to_block_id: i32,
    content: String,
}
impl From<RelationRequest> for RelationInput {
    fn from(input: RelationRequest) -> Self {
        Self {
            from_: input.from_block_id,
            to_: input.to_block_id,
            content: input.content,
        }
    }
}

async fn relation_create(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    ApiJson(input): ApiJson<RelationRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    Ok((
        StatusCode::CREATED,
        Json(relation_response(
            &uri,
            state.core.relation_create(input.into()).await?,
        )),
    ))
}

async fn relation_get(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    ApiPath(id): ApiPath<i32>,
) -> ApiResult<Json<Value>> {
    Ok(Json(relation_response(
        &uri,
        state
            .core
            .relation_get(id)
            .await?
            .ok_or_else(|| ApiError::not_found("relation not found"))?,
    )))
}

async fn relation_patch(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    ApiPath(id): ApiPath<i32>,
    ApiJson(input): ApiJson<RelationPatch>,
) -> ApiResult<Json<Value>> {
    Ok(Json(relation_response(
        &uri,
        state.core.relation_patch(id, input).await?,
    )))
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
    limit: Option<i64>,
    cursor: Option<Uuid>,
}

async fn peers(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    ApiQuery(query): ApiQuery<PeerPageQuery>,
) -> ApiResult<Json<Value>> {
    resource_page(&uri, "peers", query.limit, query.cursor, |limit, cursor| {
        state.core.peer_list(limit, cursor)
    })
    .await
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

pub(crate) fn extension_error(error: inkcre_core::extension::Error) -> ApiError {
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

async fn block_neighborhood(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i32>,
    ApiQuery(query): ApiQuery<Vec<(String, String)>>,
) -> ApiResult<Json<Value>> {
    let mut direction = inkcre_core::Direction::Both;
    let mut limit = 20;
    let mut cursor = None;
    let mut contents = Vec::new();
    let invalid = || ApiError::invalid("invalid graph neighborhood query");
    for (key, value) in query {
        match key.as_str() {
            "direction" => {
                direction = serde_json::from_value(Value::String(value)).map_err(|_| invalid())?
            }
            "limit" => limit = value.parse().map_err(|_| invalid())?,
            "cursor" => cursor = Some(value.parse().map_err(|_| invalid())?),
            "contents" => contents.push(value),
            _ => (),
        }
    }
    if !(1..=100).contains(&limit) {
        return Err(invalid());
    }
    let page = state
        .core
        .block_neighborhood(id, direction, &contents, limit, cursor)
        .await?
        .ok_or_else(|| ApiError::not_found("block not found"))?;
    Ok(Json(
        json!({"focal_block":id,"next_cursor":page.next_cursor,"graph":{
            "blocks":page.blocks,"relations":canonical_relations(page.relations)
        }}),
    ))
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
    OriginalUri(uri): OriginalUri,
    ApiPath(id): ApiPath<i32>,
) -> ApiResult<Json<Value>> {
    let graph = state
        .core
        .relation_neighborhood(id)
        .await?
        .ok_or_else(|| ApiError::not_found("relation not found"))?;
    Ok(Json(if legacy_uri(&uri) {
        json!(graph)
    } else {
        json!({"focal_relation":id,"graph":{"blocks":graph.blocks,"relations":canonical_relations(graph.relations)}})
    }))
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

#[derive(schemars::JsonSchema, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PathInput {
    #[serde(alias = "from_block_id")]
    #[schemars(rename = "from_block_id")]
    source: i32,
    #[serde(alias = "to_block_id")]
    #[schemars(rename = "to_block_id")]
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
    OriginalUri(uri): OriginalUri,
    ApiJson(input): ApiJson<PathInput>,
) -> ApiResult<Json<Value>> {
    let Json(result) = graph_path_response(state, input).await?;
    if legacy_uri(&uri) {
        return Ok(Json(json!(result)));
    }
    Ok(Json(match result.outcome {
        inkcre_core::PathOutcome::Found => json!({"status":"found", "graph":{
            "blocks":result.graph.blocks,"relations":canonical_relations(result.graph.relations)},
            "block_path":result.block_ids,"relation_path":result.relation_ids}),
        inkcre_core::PathOutcome::NotFound => json!({"status":"not_found"}),
        inkcre_core::PathOutcome::LimitReached => json!({"status":"limit_reached"}),
    }))
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

async fn blob_create(
    State(state): State<AppState>,
    ApiBytes(bytes): ApiBytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
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
    ApiBytes(bytes): ApiBytes,
) -> ApiResult<StatusCode> {
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
    limit: Option<i64>,
    cursor: Option<String>,
}

async fn job_types(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    ApiQuery(query): ApiQuery<TypePageQuery>,
) -> ApiResult<Json<Value>> {
    resource_page(
        &uri,
        "job_types",
        query.limit,
        query.cursor,
        |limit, cursor| {
            let core = state.core.clone();
            async move { core.job_types(limit, cursor.as_deref()).await }
        },
    )
    .await
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
    limit: Option<i64>,
    cursor: Option<i64>,
    #[serde(rename = "type")]
    type_: Option<String>,
    status: Option<inkcre_core::JobStatus>,
}

async fn jobs(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    ApiQuery(query): ApiQuery<JobPageQuery>,
) -> ApiResult<Json<Value>> {
    let limit = query.limit.or_else(|| (!legacy_uri(&uri)).then_some(20));
    resource_page(&uri, "jobs", limit, query.cursor, |limit, cursor| {
        state
            .core
            .jobs(limit, cursor, query.type_.as_deref(), query.status)
    })
    .await
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
    let job = state.core.job_submit(input).await?;
    if let Some(worker) = &state.worker {
        worker.notify_job(job.id);
    }
    Ok((StatusCode::CREATED, Json(job)))
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
