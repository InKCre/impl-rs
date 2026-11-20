//! Exact persisted envelopes shared with the Python Distribution.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const NAME: &str = "inkcre/caldav";
pub const COLLECTION: &str = "extensions.caldav.collection.v2";
pub const CALENDAR: &str = "extensions.caldav.calendar.v2";
pub const CONTACT: &str = "extensions.caldav.contact.v2";
pub const LEGACY_COLLECTION: &str = "extensions.caldav.collection.v1";
pub const LEGACY_CALENDAR: &str = "extensions.caldav.calendar.v1";
pub const LEGACY_CONTACT: &str = "extensions.caldav.contact.v1";
pub const PROPERTY_PREFIX: &str = "caldav:property:";
pub const RESOLVERS: [&str; 6] = [
    COLLECTION,
    CALENDAR,
    CONTACT,
    LEGACY_COLLECTION,
    LEGACY_CALENDAR,
    LEGACY_CONTACT,
];

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionContent {
    pub path: String,
    pub props: BTreeMap<String, String>,
    #[serde(default = "yes")]
    pub published: bool,
}
fn yes() -> bool {
    true
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceContent {
    pub href: String,
    pub text: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GraphContent {
    pub href: String,
    pub remainder: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Representation {
    Text,
    Uri,
    Binary,
    DataUri,
    Reference,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PropertyLink {
    #[serde(default)]
    pub component: Option<(String, String, String)>,
    #[serde(default)]
    pub alarm: Option<usize>,
    pub name: String,
    #[serde(default)]
    pub params: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub group: Option<String>,
    pub position: usize,
    pub representation: Representation,
    #[serde(default)]
    pub media_type: Option<String>,
}
impl PropertyLink {
    pub fn relation(&self) -> crate::Result<String> {
        Ok(format!("{PROPERTY_PREFIX}{}", serde_json::to_string(self)?))
    }
    pub fn parse(value: &str) -> crate::Result<Self> {
        Ok(serde_json::from_str(
            value
                .strip_prefix(PROPERTY_PREFIX)
                .ok_or(crate::Error::Invalid("invalid DAV property relation"))?,
        )?)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PropertyData {
    Text(String),
    Bytes(Vec<u8>),
}
#[derive(Clone, Debug)]
pub struct PropertyValue {
    pub link: PropertyLink,
    pub value: PropertyData,
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub password: Option<String>,
}
impl Config {
    pub fn validate(&self) -> crate::Result<()> {
        if self
            .password
            .as_ref()
            .is_some_and(|v| !(32..=128).contains(&v.chars().count()))
        {
            return Err(crate::Error::Invalid(
                "DAV password requires 32–128 characters",
            ));
        }
        Ok(())
    }
}
