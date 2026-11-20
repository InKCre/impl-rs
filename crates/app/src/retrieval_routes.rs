use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use inkcre_core::{
    lexical::{self, LexicalRequest, LexicalResult},
    peer::{DelegationError, HttpCapability, PeerHttpOptions},
    semantic,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use uuid::Uuid;

use crate::{
    error::{ApiError, ApiJson, ApiQuery, ApiResult},
    routes::AppState,
};

pub fn inbounds() -> Vec<HttpCapability> {
    [
        (lexical::CAPABILITY, "/lexical-retrieval"),
        (semantic::CAPABILITY, "/semantic-retrieval"),
    ]
    .into_iter()
    .map(|(id, path)| HttpCapability {
        id: id.into(),
        method: "POST".into(),
        path: path.into(),
    })
    .collect()
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/retrieval/lexical", post(retrieve_lexical))
        .route("/retrieval/semantic", post(retrieve_semantic))
}

#[derive(Deserialize)]
struct Routing {
    route_to_peer: Option<Uuid>,
}

async fn retrieve_lexical(
    State(state): State<AppState>,
    ApiQuery(routing): ApiQuery<Routing>,
    ApiJson(request): ApiJson<LexicalRequest>,
) -> ApiResult<Json<LexicalResult>> {
    request.validate()?;
    let result = match routing
        .route_to_peer
        .filter(|target| *target != state.peer_id)
    {
        None => state.core.lexical_retrieve(request).await?,
        Some(target) => delegate(&state, lexical::CAPABILITY, request, target).await?,
    };
    Ok(Json(result))
}

async fn retrieve_semantic(
    State(state): State<AppState>,
    ApiQuery(routing): ApiQuery<Routing>,
    ApiJson(request): ApiJson<semantic::Request>,
) -> ApiResult<Json<semantic::Retrieval>> {
    request.validate()?;
    let profile = request.profile;
    let mut result: semantic::Retrieval = match routing
        .route_to_peer
        .filter(|target| *target != state.peer_id)
    {
        None => state
            .core
            .semantic_retrieve(request)
            .await
            .map_err(|error| match error {
                inkcre_core::Error::Unavailable("semantic retrieval default profile") => {
                    ApiError::new(
                        StatusCode::CONFLICT,
                        "profile_not_configured",
                        error.to_string(),
                    )
                }
                error => error.into(),
            })?,
        Some(target) => delegate(&state, semantic::CAPABILITY, request, target).await?,
    };
    if result.metric != "cosine" || profile.is_some_and(|profile| result.profile != profile) {
        return Err(invalid_response());
    }
    for item in &mut result.matches {
        if !(-1.0..=1.0).contains(&item.score)
            || item
                .entity
                .get("id")
                .and_then(serde_json::Value::as_i64)
                .is_none_or(|id| id <= 0)
        {
            return Err(invalid_response());
        }
        match item.kind.as_str() {
            "block" => {}
            "relation" => {
                item.entity =
                    crate::entity_routes::relation_record(std::mem::take(&mut item.entity));
            }
            _ => return Err(invalid_response()),
        }
    }
    Ok(Json(result))
}

async fn delegate<T: DeserializeOwned>(
    state: &AppState,
    capability: &str,
    request: impl Serialize,
    target: Uuid,
) -> ApiResult<T> {
    let body = serde_json::to_value(request)
        .map_err(|_| ApiError::invalid("invalid retrieval request"))?;
    let mut options = PeerHttpOptions::new(state.secret.to_string());
    options.timeout = state.timeout;
    let response = state
        .core
        .peer_delegate(
            state.peer_id,
            capability,
            json!({"body":body}),
            Some(target),
            &options,
        )
        .await
        .map_err(delegate_error)?;
    let body = response
        .get("body")
        .filter(|_| response["status"] == 200)
        .cloned()
        .ok_or_else(invalid_response)?;
    serde_json::from_value(body).map_err(|_| invalid_response())
}

fn invalid_response() -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "peer_response_invalid",
        "Peer did not return a valid retrieval result",
    )
}

pub(crate) fn delegate_error(error: DelegationError) -> ApiError {
    match error {
        DelegationError::Unavailable => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "peer_unavailable",
            error.to_string(),
        ),
        DelegationError::OutcomeUnknown => ApiError::new(
            StatusCode::BAD_GATEWAY,
            "peer_outcome_unknown",
            error.to_string(),
        ),
        DelegationError::Protocol => invalid_response(),
        DelegationError::Core(error) => error.into(),
    }
}
