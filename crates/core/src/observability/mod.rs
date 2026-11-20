//! Bounded recording and querying of the deployment-selected observation backend.
mod exporter;
mod loki;
mod operation;
mod propagation;
pub use exporter::Exporter;
pub use operation::{Operation, Outcome};
pub use propagation::{TraceCarrier, TraceScope};
mod options;
mod relay;
pub use relay::{Relay, RelayError};
mod postgresql;
pub use options::{Deployment, Endpoints, LocalOptions, OtlpConnection};
mod types;
use crate::Core;
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex as SyncMutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::{
    sync::{Mutex, Semaphore, mpsc, watch},
    task::JoinHandle,
    time::{Duration, timeout},
};
pub use types::*;
use uuid::Uuid;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid observability request")]
    Invalid,
    #[error("selected observability backend is unavailable")]
    Unavailable,
}
pub type Result<T> = std::result::Result<T, Error>;
struct Counters {
    dropped: AtomicU64,
    pending: AtomicU64,
    error: SyncMutex<Option<String>>,
}
impl Counters {
    fn error(&self, message: &str) {
        *self.error.lock().unwrap_or_else(|p| p.into_inner()) = Some(message.into())
    }
}
pub struct Runtime {
    id: Uuid,
    exporter: SyncMutex<Option<Arc<Exporter>>>,
    core: Core,
    peer_id: Uuid,
    selected: String,
    backend: Option<options::Backend>,
    external_content: bool,
    accepting: AtomicBool,
    closed: AtomicBool,
    gate: SyncMutex<()>,
    counts: Arc<Counters>,
    sender: mpsc::Sender<Record>,
    stop: watch::Sender<bool>,
    writer: Mutex<Option<JoinHandle<()>>>,
    queries: Arc<Semaphore>,
    pub requests: Arc<Semaphore>,
}
impl Runtime {
    pub async fn start(core: Core, peer_id: Uuid) -> Arc<Self> {
        Self::start_with_options(core, peer_id, LocalOptions::default()).await
    }
    pub async fn start_with_options(core: Core, peer_id: Uuid, options: LocalOptions) -> Arc<Self> {
        let (selected, backend, _deployment, error) = options::configure(&core, &options).await;
        let configured = backend.is_some();
        let (sender, mut receiver) = mpsc::channel::<Record>(1024);
        let (stop, mut stopped) = watch::channel(false);
        let counts = Arc::new(Counters {
            dropped: AtomicU64::new(0),
            pending: AtomicU64::new(0),
            error: SyncMutex::new(error),
        });
        let runtime = Arc::new(Self {
            id: Uuid::new_v4(),
            exporter: SyncMutex::new(None),
            core: core.clone(),
            peer_id,
            selected,
            backend: backend.clone(),
            external_content: options.allow_external_content,
            accepting: AtomicBool::new(configured),
            closed: AtomicBool::new(false),
            gate: SyncMutex::new(()),
            counts: counts.clone(),
            sender,
            stop,
            writer: Mutex::new(None),
            queries: Arc::new(Semaphore::new(4)),
            requests: Arc::new(Semaphore::new(4)),
        });
        if let Some(backend) = backend {
            let task = tokio::spawn(async move {
                loop {
                    let first = tokio::select! {
                        biased;
                        _=stopped.changed()=>{
                            receiver.close();
                            receiver.recv().await
                        },
                        item=receiver.recv()=>item,
                    };
                    let Some(first) = first else { break };
                    let mut records = vec![first];
                    while records.len() < 100 {
                        match receiver.try_recv() {
                            Ok(record) => records.push(record),
                            Err(_) => break,
                        }
                    }
                    let count = records.len() as u64;
                    if !matches!(
                        timeout(Duration::from_secs(10), backend.write(&core, records)).await,
                        Ok(Ok(()))
                    ) {
                        counts.dropped.fetch_add(count, Ordering::Relaxed);
                        counts.error("Selected backend write failed; records were dropped");
                    }
                    counts.pending.fetch_sub(count, Ordering::Relaxed);
                }
            });
            *runtime.writer.lock().await = Some(task);
        }
        runtime.core.bind_observability(&runtime);
        runtime
    }
    /// Initialize opt-in metadata signals using this instance's private relay connections.
    pub async fn start_telemetry(&self, relay: Arc<Relay>, version: &str) {
        let exporter = Exporter::start(relay, self.peer_id, version).await;
        let old = self
            .exporter
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .replace(exporter);
        if let Some(old) = old {
            old.shutdown().await;
        }
    }
    pub fn exporter(&self) -> Option<Arc<Exporter>> {
        self.exporter
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    pub async fn shutdown_telemetry(&self) {
        let exporter = self.exporter();
        if let Some(exporter) = exporter {
            exporter.shutdown().await;
        }
    }
    pub fn status(&self) -> Status {
        Status {
            backend: self.selected.clone(),
            configured: self.backend.is_some(),
            recording: self.accepting.load(Ordering::Acquire),
            queryable: self.backend.is_some() && !self.closed.load(Ordering::Acquire),
            scope: "selected_backend",
            history: "Only the selected backend is queried; prior backend data remains there.",
            dropped: self.counts.dropped.load(Ordering::Relaxed),
            error: self
                .counts
                .error
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone(),
        }
    }
    pub fn emit(&self, mut record: Record) -> bool {
        if record.validate().is_err() {
            self.counts.dropped.fetch_add(1, Ordering::Relaxed);
            self.counts
                .error("Invalid observation metadata was dropped");
            return false;
        }
        if !record.attributes.contains_key("inkcre.peer.id") && record.attributes.len() == 64 {
            self.counts.dropped.fetch_add(1, Ordering::Relaxed);
            self.counts
                .error("No metadata capacity for the recording Peer identity");
            return false;
        }
        record
            .attributes
            .entry("inkcre.peer.id".to_owned())
            .or_insert(json!(self.peer_id));
        if self.selected == "loki" && !self.external_content {
            record
                .attributes
                .retain(|key, _| METADATA.contains(&key.as_str()));
            record.body = if record.kind != "log" {
                record
                    .event_name
                    .clone()
                    .unwrap_or_else(|| "application.log".into())
            } else {
                "application.log".into()
            };
        }
        let _gate = self.gate.lock().unwrap_or_else(|p| p.into_inner());
        if !self.accepting.load(Ordering::Acquire) {
            self.counts.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.counts.pending.fetch_add(1, Ordering::Relaxed);
        if self.sender.try_send(record).is_err() {
            self.counts.pending.fetch_sub(1, Ordering::Relaxed);
            self.counts.dropped.fetch_add(1, Ordering::Relaxed);
            self.counts
                .error("Recording queue full; records were dropped");
            return false;
        }
        true
    }
    pub async fn query(&self, query: Query) -> Result<Page> {
        query.validate()?;
        if self.backend.is_none() || self.closed.load(Ordering::Acquire) {
            return Err(Error::Unavailable);
        }
        let _permit = self
            .queries
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Unavailable)?;
        match timeout(
            Duration::from_secs(10),
            self.backend
                .as_ref()
                .ok_or(Error::Unavailable)?
                .query(&self.core, query),
        )
        .await
        {
            Ok(Ok(page)) => Ok(page),
            _ => {
                self.counts.error("Selected backend query failed");
                Err(Error::Unavailable)
            }
        }
    }
    pub async fn shutdown(&self) {
        self.shutdown_telemetry().await;
        {
            let _gate = self.gate.lock().unwrap_or_else(|p| p.into_inner());
            if self.closed.swap(true, Ordering::AcqRel) {
                return;
            }
            self.accepting.store(false, Ordering::Release);
        }
        let _ = self.stop.send(true);
        if let Some(mut task) = self.writer.lock().await.take()
            && timeout(Duration::from_secs(5), &mut task).await.is_err()
        {
            task.abort();
            let _ = task.await;
            self.counts
                .error("Recording drain timed out; pending records were dropped");
        }
        if let Some(backend) = &self.backend {
            backend.close();
        }
        let pending = self.counts.pending.swap(0, Ordering::Relaxed);
        self.counts.dropped.fetch_add(pending, Ordering::Relaxed);
    }
}
impl Core {
    pub fn bind_observability(&self, runtime: &Arc<Runtime>) {
        *self.observation.write().unwrap_or_else(|p| p.into_inner()) =
            Some(Arc::downgrade(runtime));
    }
    pub(crate) fn observation_runtime(&self) -> Option<Arc<Runtime>> {
        self.observation
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
    }
    pub fn record_event(&self, name: &'static str, attributes: serde_json::Map<String, Value>) {
        let runtime = self.observation_runtime();
        if let Some(runtime) = runtime {
            let mut event = Record::event(name, attributes);
            if event
                .attributes
                .get("outcome")
                .and_then(Value::as_str)
                .is_some_and(|outcome| matches!(outcome, "error" | "timeout"))
                || event
                    .attributes
                    .get("inkcre.job.status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| matches!(status, "failed" | "timed_out"))
            {
                event.severity_number = 17;
                event.severity_text = "ERROR".into();
            }
            if let Some(scope) = self.trace_scope() {
                operation::correlate(&mut event, &scope.context);
            }
            if let Some(exporter) = runtime.exporter() {
                exporter.log(&event);
            }
            runtime.emit(event);
        }
    }
}

pub fn register_config(
    registry: &mut crate::deployment_config::ConfigRegistry,
) -> crate::deployment_config::Result<()> {
    use crate::deployment_config::{Error, SchemaDescriptor};
    registry.register(SchemaDescriptor {
        id:"inkcre.observability.v1".into(),keys:vec!["inkcre.observability".into()],description:"Deployment identity and non-secret telemetry endpoints.".into(),
        input_schema:Some(json!({"type":"object","additionalProperties":false,"required":["deployment_id"],"properties":{"deployment_id":{"type":"string","format":"uuid"},"diagnostics_url":{"type":["string","null"],"default":null},"otlp_http_endpoints":{"type":["object","null"],"default":null,"additionalProperties":false,"properties":{"traces":{"type":["string","null"],"default":null},"logs":{"type":["string","null"],"default":null},"metrics":{"type":["string","null"],"default":null}}}}}))
    },|value|{
        let mut parsed:Deployment=serde_json::from_value(value).map_err(|_|Error::Invalid("invalid observability deployment configuration"))?;
        parsed.validate().map_err(|_|Error::Invalid("invalid observability endpoint"))?;
        let mut urls=vec![&mut parsed.diagnostics_url];
        if let Some(endpoints)=&mut parsed.otlp_http_endpoints { urls.extend([&mut endpoints.traces,&mut endpoints.logs,&mut endpoints.metrics]); }
        for value in urls.into_iter().flatten(){*value=url::Url::parse(value).map_err(|_|Error::Invalid("invalid observability endpoint"))?.to_string();}
        serde_json::to_value(parsed).map_err(|_|Error::Invalid("invalid observability deployment configuration"))
    })?;
    registry.register(SchemaDescriptor{id:"inkcre.observability.backend.v1".into(),keys:vec!["inkcre.observability.backend".into()],description:"Select the shared recording and querying backend.".into(),input_schema:Some(json!({"type":"object","additionalProperties":false,"properties":{"backend":{"enum":["postgresql","loki"],"default":"postgresql"}}}))},|mut value|{
  let object=value.as_object_mut().ok_or(Error::Invalid("value must be an object"))?;
  if object.keys().any(|key|key!="backend"){return Err(Error::Invalid("unknown backend configuration property"))}
  let backend=object.entry("backend").or_insert(json!("postgresql"));
  if !matches!(backend.as_str(),Some("postgresql"|"loki")){return Err(Error::Invalid("unknown observation backend"))}
  Ok(value)
 })
}

const METADATA: &[&str] = &[
    "duration_ms",
    "error.type",
    "gen_ai.conversation.id",
    "gen_ai.operation.name",
    "gen_ai.provider.name",
    "gen_ai.request.model",
    "gen_ai.response.model",
    "gen_ai.tool.name",
    "gen_ai.usage.input_tokens",
    "gen_ai.usage.output_tokens",
    "http.request.method",
    "http.response.status_code",
    "http.route",
    "inkcre.agent.id",
    "inkcre.agent.model_call",
    "inkcre.agent.model_calls",
    "inkcre.agent.outcome",
    "inkcre.agent.tool_call.id_hash",
    "inkcre.agent.turn",
    "inkcre.ai.model.id",
    "inkcre.ai.usage.input.source",
    "inkcre.ai.usage.output.source",
    "inkcre.block.id",
    "inkcre.cron.id",
    "inkcre.entity.block_ids",
    "inkcre.entity.read",
    "inkcre.entity.read.count",
    "inkcre.entity.read.kind",
    "inkcre.entity.relation_ids",
    "inkcre.job.id",
    "inkcre.job.status",
    "inkcre.operation",
    "inkcre.outcome",
    "inkcre.peer.id",
    "inkcre.peer.target_id",
    "inkcre.resolver.id",
    "inkcre.retrieval.candidate_count",
    "inkcre.retrieval.candidates",
    "inkcre.retrieval.method",
    "inkcre.thread.id",
    "inkcre.tool.id",
    "inkcre.tool_call.id",
    "input_tokens",
    "job_id",
    "operation",
    "outcome",
    "output_tokens",
    "reason",
];
