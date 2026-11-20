//! Shared Sink identity, configuration, and Agent Query execution.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{FromRow, Postgres, Transaction};
use uuid::Uuid;

use crate::{
    Core, Error, Job, JobInput, JobType, Result,
    agent::{Runtime, Tool, ToolFailure},
    ai::{ContentPart, FunctionTool, Message},
    worker::{JobContext, JobHandler},
};

pub const AGENT_QUERY: &str = "core.agent-query.v1";
pub const AGENT_QUERY_JOB: &str = "core.sink.agent-query.v1";
pub const MCP: &str = "core.mcp.v1";
const SUBMIT_QUERY_RESULT: &str = "submit_query_result";
const COLUMNS: &str = "id,type,nickname,config,enabled";

#[derive(Clone, Deserialize, Serialize, FromRow)]
pub struct Sink {
    pub id: i64,
    #[serde(rename = "type")]
    #[sqlx(rename = "type")]
    pub type_: String,
    pub nickname: Option<String>,
    pub config: Value,
    pub enabled: Vec<Uuid>,
}

#[derive(Clone, Deserialize, Serialize, FromRow)]
pub struct SinkType {
    pub id: String,
    pub description: String,
    pub config_schema: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SinkInput {
    #[serde(rename = "type")]
    pub type_: String,
    pub nickname: Option<String>,
    #[serde(default = "crate::models::empty_object")]
    pub config: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentQueryConfig {
    agent: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct McpConfig {
    pat: String,
}

fn agent_config(config: &Value) -> Result<AgentQueryConfig> {
    AgentQueryConfig::deserialize(config)
        .map_err(|_| Error::InvalidInput("invalid Agent Query Sink configuration".into()))
}

fn validate_local_config(type_: &str, config: &Value) -> Result<()> {
    match type_ {
        AGENT_QUERY => agent_config(config).map(|_| ()),
        MCP => {
            let cfg = McpConfig::deserialize(config)
                .map_err(|_| Error::InvalidInput("invalid MCP Sink configuration".into()))?;
            if cfg.pat.is_empty() {
                return Err(Error::InvalidInput(
                    "MCP bearer token must not be empty".into(),
                ));
            }
            Ok(())
        }
        _ => Err(Error::InvalidInput(
            "Sink type is unavailable on this Peer".into(),
        )),
    }
}

impl Core {
    pub async fn publish_sink_types(&self) -> Result<()> {
        let mut transaction = self.pool().begin().await?;
        for (id, description, schema) in [
            (
                AGENT_QUERY,
                "Answer information needs by composing registered info-base read tools.",
                json!({"additionalProperties":false,"properties":{"agent":{"title":"Agent","type":"integer"}},"required":["agent"],"title":"AgentQueryConfig","type":"object"}),
            ),
            (
                MCP,
                "Expose InKCre evidence and graph navigation through MCP.",
                json!({"additionalProperties":false,"properties":{"pat":{"minLength":1,"title":"Pat","type":"string"}},"required":["pat"],"title":"MCPSinkConfig","type":"object"}),
            ),
        ] {
            sqlx::query("INSERT INTO inkcre.sink_types(id,description,config_schema) VALUES ($1,$2,$3) ON CONFLICT(id) DO UPDATE SET description=excluded.description,config_schema=excluded.config_schema")
                .bind(id).bind(description).bind(schema).execute(&mut *transaction).await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn sink_types(&self) -> Result<Vec<SinkType>> {
        Ok(
            sqlx::query_as(
                "SELECT id,description,config_schema FROM inkcre.sink_types ORDER BY id",
            )
            .fetch_all(self.pool())
            .await?,
        )
    }

    pub async fn sink_type(&self, id: &str) -> Result<Option<SinkType>> {
        Ok(
            sqlx::query_as(
                "SELECT id,description,config_schema FROM inkcre.sink_types WHERE id=$1",
            )
            .bind(id)
            .fetch_optional(self.pool())
            .await?,
        )
    }

    pub async fn sinks(&self) -> Result<Vec<Sink>> {
        Ok(
            sqlx::query_as(&format!("SELECT {COLUMNS} FROM inkcre.sinks ORDER BY id"))
                .fetch_all(self.pool())
                .await?,
        )
    }

    pub async fn sink(&self, id: i64) -> Result<Option<Sink>> {
        Ok(
            sqlx::query_as(&format!("SELECT {COLUMNS} FROM inkcre.sinks WHERE id=$1"))
                .bind(id)
                .fetch_optional(self.pool())
                .await?,
        )
    }

    pub async fn sink_create(&self, input: SinkInput) -> Result<Sink> {
        validate_local_config(&input.type_, &input.config)?;
        Ok(sqlx::query_as(&format!(
            "INSERT INTO inkcre.sinks(type,nickname,config) VALUES ($1,$2,$3) RETURNING {COLUMNS}"
        ))
        .bind(input.type_)
        .bind(input.nickname)
        .bind(input.config)
        .fetch_one(self.pool())
        .await?)
    }

    pub async fn sink_replace_config(&self, id: i64, config: Value) -> Result<Sink> {
        let mut transaction = self.pool().begin().await?;
        let sink = sink_for_update(&mut transaction, id).await?;
        validate_local_config(&sink.type_, &config)?;
        let saved = sqlx::query_as(&format!(
            "UPDATE inkcre.sinks SET config=$2 WHERE id=$1 RETURNING {COLUMNS}"
        ))
        .bind(id)
        .bind(config)
        .fetch_one(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(saved)
    }

    pub async fn sink_enable(&self, id: i64, peer: Uuid, enabled: bool) -> Result<Sink> {
        let mut transaction = self.pool().begin().await?;
        let mut sink = sink_for_update(&mut transaction, id).await?;
        if enabled {
            validate_local_config(&sink.type_, &sink.config)?;
            if !sink.enabled.contains(&peer) {
                sink.enabled.push(peer);
            }
        } else {
            sink.enabled.retain(|member| *member != peer);
        }
        sqlx::query(
            "UPDATE inkcre.sinks SET enabled=$2 WHERE id=$1 AND enabled IS DISTINCT FROM $2",
        )
        .bind(id)
        .bind(&sink.enabled)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(sink)
    }

    pub async fn sink_delete(&self, id: i64) -> Result<()> {
        let mut transaction = self.pool().begin().await?;
        let sink = sink_for_update(&mut transaction, id).await?;
        if !sink.enabled.is_empty() {
            return Err(Error::Conflict(
                "disable the Sink on all Peers before deleting it".into(),
            ));
        }
        sqlx::query("DELETE FROM inkcre.sinks WHERE id=$1")
            .bind(id)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn sink_query(&self, id: i64, peer: Uuid, query: QueryInput) -> Result<Job> {
        let mut transaction = self.pool().begin().await?;
        let sink = sink_for_update(&mut transaction, id).await?;
        if sink.type_ != AGENT_QUERY || !sink.enabled.contains(&peer) {
            return Err(Error::NotFound("running Agent Query Sink"));
        }
        let job = Self::job_submit_in(
            &mut transaction,
            JobInput {
                type_: AGENT_QUERY_JOB.into(),
                parameters: json!({"sink":id,"query":query.query}),
                timeout_seconds: query.timeout_seconds,
                submission_traceparent: None,
                submission_tracestate: None,
            },
        )
        .await?;
        transaction.commit().await?;
        Ok(job)
    }
}

async fn sink_for_update(tx: &mut Transaction<'_, Postgres>, id: i64) -> Result<Sink> {
    sqlx::query_as(&format!(
        "SELECT {COLUMNS} FROM inkcre.sinks WHERE id=$1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::NotFound("Sink"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryInput {
    pub query: String,
    pub timeout_seconds: Option<i32>,
}

#[derive(Deserialize)]
struct AgentQueryParameters {
    sink: i64,
    query: String,
}

impl AgentQueryParameters {
    fn parse(value: &Value) -> Result<Self> {
        Self::deserialize(value)
            .map_err(|_| Error::InvalidInput("invalid Agent Query Job parameters".into()))
    }
}

pub(crate) async fn validate_job(
    tx: &mut Transaction<'_, Postgres>,
    kind: &str,
    parameters: &Value,
) -> Result<()> {
    if kind == AGENT_QUERY_JOB {
        let id = AgentQueryParameters::parse(parameters)?.sink;
        let type_: Option<String> =
            sqlx::query_scalar("SELECT type FROM inkcre.sinks WHERE id=$1 FOR SHARE")
                .bind(id)
                .fetch_optional(&mut **tx)
                .await?;
        if type_.as_deref() != Some(AGENT_QUERY) {
            return Err(Error::InvalidInput("expected an Agent Query Sink".into()));
        }
    }
    Ok(())
}

struct SubmitResult;

#[async_trait]
impl Tool for SubmitResult {
    fn definition(&self) -> FunctionTool {
        FunctionTool {
            id: SUBMIT_QUERY_RESULT.into(),
            description: "Submit the answer to this information need and its supporting entities.".into(),
            input_schema: json!({"type":"object","additionalProperties":false,"required":["answer"],"properties":{
                "answer":{"type":"string","minLength":1},
                "references":{"type":"array","default":[],"items":{
                    "type":"object","additionalProperties":false,"required":["type","id"],"properties":{
                        "type":{"enum":["block","relation"]},"id":{"type":"integer"}
                    }
                }}
            }}).as_object().cloned().unwrap_or_default(),
        }
    }

    async fn invoke(&self, mut input: Value) -> std::result::Result<Value, ToolFailure> {
        if input.get("references").is_none() {
            input["references"] = json!([]);
        }
        Ok(input)
    }
}

pub fn register_tools(runtime: &mut Runtime) -> Result<()> {
    runtime.register(Arc::new(SubmitResult))
}

pub struct AgentQueryHandler {
    pub agents: Runtime,
    pub peer: Uuid,
}

impl AgentQueryHandler {
    async fn agent(&self, core: &Core, sink_id: i64) -> Result<Option<i64>> {
        match core.sink(sink_id).await? {
            Some(sink) if sink.type_ == AGENT_QUERY && sink.enabled.contains(&self.peer) => {
                Ok(Some(agent_config(&sink.config)?.agent))
            }
            _ => Ok(None),
        }
    }
}

#[async_trait]
impl JobHandler for AgentQueryHandler {
    fn job_type(&self) -> JobType {
        JobType {
            id: AGENT_QUERY_JOB.into(),
            description: "Answer one information need through an enabled Agent Query Sink.".into(),
            parameters_schema: json!({"additionalProperties":false,"properties":{
                "query":{"minLength":1,"title":"Query","type":"string"},
                "sink":{"title":"Sink","type":"integer"}
            },"required":["sink","query"],"title":"AgentQueryJobParameters","type":"object"}),
            default_timeout_seconds: 300,
        }
    }

    async fn can_handle(&self, core: &Core, job: &Job) -> Result<bool> {
        let parameters = AgentQueryParameters::parse(&job.parameters)?;
        match self.agent(core, parameters.sink).await? {
            Some(id) => self.agents.can_execute(id, "text").await,
            None => Ok(false),
        }
    }

    async fn handle(&self, context: JobContext, job: Job) -> Result<()> {
        let parameters = AgentQueryParameters::parse(&job.parameters)?;
        let agent = self
            .agent(context.core(), parameters.sink)
            .await?
            .ok_or(Error::NotFound("running Agent Query Sink"))?;
        let thread = self.agents.create_thread(agent).await?;
        let user = Message::User {
            content: vec![ContentPart::Text {
                text: parameters.query,
            }],
        };
        let termination = tokio::select! {
            biased;
            _ = context.cancelled() => None,
            result = thread.turn(user) => Some(result),
        };
        let history = thread.messages().await;
        let mut result = None;
        for pair in history.windows(2) {
            if let (Message::Assistant { tool_calls, .. }, Message::ToolResult { results }) =
                (&pair[0], &pair[1])
            {
                for reply in results {
                    if !reply.is_error
                        && tool_calls.iter().any(|call| {
                            call.tool == SUBMIT_QUERY_RESULT && call.id == reply.tool_call_id
                        })
                    {
                        result = Some(&reply.content);
                    }
                }
            }
        }
        if let Some(answer) = result {
            context.patch_state(json!({"result":answer})).await?;
        }
        let Some(termination) = termination else {
            return Ok(());
        };
        context
            .patch_state(json!({"termination":termination?}))
            .await?;
        if result.is_none() {
            return Err(Error::Unavailable("Agent Query result submission"));
        }
        Ok(())
    }
}
