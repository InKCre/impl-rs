//! 共享模型目录与本机 AI 执行。调用者拥有历史、工具执行和图数据。

mod openai;
mod types;

pub use types::*;

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use thiserror::Error;

use crate::{Core, Page};

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Core(#[from] crate::Error),
    #[error("invalid AI input: {0}")]
    Invalid(&'static str),
    #[error("AI model not found")]
    ModelNotFound,
    #[error("AI model or provider is disabled")]
    Disabled,
    #[error("AI dialect is unavailable on this Peer")]
    UnknownDialect,
    #[error("persisted AI provider configuration is invalid")]
    InvalidProvider,
    #[error("model and adapter cannot execute the requested AI capability")]
    Unsupported,
    #[error("AI provider request failed")]
    Provider,
    #[error("invalid AI provider output: {0}")]
    Output(&'static str),
}

impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        Self::Core(error.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// An exact dialect binding. Provider configuration is local to the execution
/// call and can contain credentials. Implementations must not log its values.
#[async_trait]
pub trait Dialect: Send + Sync {
    fn validate_config(&self, config: &Value) -> Result<()>;
    fn supports(&self, requirement: &Requirement) -> bool;

    async fn embed(
        &self,
        config: &Value,
        model: &str,
        inputs: &[String],
        dimensions: usize,
    ) -> Result<Vec<Vec<f32>>>;

    async fn chat(
        &self,
        config: &Value,
        model: &str,
        messages: &[Message],
        tools: &[FunctionTool],
        choice: Option<&ToolChoice>,
    ) -> Result<AssistantMessage>;
}

#[derive(sqlx::FromRow)]
struct ModelRow {
    id: i64,
    provider: i64,
    native_model_id: String,
    name: Option<String>,
    capabilities: sqlx::types::Json<Vec<Capability>>,
    enabled: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<ModelRow> for Model {
    fn from(row: ModelRow) -> Self {
        Self {
            id: row.id,
            provider: row.provider,
            native_model_id: row.native_model_id,
            name: row.name,
            capabilities: row.capabilities.0,
            enabled: row.enabled,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

const MODEL_COLUMNS: &str =
    "id,provider,native_model_id,name,capabilities,enabled,created_at,updated_at";

impl Core {
    /// Model discovery traverses shared facts without needing a local dialect.
    pub async fn ai_model(&self, id: i64) -> Result<Option<Model>> {
        let row: Option<ModelRow> = sqlx::query_as(&format!(
            "SELECT {MODEL_COLUMNS} FROM inkcre.ai_models WHERE id=$1"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(Into::into))
    }

    pub async fn ai_models(
        &self,
        limit: Option<i64>,
        cursor: Option<i64>,
    ) -> Result<Page<Model, i64>> {
        if limit.is_some_and(|limit| limit <= 0 || limit == i64::MAX) {
            return Err(Error::Invalid("invalid model page limit"));
        }
        let mut rows: Vec<ModelRow> = sqlx::query_as(&format!(
            "SELECT {MODEL_COLUMNS} FROM inkcre.ai_models \
             WHERE ($1::bigint IS NULL OR id>$1) ORDER BY id LIMIT $2"
        ))
        .bind(cursor)
        .bind(limit.map(|limit| limit + 1))
        .fetch_all(self.pool())
        .await?;
        let next_cursor = if limit.is_some_and(|limit| rows.len() as i64 > limit) {
            rows.pop();
            rows.last().map(|row| row.id)
        } else {
            None
        };
        Ok(Page {
            items: rows.into_iter().map(Into::into).collect(),
            next_cursor,
        })
    }
}

#[derive(sqlx::FromRow)]
struct ExecutionRow {
    native_model_id: String,
    model_enabled: bool,
    capabilities: sqlx::types::Json<Vec<Capability>>,
    dialect: String,
    provider_enabled: bool,
    config: Value,
}

#[derive(Clone)]
pub struct Ai {
    core: Core,
    dialects: BTreeMap<String, Arc<dyn Dialect>>,
}

impl Ai {
    pub fn new(core: Core) -> Result<Self> {
        let client = openai::client()?;
        Ok(Self {
            core,
            dialects: BTreeMap::from([
                (
                    "core.openai-compatible.v1".into(),
                    Arc::new(openai::OpenAi::new(client.clone(), false)) as Arc<dyn Dialect>,
                ),
                (
                    "core.alibaba-model-studio.v1".into(),
                    Arc::new(openai::OpenAi::new(client, true)) as Arc<dyn Dialect>,
                ),
            ]),
        })
    }

    /// Add a local implementation of an exact dialect present in the shared catalog.
    pub fn register(&mut self, id: String, dialect: Arc<dyn Dialect>) -> Result<()> {
        use std::collections::btree_map::Entry;
        if id.trim().is_empty() {
            return Err(Error::Invalid("AI dialect ID is required"));
        }
        match self.dialects.entry(id) {
            Entry::Occupied(_) => Err(Error::Invalid("AI dialect already registered")),
            Entry::Vacant(entry) => {
                entry.insert(dialect);
                Ok(())
            }
        }
    }

    async fn target(
        &self,
        id: i64,
        requirement: &Requirement,
    ) -> Result<(ExecutionRow, &dyn Dialect)> {
        // One read obtains a consistent model/provider pair and returns its
        // connection before any adapter opens a network request.
        let row: ExecutionRow = sqlx::query_as(
            "SELECT m.native_model_id,m.enabled AS model_enabled,m.capabilities,\
             p.dialect,p.enabled AS provider_enabled,p.config FROM inkcre.ai_models m \
             JOIN inkcre.ai_providers p ON p.id=m.provider WHERE m.id=$1",
        )
        .bind(id)
        .fetch_optional(self.core.pool())
        .await?
        .ok_or(Error::ModelNotFound)?;
        if !row.model_enabled || !row.provider_enabled {
            return Err(Error::Disabled);
        }
        let adapter = self
            .dialects
            .get(&row.dialect)
            .ok_or(Error::UnknownDialect)?
            .as_ref();
        adapter.validate_config(&row.config)?;
        if !row.capabilities.iter().any(|capability| {
            capability.kind == requirement.kind
                && requirement
                    .input_modalities
                    .is_subset(&capability.input_modalities)
                && requirement
                    .output_modalities
                    .is_subset(&capability.output_modalities)
                && requirement.features.is_subset(&capability.features)
        }) || !adapter.supports(requirement)
        {
            return Err(Error::Unsupported);
        }
        Ok((row, adapter))
    }

    /// Static eligibility, without a provider probe. Infrastructure errors are
    /// distinguishable from a Peer that simply lacks execution capability.
    pub async fn can_execute(&self, model: i64, requirement: &Requirement) -> Result<bool> {
        match self.target(model, requirement).await {
            Ok(_) => Ok(true),
            Err(
                Error::ModelNotFound
                | Error::Disabled
                | Error::UnknownDialect
                | Error::InvalidProvider
                | Error::Unsupported,
            ) => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub async fn embed(
        &self,
        model: i64,
        inputs: &[String],
        dimensions: usize,
    ) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() || dimensions == 0 {
            return Err(Error::Invalid(
                "embedding requires a non-empty batch and positive dimensions",
            ));
        }
        let (row, adapter) = self.target(model, &Requirement::embedding()).await?;
        let vectors = adapter
            .embed(&row.config, &row.native_model_id, inputs, dimensions)
            .await?;
        if vectors.len() != inputs.len()
            || vectors.iter().any(|vector| {
                vector.len() != dimensions
                    || !vector.iter().all(|value| value.is_finite())
                    || !vector.iter().any(|value| *value != 0.0)
            })
        {
            return Err(Error::Output(
                "embedding count, dimension or value mismatch",
            ));
        }
        Ok(vectors)
    }

    pub async fn chat(
        &self,
        model: i64,
        messages: &[Message],
        tools: &[FunctionTool],
        choice: Option<&ToolChoice>,
    ) -> Result<AssistantMessage> {
        let requirement = types::chat_requirement(messages, tools, choice)?;
        let (row, adapter) = self.target(model, &requirement).await?;
        let response = adapter
            .chat(&row.config, &row.native_model_id, messages, tools, choice)
            .await?;
        AssistantMessage::new(response.content, response.tool_calls)
    }
}
