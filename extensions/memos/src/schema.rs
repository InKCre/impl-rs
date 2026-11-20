use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

pub const MEMO: &str = "extensions.memos.memo.v1";
pub const ATTACHMENT: &str = "extensions.memos.attachment.v2";
pub const ATTACHMENT_PREFIX: &str = "attachment:";

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    #[default]
    Private,
    Protected,
    Public,
}

fn required_option<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
    Option::deserialize(d)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalMemo {
    pub body: String,
    #[serde(deserialize_with = "required_option")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(deserialize_with = "required_option")]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub visibility: Visibility,
    #[serde(default)]
    pub pinned: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalAttachment {
    pub filename: String,
    pub media_type: String,
    pub size: u64,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SolvedAttachment {
    pub block_id: i32,
    pub content_block_id: i32,
    pub canonical: CanonicalAttachment,
    pub owner_memo_id: Option<i32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SolvedMemo {
    pub block_id: i32,
    pub canonical: CanonicalMemo,
    pub attachments: Vec<SolvedAttachment>,
    pub parent_id: Option<i32>,
    pub reference_ids: Vec<i32>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoCursor {
    pub created_at: Option<DateTime<Utc>>,
    pub block_id: i32,
}
