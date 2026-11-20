use crate::{
    error::{ApiJson, ApiResult},
    routes::AppState,
};
use axum::{Router, extract::State, http::StatusCode, routing::post};
use serde::Deserialize;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    block: i32,
}
pub fn router() -> Router<AppState> {
    Router::new().route("/organization/ruminate", post(ruminate))
}
async fn ruminate(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<Request>,
) -> ApiResult<StatusCode> {
    state
        .core
        .organization_ruminate(request.block, &state.agents)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
