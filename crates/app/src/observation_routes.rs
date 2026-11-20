use crate::{
    error::{ApiError, ApiResult},
    routes::AppState,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    routing::post,
};
use inkcre_core::observability::{Error, Request as ObservationRequest};
use serde_json::{Value, json};
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/observability", post(observe))
        .layer(DefaultBodyLimit::max(256 * 1024))
}
fn invalid() -> ApiError {
    ApiError::invalid("Invalid observability request")
}
fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "observability_unavailable",
        "Selected observability backend is unavailable",
    )
}
async fn observe(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> ApiResult<Json<Value>> {
    let runtime = &state.observations;
    let _permit = runtime.requests.clone().try_acquire_owned().map_err(|_| {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "observability_capacity",
            "Observability request capacity reached",
        )
    })?;
    let operation = async {
        let bytes = axum::body::to_bytes(request.into_body(), 256 * 1024)
            .await
            .map_err(|_| {
                ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "observability_payload_limit",
                    "Observability request exceeds 256 KiB",
                )
            })?;
        let body: ObservationRequest = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        body.validate().map_err(|_| invalid())?;
        let status = runtime.status();
        if body
            .expected_backend
            .is_some_and(|backend| backend != status.backend)
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "observability_backend_changed",
                "Observability backend selection changed",
            ));
        }
        match body.action.as_str() {
            "status" => serde_json::to_value(status)
                .map(Json)
                .map_err(|_| unavailable()),
            "query" => {
                let query = body.query.ok_or_else(invalid)?;
                let page = runtime.query(query).await.map_err(|error| match error {
                    Error::Invalid => invalid(),
                    Error::Unavailable => unavailable(),
                })?;
                serde_json::to_value(page)
                    .map(Json)
                    .map_err(|_| unavailable())
            }
            "ingest" => {
                if !status.recording {
                    return Err(unavailable());
                }
                let records = body.records.ok_or_else(invalid)?;
                let count = records.len();
                let accepted = records
                    .into_iter()
                    .filter(|record| runtime.emit(record.clone()))
                    .count();
                Ok(Json(json!({"accepted":accepted,"dropped":count-accepted})))
            }
            _ => Err(invalid()),
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(15), operation)
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::REQUEST_TIMEOUT,
                "observability_timeout",
                "Observability request timed out",
            )
        })?
}
