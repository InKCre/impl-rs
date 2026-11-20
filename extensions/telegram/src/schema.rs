use crate::{Result, fail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MATERIALIZE_PATH: &str = "/telegram/attachments/materialize";
pub const MATERIALIZE_CAPABILITY: &str = "extensions.telegram.attachment.materialize.v1";
pub const MATERIALIZE_METHOD: &str = "materialize_attachment";
pub const SOURCE: &str = "extensions.telegram.source.Source";
pub const ATTACHMENT: &str = "extensions.telegram.attachment.v1";
pub const MESSAGE: &str = "extensions.telegram.message.v1";
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub bot_token: String,
    pub bound_user_id: i64,
    #[serde(default)]
    pub download_attachments: bool,
}
impl Config {
    pub fn parse(value: Value) -> Result<Self> {
        let setup: Self =
            serde_json::from_value(value).map_err(|_| fail("Invalid Telegram Source config"))?;
        if setup.bot_token.is_empty() || setup.bound_user_id <= 0 {
            return Err(fail("Invalid Telegram Source config"));
        }
        Ok(setup)
    }
}
#[derive(Clone, Default, Deserialize, Serialize)]
pub struct State {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bot_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_update_id: Option<i64>,
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}
impl State {
    pub fn parse(value: Value) -> Result<Self> {
        let state: Self =
            serde_json::from_value(value).map_err(|_| fail("Invalid Telegram Source state"))?;
        if state.bot_id.is_some_and(|id| id <= 0)
            || state.last_update_id.is_some_and(|id| id < 0)
            || (state.last_update_id.is_some() && state.bot_id.is_none())
        {
            return Err(fail(
                "Invalid bot-scoped Telegram cursor; create a new Source",
            ));
        }
        Ok(state)
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub kind: String,
    pub file_id: String,
    pub file_unique_id: String,
    pub filename: Option<String>,
    pub mime_type: Option<String>,
    pub file_size: Option<u64>,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub duration: Option<u64>,
    pub title: Option<String>,
    pub performer: Option<String>,
    pub emoji: Option<String>,
}
impl Attachment {
    pub fn validate(self) -> Result<Self> {
        if !matches!(
            self.kind.as_str(),
            "photo"
                | "animation"
                | "audio"
                | "document"
                | "sticker"
                | "video"
                | "video_note"
                | "voice"
        ) || self.file_id.is_empty()
            || self.file_unique_id.is_empty()
        {
            return Err(fail("Invalid Telegram attachment"));
        }
        Ok(self)
    }
    pub fn parse(raw: &str) -> Result<Self> {
        serde_json::from_str::<Self>(raw)
            .map_err(|_| fail("Invalid Telegram attachment"))?
            .validate()
    }
}
#[derive(Deserialize, Serialize)]
pub struct LegacyMessage {
    pub message_id: i64,
    #[serde(deserialize_with = "legacy_date")]
    pub date: String,
    pub text: Option<String>,
    pub caption: Option<String>,
    pub reply_to_message_id: Option<i64>,
    #[serde(default)]
    pub has_media: bool,
    pub media_type: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Materialize {
    pub block: i32,
}

pub fn schemas() -> Value {
    serde_json::from_str(include_str!("schemas.json")).expect("static canonical schemas")
}

// Legacy Pydantic datetime content admits RFC3339, naive ISO and Unix seconds/milliseconds.
fn legacy_date<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
    let value = Value::deserialize(deserializer)?;
    let render = |date: DateTime<chrono::FixedOffset>| {
        date.to_rfc3339_opts(
            if date.timestamp_subsec_micros() == 0 {
                SecondsFormat::Secs
            } else {
                SecondsFormat::Micros
            },
            true,
        )
    };
    if let Some(text) = value.as_str() {
        let iso = text.replacen(' ', "T", 1);
        if let Ok(date) = DateTime::parse_from_rfc3339(&iso) {
            return Ok(render(date));
        }
        for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
            if let Ok(date) = NaiveDateTime::parse_from_str(&iso, format) {
                return Ok(date
                    .format(if date.and_utc().timestamp_subsec_micros() == 0 {
                        "%Y-%m-%dT%H:%M:%S"
                    } else {
                        "%Y-%m-%dT%H:%M:%S%.6f"
                    })
                    .to_string());
            }
        }
        if let Ok(date) = NaiveDate::parse_from_str(&iso, "%Y-%m-%d") {
            return Ok(format!("{date}T00:00:00"));
        }
    }
    let timestamp = value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.parse::<f64>().ok()));
    if let Some(mut timestamp) = timestamp.filter(|n| n.is_finite()) {
        if timestamp.abs() > 20_000_000_000.0 {
            timestamp /= 1000.0;
        }
        let seconds = timestamp.floor();
        if (-62135596800.0..=253402300799.0).contains(&seconds) {
            let micros = ((timestamp - seconds) * 1_000_000.0).round() as u32;
            let seconds = seconds as i64 + i64::from(micros == 1_000_000);
            if let Some(date) =
                DateTime::<Utc>::from_timestamp(seconds, (micros % 1_000_000) * 1000)
            {
                return Ok(render(date.fixed_offset()));
            }
        }
    }
    Err(serde::de::Error::custom("invalid legacy Telegram datetime"))
}
