use crate::{Error, Result, schema::*};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

pub const CREATOR: &str = "creator == \"users/inkcre\"";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Attachment {
    pub name: Option<String>,
    #[serde(alias = "create_time")]
    pub create_time: Option<DateTime<Utc>>,
    pub filename: Option<String>,
    #[serde(alias = "external_link")]
    pub external_link: Option<String>,
    #[serde(rename = "type")]
    pub media_type: Option<String>,
    pub size: Option<String>,
    pub memo: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CreateMemo {
    pub content: String,
    pub visibility: Option<String>,
    pub attachments: Option<Vec<Attachment>>,
    #[serde(alias = "create_time")]
    pub create_time: Option<DateTime<Utc>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct UpdateMemo {
    pub content: Option<String>,
    pub visibility: Option<String>,
    pub state: Option<String>,
    pub pinned: Option<bool>,
    #[serde(alias = "update_time")]
    pub update_time: Option<DateTime<Utc>>,
    pub attachments: Option<Vec<Attachment>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CreateAttachment {
    pub filename: String,
    #[serde(rename = "type")]
    pub media_type: String,
    pub content: String,
    pub memo: Option<String>,
}

pub fn visibility(value: Option<&str>) -> Result<Visibility> {
    match value.unwrap_or("PRIVATE") {
        "PRIVATE" => Ok(Visibility::Private),
        "PROTECTED" => Ok(Visibility::Protected),
        "PUBLIC" => Ok(Visibility::Public),
        _ => Err(Error::BadRequest("Invalid visibility")),
    }
}
pub fn create(value: &CreateMemo) -> Result<CanonicalMemo> {
    let now = value.create_time.unwrap_or_else(Utc::now);
    Ok(CanonicalMemo {
        body: value.content.clone(),
        created_at: Some(now),
        updated_at: Some(now),
        visibility: visibility(value.visibility.as_deref())?,
        archived: false,
        pinned: false,
    })
}
pub fn id(value: &str) -> Result<i32> {
    let id: i32 = value
        .parse()
        .map_err(|_| Error::BadRequest("Invalid resource ID"))?;
    if id <= 0 || id.to_string() != value {
        return Err(Error::BadRequest("Invalid resource ID"));
    }
    Ok(id)
}
pub fn resource(value: &str, prefix: &str) -> Result<i32> {
    id(value
        .strip_prefix(prefix)
        .ok_or(Error::BadRequest("Invalid resource name"))?)
}
pub fn attachment_ids(values: Option<&[Attachment]>) -> Result<Vec<i32>> {
    let mut result = Vec::new();
    let mut unique = HashSet::new();
    for attachment in values.unwrap_or_default() {
        let id = resource(
            attachment
                .name
                .as_deref()
                .ok_or(Error::BadRequest("Attachment name is required"))?,
            "attachments/",
        )?;
        if !unique.insert(id) {
            return Err(Error::BadRequest("Duplicate attachment identity"));
        }
        result.push(id);
    }
    Ok(result)
}
pub fn attachment(solved: &SolvedAttachment) -> Value {
    json!({"name":format!("attachments/{}",solved.block_id),"createTime":solved.canonical.created_at,"filename":solved.canonical.filename,"type":solved.canonical.media_type,"size":solved.canonical.size.to_string(),"memo":solved.owner_memo_id.map(|id|format!("memos/{id}"))})
}
pub fn memo(solved: &SolvedMemo) -> Value {
    let c = &solved.canonical;
    json!({"name":format!("memos/{}",solved.block_id),"state":if c.archived{"ARCHIVED"}else{"NORMAL"},"creator":"users/inkcre","createTime":c.created_at,"updateTime":c.updated_at,"content":c.body,"visibility":format!("{:?}",c.visibility).to_uppercase(),"pinned":c.pinned,"attachments":solved.attachments.iter().map(attachment).collect::<Vec<_>>(),"parent":solved.parent_id.map(|id|format!("memos/{id}"))})
}

pub fn update(raw: Value, mask: Option<&str>) -> Result<(Option<Value>, Option<Vec<i32>>)> {
    let input: UpdateMemo =
        serde_json::from_value(raw.clone()).map_err(|_| Error::BadRequest("Invalid memo patch"))?;
    if let Some(v) = input.visibility.as_deref() {
        visibility(Some(v))?;
    }
    if input
        .state
        .as_deref()
        .is_some_and(|v| !matches!(v, "NORMAL" | "ARCHIVED"))
    {
        return Err(Error::BadRequest("Invalid memo state"));
    }
    let object = raw
        .as_object()
        .ok_or(Error::BadRequest("Memo patch must be an object"))?;
    let selected: HashSet<String> = match mask {
        Some(mask) => mask
            .split(',')
            .map(|key| {
                if key.trim() == "update_time" {
                    "updateTime".into()
                } else {
                    key.trim().into()
                }
            })
            .collect(),
        None => object.keys().cloned().collect(),
    };
    if selected.is_empty()
        || selected.iter().any(|k| {
            !matches!(
                k.as_str(),
                "content" | "visibility" | "state" | "pinned" | "updateTime" | "attachments"
            )
        })
    {
        return Err(Error::BadRequest("Unsupported or empty updateMask"));
    }
    if selected
        .iter()
        .any(|k| object.get(k).is_some_and(Value::is_null))
    {
        return Err(Error::BadRequest("Selected fields cannot be null"));
    }
    let mut patch = json!({});
    if selected.contains("content") {
        patch["body"] = json!(input.content.unwrap_or_default());
    }
    if selected.contains("visibility") {
        patch["visibility"] = json!(visibility(input.visibility.as_deref())?);
    }
    if selected.contains("state") {
        patch["archived"] = json!(input.state.as_deref() == Some("ARCHIVED"));
    }
    if selected.contains("pinned") {
        patch["pinned"] = json!(input.pinned.unwrap_or(false));
    }
    if selected.contains("updateTime") {
        patch["updated_at"] = json!(input.update_time.unwrap_or_else(Utc::now));
    }
    let attachments = if selected.contains("attachments") {
        Some(attachment_ids(input.attachments.as_deref())?)
    } else {
        None
    };
    Ok((
        if patch.as_object().is_some_and(|v| v.is_empty()) {
            None
        } else {
            Some(patch)
        },
        attachments,
    ))
}

pub fn query(raw: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(raw.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}
pub fn limit(query: &HashMap<String, String>, default: i64) -> Result<usize> {
    let n = query.get("pageSize").map_or(Ok(default), |v| {
        v.parse::<i64>()
            .map_err(|_| Error::BadRequest("pageSize must be an integer"))
    })?;
    Ok(if n <= 0 { default } else { n.min(1000) } as usize)
}
