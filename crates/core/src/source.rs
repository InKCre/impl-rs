//! Source catalog、实例与本机执行能力。持久化准入不依赖本机 handler。

use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{FromRow, Postgres, Transaction};
use uuid::Uuid;

use crate::worker::{JobContext, JobHandler};
use crate::{Core, Error, Job, JobInput, JobType, Page, Result, models::validate_json};

pub const SOURCE_COLLECT_JOB_TYPE: &str = "core.source.collect.v1";
pub const SOURCE_BACKFILL_JOB_TYPE: &str = "core.source.backfill.v1";
pub const SOURCE_RESOLVER_ID: &str = "core.source.v1";

#[derive(Clone, Debug, Serialize, Deserialize, FromRow)]
pub struct SourceType {
    pub id: String,
    pub description: Option<String>,
    pub config_schema: Value,
    pub collect_config_schema: Value,
    pub backfill_config_schema: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, FromRow)]
pub struct Source {
    pub id: i32,
    #[serde(rename = "type")]
    #[sqlx(rename = "type")]
    pub type_: String,
    pub nickname: Option<String>,
    pub config: Value,
    pub storage: Option<i32>,
    pub block: Option<i32>,
    pub state: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceInput {
    #[serde(rename = "type")]
    pub type_: String,
    pub nickname: Option<String>,
    #[serde(default = "crate::models::empty_object")]
    pub config: Value,
    pub storage: Option<i32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourcePatch {
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "Option::is_none"
    )]
    pub nickname: Option<Option<String>>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub config: Option<Value>,
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "Option::is_none"
    )]
    pub storage: Option<Option<i32>>,
}

fn nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Option<Value>, D::Error> {
    let value = Value::deserialize(d)?;
    if value.is_null() {
        return Err(serde::de::Error::custom("config must be an object"));
    }
    Ok(Some(value))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceJobParameters {
    pub source: i32,
    #[serde(default = "crate::models::empty_object")]
    pub config: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceCommand {
    Collect,
    Backfill,
}

impl SourceCommand {
    pub fn job_type(self) -> &'static str {
        match self {
            Self::Collect => SOURCE_COLLECT_JOB_TYPE,
            Self::Backfill => SOURCE_BACKFILL_JOB_TYPE,
        }
    }
}

fn validate_schema(schema: &Value) -> Result<()> {
    if !schema.is_object() {
        return Err(Error::InvalidInput(
            "Source JSON schema must be an object".into(),
        ));
    }
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .build(schema)
        .map_err(|_| Error::InvalidInput("invalid Source JSON schema".into()))?;
    Ok(())
}

fn validate_type(descriptor: &SourceType) -> Result<()> {
    if descriptor.id.trim().is_empty() {
        return Err(Error::InvalidInput(
            "Source type identity is required".into(),
        ));
    }
    validate_schema(&descriptor.config_schema)?;
    validate_schema(&descriptor.collect_config_schema)?;
    if let Some(schema) = &descriptor.backfill_config_schema {
        validate_schema(schema)?;
    }
    Ok(())
}

fn validate_config(
    descriptor: &SourceType,
    value: &Value,
    command: Option<SourceCommand>,
) -> Result<()> {
    if !value.is_object() {
        return Err(Error::InvalidInput(
            "Source configuration must be an object".into(),
        ));
    }
    let schema = match command {
        None => &descriptor.config_schema,
        Some(SourceCommand::Collect) => &descriptor.collect_config_schema,
        Some(SourceCommand::Backfill) => descriptor
            .backfill_config_schema
            .as_ref()
            .ok_or_else(|| Error::Unsupported("Source type does not support backfill".into()))?,
    };
    validate_json(schema, value)
}

const SOURCE_COLUMNS: &str = "id,type,nickname,config,storage,block,state,created_at,updated_at";
const TYPE_COLUMNS: &str =
    "id,description,config_schema,collect_config_schema,backfill_config_schema";

impl Core {
    pub async fn source_type_get(&self, id: &str) -> Result<Option<SourceType>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {TYPE_COLUMNS} FROM inkcre.sources_types WHERE id=$1"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await?)
    }

    pub async fn source_types(
        &self,
        limit: i64,
        cursor: Option<&str>,
    ) -> Result<Page<SourceType, String>> {
        crate::graph::page_limit(limit)?;
        let mut items: Vec<SourceType> = sqlx::query_as(&format!("SELECT {TYPE_COLUMNS} FROM inkcre.sources_types WHERE ($1::text IS NULL OR id>$1) ORDER BY id LIMIT $2"))
            .bind(cursor).bind(limit + 1).fetch_all(self.pool()).await?;
        let next_cursor = if items.len() > limit as usize {
            items.truncate(limit as usize);
            items.last().map(|x| x.id.clone())
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }

    pub async fn source_type_upsert(&self, descriptor: &SourceType) -> Result<()> {
        validate_type(descriptor)?;
        sqlx::query("INSERT INTO inkcre.sources_types(id,description,config_schema,collect_config_schema,backfill_config_schema) VALUES($1,$2,$3,$4,$5) ON CONFLICT(id) DO UPDATE SET description=EXCLUDED.description,config_schema=EXCLUDED.config_schema,collect_config_schema=EXCLUDED.collect_config_schema,backfill_config_schema=EXCLUDED.backfill_config_schema")
            .bind(&descriptor.id).bind(&descriptor.description).bind(&descriptor.config_schema)
            .bind(&descriptor.collect_config_schema).bind(&descriptor.backfill_config_schema).execute(self.pool()).await?;
        Ok(())
    }

    pub async fn source_get(&self, id: i32) -> Result<Option<Source>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {SOURCE_COLUMNS} FROM inkcre.sources WHERE id=$1"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await?)
    }

    pub async fn sources(&self, limit: i64, cursor: Option<i32>) -> Result<Page<Source, i32>> {
        crate::graph::page_limit(limit)?;
        let mut items: Vec<Source> = sqlx::query_as(&format!("SELECT {SOURCE_COLUMNS} FROM inkcre.sources WHERE ($1::integer IS NULL OR id<$1) ORDER BY id DESC LIMIT $2"))
            .bind(cursor).bind(limit + 1).fetch_all(self.pool()).await?;
        let next_cursor = if items.len() > limit as usize {
            items.truncate(limit as usize);
            items.last().map(|x| x.id)
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }

    pub async fn source_create(&self, input: SourceInput) -> Result<Source> {
        let mut tx = self.pool().begin().await?;
        let descriptor: SourceType = sqlx::query_as(&format!(
            "SELECT {TYPE_COLUMNS} FROM inkcre.sources_types WHERE id=$1 FOR SHARE"
        ))
        .bind(&input.type_)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound("Source type"))?;
        validate_config(&descriptor, &input.config, None)?;
        let source = sqlx::query_as(&format!("INSERT INTO inkcre.sources(type,nickname,config,storage) VALUES($1,$2,$3,$4) RETURNING {SOURCE_COLUMNS}"))
            .bind(input.type_).bind(input.nickname).bind(input.config).bind(input.storage).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(source)
    }

    pub async fn source_patch(&self, id: i32, patch: SourcePatch) -> Result<Source> {
        let mut tx = self.pool().begin().await?;
        let current: Source = sqlx::query_as(&format!(
            "SELECT {SOURCE_COLUMNS} FROM inkcre.sources WHERE id=$1 FOR UPDATE"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound("Source"))?;
        if let Some(config) = &patch.config {
            let descriptor: SourceType = sqlx::query_as(&format!(
                "SELECT {TYPE_COLUMNS} FROM inkcre.sources_types WHERE id=$1 FOR SHARE"
            ))
            .bind(&current.type_)
            .fetch_one(&mut *tx)
            .await?;
            validate_config(&descriptor, config, None)?;
        }
        let updated: Source = sqlx::query_as(&format!("UPDATE inkcre.sources SET nickname=CASE WHEN $2 THEN $3 ELSE nickname END,config=COALESCE($4,config),storage=CASE WHEN $5 THEN $6 ELSE storage END WHERE id=$1 RETURNING {SOURCE_COLUMNS}"))
            .bind(id).bind(patch.nickname.is_some()).bind(patch.nickname.flatten()).bind(patch.config)
            .bind(patch.storage.is_some()).bind(patch.storage.flatten()).fetch_one(&mut *tx).await?;
        if updated.nickname != current.nickname && updated.block.is_some() {
            ensure_source_anchor_in(&mut tx, &updated).await?;
        }
        tx.commit().await?;
        Ok(updated)
    }

    /// 删除配置，不级联删除已采集的图、来源锚点或已提交命令。
    pub async fn source_delete(&self, id: i32) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM inkcre.sources WHERE id=$1")
            .bind(id)
            .execute(self.pool())
            .await?
            .rows_affected()
            > 0)
    }

    /// 只合并调用者拥有的顶层 state key，不以启动快照替换其他参与者的新状态。
    pub async fn source_patch_state(&self, id: i32, patch: &Value) -> Result<()> {
        if !patch.is_object() {
            return Err(Error::InvalidInput(
                "Source state patch must be an object".into(),
            ));
        }
        if sqlx::query("UPDATE inkcre.sources SET state=state || $2 WHERE id=$1")
            .bind(id)
            .bind(patch)
            .execute(self.pool())
            .await?
            .rows_affected()
            == 0
        {
            return Err(Error::NotFound("Source"));
        }
        Ok(())
    }

    pub async fn source_ensure_anchor(&self, id: i32) -> Result<crate::Block> {
        let mut tx = self.pool().begin().await?;
        let source: Source = sqlx::query_as(&format!(
            "SELECT {SOURCE_COLUMNS} FROM inkcre.sources WHERE id=$1 FOR UPDATE"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound("Source"))?;
        let block = ensure_source_anchor_in(&mut tx, &source).await?;
        tx.commit().await?;
        Ok(block)
    }

    pub async fn source_submit(
        &self,
        id: i32,
        command: SourceCommand,
        config: Value,
        timeout_seconds: Option<i32>,
    ) -> Result<Job> {
        self.job_submit(JobInput {
            type_: command.job_type().into(),
            parameters: json!({"source": id, "config": config}),
            timeout_seconds,
            submission_traceparent: None,
            submission_tracestate: None,
        })
        .await
    }
}

async fn ensure_source_anchor_in(
    tx: &mut Transaction<'_, Postgres>,
    source: &Source,
) -> Result<crate::Block> {
    let content =
        json!({"id": source.id,"type":source.type_,"nickname":source.nickname}).to_string();
    let block = match source.block {
        Some(id) => sqlx::query_as("UPDATE inkcre.blocks SET resolver=$2,content=$3,storage=NULL WHERE id=$1 RETURNING id,resolver,content,storage,created_at,updated_at")
            .bind(id).bind(SOURCE_RESOLVER_ID).bind(content).fetch_optional(&mut **tx).await?.ok_or(Error::NotFound("Source anchor"))?,
        None => {
            let block: crate::Block = sqlx::query_as("INSERT INTO inkcre.blocks(resolver,content) VALUES($1,$2) RETURNING id,resolver,content,storage,created_at,updated_at")
                .bind(SOURCE_RESOLVER_ID).bind(content).fetch_one(&mut **tx).await?;
            sqlx::query("UPDATE inkcre.sources SET block=$2 WHERE id=$1").bind(source.id).bind(block.id).execute(&mut **tx).await?;
            block
        }
    };
    Ok(block)
}

/// Job 与 Cron 共用的二层接受边界；持久 catalog 是事实，本机是否能执行另行判断。
pub(crate) async fn validate_source_job_in(
    tx: &mut Transaction<'_, Postgres>,
    job_type: &str,
    parameters: &Value,
) -> Result<()> {
    let command = match job_type {
        SOURCE_COLLECT_JOB_TYPE => SourceCommand::Collect,
        SOURCE_BACKFILL_JOB_TYPE => SourceCommand::Backfill,
        _ => return Ok(()),
    };
    let parameters: SourceJobParameters = serde_json::from_value(parameters.clone())
        .map_err(|_| Error::InvalidInput("invalid Source Job parameters".into()))?;
    let descriptor: SourceType = sqlx::query_as("SELECT t.id,t.description,t.config_schema,t.collect_config_schema,t.backfill_config_schema FROM inkcre.sources s JOIN inkcre.sources_types t ON t.id=s.type WHERE s.id=$1 FOR SHARE OF s,t")
        .bind(parameters.source).fetch_optional(&mut **tx).await?.ok_or(Error::NotFound("Source"))?;
    validate_config(&descriptor, &parameters.config, Some(command))
}

#[async_trait]
pub trait SourceHandler: Send + Sync {
    async fn can_handle(
        &self,
        _core: &Core,
        _source: &Source,
        _command: SourceCommand,
    ) -> Result<bool> {
        Ok(true)
    }
    async fn collect(&self, context: JobContext, source: Source, config: Value) -> Result<()>;
    async fn backfill(&self, _context: JobContext, _source: Source, _config: Value) -> Result<()> {
        Err(Error::Unsupported(
            "Source handler does not support backfill".into(),
        ))
    }
}

#[derive(Clone)]
pub struct RegisteredSource {
    pub owner: Uuid,
    pub descriptor: SourceType,
    pub handler: Arc<dyn SourceHandler>,
}

#[derive(Clone, Default)]
pub struct SourceRegistry {
    inner: Arc<RwLock<BTreeMap<String, RegisteredSource>>>,
}

impl SourceRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(
        &self,
        owner: Uuid,
        descriptor: SourceType,
        handler: Arc<dyn SourceHandler>,
    ) -> Result<()> {
        validate_type(&descriptor)?;
        let mut registry = self
            .inner
            .write()
            .map_err(|_| Error::Conflict("Source registry lock failed".into()))?;
        if registry
            .get(&descriptor.id)
            .is_some_and(|current| current.owner != owner)
        {
            return Err(Error::Conflict(
                "Source type is already registered by another runtime".into(),
            ));
        }
        registry.insert(
            descriptor.id.clone(),
            RegisteredSource {
                owner,
                descriptor,
                handler,
            },
        );
        Ok(())
    }
    pub fn unregister_owner(&self, owner: Uuid) -> Result<usize> {
        let mut registry = self
            .inner
            .write()
            .map_err(|_| Error::Conflict("Source registry lock failed".into()))?;
        let previous = registry.len();
        registry.retain(|_, current| current.owner != owner);
        Ok(previous - registry.len())
    }
    pub fn get(&self, id: &str) -> Result<Option<RegisteredSource>> {
        Ok(self
            .inner
            .read()
            .map_err(|_| Error::Conflict("Source registry lock failed".into()))?
            .get(id)
            .cloned())
    }
    pub fn descriptors(&self) -> Result<Vec<SourceType>> {
        Ok(self
            .inner
            .read()
            .map_err(|_| Error::Conflict("Source registry lock failed".into()))?
            .values()
            .map(|entry| entry.descriptor.clone())
            .collect())
    }
}

#[derive(Clone)]
pub struct SourceRuntime {
    core: Core,
    registry: SourceRegistry,
}

impl SourceRuntime {
    pub fn new(core: Core) -> Self {
        Self {
            core,
            registry: SourceRegistry::new(),
        }
    }
    pub fn with_registry(core: Core, registry: SourceRegistry) -> Self {
        Self { core, registry }
    }
    pub fn registry(&self) -> &SourceRegistry {
        &self.registry
    }
    pub fn core(&self) -> &Core {
        &self.core
    }
    pub async fn sync_catalog(&self) -> Result<()> {
        for descriptor in self.registry.descriptors()? {
            self.core.source_type_upsert(&descriptor).await?;
        }
        for handler in self.job_handlers() {
            self.core.job_type_upsert(&handler.job_type()).await?;
        }
        Ok(())
    }
    pub fn job_handlers(&self) -> Vec<Arc<dyn JobHandler>> {
        [SourceCommand::Collect, SourceCommand::Backfill]
            .into_iter()
            .map(|command| {
                Arc::new(SourceJobHandler {
                    registry: self.registry.clone(),
                    command,
                }) as Arc<dyn JobHandler>
            })
            .collect()
    }
}

struct SourceJobHandler {
    registry: SourceRegistry,
    command: SourceCommand,
}

#[async_trait]
impl JobHandler for SourceJobHandler {
    fn job_type(&self) -> JobType {
        let (title, description, timeout) = match self.command {
            SourceCommand::Collect => (
                "SourceCollectParameters",
                "Parameters of the exact ordinary Source Job type.",
                300,
            ),
            SourceCommand::Backfill => (
                "SourceBackfillParameters",
                "Parameters of the exact historical Source Job type.",
                1800,
            ),
        };
        JobType {
            id: self.command.job_type().into(),
            description: match self.command {
                SourceCommand::Collect => {
                    "Run one ordinary collection command for a configured Source."
                }
                SourceCommand::Backfill => {
                    "Run one exact historical collection command for a configured Source."
                }
            }
            .into(),
            parameters_schema: json!({"additionalProperties":false,"description":description,"properties":{"config":{"additionalProperties":true,"title":"Config","type":"object"},"source":{"title":"Source","type":"integer"}},"required":["source"],"title":title,"type":"object"}),
            default_timeout_seconds: timeout,
        }
    }
    async fn can_handle(&self, core: &Core, job: &Job) -> Result<bool> {
        let parameters: SourceJobParameters = serde_json::from_value(job.parameters.clone())
            .map_err(|_| Error::InvalidInput("invalid persisted Source Job parameters".into()))?;
        let Some(source) = core.source_get(parameters.source).await? else {
            return Ok(false);
        };
        let Some(registration) = self.registry.get(&source.type_)? else {
            return Ok(false);
        };
        if self.command == SourceCommand::Backfill
            && registration.descriptor.backfill_config_schema.is_none()
        {
            return Ok(false);
        }
        registration
            .handler
            .can_handle(core, &source, self.command)
            .await
    }
    async fn handle(&self, context: JobContext, job: Job) -> Result<()> {
        let parameters: SourceJobParameters = serde_json::from_value(job.parameters)
            .map_err(|_| Error::InvalidInput("invalid persisted Source Job parameters".into()))?;
        let source = context
            .core()
            .source_get(parameters.source)
            .await?
            .ok_or(Error::NotFound("Source"))?;
        let Some(registration) = self.registry.get(&source.type_)? else {
            context.cancel();
            return Ok(());
        };
        if !registration
            .handler
            .can_handle(context.core(), &source, self.command)
            .await?
        {
            context.cancel();
            return Ok(());
        }
        match self.command {
            SourceCommand::Collect => {
                registration
                    .handler
                    .collect(context, source, parameters.config)
                    .await
            }
            SourceCommand::Backfill => {
                registration
                    .handler
                    .backfill(context, source, parameters.config)
                    .await
            }
        }
    }
}

pub struct SourceAnchorResolver;

#[async_trait]
impl crate::knowledge::Resolver for SourceAnchorResolver {
    fn descriptor(&self) -> crate::knowledge::ResolverDescriptor {
        crate::knowledge::ResolverDescriptor {
            id: SOURCE_RESOLVER_ID.into(),
            description: "Use-time projection for lazy Source anchor Blocks.".into(),
            read_methods: vec![],
            draft: None,
        }
    }
    fn validate_content(&self, content: &crate::HydratedContent) -> crate::knowledge::Result<()> {
        let value = source_anchor_content(content)?;
        let valid = value.get("id").is_some_and(Value::is_i64)
            && value.get("type").is_some_and(Value::is_string)
            && value
                .get("nickname")
                .is_none_or(|value| value.is_string() || value.is_null());
        if !valid {
            return Err(crate::knowledge::Error::InvalidInput(
                "invalid Source anchor content",
            ));
        }
        Ok(())
    }
    async fn solve(
        &self,
        context: &crate::knowledge::ResolverContext,
        options: crate::knowledge::ReadOptions,
    ) -> crate::knowledge::Result<Value> {
        source_anchor_content(&context.actual_content(options.refresh).await?)
    }
    async fn text(
        &self,
        context: &crate::knowledge::ResolverContext,
        options: crate::knowledge::TextOptions,
    ) -> crate::knowledge::Result<Option<String>> {
        let source = source_anchor_content(&context.actual_content(options.refresh).await?)?;
        let id = source.get("id").and_then(Value::as_i64).ok_or(
            crate::knowledge::Error::InvalidInput("Source anchor has no identity"),
        )?;
        let type_id = source.get("type").and_then(Value::as_str).ok_or(
            crate::knowledge::Error::InvalidInput("Source anchor has no type"),
        )?;
        let name = source
            .get("nickname")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(|name| format!(" named {name}"))
            .unwrap_or_default();
        Ok(Some(format!("Source {id}{name} uses {type_id}.")))
    }
    async fn label(
        &self,
        context: &crate::knowledge::ResolverContext,
        refresh: bool,
    ) -> crate::knowledge::Result<String> {
        let source = source_anchor_content(&context.actual_content(refresh).await?)?;
        let identifier = source
            .get("nickname")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .or_else(|| source.get("type").and_then(Value::as_str));
        Ok(crate::knowledge::format_label("source", identifier, false))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceAnchorContent {
    id: i32,
    #[serde(rename = "type")]
    type_: String,
    nickname: Option<String>,
}

fn source_anchor_content(content: &crate::HydratedContent) -> crate::knowledge::Result<Value> {
    let text = match content {
        crate::HydratedContent::Text(text) => text.clone(),
        crate::HydratedContent::Bytes(bytes) => crate::knowledge::decode_unicode_bytes(bytes)?,
    };
    let content: SourceAnchorContent =
        serde_json::from_str(&text).map_err(|_| crate::knowledge::Error::InvalidContent {
            resolver: SOURCE_RESOLVER_ID.into(),
            reason: "invalid JSON encoding",
        })?;
    serde_json::to_value(content)
        .map_err(|_| crate::knowledge::Error::InvalidInput("Source anchor cannot be represented"))
}
