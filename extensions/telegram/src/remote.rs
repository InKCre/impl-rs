use crate::{Result, fail};
use serde_json::{Value, json};
use std::time::Duration;

pub struct Bot {
    client: reqwest::Client,
    base: url::Url,
    token: String,
}
impl Bot {
    pub fn new(token: String) -> Result<Self> {
        let base = url::Url::parse(
            &std::env::var("INKCRE_TELEGRAM_API_BASE_URL")
                .unwrap_or_else(|_| "https://api.telegram.org/".into()),
        )
        .map_err(|_| fail("Invalid Telegram endpoint"))?;
        if !matches!(base.scheme(), "http" | "https")
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(fail("Invalid Telegram endpoint"));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| fail("Telegram client unavailable"))?;
        Ok(Self {
            client,
            base,
            token,
        })
    }
    fn endpoint(&self, file: bool, path: &str) -> Result<url::Url> {
        let mut url = self.base.clone();
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| fail("Invalid Telegram endpoint"))?;
        segments.pop_if_empty();
        if file {
            segments.push("file");
        }
        segments.push(&format!("bot{}", self.token));
        for part in path.split('/') {
            if part.is_empty() || matches!(part, "." | "..") {
                return Err(fail("Invalid Telegram file path"));
            }
            segments.push(part);
        }
        drop(segments);
        Ok(url)
    }
    pub async fn call(&self, method: &str, body: Value) -> Result<Value> {
        let response = self
            .client
            .post(self.endpoint(false, method)?)
            .json(&body)
            .send()
            .await
            .map_err(|_| fail("Telegram request failed"))?;
        let bytes = read(response, 16 * 1024 * 1024).await?;
        let payload: Value =
            serde_json::from_slice(&bytes).map_err(|_| fail("Invalid Telegram response"))?;
        if payload["ok"] != true {
            return Err(fail("Telegram rejected the operation"));
        }
        payload
            .get("result")
            .cloned()
            .ok_or_else(|| fail("Telegram result missing"))
    }
    pub async fn identity(&self) -> Result<i64> {
        self.call("getMe", json!({})).await?["id"]
            .as_i64()
            .filter(|id| *id > 0)
            .ok_or_else(|| fail("Invalid Telegram bot identity"))
    }
    pub async fn download(&self, file: &str) -> Result<Vec<u8>> {
        let remote = self.call("getFile", json!({"file_id":file})).await?;
        let path = remote["file_path"]
            .as_str()
            .ok_or_else(|| fail("Telegram file path unavailable"))?;
        let response = self
            .client
            .get(self.endpoint(true, path)?)
            .send()
            .await
            .map_err(|_| fail("Telegram file retrieval failed"))?;
        // Cloud Bot API currently admits files up to 20 MiB. Preserve a bounded local envelope.
        read(response, 64 * 1024 * 1024).await
    }
    pub async fn notify(&self, message: &Value, outcome: &str) -> Result<()> {
        let chat = message["chat"]["id"]
            .as_i64()
            .ok_or_else(|| fail("Telegram chat missing"))?;
        let id = message["message_id"]
            .as_i64()
            .ok_or_else(|| fail("Telegram message ID missing"))?;
        if matches!(outcome, "saved" | "partial") {
            self.call(
                "setMessageReaction",
                json!({"chat_id":chat,"message_id":id,"reaction":[{"type":"emoji","emoji":"👍"}]}),
            )
            .await?;
        }
        let text = match outcome {
            "start" => "Send or forward useful text and files here. Saved messages receive 👍.",
            "unsupported" => "This message has no supported content to save.",
            "partial" => {
                "Attachment metadata was saved, but its bytes were not downloaded; retry materialization later."
            }
            "failed" => "This message was not saved and will be retried by the next collection.",
            _ => return Ok(()),
        };
        self.call(
            "sendMessage",
            json!({"chat_id":chat,"text":text,"reply_parameters":{"message_id":id}}),
        )
        .await?;
        Ok(())
    }
}
async fn read(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if !response.status().is_success() {
        return Err(fail("Telegram HTTP request rejected"));
    }
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(fail("Telegram response exceeded limit"));
    }
    let mut bytes = Vec::new();
    while let Some(part) = response
        .chunk()
        .await
        .map_err(|_| fail("Telegram response interrupted"))?
    {
        if part.len() > limit.saturating_sub(bytes.len()) {
            return Err(fail("Telegram response exceeded limit"));
        }
        bytes.extend(part)
    }
    Ok(bytes)
}
