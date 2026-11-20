use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

pub const CAPTURE: &str = "extensions.capture.capture.v1";
pub const ATTACHMENT: &str = "extensions.capture.attachment.v1";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    pub application_id: Option<String>,
    pub title: Option<String>,
    pub url: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Entry {
    Selection,
    Paste,
    Screenshot,
    Manual,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub submission_id: Uuid,
    pub payload_digest: String,
    pub captured_at: DateTime<Utc>,
    pub entry: Entry,
    pub origin: Option<Origin>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub mime_type: String,
    pub filename: Option<String>,
}
#[derive(Serialize)]
pub struct Material {
    pub block_id: i32,
    pub resolver: String,
}
#[derive(Serialize)]
pub struct SolvedCapture {
    pub metadata: Metadata,
    pub materials: Vec<Material>,
    pub annotation_block_id: Option<i32>,
}
#[derive(Serialize)]
pub struct SolvedAttachment {
    pub metadata: Attachment,
    pub content_block_id: i32,
}

pub fn capture_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["submission_id","payload_digest","captured_at","entry"],"properties":{
        "submission_id":{"type":"string","format":"uuid"},
        "payload_digest":{"type":"string","pattern":"^[0-9a-f]{64}$"},
        "captured_at":{"type":"string","format":"date-time"},
        "entry":{"enum":["selection","paste","screenshot","manual"]},
        "origin":{"anyOf":[{"type":"null"},{"type":"object","additionalProperties":false,"properties":{
            "application_id":{"type":["string","null"],"maxLength":1024},
            "title":{"type":["string","null"],"maxLength":4096},
            "url":{"type":["string","null"],"maxLength":8192}
        }}]}
    }})
}
pub fn attachment_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["mime_type"],"properties":{
        "mime_type":{"enum":["image/png","image/jpeg","image/gif","image/webp"]},
        "filename":{"type":["string","null"],"maxLength":512}
    }})
}
