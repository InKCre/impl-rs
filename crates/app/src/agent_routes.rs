use crate::{
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
    routes::AppState,
};
use axum::{
    Json, Router,
    extract::{OriginalUri, State},
    http::{StatusCode, header},
    routing::get,
};
use inkcre_core::agent::{Definition, Form};
use serde::Deserialize;
use serde_json::{Value, json};
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/agents", get(list).post(create))
        .route("/agents/{id}", get(read).patch(patch).delete(delete))
        .route("/agent-tools", get(tools))
        .route("/agent-tools/{id}", get(tool))
}
#[derive(Deserialize)]
struct Page {
    limit: Option<i64>,
    cursor: Option<i64>,
}
async fn list(
    State(state): State<AppState>,
    ApiQuery(page): ApiQuery<Page>,
) -> ApiResult<Json<Value>> {
    let page = state.core.agents(page.limit, page.cursor).await?;
    Ok(Json(
        json!({"agents":page.items,"next_cursor":page.next_cursor}),
    ))
}
async fn read(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<i64>,
) -> ApiResult<Json<Definition>> {
    Ok(Json(
        state
            .core
            .agent_get(id)
            .await?
            .ok_or_else(|| ApiError::not_found("Agent not found"))?,
    ))
}
async fn create(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    ApiJson(form): ApiJson<Form>,
) -> ApiResult<(
    StatusCode,
    [(header::HeaderName, String); 1],
    Json<Definition>,
)> {
    let definition = state.core.agent_create(form).await.map_err(|error| {
        if crate::routes::legacy_uri(&uri) {
            ApiError::from(error)
        } else {
            crate::error::database_write(error)
        }
    })?;
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, format!("/agents/{}", definition.id))],
        Json(definition),
    ))
}
async fn patch(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    ApiPath(id): ApiPath<i64>,
    ApiJson(value): ApiJson<Value>,
) -> ApiResult<Json<Definition>> {
    Ok(Json(state.core.agent_patch(id, value).await.map_err(
        |error| {
            if crate::routes::legacy_uri(&uri) {
                ApiError::from(error)
            } else {
                crate::error::database_write(error)
            }
        },
    )?))
}
async fn delete(State(state): State<AppState>, ApiPath(id): ApiPath<i64>) -> ApiResult<StatusCode> {
    if state.core.agent_delete(id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("Agent not found"))
    }
}
#[derive(Deserialize)]
struct ToolPage {
    limit: Option<usize>,
    cursor: Option<String>,
}
async fn tools(
    State(state): State<AppState>,
    ApiQuery(page): ApiQuery<ToolPage>,
) -> ApiResult<Json<Value>> {
    if page.limit == Some(0) {
        return Err(ApiError::invalid("tool page limit must be positive"));
    }
    let mut tools = state
        .agents
        .tools()
        .into_iter()
        .filter(|tool| page.cursor.as_ref().is_none_or(|cursor| tool.id > *cursor))
        .collect::<Vec<_>>();
    let next = if page.limit.is_some_and(|n| tools.len() > n) {
        tools.truncate(page.limit.unwrap_or_default());
        tools.last().map(|t| t.id.clone())
    } else {
        None
    };
    Ok(Json(
        json!({"tools":tools.into_iter().map(|tool|json!({"id":tool.id,"description":tool.description})).collect::<Vec<_>>(),"next_cursor":next}),
    ))
}
async fn tool(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<inkcre_core::ai::FunctionTool>> {
    Ok(Json(state.agents.tool(&id)?))
}
