use super::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;
fn identity() -> String {
    Uuid::new_v4().to_string()
}
fn now() -> DateTime<Utc> {
    Utc::now()
}
fn log_kind() -> String {
    "log".into()
}
fn info() -> String {
    "INFO".into()
}
fn severity() -> i32 {
    9
}
fn limit() -> i64 {
    100
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    #[serde(default = "identity")]
    pub id: String,
    #[serde(default = "now")]
    pub timestamp: DateTime<Utc>,
    #[serde(default = "log_kind")]
    pub kind: String,
    pub event_name: Option<String>,
    #[serde(default = "severity")]
    pub severity_number: i32,
    #[serde(default = "info")]
    pub severity_text: String,
    pub body: String,
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
    #[serde(default)]
    pub attributes: Map<String, Value>,
}
fn bounded(value: &str, maximum: usize) -> bool {
    value.chars().count() <= maximum
}
impl Record {
    pub fn event(name: &str, attributes: Map<String, Value>) -> Self {
        Self {
            id: identity(),
            timestamp: now(),
            kind: "event".into(),
            event_name: Some(name.into()),
            severity_number: 9,
            severity_text: info(),
            body: name.into(),
            trace_id: None,
            span_id: None,
            attributes,
        }
    }
    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty()
            || !bounded(&self.id, 128)
            || !matches!(self.kind.as_str(), "log" | "event" | "span")
            || !bounded(&self.body, 8192)
            || !bounded(&self.severity_text, 32)
            || !(1..=24).contains(&self.severity_number)
            || [&self.event_name, &self.trace_id, &self.span_id]
                .iter()
                .any(|v| v.as_ref().is_some_and(|s| !bounded(s, 128)))
            || self.attributes.len() > 64
        {
            return Err(Error::Invalid);
        }
        for (key, value) in &self.attributes {
            if !bounded(key, 128) {
                return Err(Error::Invalid);
            }
            let values = match value {
                Value::Array(items) => items.iter().collect::<Vec<_>>(),
                item => vec![item],
            };
            if values.len() > 32
                || values.iter().any(|item| {
                    item.is_object()
                        || item.is_array()
                        || item.as_str().is_some_and(|s| !bounded(s, 1024))
                })
            {
                return Err(Error::Invalid);
            }
        }
        Ok(())
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
    #[serde(default = "limit")]
    pub limit: i64,
    pub kind: Option<String>,
    pub severity_min: Option<i32>,
    pub job_id: Option<i64>,
    pub trace_id: Option<String>,
    pub peer_id: Option<Uuid>,
    pub operation: Option<String>,
    pub text: Option<String>,
}
impl Query {
    pub fn validate(&self) -> Result<()> {
        if self.since >= self.until
            || (self.until - self.since) > chrono::Duration::days(31)
            || !(1..=500).contains(&self.limit)
            || self
                .kind
                .as_ref()
                .is_some_and(|v| !matches!(v.as_str(), "log" | "event" | "span"))
            || self.severity_min.is_some_and(|v| !(1..=24).contains(&v))
            || self.job_id.is_some_and(|v| v < 1)
            || [&self.trace_id, &self.operation]
                .iter()
                .any(|v| v.as_ref().is_some_and(|s| !bounded(s, 128)))
            || self.text.as_ref().is_some_and(|s| !bounded(s, 256))
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
#[derive(Serialize)]
pub struct Page {
    pub records: Vec<Record>,
    pub truncated: bool,
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
    pub backend: String,
}
#[derive(Serialize)]
pub struct Status {
    pub backend: String,
    pub configured: bool,
    pub recording: bool,
    pub queryable: bool,
    pub scope: &'static str,
    pub history: &'static str,
    pub dropped: u64,
    pub error: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub action: String,
    pub expected_backend: Option<String>,
    pub query: Option<Query>,
    pub records: Option<Vec<Record>>,
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.action.as_str(), "status" | "query" | "ingest")
            || (self.action == "query") != self.query.is_some()
            || (self.action == "ingest") != self.records.is_some()
            || self
                .expected_backend
                .as_ref()
                .is_some_and(|s| !matches!(s.as_str(), "postgresql" | "loki"))
        {
            return Err(Error::Invalid);
        }
        if let Some(query) = &self.query {
            query.validate()?
        }
        if let Some(records) = &self.records {
            if records.len() > 100 {
                return Err(Error::Invalid);
            }
            for record in records {
                record.validate()?
            }
        }
        Ok(())
    }
}
