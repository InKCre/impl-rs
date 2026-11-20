//! Bounded standard W3C carrier; no process-global context or baggage.
use opentelemetry::{
    Context,
    propagation::TextMapPropagator,
    trace::{SpanContext, TraceContextExt},
};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use std::{collections::HashMap, future::Future};
use uuid::Uuid;

#[derive(Clone, Default)]
pub struct TraceCarrier {
    pub traceparent: Option<String>,
    pub tracestate: Option<String>,
}
impl TraceCarrier {
    pub(crate) fn extract(&self) -> SpanContext {
        let headers: HashMap<String, String> = [
            ("traceparent", &self.traceparent),
            ("tracestate", &self.tracestate),
        ]
        .into_iter()
        .filter_map(|(k, v)| {
            v.as_ref()
                .filter(|s| s.len() <= 512)
                .map(|s| (k.into(), s.clone()))
        })
        .collect();
        TraceContextPropagator::new()
            .extract_with_context(&Context::new(), &headers)
            .span()
            .span_context()
            .clone()
    }
    pub(crate) fn from_context(context: &SpanContext) -> Self {
        let mut headers = HashMap::new();
        TraceContextPropagator::new().inject_context(
            &Context::new().with_remote_span_context(context.clone()),
            &mut headers,
        );
        Self {
            traceparent: headers.remove("traceparent"),
            tracestate: headers.remove("tracestate").filter(|s| s.len() <= 512),
        }
    }
}
#[derive(Clone)]
pub struct TraceScope {
    pub(crate) owner: Uuid,
    pub(crate) context: SpanContext,
    pub(crate) enabled: bool,
}
tokio::task_local! { static CURRENT: TraceScope; }
impl TraceScope {
    /// Carry an explicitly captured scope across a spawned task boundary.
    pub fn scope<F: Future>(&self, future: F) -> impl Future<Output = F::Output> {
        let scope = self.clone();
        let future = Box::pin(future);
        async move { CURRENT.scope(scope, future).await }
    }
    pub(crate) fn current(owner: Uuid) -> Option<Self> {
        CURRENT
            .try_with(Clone::clone)
            .ok()
            .filter(|scope| scope.owner == owner)
    }
    pub(crate) fn carrier(&self) -> TraceCarrier {
        if self.enabled {
            TraceCarrier::from_context(&self.context)
        } else {
            TraceCarrier::default()
        }
    }
}
