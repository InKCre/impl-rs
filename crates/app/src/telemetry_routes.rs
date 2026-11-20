use crate::{error::ApiError, routes::AppState};
use axum::{
    Router,
    extract::{Path, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use inkcre_core::observability::{LocalOptions, OtlpConnection, RelayError};
use std::collections::BTreeMap;
pub fn router() -> Router<AppState> {
    Router::new().route("/telemetry/v1/{signal}", post(relay))
}
async fn relay(
    State(state): State<AppState>,
    Path(signal): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    if !matches!(signal.as_str(), "traces" | "logs" | "metrics") {
        return Err(ApiError::invalid("unsupported telemetry signal"));
    }
    let Some(budget) = state.telemetry.timeout(&signal) else {
        return Err(error(RelayError::Disabled));
    };
    let _permit = state
        .telemetry
        .requests
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "telemetry_capacity",
                "Telemetry forwarding capacity reached",
            )
        })?;
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim();
    if content_type != "application/x-protobuf"
        || request
            .headers()
            .get(header::CONTENT_ENCODING)
            .is_some_and(|v| v != "identity")
    {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "telemetry_media_type",
            "Uncompressed OTLP protobuf is required",
        ));
    }
    let operation = async {
        let body = axum::body::to_bytes(request.into_body(), 256 * 1024)
            .await
            .map_err(|_| {
                ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "telemetry_payload_limit",
                    "Telemetry request exceeds 256 KiB",
                )
            })?;
        let result = state
            .telemetry
            .forward(&signal, &body)
            .await
            .map_err(error)?;
        Ok(([(header::CONTENT_TYPE, "application/x-protobuf")], result).into_response())
    };
    tokio::time::timeout(budget + std::time::Duration::from_secs(2), operation)
        .await
        .map_err(|_| error(RelayError::Upstream))?
}
fn error(error: RelayError) -> ApiError {
    let status = match error {
        RelayError::Disabled => StatusCode::SERVICE_UNAVAILABLE,
        RelayError::InvalidRequest => StatusCode::BAD_REQUEST,
        RelayError::Upstream | RelayError::InvalidResponse => StatusCode::BAD_GATEWAY,
    };
    ApiError::new(status, "telemetry_unavailable", error.to_string())
}
/// Application-only environment adapter. Core SDK users pass LocalOptions directly.
pub fn local_options() -> LocalOptions {
    let mut options = LocalOptions {
        telemetry_enabled: boolean("OBSRV__TELEMETRY_ENABLED"),
        allow_external_content: boolean("OBSRV__EXTERNAL_CONTENT_ENABLED"),
        loki_url: std::env::var("OBSRV__LOKI_URL")
            .ok()
            .filter(|value| !value.is_empty()),
        ..Default::default()
    };
    if let Ok(value) = std::env::var("OBSRV__LOKI_HEADERS") {
        match serde_json::from_str(&value) {
            Ok(headers) => options.loki_headers = headers,
            Err(_) => options.loki_url = None,
        }
    }
    let common_headers = std::env::var("OTEL_EXPORTER_OTLP_HEADERS").unwrap_or_default();
    let common_timeout =
        std::env::var("OTEL_EXPORTER_OTLP_TIMEOUT").unwrap_or_else(|_| "10".into());
    for signal in ["TRACES", "LOGS", "METRICS"] {
        let endpoint =
            std::env::var(format!("OTEL_EXPORTER_OTLP_{signal}_ENDPOINT")).unwrap_or_default();
        let timeout = std::env::var(format!("OTEL_EXPORTER_OTLP_{signal}_TIMEOUT"))
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| common_timeout.clone())
            .parse()
            .unwrap_or(f64::NAN);
        let raw = std::env::var(format!("OTEL_EXPORTER_OTLP_{signal}_HEADERS"))
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| common_headers.clone());
        let (headers, timeout) = match parse_headers(&raw) {
            Some(headers) => (headers, timeout),
            None => (BTreeMap::new(), f64::NAN),
        };
        options.otlp.insert(
            signal.to_ascii_lowercase(),
            OtlpConnection {
                endpoint,
                headers,
                timeout_seconds: timeout,
            },
        );
    }
    options
}
fn boolean(key: &str) -> bool {
    std::env::var(key).is_ok_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}
fn parse_headers(value: &str) -> Option<BTreeMap<String, String>> {
    let mut output = BTreeMap::new();
    for entry in value.split(',').filter(|v| !v.trim().is_empty()) {
        let (key, value) = entry.trim().split_once('=')?;
        let key = key.trim().to_ascii_lowercase();
        let decoded = url_decode(value.trim())?;
        if key.is_empty() || output.insert(key, decoded).is_some() {
            return None;
        }
    }
    Some(output)
}
fn url_decode(value: &str) -> Option<String> {
    let mut bytes = Vec::new();
    let mut input = value.bytes();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let a = (input.next()? as char).to_digit(16)?;
            let b = (input.next()? as char).to_digit(16)?;
            bytes.push((a * 16 + b) as u8);
        } else {
            bytes.push(byte)
        }
    }
    String::from_utf8(bytes).ok()
}
