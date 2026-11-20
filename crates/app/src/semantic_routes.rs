use crate::{
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
    routes::AppState,
};
use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};
use inkcre_core::semantic::{Profile, Request, Retrieval};
use serde::Deserialize;
use serde_json::{Value, json};
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/semantic-retrieval", post(retrieve))
        .route("/embedding-profiles", get(profiles))
        .route("/embedding-profiles/{id}", get(profile))
}
async fn retrieve(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<Request>,
) -> ApiResult<Json<Retrieval>> {
    Ok(Json(state.core.semantic_retrieve(request).await?))
}
#[derive(Deserialize)]
struct ProfileQuery {
    limit: Option<i64>,
    cursor: Option<i64>,
}
async fn profiles(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<ProfileQuery>,
) -> ApiResult<Json<Value>> {
    let page = state
        .core
        .semantic_profiles(query.limit.unwrap_or(i64::MAX - 1), query.cursor)
        .await?;
    Ok(Json(
        json!({"profiles":page.items,"next_cursor":page.next_cursor}),
    ))
}
async fn profile(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i64>,
) -> ApiResult<Json<Profile>> {
    Ok(Json(state.core.semantic_profile(id).await?.ok_or_else(
        || ApiError::not_found("embedding profile not found"),
    )?))
}
