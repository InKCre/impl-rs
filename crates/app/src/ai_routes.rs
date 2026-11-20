use axum::{Json, Router, extract::State, routing::get};
use inkcre_core::ai::{Error, Model};
use serde::{Deserialize, Serialize};

use crate::{
    error::{ApiError, ApiPath, ApiQuery, ApiResult},
    routes::AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/ai/models", get(models))
        .route("/ai/models/{id}", get(model))
}

#[derive(Deserialize)]
struct ModelQuery {
    limit: Option<i64>,
    cursor: Option<i64>,
}

#[derive(Serialize)]
struct ModelPage {
    models: Vec<Model>,
    next_cursor: Option<i64>,
}

async fn models(
    State(state): State<AppState>,
    ApiQuery(query): ApiQuery<ModelQuery>,
) -> ApiResult<Json<ModelPage>> {
    let result = state
        .core
        .ai_models(query.limit, query.cursor)
        .await
        .map_err(model_error)?;
    Ok(Json(ModelPage {
        models: result.items,
        next_cursor: result.next_cursor,
    }))
}

async fn model(State(state): State<AppState>, ApiPath(id): ApiPath<i64>) -> ApiResult<Json<Model>> {
    let result = state.core.ai_model(id).await.map_err(model_error)?;
    Ok(Json(result.ok_or_else(|| {
        ApiError::not_found("AI model not found")
    })?))
}

fn model_error(error: Error) -> ApiError {
    match error {
        Error::Core(error) => error.into(),
        Error::Invalid(message) => ApiError::invalid(message),
        _ => ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "capability_unavailable",
            "AI model discovery unavailable",
        ),
    }
}
