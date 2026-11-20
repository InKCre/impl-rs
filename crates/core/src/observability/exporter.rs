//! Instance-owned standard OTLP protobuf export of explicit metadata only.
use super::*;
use opentelemetry_proto::tonic::{
    collector::{
        logs::v1::ExportLogsServiceRequest, metrics::v1::ExportMetricsServiceRequest,
        trace::v1::ExportTraceServiceRequest,
    },
    common::v1::{AnyValue, InstrumentationScope, KeyValue, any_value},
    logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
    metrics::v1::{
        Histogram, HistogramDataPoint, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics, Sum,
        metric, number_data_point,
    },
    resource::v1::Resource,
    trace::v1::{ResourceSpans, ScopeSpans, Span},
};
use prost::Message;
use serde_json::Map;
use std::collections::BTreeMap;
const QUEUE: usize = 256;
const BUCKETS: [f64; 14] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1., 2.5, 5., 10., 30., 60., 300.,
];
#[derive(Clone, Default)]
struct Aggregate {
    integer: u64,
    count: u64,
    sum: f64,
    min: Option<f64>,
    max: Option<f64>,
    buckets: Vec<u64>,
}
enum Measurement {
    Counter(u64),
    Duration(f64),
}
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct MetricKey {
    name: &'static str,
    operation: String,
    label: String,
}
pub struct Exporter {
    logs: Option<mpsc::Sender<LogRecord>>,
    traces: Option<mpsc::Sender<Span>>,
    metrics: Arc<SyncMutex<BTreeMap<MetricKey, Aggregate>>>,
    has_metrics: bool,
    closed: AtomicBool,
    gate: SyncMutex<()>,
    dropped: Arc<AtomicU64>,
    pending: Arc<AtomicU64>,
    stop: watch::Sender<bool>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}
impl Exporter {
    pub(crate) async fn start(relay: Arc<Relay>, peer_id: Uuid, version: &str) -> Arc<Self> {
        let mut resource = Resource {
            attributes: vec![
                kv("service.name", json!("core-rs")),
                kv("service.version", json!(version)),
                kv("service.instance.id", json!(Uuid::new_v4())),
                kv("inkcre.peer.id", json!(peer_id)),
            ],
            ..Default::default()
        };
        if let Some(deployment_id) = relay.deployment_id {
            resource
                .attributes
                .push(kv("inkcre.deployment.id", json!(deployment_id)));
        }
        let scope = Some(InstrumentationScope {
            name: "inkcre.metadata".into(),
            ..Default::default()
        });
        let (stop, _) = watch::channel(false);
        let dropped = Arc::new(AtomicU64::new(0));
        let pending = Arc::new(AtomicU64::new(0));
        let mut tasks = vec![];
        let logs = if relay.timeout("logs").is_some() {
            let (sender, receiver) = mpsc::channel(QUEUE);
            let (r, s) = (resource.clone(), scope.clone());
            tasks.push(spawn_batch(
                "logs",
                relay.clone(),
                receiver,
                stop.subscribe(),
                dropped.clone(),
                pending.clone(),
                move |batch| {
                    ExportLogsServiceRequest {
                        resource_logs: vec![ResourceLogs {
                            resource: Some(r.clone()),
                            scope_logs: vec![ScopeLogs {
                                scope: s.clone(),
                                log_records: batch,
                                ..Default::default()
                            }],
                            ..Default::default()
                        }],
                    }
                    .encode_to_vec()
                },
            ));
            Some(sender)
        } else {
            None
        };
        let traces = if relay.timeout("traces").is_some() {
            let (sender, receiver) = mpsc::channel(QUEUE);
            let (r, s) = (resource.clone(), scope.clone());
            tasks.push(spawn_batch(
                "traces",
                relay.clone(),
                receiver,
                stop.subscribe(),
                dropped.clone(),
                pending.clone(),
                move |batch| {
                    ExportTraceServiceRequest {
                        resource_spans: vec![ResourceSpans {
                            resource: Some(r.clone()),
                            scope_spans: vec![ScopeSpans {
                                scope: s.clone(),
                                spans: batch,
                                ..Default::default()
                            }],
                            ..Default::default()
                        }],
                    }
                    .encode_to_vec()
                },
            ));
            Some(sender)
        } else {
            None
        };
        let metrics = Arc::new(SyncMutex::new(BTreeMap::<MetricKey, Aggregate>::new()));
        let has_metrics = relay.timeout("metrics").is_some();
        if has_metrics {
            let mut stopped = stop.subscribe();
            let data = metrics.clone();
            let relay = relay.clone();
            let dropped = dropped.clone();
            let started = nanoseconds();
            tasks.push(tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(30));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                tick.tick().await;
                loop {
                    let closing =
                        tokio::select! {biased;_=stopped.changed()=>true,_=tick.tick()=>false};
                    let items = data.lock().unwrap_or_else(|p| p.into_inner()).clone();
                    if !items.is_empty() {
                        let payload =
                            metric_request(&resource, &scope, started, &items).encode_to_vec();
                        if relay.forward("metrics", &payload).await.is_err() {
                            dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    if closing {
                        break;
                    }
                }
            }));
        }
        Arc::new(Self {
            logs,
            traces,
            metrics,
            has_metrics,
            closed: AtomicBool::new(false),
            gate: SyncMutex::new(()),
            dropped,
            pending,
            stop,
            tasks: Mutex::new(tasks),
        })
    }
    pub fn enabled(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
            && (self.logs.is_some() || self.traces.is_some() || self.has_metrics)
    }
    pub fn traces_enabled(&self) -> bool {
        !self.closed.load(Ordering::Acquire) && self.traces.is_some()
    }
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
    pub(crate) fn log(&self, record: &Record) {
        let Some(sender) = &self.logs else { return };
        let log = LogRecord {
            time_unix_nano: record.timestamp.timestamp_nanos_opt().unwrap_or(0).max(0) as u64,
            observed_time_unix_nano: nanoseconds(),
            severity_number: record.severity_number,
            severity_text: record.severity_text.clone(),
            body: Some(any(json!(
                record.event_name.as_deref().unwrap_or("application.event")
            ))),
            attributes: attributes(&record.attributes),
            trace_id: decode_hex(record.trace_id.as_deref()),
            span_id: decode_hex(record.span_id.as_deref()),
            ..Default::default()
        };
        self.enqueue(sender, log);
    }
    pub(crate) fn span(&self, span: Span) {
        if let Some(sender) = &self.traces {
            self.enqueue(sender, span)
        }
    }
    fn enqueue<T>(&self, sender: &mpsc::Sender<T>, item: T) {
        let _gate = self.gate.lock().unwrap_or_else(|p| p.into_inner());
        if self.closed.load(Ordering::Acquire) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.pending.fetch_add(1, Ordering::Relaxed);
        if sender.try_send(item).is_err() {
            self.pending.fetch_sub(1, Ordering::Relaxed);
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub(crate) fn operation(&self, name: &'static str, outcome: &str, duration: f64) {
        self.metric(
            "inkcre.operation.count",
            name,
            outcome,
            Measurement::Counter(1),
        );
        self.metric(
            "inkcre.operation.duration",
            name,
            outcome,
            Measurement::Duration(duration),
        );
    }
    pub(crate) fn usage(&self, name: &'static str, input: Option<u64>, output: Option<u64>) {
        for (kind, value) in [("input", input), ("output", output)] {
            match value {
                Some(value) if value <= i64::MAX as u64 => self.metric(
                    "inkcre.ai.token.usage",
                    name,
                    kind,
                    Measurement::Counter(value),
                ),
                _ => self.metric(
                    "inkcre.ai.usage.missing",
                    name,
                    kind,
                    Measurement::Counter(1),
                ),
            }
        }
    }
    fn metric(
        &self,
        name: &'static str,
        operation: &'static str,
        label: &str,
        measurement: Measurement,
    ) {
        let _gate = self.gate.lock().unwrap_or_else(|p| p.into_inner());
        if !self.has_metrics || self.closed.load(Ordering::Acquire) {
            return;
        }
        let mut values = self.metrics.lock().unwrap_or_else(|p| p.into_inner());
        let key = MetricKey {
            name,
            operation: operation.into(),
            label: label.into(),
        };
        if values.len() >= 2000 && !values.contains_key(&key) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let item = values.entry(key).or_default();
        item.count = item.count.saturating_add(1);
        if let Measurement::Counter(value) = measurement {
            item.integer = item.integer.saturating_add(value).min(i64::MAX as u64);
        }
        if let Measurement::Duration(value) = measurement {
            item.sum += value;
            item.min = Some(item.min.map_or(value, |v| v.min(value)));
            item.max = Some(item.max.map_or(value, |v| v.max(value)));
            if item.buckets.is_empty() {
                item.buckets = vec![0; BUCKETS.len() + 1]
            }
            let slot = BUCKETS
                .iter()
                .position(|b| value <= *b)
                .unwrap_or(BUCKETS.len());
            item.buckets[slot] = item.buckets[slot].saturating_add(1);
        }
    }
    pub async fn shutdown(&self) {
        {
            let _gate = self.gate.lock().unwrap_or_else(|p| p.into_inner());
            if self.closed.swap(true, Ordering::AcqRel) {
                return;
            }
        }
        self.stop.send_replace(true);
        let tasks = std::mem::take(&mut *self.tasks.lock().await);
        // Connections own <=30s request budgets. All signal drains run concurrently.
        futures_util::future::join_all(tasks.into_iter().map(|mut task| async move {
            if timeout(Duration::from_secs(32), &mut task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }))
        .await;
        self.dropped
            .fetch_add(self.pending.swap(0, Ordering::Relaxed), Ordering::Relaxed);
    }
}
fn spawn_batch<T: Send + 'static>(
    signal: &'static str,
    relay: Arc<Relay>,
    mut receiver: mpsc::Receiver<T>,
    mut stopped: watch::Receiver<bool>,
    dropped: Arc<AtomicU64>,
    pending: Arc<AtomicU64>,
    encode: impl Fn(Vec<T>) -> Vec<u8> + Send + 'static,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.tick().await;
        let mut batch = Vec::with_capacity(QUEUE);
        loop {
            let (closing, flush) = tokio::select! {biased;_=stopped.changed()=>{receiver.close();(true,true)},item=receiver.recv()=>{match item{Some(item)=>batch.push(item),None=>break};(false,false)},_=tick.tick()=>(false,true)};
            if closing {
                while batch.len() < QUEUE {
                    match receiver.try_recv() {
                        Ok(item) => batch.push(item),
                        Err(_) => break,
                    }
                }
            }
            if !batch.is_empty() && (flush || batch.len() == QUEUE) {
                let count = batch.len() as u64;
                let payload = encode(std::mem::take(&mut batch));
                if relay.forward(signal, &payload).await.is_err() {
                    dropped.fetch_add(count, Ordering::Relaxed);
                }
                pending.fetch_sub(count, Ordering::Relaxed);
            }
            if closing {
                while let Some(item) = receiver.recv().await {
                    batch.push(item);
                    if batch.len() == QUEUE {
                        let count = batch.len() as u64;
                        if relay
                            .forward(signal, &encode(std::mem::take(&mut batch)))
                            .await
                            .is_err()
                        {
                            dropped.fetch_add(count, Ordering::Relaxed);
                        }
                        pending.fetch_sub(count, Ordering::Relaxed);
                    }
                }
                if !batch.is_empty() {
                    let count = batch.len() as u64;
                    if relay.forward(signal, &encode(batch)).await.is_err() {
                        dropped.fetch_add(count, Ordering::Relaxed);
                    }
                    pending.fetch_sub(count, Ordering::Relaxed);
                }
                break;
            }
        }
    })
}
fn metric_request(
    resource: &Resource,
    scope: &Option<InstrumentationScope>,
    start: u64,
    items: &BTreeMap<MetricKey, Aggregate>,
) -> ExportMetricsServiceRequest {
    let mut metrics: BTreeMap<&str, Metric> = BTreeMap::new();
    let now = nanoseconds();
    for (key, item) in items {
        let labels = vec![
            kv("operation", json!(key.operation)),
            kv(
                if key.name.starts_with("inkcre.ai.") {
                    "token.type"
                } else {
                    "outcome"
                },
                json!(key.label),
            ),
        ];
        let histogram = key.name == "inkcre.operation.duration";
        let metric = metrics.entry(key.name).or_insert_with(|| Metric {
            name: key.name.into(),
            unit: if histogram {
                "s"
            } else if key.name == "inkcre.operation.count" {
                "{operation}"
            } else if key.name == "inkcre.ai.token.usage" {
                "{token}"
            } else {
                "{field}"
            }
            .into(),
            data: Some(if histogram {
                metric::Data::Histogram(Histogram {
                    aggregation_temporality: 2,
                    ..Default::default()
                })
            } else {
                metric::Data::Sum(Sum {
                    aggregation_temporality: 2,
                    is_monotonic: true,
                    ..Default::default()
                })
            }),
            ..Default::default()
        });
        match metric.data.as_mut() {
            Some(metric::Data::Histogram(hist)) => hist.data_points.push(HistogramDataPoint {
                attributes: labels,
                start_time_unix_nano: start,
                time_unix_nano: now,
                count: item.count,
                sum: Some(item.sum),
                bucket_counts: item.buckets.clone(),
                explicit_bounds: BUCKETS.to_vec(),
                min: item.min,
                max: item.max,
                ..Default::default()
            }),
            Some(metric::Data::Sum(sum)) => sum.data_points.push(NumberDataPoint {
                attributes: labels,
                start_time_unix_nano: start,
                time_unix_nano: now,
                value: Some(number_data_point::Value::AsInt(item.integer as i64)),
                ..Default::default()
            }),
            _ => {}
        }
    }
    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(resource.clone()),
            scope_metrics: vec![ScopeMetrics {
                scope: scope.clone(),
                metrics: metrics.into_values().collect(),
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}
pub(crate) fn nanoseconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos().min(u64::MAX as u128) as u64)
}
pub(crate) fn admitted(values: &Map<String, Value>) -> Map<String, Value> {
    values
        .iter()
        .filter(|(key, _)| allowed(key))
        .take(64)
        .map(|(key, v)| (key.clone(), bounded_value(v)))
        .collect()
}
fn bounded_value(value: &Value) -> Value {
    match value {
        Value::String(s) => json!(s.chars().take(256).collect::<String>()),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .filter(|v| !v.is_array() && !v.is_object())
                .take(32)
                .map(bounded_value)
                .collect(),
        ),
        Value::Object(_) => Value::Null,
        v => v.clone(),
    }
}
pub(crate) fn attributes(values: &Map<String, Value>) -> Vec<KeyValue> {
    admitted(values)
        .into_iter()
        .map(|(key, v)| kv(&key, v))
        .collect()
}
fn kv(key: &str, value: Value) -> KeyValue {
    KeyValue {
        key: key.into(),
        value: Some(any(value)),
    }
}
fn any(value: Value) -> AnyValue {
    AnyValue {
        value: Some(match value {
            Value::Bool(v) => any_value::Value::BoolValue(v),
            Value::Number(v) => {
                if let Some(v) = v.as_i64() {
                    any_value::Value::IntValue(v)
                } else {
                    any_value::Value::DoubleValue(v.as_f64().unwrap_or(0.))
                }
            }
            Value::Array(values) => {
                any_value::Value::ArrayValue(opentelemetry_proto::tonic::common::v1::ArrayValue {
                    values: values.into_iter().map(any).collect(),
                })
            }
            Value::String(v) => any_value::Value::StringValue(v),
            _ => any_value::Value::StringValue(String::new()),
        }),
    }
}
fn decode_hex(value: Option<&str>) -> Vec<u8> {
    let Some(value) = value else { return vec![] };
    if value.len() % 2 != 0 {
        return vec![];
    }
    let mut output = Vec::new();
    for pair in value.as_bytes().chunks(2) {
        let Some(a) = (pair[0] as char).to_digit(16) else {
            return vec![];
        };
        let Some(b) = (pair[1] as char).to_digit(16) else {
            return vec![];
        };
        output.push((a * 16 + b) as u8)
    }
    output
}

pub(crate) fn allowed(key: &str) -> bool {
    METADATA.contains(&key)
        || matches!(
            key,
            "inkcre.ai.provider.id"
                | "inkcre.ai.dialect"
                | "inkcre.peer.outcome"
                | "gen_ai.response.finish_reasons"
                | "gen_ai.usage.cache_read.input_tokens"
                | "gen_ai.usage.reasoning.output_tokens"
        )
}
