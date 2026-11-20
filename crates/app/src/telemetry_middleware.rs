//! HTTP instrumentation uses route templates and status, never request paths or bodies.
use crate::routes::AppState;
use axum::{
    extract::{MatchedPath, Request, State},
    middleware::Next,
    response::Response,
};
use inkcre_core::observability::{Outcome, TraceCarrier};
use serde_json::json;
pub async fn observe(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if state
        .observations
        .exporter()
        .is_none_or(|exporter| !exporter.enabled())
    {
        return next.run(request).await;
    }
    let carrier = TraceCarrier {
        traceparent: request
            .headers()
            .get("traceparent")
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.len() <= 512)
            .map(str::to_owned),
        tracestate: request
            .headers()
            .get("tracestate")
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.len() <= 512)
            .map(str::to_owned),
    };
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("unmatched", MatchedPath::as_str);
    let method = match request.method().as_str() {
        "GET" => "GET",
        "POST" => "POST",
        "PUT" => "PUT",
        "PATCH" => "PATCH",
        "DELETE" => "DELETE",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        "CONNECT" => "CONNECT",
        "TRACE" => "TRACE",
        _ => "_OTHER",
    };
    let attrs = json!({"http.request.method":method,"http.route":route})
        .as_object()
        .expect("metadata object")
        .clone();
    let operation = state.core.server_operation("http.server", attrs, carrier);
    let observation = operation.clone();
    operation
        .scope(async move {
            let response = next.run(request).await;
            observation.set_attribute(
                "http.response.status_code",
                json!(response.status().as_u16()),
            );
            if response.status().is_server_error() {
                observation.set_outcome(Outcome::Error)
            }
            response
        })
        .await
}
