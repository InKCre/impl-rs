use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post, put},
};
use inkcre_core::{Extension, extension::ExtensionStatus};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
    routes::{AppState, extension_error},
};

pub const CAPABILITY: &str = "core.extension.management.v2";
pub const INBOUND: &str = "/extension-management/v2";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/extensions", get(list))
        .route("/extensions/{namespace}/{name}", get(read))
        .route(
            "/extensions/{namespace}/{name}/selection",
            post(select).delete(unselect),
        )
        .route("/extensions/{namespace}/{name}/enable", post(enable))
        .route("/extensions/{namespace}/{name}/disable", post(disable))
        .route(
            "/extensions/{namespace}/{name}/config",
            put(replace).patch(patch),
        )
        .route(INBOUND, post(local))
        .route("/extension-management", post(|| async { StatusCode::GONE }))
}

#[derive(Deserialize)]
struct Pagination {
    limit: Option<usize>,
    cursor: Option<String>,
}

async fn list(
    State(state): State<AppState>,
    ApiQuery(page): ApiQuery<Pagination>,
) -> ApiResult<Json<Value>> {
    if page.limit == Some(0) {
        return Err(ApiError::invalid("limit must be positive"));
    }
    let mut extensions = state.core.extensions().await?;
    if let Some(cursor) = page.cursor {
        extensions.retain(|extension| extension.name > cursor);
    }
    let next_cursor = if let Some(limit) = page.limit.filter(|limit| extensions.len() > *limit) {
        extensions.truncate(limit);
        extensions.last().map(|extension| extension.name.clone())
    } else {
        None
    };
    Ok(Json(
        json!({"extensions":extensions,"next_cursor":next_cursor}),
    ))
}

fn coordinate(parts: (String, String)) -> String {
    format!("{}/{}", parts.0, parts.1)
}

async fn read(
    State(state): State<AppState>,
    ApiPath(parts): ApiPath<(String, String)>,
) -> ApiResult<Json<Extension>> {
    Ok(Json(
        state
            .core
            .extension_get(&coordinate(parts))
            .await?
            .ok_or_else(|| ApiError::not_found("extension not found"))?,
    ))
}

async fn replace(
    State(state): State<AppState>,
    ApiPath(parts): ApiPath<(String, String)>,
    ApiJson(config): ApiJson<Value>,
) -> ApiResult<Json<Extension>> {
    Ok(Json(
        state
            .core
            .extension_replace_config_for_peer(&coordinate(parts), state.peer_id, &config)
            .await?,
    ))
}

async fn patch(
    State(state): State<AppState>,
    ApiPath(parts): ApiPath<(String, String)>,
    ApiJson(config): ApiJson<Value>,
) -> ApiResult<Json<Extension>> {
    Ok(Json(
        state
            .core
            .extension_patch_config_for_peer(&coordinate(parts), state.peer_id, &config)
            .await?,
    ))
}

#[derive(Deserialize)]
struct Routing {
    route_to_peer: Option<Uuid>,
}

#[derive(schemars::JsonSchema, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection {
    distribution: String,
    version: String,
}

#[derive(schemars::JsonSchema, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Command {
    Select {
        extension: String,
        distribution: String,
        version: String,
    },
    Status {
        extension: Option<String>,
    },
    Unselect {
        extension: String,
    },
    Enable {
        extension: String,
    },
    Disable {
        extension: String,
    },
    PatchConfig {
        extension: String,
        patch: Value,
    },
}

async fn select(
    State(state): State<AppState>,
    ApiPath(parts): ApiPath<(String, String)>,
    ApiQuery(routing): ApiQuery<Routing>,
    ApiJson(selection): ApiJson<Selection>,
) -> ApiResult<Json<Value>> {
    dispatch(
        &state,
        routing,
        Command::Select {
            extension: coordinate(parts),
            distribution: selection.distribution,
            version: selection.version,
        },
    )
    .await
}

async fn unselect(
    State(state): State<AppState>,
    ApiPath(parts): ApiPath<(String, String)>,
    ApiQuery(routing): ApiQuery<Routing>,
) -> ApiResult<Json<Value>> {
    dispatch(
        &state,
        routing,
        Command::Unselect {
            extension: coordinate(parts),
        },
    )
    .await
}

async fn enable(
    State(state): State<AppState>,
    ApiPath(parts): ApiPath<(String, String)>,
    ApiQuery(routing): ApiQuery<Routing>,
) -> ApiResult<Json<Value>> {
    dispatch(
        &state,
        routing,
        Command::Enable {
            extension: coordinate(parts),
        },
    )
    .await
}

async fn disable(
    State(state): State<AppState>,
    ApiPath(parts): ApiPath<(String, String)>,
    ApiQuery(routing): ApiQuery<Routing>,
) -> ApiResult<Json<Value>> {
    dispatch(
        &state,
        routing,
        Command::Disable {
            extension: coordinate(parts),
        },
    )
    .await
}

async fn local(
    State(state): State<AppState>,
    ApiJson(command): ApiJson<Command>,
) -> ApiResult<Json<Value>> {
    Ok(Json(execute(&state, command).await?))
}

async fn dispatch(state: &AppState, routing: Routing, command: Command) -> ApiResult<Json<Value>> {
    let Some(target) = routing.route_to_peer.filter(|id| *id != state.peer_id) else {
        return Ok(Json(execute(state, command).await?));
    };
    let mut options = inkcre_core::peer::PeerHttpOptions::new(state.secret.to_string());
    options.timeout = state.timeout;
    let response = state
        .core
        .peer_delegate(
            state.peer_id,
            CAPABILITY,
            json!({"body":command}),
            Some(target),
            &options,
        )
        .await
        .map_err(crate::retrieval_routes::delegate_error)?;
    if response["status"] != 200
        || response["body"]["peer_id"] != json!(target)
        || !response["body"]["extensions"].is_array()
        || !response["body"]["supported_distributions"].is_array()
    {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "invalid_peer_response",
            "Peer did not return extension status",
        ));
    }
    Ok(Json(response["body"].clone()))
}

fn row(status: ExtensionStatus) -> Value {
    json!({
        "name":status.name, "peer_id":status.peer_id, "distribution":status.distribution, "version":status.version,
        "enabled":status.enabled, "config_schema":status.config_schema, "supported_distributions":status.supported_distributions,
        "loaded_version":status.loaded_version, "running":status.running, "restart_required":false,
        "operation":null, "error":status.error,
    })
}

async fn execute(state: &AppState, command: Command) -> ApiResult<Value> {
    let statuses = match command {
        Command::Select {
            extension,
            distribution,
            version,
        } => {
            if distribution != "rust_stdio" {
                return Err(ApiError::invalid("this Peer supports rust_stdio selection"));
            }
            let key = (extension.clone(), version.clone());
            let status = match state.catalog.get(&key) {
                Some(path) => state.host.install(path.clone(), &extension, &version).await,
                None => state.host.install_registry(&extension, &version).await,
            }
            .map_err(extension_error)?;
            vec![status]
        }
        Command::Status { extension } => state
            .host
            .list()
            .await
            .map_err(extension_error)?
            .into_iter()
            .filter(|status| extension.as_ref().is_none_or(|name| status.name == *name))
            .collect(),
        Command::Unselect { extension } => vec![
            state
                .host
                .unselect(&extension)
                .await
                .map_err(extension_error)?,
        ],
        Command::Enable { extension } => vec![
            state
                .host
                .enable(&extension)
                .await
                .map_err(extension_error)?,
        ],
        Command::Disable { extension } => vec![
            state
                .host
                .disable(&extension)
                .await
                .map_err(extension_error)?,
        ],
        Command::PatchConfig { extension, patch } => {
            state
                .core
                .extension_patch_config_for_peer(&extension, state.peer_id, &patch)
                .await?;
            state
                .host
                .list()
                .await
                .map_err(extension_error)?
                .into_iter()
                .filter(|status| status.name == extension)
                .collect()
        }
    };
    Ok(
        json!({"peer_id":state.peer_id,"supported_distributions":["rust_stdio"],
        "extensions":statuses.into_iter().map(row).collect::<Vec<_>>() }),
    )
}
