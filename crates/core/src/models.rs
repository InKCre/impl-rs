use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Block {
    pub id: i32,
    pub storage: Option<i32>,
    pub resolver: String,
    pub content: String,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockInput {
    #[serde(default)]
    pub storage: Option<i32>,
    pub resolver: String,
    pub content: String,
}

/// An omitted field is unchanged; explicit null clears only `storage`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockPatch {
    #[serde(
        default,
        deserialize_with = "double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub storage: Option<Option<i32>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub resolver: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub content: Option<String>,
}

fn double_option<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<Option<i32>>, D::Error> {
    Option::<i32>::deserialize(d).map(Some)
}

fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Relation {
    pub id: i32,
    pub from_: Option<i32>,
    pub to_: Option<i32>,
    pub content: String,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelationInput {
    pub from_: i32,
    pub to_: i32,
    pub content: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelationPatch {
    #[serde(
        default,
        alias = "from_block_id",
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub from_: Option<i32>,
    #[serde(
        default,
        alias = "to_block_id",
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub to_: Option<i32>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub content: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Page<T, C> {
    pub items: Vec<T>,
    pub next_cursor: Option<C>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Admission {
    pub revision: String,
    pub migration_heads: Vec<String>,
    pub environment: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Peer {
    pub id: Uuid,
    pub name: String,
    pub application_version: Option<String>,
    pub labels: Vec<String>,
    pub config: Value,
    pub config_schema: Value,
    pub capabilities: Value,
    pub lease_expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[sqlx(default)]
    pub lease_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    pub id: String,
    pub inbound: Inbound,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inbound {
    pub protocol: String,
    pub parameters: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum JobStatus {
    Pending,
    Running,
    Finished,
    Failed,
    TimedOut,
    Aborted,
}

impl JobStatus {
    pub fn terminal(self) -> bool {
        !matches!(self, Self::Pending | Self::Running)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct JobType {
    pub id: String,
    pub description: String,
    pub parameters_schema: Value,
    pub default_timeout_seconds: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Job {
    pub id: i64,
    #[serde(rename = "type")]
    #[sqlx(rename = "type")]
    pub type_: String,
    pub parameters: Value,
    pub state: Value,
    pub timeout_seconds: i32,
    pub status: JobStatus,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub closed_at: Option<DateTime<Utc>>,
    pub abort_requested: bool,
    pub submission_traceparent: Option<String>,
    pub submission_tracestate: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobInput {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(default = "empty_object")]
    pub parameters: Value,
    pub timeout_seconds: Option<i32>,
    pub submission_traceparent: Option<String>,
    pub submission_tracestate: Option<String>,
}

pub fn empty_object() -> Value {
    serde_json::json!({})
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Extension {
    pub name: String,
    pub nickname: Option<String>,
    pub config: Value,
    #[serde(skip_serializing)]
    pub state: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Participation {
    pub name: String,
    pub peer_id: Uuid,
    pub distribution: Option<String>,
    pub version: String,
    pub enabled: bool,
    pub config_schema: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Storage {
    pub id: i32,
    #[serde(rename = "type")]
    #[sqlx(rename = "type")]
    pub type_: String,
    pub nickname: Option<String>,
    pub config: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", content = "content", rename_all = "snake_case")]
pub enum HydratedContent {
    Text(String),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, Serialize)]
pub struct Neighborhood {
    pub blocks: Vec<Block>,
    pub relations: Vec<Relation>,
    pub truncated: bool,
    pub next_cursor: Option<i32>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct GraphModel {
    pub blocks: Vec<Block>,
    pub relations: Vec<Relation>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    In,
    Out,
    #[default]
    Both,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Out => "out",
            Self::Both => "both",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathOptions {
    pub direction: Direction,
    pub contents: Vec<String>,
    pub max_hops: u8,
    pub max_explored_blocks: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PathOutcome {
    Found,
    NotFound,
    LimitReached,
}

#[derive(Debug, Clone, Serialize)]
pub struct GraphPath {
    pub outcome: PathOutcome,
    pub graph: GraphModel,
    pub block_ids: Vec<i32>,
    pub relation_ids: Vec<i32>,
}

/// Shared submission and configuration boundaries validate once, before writes.
pub fn validate_json(schema: &Value, value: &Value) -> crate::Result<()> {
    let validator = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(schema)
        .map_err(|_| crate::Error::InvalidInput("invalid JSON schema".into()))?;
    if let Some(error) = validator.iter_errors(value).next() {
        return Err(crate::Error::InvalidInput(format!(
            "JSON schema mismatch at {}",
            error.instance_path
        )));
    }
    Ok(())
}
