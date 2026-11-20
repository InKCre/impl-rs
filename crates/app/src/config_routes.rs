use crate::{
    error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult},
    routes::AppState,
};
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use inkcre_core::deployment_config::{ConfigRecord, Error};
use serde::Deserialize;
use serde_json::{Value, json};
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/configs", get(list))
        .route(
            "/configs/{key}",
            get(read).put(replace).patch(patch).delete(delete),
        )
        .route("/config-schemas", get(schemas))
        .route("/config-schemas/{id}", get(schema))
}
#[derive(Deserialize)]
struct Pagination {
    limit: Option<i64>,
    cursor: Option<String>,
}
#[derive(Deserialize)]
struct Replacement {
    #[serde(rename = "schema", alias = "schema_id")]
    schema_id: String,
    value: Value,
}
fn error(error: Error) -> ApiError {
    match error {
        Error::Core(error) => error.into(),
        Error::UnknownSchema => ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_config_schema",
            error.to_string(),
        ),
        Error::NotFound => ApiError::not_found("deployment config not found"),
        Error::Invalid(message) => ApiError::invalid(message),
    }
}
async fn list(
    State(state): State<AppState>,
    ApiQuery(page): ApiQuery<Pagination>,
) -> ApiResult<Json<Value>> {
    let result = state
        .core
        .config_list(page.limit, page.cursor.as_deref())
        .await
        .map_err(error)?;
    Ok(Json(
        json!({"configs":result.items,"next_cursor":result.next_cursor}),
    ))
}
async fn read(
    State(state): State<AppState>,
    ApiPath(key): ApiPath<String>,
) -> ApiResult<Json<ConfigRecord>> {
    Ok(Json(
        state
            .core
            .config_get(&key)
            .await
            .map_err(error)?
            .ok_or_else(|| ApiError::not_found("deployment config not found"))?,
    ))
}
async fn replace(
    State(state): State<AppState>,
    ApiPath(key): ApiPath<String>,
    ApiJson(body): ApiJson<Replacement>,
) -> ApiResult<(StatusCode, Json<ConfigRecord>)> {
    let (record, created) = state
        .core
        .config_replace(&state.configs, &key, &body.schema_id, body.value)
        .await
        .map_err(error)?;
    Ok((
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(record),
    ))
}
async fn patch(
    State(state): State<AppState>,
    ApiPath(key): ApiPath<String>,
    ApiJson(body): ApiJson<Value>,
) -> ApiResult<Json<ConfigRecord>> {
    state
        .core
        .config_patch(&state.configs, &key, body)
        .await
        .map(Json)
        .map_err(|failure| match failure {
            Error::UnknownSchema => ApiError::new(
                StatusCode::CONFLICT,
                "unknown_config_schema",
                failure.to_string(),
            ),
            failure => error(failure),
        })
}
async fn delete(
    State(state): State<AppState>,
    ApiPath(key): ApiPath<String>,
) -> ApiResult<StatusCode> {
    if state.core.config_delete(&key).await.map_err(error)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("deployment config not found"))
    }
}
async fn schemas(
    State(state): State<AppState>,
    ApiQuery(page): ApiQuery<Pagination>,
) -> ApiResult<Json<Value>> {
    let result = state
        .configs
        .schemas(page.limit, page.cursor.as_deref())
        .map_err(error)?;
    Ok(Json(
        json!({"schemas":result.items,"next_cursor":result.next_cursor}),
    ))
}
async fn schema(
    State(state): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<inkcre_core::deployment_config::SchemaDescriptor>> {
    state
        .configs
        .schema(&id)
        .map(Json)
        .map_err(|_| ApiError::not_found("config schema not found"))
}
