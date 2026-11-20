use crate::{Result, fail};
use chrono::{DateTime, NaiveDate, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const SOURCE: &str = "extensions.mail.source.Source";
pub const EMAIL: &str = "extensions.mail.email.v1";
pub const MAILBOX: &str = "extensions.mail.mailbox.v1";
pub const ADDRESS: &str = "extensions.mail.email_address.v1";
pub const FLAG: &str = "extensions.mail.flag.v1";
pub const PART: &str = "extensions.mail.mime_part.v1";
pub const LIST: &str = "extensions.mail.mailing_list.v1";
pub const NEWS_HTML: &str = "extensions.mail.newsletter_html.v1";
pub const NEWS_TEXT: &str = "extensions.mail.newsletter_text.v1";
pub const MATERIALIZE: &str = "extensions.mail.mime_part.materialize.v1";
pub const MATERIALIZE_PATH: &str = "/mail/mime-parts/materialize";
fn yes() -> bool {
    true
}
fn port() -> u16 {
    993
}
#[derive(Clone, Default, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    #[default]
    Tls,
    Starttls,
    Plain,
}
#[derive(Clone, Default, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Imap,
}
#[derive(Clone, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parameters {
    #[schemars(length(min = 1))]
    pub host: String,
    #[serde(default = "port")]
    #[schemars(range(min = 1))]
    pub port: u16,
    #[serde(default)]
    pub security: Security,
    #[schemars(length(min = 1))]
    pub username: String,
    #[schemars(length(min = 1))]
    pub password: String,
}
#[derive(Clone, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exclusions {
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default = "special")]
    pub special_uses: Vec<String>,
}
fn special() -> Vec<String> {
    vec!["\\Drafts".into(), "\\Junk".into(), "\\Trash".into()]
}
impl Default for Exclusions {
    fn default() -> Self {
        Self {
            names: vec![],
            special_uses: special(),
        }
    }
}
#[derive(Clone, Default, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionConfig {
    #[serde(default)]
    pub default_excluded_mailboxes: Exclusions,
}
#[derive(Clone, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub protocol: Protocol,
    pub parameters: Parameters,
    pub excluded_mailboxes: Option<Exclusions>,
    #[serde(default = "yes")]
    pub ordinary_mark_as_seen: bool,
    #[serde(default)]
    pub backfill_mark_as_seen: bool,
    #[serde(default)]
    pub synchronize_deletions: bool,
}
impl Config {
    pub fn parse(value: Value) -> Result<Self> {
        let config: Self =
            serde_json::from_value(value).map_err(|_| fail("Invalid Mail Source configuration"))?;
        let p = &config.parameters;
        if p.host.trim().is_empty() || p.username.is_empty() || p.password.is_empty() || p.port == 0
        {
            return Err(fail("Mail access parameters must be nonempty"));
        }
        Ok(config)
    }
    pub fn binding(&self) -> Binding {
        let p = &self.parameters;
        Binding {
            protocol: self.protocol.clone(),
            host: p.host.trim().to_lowercase(),
            port: p.port,
            security: p.security.clone(),
            username: p.username.clone(),
        }
    }
}
#[derive(Clone, Default, JsonSchema, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Collect {}
#[derive(Clone, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Backfill {
    pub since: NaiveDate,
    pub before: Option<NaiveDate>,
}
impl Backfill {
    pub fn validate(&self) -> Result<()> {
        if self.before.is_some_and(|before| self.since >= before) {
            Err(fail("since must be earlier than before"))
        } else {
            Ok(())
        }
    }
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub protocol: Protocol,
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub username: String,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub binding: Option<Binding>,
    #[serde(default)]
    pub checkpoints: BTreeMap<String, Checkpoint>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub uid_validity: u32,
    #[serde(default)]
    pub last_uid: u32,
    pub highest_modseq: Option<u64>,
}
#[derive(Clone, Default, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Email {
    pub message_id: Option<String>,
    pub email_id: Option<String>,
    pub subject: Option<String>,
    pub authored_at: Option<DateTime<Utc>>,
}
#[derive(Clone, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mailbox {
    pub name: String,
    #[serde(default)]
    pub special_uses: Vec<String>,
    pub mailbox_id: Option<String>,
}
#[derive(Clone, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MailingList {
    pub list_id: String,
    pub name: Option<String>,
}
#[derive(Clone, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Address {
    pub address: String,
}
#[derive(Clone, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Flag {
    pub name: String,
    pub description: Option<String>,
}
#[derive(Clone, JsonSchema, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MimePart {
    pub media_type: String,
    pub charset: Option<String>,
    pub filename: Option<String>,
    pub content_id: Option<String>,
    pub description: Option<String>,
    pub transfer_encoding: Option<String>,
    #[schemars(range(min = 0))]
    pub encoded_size: Option<u32>,
    pub content_location: Option<String>,
}
#[derive(Clone)]
pub struct Part {
    pub id: String,
    pub role: String,
    pub metadata: MimePart,
    pub text: Option<String>,
    pub alternatives: Vec<String>,
}
pub struct Participant {
    pub role: &'static str,
    pub order: usize,
    pub address: String,
    pub name: Option<String>,
}
pub struct Message {
    pub uid: u32,
    pub root: Email,
    pub parts: Vec<Part>,
    pub list: Option<MailingList>,
    pub participants: Vec<Participant>,
    pub parents: Vec<String>,
    pub references: Vec<String>,
    pub flags: Vec<String>,
}
pub struct Batch {
    pub messages: Vec<Message>,
    pub flags: Vec<(u32, Vec<String>)>,
    pub removed: Vec<u32>,
    pub next: Checkpoint,
}
