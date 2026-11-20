//! Private, opt-in OTLP/HTTP protobuf forwarding. Callers cannot choose destinations.
use super::*;
use opentelemetry_proto::tonic::collector::{
    logs::v1::{ExportLogsServiceRequest, ExportLogsServiceResponse},
    metrics::v1::{ExportMetricsServiceRequest, ExportMetricsServiceResponse},
    trace::v1::{ExportTraceServiceRequest, ExportTraceServiceResponse},
};
use prost::Message;
use std::collections::BTreeMap;
#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("telemetry destination is not enabled")]
    Disabled,
    #[error("invalid OTLP request")]
    InvalidRequest,
    #[error("telemetry forwarding did not complete")]
    Upstream,
    #[error("invalid telemetry destination response")]
    InvalidResponse,
}
#[derive(Clone)]
struct Connection {
    endpoint: String,
    client: reqwest::Client,
    timeout: Duration,
}
pub struct Relay {
    connections: SyncMutex<BTreeMap<String, Connection>>,
    stop: watch::Sender<bool>,
    pub requests: Arc<Semaphore>,
    closed: AtomicBool,
}
impl Relay {
    pub async fn start(core: &Core, options: &LocalOptions) -> Arc<Self> {
        let mut connections = BTreeMap::new();
        if options.telemetry_enabled {
            let deployment = timeout(Duration::from_secs(5), async {
                let row: Option<(String, Value)> = sqlx::query_as(
                    "SELECT schema,value FROM inkcre.configs WHERE key='inkcre.observability'",
                )
                .fetch_optional(core.pool())
                .await
                .map_err(|_| ())?;
                let Some((schema, value)) = row else {
                    return Ok(None);
                };
                if schema != "inkcre.observability.v1" {
                    return Err(());
                }
                let deployment: Deployment = serde_json::from_value(value).map_err(|_| ())?;
                deployment.validate().map_err(|_| ())?;
                Ok::<_, ()>(Some(deployment))
            })
            .await;
            // A deployment identity is required for telemetry, independently of the
            // selected observation recording backend's availability.
            if let Ok(Ok(Some(deployment))) = deployment {
                for signal in ["traces", "logs", "metrics"] {
                    let configured = options.otlp.get(signal);
                    let endpoint = configured
                        .map(|v| v.endpoint.as_str())
                        .filter(|v| !v.is_empty())
                        .or_else(|| deployment.endpoint(signal));
                    let Some(endpoint) = endpoint else { continue };
                    let timeout = configured.map_or(10.0, |v| v.timeout_seconds);
                    if !timeout.is_finite()
                        || timeout <= 0.0
                        || timeout > 30.0
                        || endpoint.chars().any(char::is_whitespace)
                        || super::loki::validate_url(endpoint).is_err()
                    {
                        continue;
                    }
                    let Ok(url) = url::Url::parse(endpoint) else {
                        continue;
                    };
                    if url.path().is_empty()
                        || !endpoint
                            .split_once("://")
                            .is_some_and(|(_, rest)| rest.contains('/'))
                    {
                        continue;
                    }
                    let headers = configured.map(|v| v.headers.clone()).unwrap_or_default();
                    let Ok(mut headers) = super::loki::headers(&headers) else {
                        continue;
                    };
                    headers.remove(reqwest::header::CONTENT_TYPE);
                    headers.remove(reqwest::header::ACCEPT);
                    headers.insert(
                        reqwest::header::CONTENT_TYPE,
                        reqwest::header::HeaderValue::from_static("application/x-protobuf"),
                    );
                    headers.insert(
                        reqwest::header::ACCEPT,
                        reqwest::header::HeaderValue::from_static("application/x-protobuf"),
                    );
                    let duration = Duration::from_secs_f64(timeout);
                    let Ok(client) = reqwest::Client::builder()
                        .default_headers(headers)
                        .timeout(duration)
                        .redirect(reqwest::redirect::Policy::none())
                        .build()
                    else {
                        continue;
                    };
                    connections.insert(
                        signal.into(),
                        Connection {
                            endpoint: endpoint.into(),
                            client,
                            timeout: duration,
                        },
                    );
                }
            }
        }
        Arc::new(Self {
            connections: SyncMutex::new(connections),
            stop: watch::channel(false).0,
            requests: Arc::new(Semaphore::new(4)),
            closed: AtomicBool::new(false),
        })
    }
    pub fn timeout(&self, signal: &str) -> Option<Duration> {
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        self.connections
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(signal)
            .map(|c| c.timeout)
    }
    pub async fn forward(
        &self,
        signal: &str,
        bytes: &[u8],
    ) -> std::result::Result<Vec<u8>, RelayError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(RelayError::Disabled);
        }
        let mut stopped = self.stop.subscribe();
        let connection = self
            .connections
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(signal)
            .cloned()
            .ok_or(RelayError::Disabled)?;
        let operation = async {
            let body = normalize_request(signal, bytes)?;
            let response = connection
                .client
                .post(&connection.endpoint)
                .body(body)
                .send()
                .await
                .map_err(|_| RelayError::Upstream)?;
            if !matches!(response.status().as_u16(), 200 | 204) {
                return Err(RelayError::Upstream);
            }
            let bytes = super::loki::bounded_response(response, 8 * 1024 * 1024)
                .await
                .map_err(|_| RelayError::Upstream)?;
            normalize_response(signal, &bytes)
        };
        tokio::select! {biased;_=stopped.changed()=>Err(RelayError::Disabled),result=operation=>result}
    }
    pub fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        self.requests.close();
        self.stop.send_replace(true);
        self.connections
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }
}
fn normalize_request(signal: &str, bytes: &[u8]) -> std::result::Result<Vec<u8>, RelayError> {
    match signal {
        "traces" => ExportTraceServiceRequest::decode(bytes).map(|message| message.encode_to_vec()),
        "logs" => ExportLogsServiceRequest::decode(bytes).map(|message| message.encode_to_vec()),
        "metrics" => {
            ExportMetricsServiceRequest::decode(bytes).map(|message| message.encode_to_vec())
        }
        _ => return Err(RelayError::Disabled),
    }
    .map_err(|_| RelayError::InvalidRequest)
}
fn normalize_response(signal: &str, bytes: &[u8]) -> std::result::Result<Vec<u8>, RelayError> {
    match signal {
        "traces" => {
            ExportTraceServiceResponse::decode(bytes).map(|message| message.encode_to_vec())
        }
        "logs" => ExportLogsServiceResponse::decode(bytes).map(|message| message.encode_to_vec()),
        "metrics" => {
            ExportMetricsServiceResponse::decode(bytes).map(|message| message.encode_to_vec())
        }
        _ => return Err(RelayError::Disabled),
    }
    .map_err(|_| RelayError::InvalidResponse)
}
