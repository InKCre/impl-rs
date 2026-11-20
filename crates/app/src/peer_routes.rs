use crate::{
    error::{ApiError, ApiResult},
    routes::AppState,
};
use axum::{Json, Router, extract::State, routing::get};

/// Canonical Python-compatible self observation path.
pub fn router() -> Router<AppState> {
    Router::new().route("/peers/self", get(self_peer))
}
async fn self_peer(State(state): State<AppState>) -> ApiResult<Json<inkcre_core::Peer>> {
    Ok(Json(
        state
            .core
            .peer_get(state.peer_id)
            .await?
            .ok_or_else(|| ApiError::not_found("peer not found"))?,
    ))
}
