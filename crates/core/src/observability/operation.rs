//! Fixed-name operation scopes. Cancellation and errors never capture business payloads.
use super::{propagation::TraceScope, *};
use opentelemetry::trace::{SpanContext, SpanId, TraceFlags, TraceId, TraceState};
use opentelemetry_proto::tonic::trace::v1::{
    Span, Status,
    span::{Link, SpanKind},
    status::StatusCode,
};
use serde_json::Map;
use std::{future::Future, time::Instant};
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Success,
    Error,
    Cancelled,
}
impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}
struct State {
    outcome: Outcome,
    attributes: Map<String, Value>,
    failure_attributes: Vec<(&'static str, Value, Value)>,
}
#[derive(Clone)]
pub struct Operation {
    runtime: Option<Arc<Runtime>>,
    state: Arc<SyncMutex<State>>,
    scope: TraceScope,
    name: &'static str,
    start: u64,
    clock: Instant,
    parent: SpanContext,
    link: SpanContext,
    kind: i32,
}
impl Operation {
    fn new(
        core: &Core,
        name: &'static str,
        attributes: Map<String, Value>,
        parent: Option<TraceCarrier>,
        link: Option<TraceCarrier>,
        kind: i32,
    ) -> Self {
        let runtime = core.observation_runtime();
        let owner = runtime.as_ref().map_or(Uuid::nil(), |r| r.id);
        let enabled = runtime
            .as_ref()
            .and_then(|r| r.exporter())
            .is_some_and(|e| e.enabled());
        let parent = if enabled {
            parent
                .map(|c| c.extract())
                .or_else(|| TraceScope::current(owner).map(|s| s.context))
                .unwrap_or_else(SpanContext::empty_context)
        } else {
            SpanContext::empty_context()
        };
        let link = if enabled {
            link.map(|c| c.extract())
                .unwrap_or_else(SpanContext::empty_context)
        } else {
            SpanContext::empty_context()
        };
        let trace_id = if parent.is_valid() {
            parent.trace_id()
        } else {
            TraceId::from_bytes(*Uuid::new_v4().as_bytes())
        };
        let uuid = Uuid::new_v4();
        let mut span_id = [0; 8];
        span_id.copy_from_slice(&uuid.as_bytes()[..8]);
        let traces_enabled = runtime
            .as_ref()
            .and_then(|r| r.exporter())
            .is_some_and(|e| e.traces_enabled());
        let context = if traces_enabled {
            SpanContext::new(
                trace_id,
                SpanId::from_bytes(span_id),
                TraceFlags::SAMPLED,
                false,
                if parent.is_valid() {
                    parent.trace_state().clone()
                } else {
                    TraceState::default()
                },
            )
        } else {
            SpanContext::empty_context()
        };
        Self {
            runtime,
            state: Arc::new(SyncMutex::new(State {
                outcome: Outcome::Success,
                attributes: exporter::admitted(&attributes),
                failure_attributes: Vec::new(),
            })),
            scope: TraceScope {
                owner,
                context,
                enabled,
            },
            name,
            start: exporter::nanoseconds(),
            clock: Instant::now(),
            parent,
            link,
            kind,
        }
    }
    pub fn set_outcome(&self, outcome: Outcome) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).outcome = outcome;
    }
    pub fn set_attribute(&self, key: &str, value: Value) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if exporter::allowed(key)
            && (state.attributes.len() < 64 || state.attributes.contains_key(key))
        {
            let mut values = Map::new();
            values.insert(key.into(), value);
            state.attributes.extend(exporter::admitted(&values));
        }
    }
    /// Supply unit-owned terminal labels even when its future is cancelled.
    pub fn set_failure_attribute(&self, key: &'static str, error: Value, cancelled: Value) {
        if !exporter::allowed(key) {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.failure_attributes.len() < 16 {
            let admit = |value| {
                let mut values = Map::new();
                values.insert(key.into(), value);
                exporter::admitted(&values)
                    .remove(key)
                    .unwrap_or(Value::Null)
            };
            state
                .failure_attributes
                .push((key, admit(error), admit(cancelled)));
        }
    }
    pub fn trace_scope(&self) -> TraceScope {
        self.scope.clone()
    }
    pub fn scope<F: Future>(self, future: F) -> impl Future<Output = F::Output> {
        // Box before constructing the async state: nested AI/Agent futures must
        // not multiply their stack footprint on the default Tokio worker stack.
        let future = Box::pin(future);
        async move {
            let scope = self.scope.clone();
            let mut completion = Completion {
                operation: self,
                completed: false,
            };
            let result = scope.scope(future).await;
            completion.completed = true;
            result
        }
    }
    /// Result errors are classified without recording their Display/Debug representation.
    pub fn run<T, E, F: Future<Output = std::result::Result<T, E>>>(
        self,
        future: F,
    ) -> impl Future<Output = std::result::Result<T, E>> {
        let observation = self.clone();
        let future = Box::pin(future);
        self.scope(async move {
            let result = future.await;
            if result.is_err() {
                observation.set_outcome(Outcome::Error);
            }
            result
        })
    }
    fn finish(&self, cancelled: bool) {
        let Some(runtime) = &self.runtime else { return };
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let outcome = if cancelled {
            Outcome::Cancelled
        } else {
            state.outcome
        };
        let seconds = self.clock.elapsed().as_secs_f64();
        let mut attrs = state.attributes.clone();
        for (key, error, cancelled) in &state.failure_attributes {
            if outcome != Outcome::Success {
                attrs.insert(
                    (*key).into(),
                    if outcome == Outcome::Error {
                        error.clone()
                    } else {
                        cancelled.clone()
                    },
                );
            }
        }
        attrs.insert("inkcre.operation".into(), json!(self.name));
        attrs.insert("inkcre.outcome".into(), json!(outcome.as_str()));
        let mut record_attrs = attrs.clone();
        record_attrs.insert("outcome".into(), json!(outcome.as_str()));
        record_attrs.insert("duration_ms".into(), json!(seconds * 1000.));
        let mut record = Record::event(self.name, record_attrs);
        record.kind = "span".into();
        if outcome == Outcome::Error {
            record.severity_number = 17;
            record.severity_text = "ERROR".into();
        }
        correlate(&mut record, &self.scope.context);
        runtime.emit(record);
        if let Some(exporter) = runtime.exporter() {
            exporter.operation(self.name, outcome.as_str(), seconds);
            if self.scope.context.is_valid() {
                exporter.span(Span {
                    trace_id: self.scope.context.trace_id().to_bytes().to_vec(),
                    span_id: self.scope.context.span_id().to_bytes().to_vec(),
                    trace_state: self.scope.context.trace_state().header(),
                    parent_span_id: if self.parent.is_valid() {
                        self.parent.span_id().to_bytes().to_vec()
                    } else {
                        vec![]
                    },
                    name: self.name.into(),
                    kind: self.kind,
                    start_time_unix_nano: self.start,
                    end_time_unix_nano: exporter::nanoseconds(),
                    attributes: exporter::attributes(&attrs),
                    links: if self.link.is_valid() {
                        vec![Link {
                            trace_id: self.link.trace_id().to_bytes().to_vec(),
                            span_id: self.link.span_id().to_bytes().to_vec(),
                            trace_state: self.link.trace_state().header(),
                            flags: u32::from(self.link.trace_flags().to_u8()),
                            ..Default::default()
                        }]
                    } else {
                        vec![]
                    },
                    status: Some(Status {
                        message: String::new(),
                        code: if outcome == Outcome::Success {
                            StatusCode::Unset
                        } else {
                            StatusCode::Error
                        } as i32,
                    }),
                    flags: u32::from(self.scope.context.trace_flags().to_u8()),
                    ..Default::default()
                });
            }
        }
    }
}
struct Completion {
    operation: Operation,
    completed: bool,
}
impl Drop for Completion {
    fn drop(&mut self) {
        self.operation.finish(!self.completed)
    }
}
pub(crate) fn correlate(record: &mut Record, context: &SpanContext) {
    if context.is_valid() {
        record.trace_id = Some(context.trace_id().to_string());
        record.span_id = Some(context.span_id().to_string());
    }
}
impl Core {
    pub fn operation(&self, name: &'static str, attributes: Map<String, Value>) -> Operation {
        Operation::new(
            self,
            name,
            attributes,
            None,
            None,
            SpanKind::Internal as i32,
        )
    }
    pub fn client_operation(
        &self,
        name: &'static str,
        attributes: Map<String, Value>,
    ) -> Operation {
        Operation::new(self, name, attributes, None, None, SpanKind::Client as i32)
    }
    /// An HTTP/server span follows the validated W3C remote parent.
    pub fn server_operation(
        &self,
        name: &'static str,
        attributes: Map<String, Value>,
        carrier: TraceCarrier,
    ) -> Operation {
        Operation::new(
            self,
            name,
            attributes,
            Some(carrier),
            None,
            SpanKind::Server as i32,
        )
    }
    /// A separately scheduled execution starts a new trace and links submission causality.
    pub fn linked_operation(
        &self,
        name: &'static str,
        attributes: Map<String, Value>,
        carrier: TraceCarrier,
    ) -> Operation {
        Operation::new(
            self,
            name,
            attributes,
            Some(TraceCarrier::default()),
            Some(carrier),
            SpanKind::Internal as i32,
        )
    }
    pub fn trace_scope(&self) -> Option<TraceScope> {
        self.observation_runtime()
            .and_then(|r| TraceScope::current(r.id))
    }
    pub fn trace_carrier(&self) -> TraceCarrier {
        self.trace_scope().map(|s| s.carrier()).unwrap_or_default()
    }
    pub fn record_ai_usage(
        &self,
        operation: &'static str,
        input: Option<u64>,
        output: Option<u64>,
    ) {
        let mut attrs = Map::new();
        attrs.insert("inkcre.operation".into(), json!(operation));
        if let Some(v) = input.filter(|v| *v <= i64::MAX as u64) {
            attrs.insert("input_tokens".into(), json!(v));
        }
        if let Some(v) = output.filter(|v| *v <= i64::MAX as u64) {
            attrs.insert("output_tokens".into(), json!(v));
        }
        self.record_event("ai.usage", attrs);
        if let Some(exporter) = self.observation_runtime().and_then(|r| r.exporter()) {
            exporter.usage(operation, input, output)
        }
    }
}
