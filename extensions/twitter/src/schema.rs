//! Canonical sparse JSON models; generated schemas own field admission.
use crate::{Result, invalid};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::OnceLock};
pub const EXTENSION: &str = "inkcre/twitter";
pub const SOURCE: &str = "extensions.twitter.bookmark.Source";
pub const POST: &str = "extensions.twitter.tweet.v2";
pub const LEGACY: &str = "extensions.twitter.tweet.v1";
pub const USER: &str = "extensions.twitter.user.v1";
pub const MEDIA: &str = "extensions.twitter.media.v1";
pub const REFERENCE: &str = "extensions.twitter.reference.v1";
pub const KINDS: [&str; 3] = ["users", "posts", "media"];
pub fn resolver(kind: &str) -> &'static str {
    match kind {
        "users" => USER,
        "posts" => POST,
        _ => MEDIA,
    }
}
pub fn model(kind: &str) -> &'static str {
    match kind {
        "users" => "TwitterUser",
        "posts" => "TwitterPost",
        _ => "TwitterMedia",
    }
}
pub fn schemas() -> &'static Value {
    static SCHEMAS: OnceLock<Value> = OnceLock::new();
    SCHEMAS.get_or_init(|| {
        let bundle: Value =
            serde_json::from_str(include_str!("schemas.json")).expect("canonical Twitter schemas");
        let mut models = serde_json::Map::new();
        for name in bundle["models"]
            .as_array()
            .expect("schema names")
            .iter()
            .filter_map(Value::as_str)
        {
            let mut schema = bundle["$defs"][name].clone();
            schema["$defs"] = bundle["$defs"].clone();
            models.insert(name.into(), schema);
        }
        Value::Object(models)
    })
}
fn validators() -> &'static BTreeMap<String, jsonschema::Validator> {
    static VALIDATORS: OnceLock<BTreeMap<String, jsonschema::Validator>> = OnceLock::new();
    VALIDATORS.get_or_init(|| {
        schemas()
            .as_object()
            .expect("schema map")
            .iter()
            .map(|(name, schema)| {
                (
                    name.clone(),
                    jsonschema::validator_for(schema).expect("valid canonical schema"),
                )
            })
            .collect()
    })
}
pub fn validate(name: &str, value: &Value) -> Result<()> {
    if !validators().get(name).is_some_and(|v| v.is_valid(value)) {
        return Err(invalid("Invalid canonical Twitter content"));
    }
    if name != "Tweet" {
        strict_types(&schemas()[name], value, &schemas()[name]["$defs"])?;
        extra_validation(value)?
    }
    Ok(())
}
fn strict_types(schema: &Value, value: &Value, definitions: &Value) -> Result<()> {
    if let Some(reference) = schema["$ref"].as_str() {
        return strict_types(
            &definitions[reference.rsplit('/').next().unwrap_or_default()],
            value,
            definitions,
        );
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(variants) = schema[key].as_array() {
            if value.is_number() && variants.iter().any(|s| s["type"] == "number") {
                return Ok(());
            }
            for variant in variants {
                let concrete = if let Some(reference) = variant["$ref"].as_str() {
                    &definitions[reference.rsplit('/').next().unwrap_or_default()]
                } else {
                    variant
                };
                let matches = match concrete["type"].as_str() {
                    Some("object") => {
                        value.is_object()
                            && concrete["required"].as_array().is_none_or(|fields| {
                                fields
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .all(|field| value.get(field).is_some())
                            })
                            && concrete["properties"].as_object().is_none_or(|fields| {
                                fields.iter().all(|(key, s)| {
                                    s.get("const")
                                        .is_none_or(|expected| value.get(key) == Some(expected))
                                })
                            })
                    }
                    Some("array") => value.is_array(),
                    Some("integer" | "number") => value.is_number(),
                    Some("string") => value.is_string(),
                    Some("boolean") => value.is_boolean(),
                    Some("null") => value.is_null(),
                    _ => false,
                };
                if matches {
                    return strict_types(concrete, value, definitions);
                }
            }
        }
    }
    if schema["type"] == "integer" && !(value.is_i64() || value.is_u64()) {
        return Err(invalid("Twitter integer facts must be integers"));
    }
    if let Some(object) = value.as_object()
        && let Some(properties) = schema["properties"].as_object()
    {
        for (key, value) in object {
            if let Some(schema) = properties.get(key) {
                strict_types(schema, value, definitions)?
            }
        }
    }
    if let Some(values) = value.as_array() {
        for (i, value) in values.iter().enumerate() {
            strict_types(
                schema
                    .get("prefixItems")
                    .and_then(|items| items.get(i))
                    .unwrap_or(&schema["items"]),
                value,
                definitions,
            )?
        }
    }
    Ok(())
}
fn extra_validation(value: &Value) -> Result<()> {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if value.is_null() {
                    return Err(invalid("Omit unknown Twitter facts instead of null"));
                }
                if matches!(key.as_str(), "range" | "visible")
                    && let Some(range) = value.as_array()
                    && range.len() == 2
                    && range[0].as_u64() > range[1].as_u64()
                {
                    return Err(invalid("Twitter range end precedes start"));
                }
                if matches!(
                    key.as_str(),
                    "createdAt" | "publishedAt" | "modifiedAt" | "endsAt" | "updatedAt"
                ) {
                    validate_time(value, false)?;
                }
                if key == "countsObservedAt"
                    && let Some(times) = value.as_object()
                {
                    for time in times.values() {
                        validate_time(time, true)?;
                    }
                }
                if key != "data" {
                    extra_validation(value)?;
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                extra_validation(value)?
            }
        }
        _ => (),
    }
    Ok(())
}
fn validate_time(value: &Value, fraction: bool) -> Result<()> {
    let s = value
        .as_str()
        .ok_or_else(|| invalid("Invalid Twitter timestamp"))?;
    let bytes = s.as_bytes();
    let shape = bytes.len() >= 20
        && &bytes[..4] != b"0000"
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[..19]
            .iter()
            .enumerate()
            .all(|(i, c)| matches!(i, 4 | 7 | 10 | 13 | 16) || c.is_ascii_digit())
        && &bytes[17..19] != b"60"
        && bytes.last() == Some(&b'Z')
        && (bytes.len() == 20
            || (fraction
                && bytes[19] == b'.'
                && bytes.len() > 21
                && bytes[20..bytes.len() - 1].iter().all(u8::is_ascii_digit)));
    if !shape || chrono::DateTime::parse_from_rfc3339(s).is_err() {
        return Err(invalid("Twitter timestamps must be UTC with seconds"));
    }
    Ok(())
}
pub fn native(kind: &str, value: &Value) -> Result<()> {
    validate(
        if value
            .as_object()
            .is_some_and(|o| o.len() == 1 && o.contains_key("id"))
        {
            "TwitterIdentity"
        } else {
            model(kind)
        },
        value,
    )
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Batch {
    #[serde(default)]
    pub users: Vec<Value>,
    #[serde(default)]
    pub posts: Vec<Value>,
    #[serde(default)]
    pub media: Vec<Value>,
}
impl Batch {
    pub fn parse(value: Value) -> Result<Self> {
        validate("TwitterBatch", &value)?;
        serde_json::from_value(value).map_err(|_| invalid("Invalid Twitter batch"))
    }
    pub fn items(&self, kind: &str) -> &[Value] {
        match kind {
            "users" => &self.users,
            "posts" => &self.posts,
            _ => &self.media,
        }
    }
    pub fn selected(&self, primary: &[String]) -> Result<Self> {
        let posts: BTreeMap<_, _> = self
            .posts
            .iter()
            .map(|post| (post["id"].as_str().unwrap_or_default(), post))
            .collect();
        let mut selected: std::collections::BTreeSet<String> = primary.iter().cloned().collect();
        let mut pending = primary.to_vec();
        while let Some(id) = pending.pop() {
            let post = posts
                .get(id.as_str())
                .ok_or_else(|| invalid("Primary Twitter post missing"))?;
            for reference in [
                &post["repostOf"],
                &post["quote"]["postId"],
                &post["replyTo"]["postId"],
            ] {
                if let Some(id) = reference.as_str()
                    && posts.contains_key(id)
                    && selected.insert(id.into())
                {
                    pending.push(id.into());
                }
            }
        }
        let chosen: Vec<Value> = self
            .posts
            .iter()
            .filter(|p| selected.contains(p["id"].as_str().unwrap_or_default()))
            .cloned()
            .collect();
        let authors: std::collections::BTreeSet<_> = chosen
            .iter()
            .filter_map(|p| p["authorId"].as_str())
            .collect();
        let mut media = std::collections::BTreeSet::new();
        for post in &chosen {
            for ids in [&post["mediaIds"], &post["article"]["mediaIds"]] {
                if let Some(ids) = ids.as_array() {
                    media.extend(ids.iter().filter_map(Value::as_str));
                }
            }
            if let Some(id) = post["article"]["coverMediaId"].as_str() {
                media.insert(id);
            }
        }
        Ok(Self {
            users: self
                .users
                .iter()
                .filter(|u| authors.contains(u["id"].as_str().unwrap_or_default()))
                .cloned()
                .collect(),
            media: self
                .media
                .iter()
                .filter(|m| media.contains(m["id"].as_str().unwrap_or_default()))
                .cloned()
                .collect(),
            posts: chosen,
        })
    }
}
/// Solved models expose their declared defaults; persisted facts remain sparse.
pub fn solved(kind: &str, value: &Value) -> Result<Value> {
    native(kind, value)?;
    if value.as_object().is_some_and(|o| o.len() == 1) {
        return Ok(value.clone());
    }
    let schema = &schemas()[model(kind)];
    Ok(project(
        schema,
        value,
        schema.get("$defs").unwrap_or(&Value::Null),
        true,
    ))
}
fn project(schema: &Value, value: &Value, definitions: &Value, fill_defaults: bool) -> Value {
    if let Some(reference) = schema["$ref"].as_str() {
        return project(
            &definitions[reference.rsplit('/').next().unwrap_or_default()],
            value,
            definitions,
            fill_defaults,
        );
    }
    for key in ["anyOf", "oneOf"] {
        if let Some(variants) = schema[key].as_array() {
            for variant in variants {
                let concrete = if let Some(reference) = variant["$ref"].as_str() {
                    &definitions[reference.rsplit('/').next().unwrap_or_default()]
                } else {
                    variant
                };
                if value.is_object()
                    && concrete["type"] == "object"
                    && concrete["required"].as_array().is_none_or(|r| {
                        r.iter()
                            .filter_map(Value::as_str)
                            .all(|k| value.get(k).is_some())
                    })
                    && concrete["properties"].as_object().is_none_or(|p| {
                        p.iter().all(|(k, s)| {
                            s.get("const")
                                .is_none_or(|expected| value.get(k) == Some(expected))
                        })
                    })
                {
                    return project(concrete, value, definitions, fill_defaults);
                }
            }
        }
    }
    match value {
        Value::Object(object) => {
            let mut result = if !fill_defaults && schema["additionalProperties"] == false {
                object
                    .iter()
                    .filter(|(key, _)| schema["properties"].get(*key).is_some())
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            } else {
                object.clone()
            };
            if let Some(properties) = schema["properties"].as_object() {
                for (key, property) in properties {
                    if let Some(value) = result.get_mut(key) {
                        *value = project(property, value, definitions, fill_defaults)
                    } else if fill_defaults && let Some(default) = property.get("default") {
                        result.insert(key.clone(), default.clone());
                    }
                }
            }
            Value::Object(result)
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    project(
                        schema
                            .get("prefixItems")
                            .and_then(|v| v.get(i))
                            .unwrap_or(&schema["items"]),
                        v,
                        definitions,
                        fill_defaults,
                    )
                })
                .collect(),
        ),
        _ => value.clone(),
    }
}
pub fn collect_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"properties":{"full":{"type":"boolean","default":false},"result_limit":{"type":"integer","minimum":5,"maximum":100,"default":40}}})
}
pub fn backfill_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"properties":{"skip":{"type":"integer","minimum":0,"maximum":10000,"default":0},"limit":{"type":"integer","minimum":1,"maximum":1000,"default":200},"page_size":{"type":"integer","minimum":1,"maximum":100,"default":50}}})
}

pub fn wire(kind: &str, value: &Value) -> Result<Value> {
    let name = match kind {
        "users" => "ArchiveUser",
        "posts" => "ArchivePost",
        _ => "ArchiveMedia",
    };
    let schema = &schemas()[name];
    let value = project(schema, value, &schema["$defs"], false);
    validate(name, &value)?;
    Ok(value)
}
