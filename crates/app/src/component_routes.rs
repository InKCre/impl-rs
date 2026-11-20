use crate::{
    error::{ApiJson, ApiResult},
    routes::AppState,
};
use axum::{Json, Router, extract::State, routing::post};
use inkcre_core::graph_components::ComponentsRequest;
pub fn router() -> Router<AppState> {
    Router::new().route("/graph/components", post(components))
}
async fn components(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<ComponentsRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    let result = state.core.connected_components(request).await?;
    Ok(Json(
        serde_json::json!({"components":result.components,"proof_graph":{"blocks":result.proof_graph.blocks,"relations":crate::routes::canonical_relations(result.proof_graph.relations)},"missing_seed_block_ids":result.missing_seed_block_ids,"truncated":result.truncated}),
    ))
}
