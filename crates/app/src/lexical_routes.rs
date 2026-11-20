use crate::{
    error::{ApiJson, ApiResult},
    routes::AppState,
};
use axum::{Json, Router, extract::State, routing::post};
use inkcre_core::lexical::{LexicalRequest, LexicalResult};
pub fn router() -> Router<AppState> {
    Router::new().route("/lexical-retrieval", post(retrieve))
}
async fn retrieve(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<LexicalRequest>,
) -> ApiResult<Json<LexicalResult>> {
    Ok(Json(state.core.lexical_retrieve(request).await?))
}
