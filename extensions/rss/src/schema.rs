use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use crate::{Error, Result};

pub const EXTENSION_NAME: &str = "inkcre/rss";
pub const RSS_SOURCE: &str = "extensions.rss.rss.Source";
pub const ATOM_SOURCE: &str = "extensions.rss.atom.Source";
pub const FEED_RESOLVER: &str = "extensions.rss.feed.v1";
pub const ITEM_RESOLVER: &str = "extensions.rss.feed_item.v1";
pub const ENCLOSURE_RESOLVER: &str = "extensions.rss.enclosure.v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    Rss,
    Atom,
}

impl Family {
    pub fn from_type(id: &str) -> Result<Self> {
        match id {
            RSS_SOURCE => Ok(Self::Rss),
            ATOM_SOURCE => Ok(Self::Atom),
            _ => Err(Error::Invalid("unsupported RSS Source type")),
        }
    }
}

fn timeout() -> u64 {
    30
}
fn feed_bytes() -> usize {
    8 * 1024 * 1024
}
fn enclosure_bytes() -> usize {
    64 * 1024 * 1024
}
fn yes() -> bool {
    true
}
fn storage() -> i32 {
    -4
}
fn unidentified() -> String {
    "create".into()
}
fn user_agent() -> String {
    "InKCre RSS/0.1".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedConfig {
    pub feed_url: String,
    #[serde(default = "timeout")]
    pub request_timeout_seconds: u64,
    #[serde(default = "feed_bytes")]
    pub max_feed_bytes: usize,
    #[serde(default = "yes")]
    pub fetch_full_text: bool,
    #[serde(default = "feed_bytes")]
    pub max_article_bytes: usize,
    #[serde(default)]
    pub download_enclosures: bool,
    #[serde(default = "enclosure_bytes")]
    pub max_enclosure_bytes: usize,
    #[serde(default = "storage")]
    pub target_storage_id: i32,
    #[serde(default = "unidentified")]
    pub unidentified_item_behavior: String,
    #[serde(default = "user_agent")]
    pub user_agent: String,
}

impl FeedConfig {
    pub fn parse(value: &Value) -> Result<Self> {
        let mut config: Self = serde_json::from_value(value.clone())
            .map_err(|_| Error::Invalid("invalid RSS Source configuration"))?;
        config.feed_url = config.feed_url.trim().to_owned();
        require_http(&config.feed_url)?;
        if config.request_timeout_seconds == 0
            || config.max_feed_bytes == 0
            || config.max_article_bytes == 0
            || config.max_enclosure_bytes == 0
        {
            return Err(Error::Invalid("RSS limits must be positive"));
        }
        if !matches!(
            config.unidentified_item_behavior.as_str(),
            "create" | "discard"
        ) {
            return Err(Error::Invalid("invalid unidentified item behavior"));
        }
        Ok(config)
    }
    pub fn schema() -> Value {
        json!({"title":"FeedSourceConfig","description":"Durable configuration shared by the RSS and Atom source wrappers.","type":"object","additionalProperties":false,
            "required":["feed_url"],"properties":{
                "feed_url":{"type":"string","title":"Feed Url"},
                "request_timeout_seconds":{"type":"integer","exclusiveMinimum":0,"default":30},
                "max_feed_bytes":{"type":"integer","exclusiveMinimum":0,"default":8388608},
                "fetch_full_text":{"type":"boolean","default":true},
                "max_article_bytes":{"type":"integer","exclusiveMinimum":0,"default":8388608},
                "download_enclosures":{"type":"boolean","default":false},
                "max_enclosure_bytes":{"type":"integer","exclusiveMinimum":0,"default":67108864},
                "target_storage_id":{"type":"integer","default":-4},
                "unidentified_item_behavior":{"enum":["create","discard"],"default":"create"},
                "user_agent":{"type":"string","default":"InKCre RSS/0.1"}}})
    }
}

pub fn require_http(value: &str) -> Result<Url> {
    let url =
        Url::parse(value).map_err(|_| Error::Invalid("an absolute HTTP(S) URL is required"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(Error::Invalid("an absolute HTTP(S) URL is required"));
    }
    Ok(url)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectConfig {
    pub fetch_full_text: Option<bool>,
    pub download_enclosures: Option<bool>,
    pub target_storage_id: Option<i32>,
}

impl CollectConfig {
    pub fn schema() -> Value {
        json!({"title":"FeedCollectJobConfig","description":"Per-run overrides admitted by the RSS collection command.","type":"object","additionalProperties":false,
            "properties":{"fetch_full_text":{"type":["boolean","null"],"default":null},"download_enclosures":{"type":["boolean","null"],"default":null},"target_storage_id":{"type":["integer","null"],"default":null}}})
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FeedState {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub last_successful_contentful_snapshot_observed_at: Option<DateTime<Utc>>,
    pub snapshot_configured_url: Option<String>,
    pub snapshot_feed_block_id: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalFeed {
    pub source_instance_id: i32,
    pub family: Family,
    pub configured_url: String,
    pub source_native_id: Option<String>,
    pub declared_self_url: Option<String>,
    pub title: Option<String>,
    pub home_url: Option<String>,
    pub description: Option<String>,
    pub language: Option<String>,
    pub authored_updated_at: Option<DateTime<Utc>>,
}

impl CanonicalFeed {
    pub fn identity(&self) -> Value {
        if let Some(id) = &self.source_native_id {
            json!(["source_native_id", self.source_instance_id, id])
        } else if let Some(url) = &self.declared_self_url {
            json!(["declared_self_url", self.source_instance_id, url])
        } else {
            json!([
                "configured_url",
                self.source_instance_id,
                self.configured_url
            ])
        }
    }
    pub fn schema() -> Value {
        let mut properties = serde_json::Map::new();
        properties.insert("source_instance_id".into(), json!({"type":"integer"}));
        properties.insert("family".into(), json!({"enum":["rss","atom"]}));
        properties.insert("configured_url".into(), json!({"type":"string"}));
        for field in [
            "source_native_id",
            "declared_self_url",
            "title",
            "home_url",
            "description",
            "language",
            "authored_updated_at",
        ] {
            properties.insert(field.into(), json!({"type":["string","null"]}));
        }
        json!({"title":"CanonicalFeed","type":"object","additionalProperties":false,"properties":properties,"required":["source_instance_id","family","configured_url"]})
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedAuthor {
    pub name: Option<String>,
    pub email: Option<String>,
    pub url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalItem {
    pub source_native_id: Option<String>,
    pub source_native_id_kind: Option<String>,
    pub alternate_url: Option<String>,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub authored_content: Option<String>,
    pub authored_content_type: Option<String>,
    pub authored_published_at: Option<DateTime<Utc>>,
    pub authored_updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub authors: Vec<FeedAuthor>,
    #[serde(default)]
    pub categories: Vec<String>,
}

impl CanonicalItem {
    pub fn identity(&self) -> Option<Value> {
        match (&self.source_native_id, &self.source_native_id_kind) {
            (Some(id), Some(kind)) => Some(json!([kind, id])),
            _ => self
                .alternate_url
                .as_ref()
                .map(|url| json!(["alternate_url", url])),
        }
    }
    pub fn source_time(&self) -> Option<DateTime<Utc>> {
        self.authored_updated_at.or(self.authored_published_at)
    }
    pub fn validate(&self) -> Result<()> {
        if self.source_native_id.is_some() != self.source_native_id_kind.is_some()
            || self
                .source_native_id_kind
                .as_ref()
                .is_some_and(|kind| !matches!(kind.as_str(), "rss_guid" | "atom_id"))
        {
            return Err(Error::Invalid("invalid canonical item identity"));
        }
        if [
            &self.source_native_id,
            &self.alternate_url,
            &self.title,
            &self.summary,
            &self.authored_content,
        ]
        .into_iter()
        .all(|value| value.as_ref().is_none_or(String::is_empty))
        {
            return Err(Error::Invalid(
                "feed item has no usable authored information",
            ));
        }
        Ok(())
    }
    pub fn schema() -> Value {
        let mut properties = serde_json::Map::new();
        for field in [
            "source_native_id",
            "alternate_url",
            "title",
            "summary",
            "authored_content",
            "authored_content_type",
            "authored_published_at",
            "authored_updated_at",
        ] {
            properties.insert(field.into(), json!({"type":["string","null"]}));
        }
        properties.insert(
            "source_native_id_kind".into(),
            json!({"enum":["rss_guid","atom_id",null]}),
        );
        properties.insert("authors".into(),json!({"type":"array","items":{"type":"object","additionalProperties":false,"properties":{"name":{"type":["string","null"]},"email":{"type":["string","null"]},"url":{"type":["string","null"]}}}}));
        properties.insert(
            "categories".into(),
            json!({"type":"array","items":{"type":"string"}}),
        );
        json!({"title":"CanonicalFeedItem","type":"object","additionalProperties":false,"properties":properties})
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalEnclosure {
    pub family: Family,
    pub url: String,
    pub declared_media_type: Option<String>,
    pub declared_length: Option<u64>,
    pub title: Option<String>,
}

impl CanonicalEnclosure {
    pub fn schema() -> Value {
        json!({"title":"CanonicalEnclosure","type":"object","additionalProperties":false,"required":["family","url"],"properties":{"family":{"enum":["rss","atom"]},"url":{"type":"string"},"declared_media_type":{"type":["string","null"]},"declared_length":{"type":["integer","null"],"minimum":0},"title":{"type":["string","null"]}}})
    }
}

pub struct ParsedItem {
    pub item: CanonicalItem,
    pub enclosures: Vec<CanonicalEnclosure>,
}
pub struct Snapshot {
    pub feed: CanonicalFeed,
    pub items: Vec<ParsedItem>,
    pub diagnostics: Vec<Value>,
}
