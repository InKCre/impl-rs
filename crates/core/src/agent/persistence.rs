//! Whole-thread persistence boundary. Implementations commit each append atomically.
use super::*;
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadState {
    pub model: i64,
    pub tools: Vec<FunctionTool>,
    pub tool_choice: Option<ToolChoice>,
    pub max_model_calls_per_turn: i32,
    pub messages: Vec<Message>,
}
impl ThreadState {
    pub fn validate(&self) -> Result<()> {
        let ids: BTreeSet<_> = self.tools.iter().map(|t| &t.id).collect();
        if self.max_model_calls_per_turn <= 0
            || ids.len() != self.tools.len()
            || self.tools.iter().any(|tool| tool.id.is_empty())
        {
            return Err(Error::InvalidInput("invalid Thread snapshot".into()));
        }
        if let Some(Message::Assistant {
            content,
            tool_calls,
        }) = self.messages.last()
        {
            ai::AssistantMessage::new(content.clone(), tool_calls.clone())
                .map_err(|_| Error::InvalidInput("invalid trailing Thread tool calls".into()))?;
        }
        let history = if matches!(self.messages.last(),Some(Message::Assistant{tool_calls,..}) if !tool_calls.is_empty())
        {
            &self.messages[..self.messages.len() - 1]
        } else {
            &self.messages[..]
        };
        ai::validate_history(history)
            .map_err(|_| Error::InvalidInput("invalid Thread history".into()))
    }
}
#[async_trait]
pub trait ThreadPersistence: Send + Sync {
    async fn create(&self, state: ThreadState) -> Result<(Uuid, ThreadState)>;
    async fn read(&self, id: Uuid) -> Result<ThreadState>;
    async fn append(&self, id: Uuid, messages: Vec<Message>) -> Result<ThreadState>;
    async fn discard_trailing_incomplete_tool_calls(&self, id: Uuid) -> Result<ThreadState>;
}
#[derive(Default)]
pub struct InMemoryThreadPersistence {
    states: Mutex<BTreeMap<Uuid, ThreadState>>,
}
#[async_trait]
impl ThreadPersistence for InMemoryThreadPersistence {
    async fn create(&self, state: ThreadState) -> Result<(Uuid, ThreadState)> {
        state.validate()?;
        let id = Uuid::new_v4();
        self.states.lock().await.insert(id, state.clone());
        Ok((id, state))
    }
    async fn read(&self, id: Uuid) -> Result<ThreadState> {
        self.states
            .lock()
            .await
            .get(&id)
            .cloned()
            .ok_or(Error::NotFound("Agent Thread"))
    }
    async fn append(&self, id: Uuid, messages: Vec<Message>) -> Result<ThreadState> {
        let mut states = self.states.lock().await;
        let current = states.get_mut(&id).ok_or(Error::NotFound("Agent Thread"))?;
        let mut next = current.clone();
        next.messages.extend(messages);
        next.validate()?;
        *current = next.clone();
        Ok(next)
    }
    async fn discard_trailing_incomplete_tool_calls(&self, id: Uuid) -> Result<ThreadState> {
        let mut states = self.states.lock().await;
        let current = states.get_mut(&id).ok_or(Error::NotFound("Agent Thread"))?;
        if matches!(current.messages.last(),Some(Message::Assistant{tool_calls,..}) if !tool_calls.is_empty())
        {
            current.messages.pop();
        }
        Ok(current.clone())
    }
}
