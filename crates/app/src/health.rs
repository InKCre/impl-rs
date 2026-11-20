use std::sync::atomic::Ordering;

use axum::{Json, extract::State, http::StatusCode};
use inkcre_core::CONTRACT_REVISION;
use serde_json::{Value, json};

use crate::{
    error::{ApiError, ApiResult},
    routes::AppState,
};

pub async fn live() -> Json<Value> {
    Json(json!({"status":"ok"}))
}

pub async fn legacy_live() -> Json<Value> {
    Json(json!({"status":"live"}))
}

pub async fn ready(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let admission = state.core.admission().await;
    let lease_valid = state.lease_valid.load(Ordering::Relaxed);
    let (database_status, database, migration) = match admission {
        Ok(admission) => (
            "ok",
            json!({"status":"ok", "environment":admission.environment}),
            json!({"status":"ok", "current":admission.migration_heads}),
        ),
        Err(_) => (
            "error",
            json!({"status":"error", "reason":"unreachable_or_contract_unavailable"}),
            json!({"status":"error", "current":[]}),
        ),
    };
    let (phase, reason) = if lease_valid {
        ("ready", "ready")
    } else {
        ("failed", "peer_lease_unavailable")
    };
    let (status, label) = if database_status == "ok" && lease_valid {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not_ready")
    };
    (
        status,
        Json(json!({
            "status":label,
            "database":{
                "format":1,"status":database_status,"profile":"runtime",
                "contract":{"revision":CONTRACT_REVISION},
                "database":database,"migration":migration
            },
            "runtime":{"phase":phase,"reason":reason,"step":null}
        })),
    )
}

pub async fn legacy_ready(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    if !state.lease_valid.load(Ordering::Relaxed) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "peer_unavailable",
            "Peer lease is not current",
        ));
    }
    state.core.admission().await?;
    Ok(Json(json!({"status":"ready"})))
}
