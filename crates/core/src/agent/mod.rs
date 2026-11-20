mod component_tool;
mod persistence;
pub use persistence::{InMemoryThreadPersistence, ThreadPersistence, ThreadState};
mod graph_tools;
mod resolver_tool;
mod tools;
pub use tools::register_builtin_tools;

use crate::{
    Core, Error, Page, Result,
    ai::{self, Ai, FunctionTool, Message, ToolCall, ToolChoice, ToolResult},
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(schemars::JsonSchema, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Form {
    pub name: String,
    pub system_prompt: String,
    pub model: i64,
    #[serde(default)]
    pub tools: Vec<String>,
    pub tool_choice: Option<ToolChoice>,
    #[schemars(range(min = 1))]
    pub max_model_calls_per_turn: i32,
}
impl Form {
    fn validate(&mut self) -> Result<()> {
        let unique: BTreeSet<_> = self.tools.iter().collect();
        if self.max_model_calls_per_turn <= 0
            || unique.len() != self.tools.len()
            || self.tools.iter().any(String::is_empty)
        {
            return Err(Error::InvalidInput(
                "invalid Agent budget or tool IDs".into(),
            ));
        }
        self.tools.sort();
        Ok(())
    }
}
#[derive(Clone, Deserialize, Serialize, sqlx::FromRow)]
pub struct Definition {
    pub id: i64,
    pub name: String,
    pub system_prompt: String,
    pub model: i64,
    pub tools: Vec<String>,
    pub tool_choice: Option<sqlx::types::Json<ToolChoice>>,
    pub max_model_calls_per_turn: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
const COLUMNS: &str =
    "id,name,system_prompt,model,tools,tool_choice,max_model_calls_per_turn,created_at,updated_at";
impl Core {
    pub async fn agent_get(&self, id: i64) -> Result<Option<Definition>> {
        Ok(
            sqlx::query_as(&format!("SELECT {COLUMNS} FROM inkcre.agents WHERE id=$1"))
                .bind(id)
                .fetch_optional(self.pool())
                .await?,
        )
    }
    pub async fn agents(
        &self,
        limit: Option<i64>,
        cursor: Option<i64>,
    ) -> Result<Page<Definition, i64>> {
        if limit.is_some_and(|n| n <= 0 || n == i64::MAX) {
            return Err(Error::InvalidInput("invalid Agent page limit".into()));
        }
        let mut items:Vec<Definition>=sqlx::query_as(&format!("SELECT {COLUMNS} FROM inkcre.agents WHERE ($1::bigint IS NULL OR id>$1) ORDER BY id LIMIT $2"))
   .bind(cursor).bind(limit.map(|n|n+1)).fetch_all(self.pool()).await?;
        let next_cursor = if limit.is_some_and(|n| items.len() as i64 > n) {
            items.pop();
            items.last().map(|r| r.id)
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }
    pub async fn agent_create(&self, mut form: Form) -> Result<Definition> {
        form.validate()?;
        Ok(sqlx::query_as(&format!("INSERT INTO inkcre.agents(name,system_prompt,model,tools,tool_choice,max_model_calls_per_turn) VALUES($1,$2,$3,$4,$5,$6) RETURNING {COLUMNS}"))
   .bind(form.name).bind(form.system_prompt).bind(form.model).bind(form.tools).bind(form.tool_choice.map(sqlx::types::Json)).bind(form.max_model_calls_per_turn).fetch_one(self.pool()).await?)
    }
    pub async fn agent_patch(&self, id: i64, patch: Value) -> Result<Definition> {
        let patch = patch
            .as_object()
            .ok_or_else(|| Error::InvalidInput("Agent patch must be an object".into()))?;
        let mut tx = self.pool().begin().await?;
        let old: Definition = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM inkcre.agents WHERE id=$1 FOR UPDATE"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound("Agent"))?;
        let mut value = json!({"name":old.name,"system_prompt":old.system_prompt,"model":old.model,"tools":old.tools,"tool_choice":old.tool_choice,"max_model_calls_per_turn":old.max_model_calls_per_turn});
        for (key, next) in patch {
            value[key] = next.clone();
        }
        let mut form: Form = serde_json::from_value(value)
            .map_err(|_| Error::InvalidInput("invalid Agent patch".into()))?;
        form.validate()?;
        let saved=sqlx::query_as(&format!("UPDATE inkcre.agents SET name=$2,system_prompt=$3,model=$4,tools=$5,tool_choice=$6,max_model_calls_per_turn=$7 WHERE id=$1 RETURNING {COLUMNS}"))
   .bind(id).bind(form.name).bind(form.system_prompt).bind(form.model).bind(form.tools).bind(form.tool_choice.map(sqlx::types::Json)).bind(form.max_model_calls_per_turn).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(saved)
    }
    pub async fn agent_delete(&self, id: i64) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM inkcre.agents WHERE id=$1")
            .bind(id)
            .execute(self.pool())
            .await?
            .rows_affected()
            > 0)
    }
}

pub enum ToolFailure {
    Actionable(Value),
    Failed,
}
#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> FunctionTool;
    /// Bind current capability schemas and handler state for one Thread snapshot.
    fn snapshot(&self) -> Option<Arc<dyn Tool>> {
        None
    }
    async fn invoke(&self, input: Value) -> std::result::Result<Value, ToolFailure>;
}
#[derive(Clone)]
struct BoundTool {
    definition: FunctionTool,
    handler: Arc<dyn Tool>,
}
impl BoundTool {
    fn snapshot(&self) -> Result<Self> {
        let Some(handler) = self.handler.snapshot() else {
            return Ok(self.clone());
        };
        let definition = handler.definition();
        if definition.id != self.definition.id {
            return Err(Error::InvalidInput(
                "bound Agent tool changed its ID".into(),
            ));
        }
        jsonschema::validator_for(&Value::Object(definition.input_schema.clone()))
            .map_err(|_| Error::InvalidInput("invalid bound Agent tool schema".into()))?;
        Ok(Self {
            definition,
            handler,
        })
    }
}
#[derive(Clone)]
pub struct Runtime {
    core: Core,
    tools: BTreeMap<String, BoundTool>,
    persistence: Arc<dyn ThreadPersistence>,
}
impl Runtime {
    pub fn new(core: Core) -> Self {
        Self {
            core,
            tools: BTreeMap::new(),
            persistence: Arc::new(InMemoryThreadPersistence::default()),
        }
    }
    pub fn with_persistence(mut self, persistence: Arc<dyn ThreadPersistence>) -> Self {
        self.persistence = persistence;
        self
    }
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<()> {
        let definition = tool.definition();
        if definition.id.is_empty() || self.tools.contains_key(&definition.id) {
            return Err(Error::Conflict(
                "Agent tool ID already registered or empty".into(),
            ));
        }
        jsonschema::validator_for(&Value::Object(definition.input_schema.clone()))
            .map_err(|_| Error::InvalidInput("invalid Agent tool schema".into()))?;
        self.tools.insert(
            definition.id.clone(),
            BoundTool {
                definition,
                handler: tool,
            },
        );
        Ok(())
    }
    pub fn tool(&self, id: &str) -> Result<FunctionTool> {
        self.tools
            .get(id)
            .ok_or(Error::NotFound("Agent tool"))?
            .snapshot()
            .map(|tool| tool.definition)
    }
    pub fn tools(&self) -> Vec<FunctionTool> {
        self.tools.values().map(|t| t.definition.clone()).collect()
    }
    pub async fn can_execute(&self, id: i64, modality: &str) -> Result<bool> {
        let Some(def) = self.core.agent_get(id).await? else {
            return Ok(false);
        };
        if def.tools.iter().any(|id| !self.tools.contains_key(id)) {
            return Ok(false);
        }
        let requirement = ai::Requirement {
            kind: ai::CapabilityKind::Chat,
            input_modalities: BTreeSet::from(["text".into(), modality.into()]),
            output_modalities: BTreeSet::from(["text".into()]),
            features: if def.tools.is_empty() && def.tool_choice.is_none() {
                BTreeSet::new()
            } else {
                BTreeSet::from(["tool_calling".into()])
            },
            tool_choice: def.tool_choice.map(|x| x.0),
        };
        Ai::new(self.core.clone())
            .map_err(ai_error)?
            .can_execute(def.model, &requirement)
            .await
            .map_err(ai_error)
    }
    pub async fn create_thread(&self, id: i64) -> Result<Thread> {
        let def = self
            .core
            .agent_get(id)
            .await?
            .ok_or(Error::NotFound("Agent"))?;
        let tools = def
            .tools
            .iter()
            .map(|id| {
                self.tools
                    .get(id)
                    .ok_or(Error::NotFound("Agent tool"))?
                    .snapshot()
            })
            .collect::<Result<Vec<_>>>()?;
        let snapshot = ThreadState {
            model: def.model,
            tools: tools.iter().map(|t| t.definition.clone()).collect(),
            tool_choice: def.tool_choice.map(|v| v.0),
            max_model_calls_per_turn: def.max_model_calls_per_turn,
            messages: vec![Message::System {
                content: def.system_prompt,
            }],
        };
        snapshot.validate()?;
        let (id, snapshot) = self.persistence.create(snapshot).await?;
        snapshot.validate()?;
        Ok(Thread {
            id,
            core: self.core.clone(),
            persistence: self.persistence.clone(),
            state: Mutex::new(State {
                snapshot,
                tools,
                turn_index: 0,
            }),
        })
    }

    pub async fn run(&self, id: i64, input: Message) -> Result<Thread> {
        let thread = self.create_thread(id).await?;
        thread.turn(input).await?;
        Ok(thread)
    }
}
struct State {
    turn_index: u64,
    snapshot: ThreadState,
    tools: Vec<BoundTool>,
}
pub struct Thread {
    pub id: Uuid,
    core: Core,
    persistence: Arc<dyn ThreadPersistence>,
    state: Mutex<State>,
}
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Termination {
    Completed,
    MaxModelCalls,
}
fn ai_error(_: ai::Error) -> Error {
    Error::Unavailable("Agent model execution")
}
impl Thread {
    pub async fn messages(&self) -> Vec<Message> {
        self.state.lock().await.snapshot.messages.clone()
    }
    pub async fn refresh(&self) -> Result<ThreadState> {
        let mut state = self.state.lock().await;
        let snapshot = self.persistence.read(self.id).await?;
        snapshot.validate()?;
        state.snapshot = snapshot.clone();
        Ok(snapshot)
    }
    /// Cancelling the future drops in-flight model and tool operations. Tool effects
    /// may have committed; an incomplete batch is never appended to history.
    pub async fn turn(&self, input: Message) -> Result<Termination> {
        if !matches!(input, Message::User { .. }) {
            return Err(Error::InvalidInput(
                "Agent turn requires a user message".into(),
            ));
        }
        ai::validate_history(std::slice::from_ref(&input)).map_err(ai_error)?;
        let mut state = self
            .state
            .try_lock()
            .map_err(|_| Error::Conflict("Agent turn already active".into()))?;
        state.turn_index = state.turn_index.saturating_add(1);
        let turn = state.turn_index;
        let operation = self.core.operation(
            "agent.turn",
            json!({"gen_ai.conversation.id": self.id, "inkcre.agent.turn": turn})
                .as_object()
                .expect("metadata object")
                .clone(),
        );
        operation.set_failure_attribute(
            "inkcre.agent.outcome",
            json!("failed"),
            json!("cancelled"),
        );
        let observation = operation.clone();
        operation
            .run(async {
                state.snapshot = self
                    .persistence
                    .discard_trailing_incomplete_tool_calls(self.id)
                    .await?;
                state.snapshot = self.persistence.append(self.id, vec![input]).await?;
                let ai = Ai::new(self.core.clone()).map_err(ai_error)?;
                let definitions = state
                    .tools
                    .iter()
                    .map(|t| t.definition.clone())
                    .collect::<Vec<_>>();
                for model_call in 1..=state.snapshot.max_model_calls_per_turn {
                    observation.set_attribute("inkcre.agent.model_calls", json!(model_call));
                    if self
                        .step(&mut state, &ai, &definitions, turn, model_call)
                        .await?
                    {
                        observation.set_attribute("inkcre.agent.outcome", json!("completed"));
                        return Ok(Termination::Completed);
                    }
                }
                observation.set_attribute("inkcre.agent.outcome", json!("max_model_calls"));
                Ok(Termination::MaxModelCalls)
            })
            .await
    }
    async fn step(
        &self,
        state: &mut State,
        ai: &Ai,
        definitions: &[FunctionTool],
        turn: u64,
        model_call: i32,
    ) -> Result<bool> {
        let operation=self.core.operation("agent.step",json!({"gen_ai.conversation.id":self.id,"inkcre.agent.turn":turn,"inkcre.agent.model_call":model_call}).as_object().expect("metadata object").clone());
        operation
            .run(async {
                let assistant = ai
                    .chat(
                        state.snapshot.model,
                        &state.snapshot.messages,
                        definitions,
                        state.snapshot.tool_choice.as_ref(),
                    )
                    .await
                    .map_err(ai_error)?;
                if assistant.tool_calls.is_empty() {
                    state.snapshot = self
                        .persistence
                        .append(self.id, vec![assistant.into()])
                        .await?;
                    return Ok(true);
                }
                let calls = assistant.tool_calls.iter().cloned().map(|call| {
                    let bound = state
                        .tools
                        .iter()
                        .find(|t| t.definition.id == call.tool)
                        .cloned();
                    self.execute_tool(call, bound, turn, model_call)
                });
                let results = futures_util::future::join_all(calls).await;
                state.snapshot = self
                    .persistence
                    .append(
                        self.id,
                        vec![assistant.into(), Message::ToolResult { results }],
                    )
                    .await?;
                Ok(false)
            })
            .await
    }
    async fn execute_tool(
        &self,
        call: ToolCall,
        bound: Option<BoundTool>,
        turn: u64,
        model_call: i32,
    ) -> ToolResult {
        let operation=self.core.operation("agent.tool",json!({"gen_ai.operation.name":"execute_tool","gen_ai.conversation.id":self.id,"inkcre.agent.turn":turn,"inkcre.agent.model_call":model_call,"inkcre.agent.tool_call.id_hash":format!("{:x}",Sha256::digest(call.id.as_bytes()))}).as_object().expect("metadata object").clone());
        if let Some(bound) = &bound {
            operation.set_attribute("gen_ai.tool.name", json!(bound.definition.id));
        }
        let observation = operation.clone();
        operation
            .scope(async move {
                let result = match bound {
                    None => Err(ToolFailure::Actionable(
                        json!({"error":"unknown_tool","tool":call.tool}),
                    )),
                    Some(tool) => {
                        let input = Value::Object(call.arguments);
                        let schema = Value::Object(tool.definition.input_schema);
                        if jsonschema::is_valid(&schema, &input) {
                            tool.handler.invoke(input).await
                        } else {
                            Err(ToolFailure::Actionable(
                                json!({"error":"invalid_arguments"}),
                            ))
                        }
                    }
                };
                let (content, is_error) = match result {
                    Ok(value) => (value, false),
                    Err(ToolFailure::Actionable(value)) => (value, true),
                    Err(ToolFailure::Failed) => (json!({"error":"tool_execution_failed"}), true),
                };
                if is_error {
                    observation.set_outcome(crate::observability::Outcome::Error);
                }
                ToolResult {
                    tool_call_id: call.id,
                    content,
                    is_error,
                }
            })
            .await
    }
}
