use std::{collections::BTreeMap, time::Duration};

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::{Client, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

use super::*;

const INLINE_LIMIT: usize = 7 * 1024 * 1024;
const RESPONSE_LIMIT: usize = 32 * 1024 * 1024;
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

pub(super) fn client() -> Result<Client> {
    Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| Error::Provider)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config<'a> {
    api_key: &'a str,
    base_url: Option<&'a str>,
}

impl<'a> Config<'a> {
    fn read(config: &'a Value) -> Result<(Self, Url)> {
        let parsed = Self::deserialize(config).map_err(|_| Error::InvalidProvider)?;
        let base = Url::parse(parsed.base_url.unwrap_or(DEFAULT_BASE_URL))
            .ok()
            .filter(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.has_host()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
            })
            .ok_or(Error::InvalidProvider)?;
        let key_usable = !parsed.api_key.is_empty()
            && parsed
                .api_key
                .bytes()
                .all(|byte| (0x21..=0x7e).contains(&byte));
        if !key_usable {
            return Err(Error::InvalidProvider);
        }
        Ok((parsed, base))
    }
}

pub(super) struct OpenAi {
    client: Client,
    alibaba: bool,
}

impl OpenAi {
    pub(super) fn new(client: Client, alibaba: bool) -> Self {
        Self { client, alibaba }
    }

    async fn post(
        &self,
        configuration: &Value,
        endpoint: &[&str],
        body: Value,
    ) -> Result<Response> {
        let (config, mut url) = Config::read(configuration)?;
        url.path_segments_mut()
            .map_err(|_| Error::InvalidProvider)?
            .pop_if_empty()
            .extend(endpoint);
        self.client
            .post(url)
            .bearer_auth(config.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|_| Error::Provider)?
            .error_for_status()
            .map_err(|_| Error::Provider)
    }

    fn message_params(&self, history: &[Message]) -> Result<Vec<Value>> {
        let mut messages = Vec::with_capacity(history.len());
        for message in history {
            match message {
                Message::System { content } => messages.push(json!({"role":"system","content":content})),
                Message::User { content } => {
                    let parts = content.iter().map(|part| self.part(part)).collect::<Result<Vec<_>>>()?;
                    messages.push(json!({"role":"user","content":parts}));
                }
                Message::Assistant { content, tool_calls } => {
                    let mut message = json!({"role":"assistant","content":content});
                    if !tool_calls.is_empty() {
                        message["tool_calls"] = json!(tool_calls.iter().map(|call| {
                            json!({"id":call.id,"type":"function","function":{
                                "name":call.tool,"arguments":Value::Object(call.arguments.clone()).to_string()
                            }})
                        }).collect::<Vec<_>>());
                    }
                    messages.push(message);
                }
                Message::ToolResult { results } => messages.extend(results.iter().map(|result| {
                    let content = json!({"content":result.content,"is_error":result.is_error});
                    json!({"role":"tool","tool_call_id":result.tool_call_id,"content":content.to_string()})
                })),
            }
        }
        Ok(messages)
    }

    fn part(&self, part: &ContentPart) -> Result<Value> {
        match part {
            ContentPart::Text { text } => Ok(json!({"type":"text","text":text})),
            ContentPart::Image(media) => {
                Ok(json!({"type":"image_url","image_url":{"url":inline_or_url(media)?}}))
            }
            ContentPart::Audio(media) if self.alibaba => {
                let mime = media.normalized_mime();
                Ok(json!({"type":"input_audio","input_audio":{
                    "data":inline_or_url(media)?,"format":mime.strip_prefix("audio/").unwrap_or(&mime)
                }}))
            }
            ContentPart::Audio(media) => {
                let format = match media.normalized_mime().as_str() {
                    "audio/mpeg" | "audio/mp3" => "mp3",
                    "audio/wav" | "audio/x-wav" => "wav",
                    _ => return Err(Error::Invalid("unsupported chat audio MIME type")),
                };
                if media.data.len() > INLINE_LIMIT {
                    return Err(Error::Invalid("chat audio exceeds the inline limit"));
                }
                Ok(json!({"type":"input_audio","input_audio":{
                    "data":STANDARD.encode(&media.data),"format":format
                }}))
            }
            ContentPart::Video(media) if self.alibaba => {
                Ok(json!({"type":"video_url","video_url":{"url":inline_or_url(media)?}}))
            }
            ContentPart::Video(_) => Err(Error::Unsupported),
        }
    }
}

fn inline_or_url(media: &Media) -> Result<String> {
    if media.data.len() <= INLINE_LIMIT {
        return Ok(format!(
            "data:{};base64,{}",
            media.normalized_mime(),
            STANDARD.encode(&media.data)
        ));
    }
    media.transfer_url.clone().ok_or(Error::Invalid(
        "media exceeds inline limit and has no transfer URL",
    ))
}

async fn read_response(mut response: Response) -> Result<Vec<u8>> {
    if !response.status().is_success() {
        return Err(Error::Provider);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Provider)? {
        if chunk.len() > RESPONSE_LIMIT.saturating_sub(bytes.len()) {
            return Err(Error::Output("response exceeded byte limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<Embedding>,
}

#[derive(Deserialize)]
struct Embedding {
    index: usize,
    embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: WireAssistant,
}

#[derive(Deserialize)]
struct WireAssistant {
    content: Option<String>,
    tool_calls: Option<Vec<WireCall>>,
}

#[derive(Deserialize)]
struct WireCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: WireFunction,
}

#[derive(Deserialize)]
struct WireFunction {
    name: String,
    arguments: String,
}

fn call(id: String, name: String, arguments: &str) -> Result<ToolCall> {
    Ok(ToolCall {
        id,
        tool: name,
        arguments: serde_json::from_str(arguments)
            .map_err(|_| Error::Output("tool arguments must be a JSON object"))?,
    })
}

fn assistant(bytes: &[u8]) -> Result<AssistantMessage> {
    let response: ChatResponse =
        serde_json::from_slice(bytes).map_err(|_| Error::Output("invalid chat response"))?;
    let message = response
        .choices
        .into_iter()
        .next()
        .ok_or(Error::Output("empty chat choices"))?
        .message;
    let calls = message
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .map(|wire| {
            if wire.kind != "function" {
                return Err(Error::Output("unsupported chat tool call type"));
            }
            call(wire.id, wire.function.name, &wire.function.arguments)
        })
        .collect::<Result<_>>()?;
    AssistantMessage::new(message.content, calls)
}

#[derive(Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
}

#[derive(Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct StreamDelta {
    content: Option<String>,
    tool_calls: Option<Vec<StreamTool>>,
}

#[derive(Deserialize)]
struct StreamTool {
    index: usize,
    id: Option<String>,
    function: Option<StreamFunction>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct StreamFunction {
    name: String,
    arguments: String,
}

#[derive(Default)]
struct StreamAssistant {
    text: String,
    tools: BTreeMap<usize, (String, StreamFunction)>,
    finished: bool,
}

impl StreamAssistant {
    fn append(&mut self, data: &str) -> Result<()> {
        let chunk: StreamChunk =
            serde_json::from_str(data).map_err(|_| Error::Output("invalid chat stream chunk"))?;
        if let Some(choice) = chunk.choices.into_iter().next() {
            self.finished |= choice.finish_reason.is_some();
            if let Some(content) = choice.delta.content {
                self.text.push_str(&content);
            }
            for tool in choice.delta.tool_calls.unwrap_or_default() {
                let (id, current) = self.tools.entry(tool.index).or_default();
                if let Some(next_id) = tool.id.filter(|id| !id.is_empty()) {
                    if !id.is_empty() && *id != next_id {
                        return Err(Error::Output("chat stream changed tool call ID"));
                    }
                    *id = next_id;
                }
                if let Some(next) = tool.function {
                    current.name.push_str(&next.name);
                    current.arguments.push_str(&next.arguments);
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> Result<AssistantMessage> {
        if !self.finished || (self.text.is_empty() && self.tools.is_empty()) {
            return Err(Error::Output("incomplete chat stream"));
        }
        let calls = self
            .tools
            .into_values()
            .map(|(id, function)| call(id, function.name, &function.arguments))
            .collect::<Result<_>>()?;
        AssistantMessage::new((!self.text.is_empty()).then_some(self.text), calls)
    }
}

fn streamed_assistant(bytes: &[u8]) -> Result<AssistantMessage> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::Output("chat stream is not UTF-8"))?;
    let mut result = StreamAssistant::default();
    let mut data = Vec::new();
    for line in text.lines().chain(std::iter::once("")) {
        if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value));
        } else if line.is_empty() && !data.is_empty() {
            let event = data.join("\n");
            data.clear();
            if event == "[DONE]" {
                result.finished = true;
                break;
            }
            result.append(&event)?;
        }
    }
    result.finish()
}

#[async_trait]
impl Dialect for OpenAi {
    fn validate_config(&self, config: &Value) -> Result<()> {
        Config::read(config).map(|_| ())
    }

    fn supports(&self, requirement: &Requirement) -> bool {
        let allowed = match requirement.kind {
            CapabilityKind::Embedding => &["text"][..],
            CapabilityKind::Chat if self.alibaba => &["text", "image", "audio", "video"],
            CapabilityKind::Chat => &["text", "image", "audio"],
        };
        requirement
            .input_modalities
            .iter()
            .all(|modality| allowed.contains(&modality.as_str()))
            && requirement.features.iter().all(|feature| {
                requirement.kind == CapabilityKind::Chat && feature == "tool_calling"
            })
            && (requirement.tool_choice.is_none() || requirement.kind == CapabilityKind::Chat)
    }

    async fn embed(
        &self,
        config: &Value,
        model: &str,
        inputs: &[String],
        dimensions: usize,
    ) -> Result<Vec<Vec<f32>>> {
        let response = self
            .post(
                config,
                &["embeddings"],
                json!({
                    "model":model,"input":inputs,"dimensions":dimensions,"encoding_format":"float"
                }),
            )
            .await?;
        let bytes = read_response(response).await?;
        let parsed: EmbeddingResponse = serde_json::from_slice(&bytes)
            .map_err(|_| Error::Output("invalid embedding response"))?;
        let mut indexed = vec![None; inputs.len()];
        for item in parsed.data {
            let slot = indexed
                .get_mut(item.index)
                .ok_or(Error::Output("embedding index out of range"))?;
            if slot.replace(item.embedding).is_some() {
                return Err(Error::Output("duplicate embedding index"));
            }
        }
        indexed
            .into_iter()
            .map(|value| value.ok_or(Error::Output("missing embedding index")))
            .collect()
    }

    async fn chat(
        &self,
        config: &Value,
        model: &str,
        messages: &[Message],
        tools: &[FunctionTool],
        choice: Option<&ToolChoice>,
    ) -> Result<AssistantMessage> {
        let mut body =
            json!({"model":model,"messages":self.message_params(messages)?,"stream":self.alibaba});
        if self.alibaba {
            body["stream_options"] = json!({"include_usage":true});
            body["modalities"] = json!(["text"]);
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools.iter().map(|tool| json!({
                "type":"function","function":{"name":tool.id,"description":tool.description,"parameters":tool.input_schema}
            })).collect::<Vec<_>>());
        }
        if let Some(choice) = choice {
            body["tool_choice"] = match choice {
                ToolChoice::Mode(mode) => json!(mode),
                ToolChoice::Named(NamedToolChoice::Function { tool }) => json!({
                    "type":"function","function":{"name":tool}
                }),
            };
        }
        let response = self.post(config, &["chat", "completions"], body).await?;
        let bytes = read_response(response).await?;
        if self.alibaba {
            streamed_assistant(&bytes)
        } else {
            assistant(&bytes)
        }
    }
}
