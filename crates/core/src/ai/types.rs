use base64::{
    Engine,
    engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use url::Url;

use super::{Error, Result};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    Chat,
    Embedding,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Capability {
    #[serde(rename = "type")]
    pub kind: CapabilityKind,
    pub input_modalities: BTreeSet<String>,
    pub output_modalities: BTreeSet<String>,
    #[serde(default)]
    pub features: BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Model {
    pub id: i64,
    pub provider: i64,
    pub native_model_id: String,
    pub name: Option<String>,
    pub capabilities: Vec<Capability>,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct Requirement {
    pub kind: CapabilityKind,
    pub input_modalities: BTreeSet<String>,
    pub output_modalities: BTreeSet<String>,
    pub features: BTreeSet<String>,
    pub tool_choice: Option<ToolChoice>,
}

impl Requirement {
    pub fn embedding() -> Self {
        Self {
            kind: CapabilityKind::Embedding,
            input_modalities: BTreeSet::from(["text".into()]),
            output_modalities: BTreeSet::from(["vector".into()]),
            features: BTreeSet::new(),
            tool_choice: None,
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FunctionTool {
    pub id: String,
    pub description: String,
    pub input_schema: Map<String, Value>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub id: String,
    pub tool: String,
    pub arguments: Map<String, Value>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    pub tool_call_id: String,
    pub content: Value,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(schemars::JsonSchema, Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoiceMode {
    None,
    Auto,
    Required,
}

#[derive(schemars::JsonSchema, Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum NamedToolChoice {
    Function { tool: String },
}

#[derive(schemars::JsonSchema, Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ToolChoice {
    Mode(ToolChoiceMode),
    Named(NamedToolChoice),
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Media {
    #[serde(serialize_with = "encode_bytes", deserialize_with = "decode_bytes")]
    pub data: Vec<u8>,
    pub mime_type: String,
    pub transfer_url: Option<String>,
}

fn encode_bytes<S: Serializer>(
    bytes: &[u8],
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    serializer.serialize_str(&URL_SAFE.encode(bytes))
}

fn decode_bytes<'de, D: Deserializer<'de>>(input: D) -> std::result::Result<Vec<u8>, D::Error> {
    let encoded = String::deserialize(input)?;
    URL_SAFE
        .decode(&encoded)
        .or_else(|_| URL_SAFE_NO_PAD.decode(&encoded))
        .map_err(|_| serde::de::Error::custom("media must be URL-safe base64"))
}

impl Media {
    pub(super) fn normalized_mime(&self) -> String {
        self.mime_type
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
    }

    fn validate(&self, family: &str) -> Result<()> {
        let mime = self.normalized_mime();
        if self.data.is_empty()
            || !mime
                .split_once('/')
                .is_some_and(|(actual, subtype)| actual == family && !subtype.is_empty())
            || mime.chars().any(char::is_whitespace)
        {
            return Err(Error::Invalid("invalid media bytes or MIME type"));
        }
        if self.transfer_url.as_deref().is_some_and(|url| {
            !Url::parse(url).is_ok_and(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.has_host()
                    && url.username().is_empty()
                    && url.password().is_none()
            })
        }) {
            return Err(Error::Invalid("invalid media transfer URL"));
        }
        Ok(())
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentPart {
    Text { text: String },
    Image(Media),
    Audio(Media),
    Video(Media),
}

impl ContentPart {
    pub fn modality(&self) -> &'static str {
        match self {
            Self::Text { .. } => "text",
            Self::Image(_) => "image",
            Self::Audio(_) => "audio",
            Self::Video(_) => "video",
        }
    }

    fn validate(&self) -> Result<()> {
        match self {
            Self::Text { text } if text.is_empty() => Err(Error::Invalid("empty user text")),
            Self::Text { .. } => Ok(()),
            Self::Image(media) | Self::Audio(media) | Self::Video(media) => {
                media.validate(self.modality())
            }
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: Vec<ContentPart>,
    },
    Assistant {
        content: Option<String>,
        #[serde(default)]
        tool_calls: Vec<ToolCall>,
    },
    ToolResult {
        results: Vec<ToolResult>,
    },
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum AssistantRole {
    Assistant,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssistantMessage {
    #[serde(rename = "type")]
    role: AssistantRole,
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
}

impl AssistantMessage {
    pub fn new(content: Option<String>, tool_calls: Vec<ToolCall>) -> Result<Self> {
        call_ids(&tool_calls).map_err(|_| Error::Output("invalid assistant tool calls"))?;
        Ok(Self {
            role: AssistantRole::Assistant,
            content,
            tool_calls,
        })
    }
}

impl From<AssistantMessage> for Message {
    fn from(message: AssistantMessage) -> Self {
        Self::Assistant {
            content: message.content,
            tool_calls: message.tool_calls,
        }
    }
}

fn unique_ids<'a>(ids: impl IntoIterator<Item = &'a str>) -> Result<BTreeSet<&'a str>> {
    let mut result = BTreeSet::new();
    for id in ids {
        if id.is_empty() || !result.insert(id) {
            return Err(Error::Invalid("IDs must be non-empty and unique"));
        }
    }
    Ok(result)
}

fn call_ids(calls: &[ToolCall]) -> Result<BTreeSet<&str>> {
    if calls.iter().any(|call| call.tool.is_empty()) {
        return Err(Error::Invalid("empty tool name in chat history"));
    }
    unique_ids(calls.iter().map(|call| call.id.as_str()))
}

pub fn validate_history(messages: &[Message]) -> Result<()> {
    let mut remaining = messages.iter();
    while let Some(message) = remaining.next() {
        match message {
            Message::System { .. } => {}
            Message::User { content } => {
                if content.is_empty() {
                    return Err(Error::Invalid("user turn must have content"));
                }
                for part in content {
                    part.validate()?;
                }
            }
            Message::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                let Some(Message::ToolResult { results }) = remaining.next() else {
                    return Err(Error::Invalid("tool calls require adjacent results"));
                };
                if call_ids(tool_calls)?
                    != unique_ids(results.iter().map(|result| result.tool_call_id.as_str()))?
                {
                    return Err(Error::Invalid("results must exactly cover tool call IDs"));
                }
            }
            Message::Assistant { .. } => {}
            Message::ToolResult { .. } => {
                return Err(Error::Invalid("tool results have no preceding calls"));
            }
        }
    }
    Ok(())
}

pub(super) fn chat_requirement(
    messages: &[Message],
    tools: &[FunctionTool],
    tool_choice: Option<&ToolChoice>,
) -> Result<Requirement> {
    validate_history(messages)?;
    let ids = unique_ids(tools.iter().map(|tool| tool.id.as_str()))?;
    if tool_choice.is_some() && ids.is_empty() {
        return Err(Error::Invalid("tool_choice requires at least one tool"));
    }
    if let Some(ToolChoice::Named(NamedToolChoice::Function { tool })) = tool_choice
        && !ids.contains(tool.as_str())
    {
        return Err(Error::Invalid(
            "named tool_choice must identify a supplied tool",
        ));
    }
    let mut input_modalities = BTreeSet::from(["text".into()]);
    for message in messages {
        if let Message::User { content } = message {
            input_modalities.extend(content.iter().map(|part| part.modality().to_owned()));
        }
    }
    Ok(Requirement {
        kind: CapabilityKind::Chat,
        input_modalities,
        output_modalities: BTreeSet::from(["text".into()]),
        features: if tools.is_empty() {
            BTreeSet::new()
        } else {
            BTreeSet::from(["tool_calling".into()])
        },
        tool_choice: tool_choice.cloned(),
    })
}
