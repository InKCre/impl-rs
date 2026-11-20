//! Exact deployment configuration contracts and transactional mutation.
use crate::{Core, Page};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Core(#[from] crate::Error),
    #[error("unknown deployment config schema")]
    UnknownSchema,
    #[error("deployment config not found")]
    NotFound,
    #[error("invalid deployment config: {0}")]
    Invalid(&'static str),
}
pub type Result<T> = std::result::Result<T, Error>;
impl From<sqlx::Error> for Error {
    fn from(error: sqlx::Error) -> Self {
        Self::Core(error.into())
    }
}
#[derive(Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ConfigRecord {
    pub key: String,
    #[serde(rename = "schema")]
    pub schema_id: String,
    pub value: Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
#[derive(Clone, Serialize)]
pub struct SchemaDescriptor {
    pub id: String,
    pub keys: Vec<String>,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<Value>,
}
type Normalize = Arc<dyn Fn(Value) -> Result<Value> + Send + Sync>;
#[derive(Clone)]
struct Registration {
    descriptor: SchemaDescriptor,
    normalize: Normalize,
}
#[derive(Clone)]
pub struct ConfigRegistry {
    registrations: BTreeMap<String, Registration>,
}
impl Default for ConfigRegistry {
    fn default() -> Self {
        let mut registry = Self {
            registrations: BTreeMap::new(),
        };
        for (id, key, property, value, kind) in [
            (
                "core.cron.config.v1",
                "core.cron",
                "timezone",
                json!("UTC"),
                "string",
            ),
            (
                "core.source.config.v1",
                "core.source",
                "default_storage",
                json!(-4),
                "integer",
            ),
            (
                "core.semantic_retrieval.config.v1",
                "semantic_retrieval",
                "default_profile",
                Value::Null,
                "integer",
            ),
        ] {
            let descriptor = SchemaDescriptor {
                id: id.into(),
                keys: vec![key.into()],
                description: match key {
                    "core.cron" => "Deployment-wide Cron timezone.",
                    "core.source" => "Deployment-wide Source materialization storage.",
                    _ => "Default semantic vector space.",
                }
                .into(),
                input_schema: Some(
                    json!({"type":"object","additionalProperties":false,"properties":{property:{"type":if value.is_null(){json!([kind,"null"])}else{json!(kind)},"default":value}}}),
                ),
            };
            registry.registrations.insert(
                id.into(),
                Registration {
                    descriptor,
                    normalize: Arc::new(move |mut input| {
                        let object = input
                            .as_object_mut()
                            .ok_or(Error::Invalid("value must be an object"))?;
                        if object.keys().any(|key| key != property) {
                            return Err(Error::Invalid("unknown config property"));
                        }
                        let actual = object
                            .entry(property.to_owned())
                            .or_insert_with(|| value.clone());
                        match property {
                            "timezone" => {
                                actual
                                    .as_str()
                                    .and_then(|zone| zone.parse::<chrono_tz::Tz>().ok())
                                    .ok_or(Error::Invalid("invalid IANA timezone"))?;
                            }
                            "default_storage" => {
                                actual
                                    .as_i64()
                                    .and_then(|id| i32::try_from(id).ok())
                                    .ok_or(Error::Invalid("invalid Storage ID"))?;
                            }
                            _ if !actual.is_null() && actual.as_i64().is_none() => {
                                return Err(Error::Invalid("invalid profile ID"));
                            }
                            _ => {}
                        }
                        Ok(input)
                    }),
                },
            );
        }
        registry
    }
}
impl ConfigRegistry {
    pub fn register(
        &mut self,
        descriptor: SchemaDescriptor,
        normalize: impl Fn(Value) -> Result<Value> + Send + Sync + 'static,
    ) -> Result<()> {
        if descriptor.id.is_empty()
            || self.registrations.contains_key(&descriptor.id)
            || !descriptor
                .input_schema
                .as_ref()
                .is_some_and(Value::is_object)
        {
            return Err(Error::Invalid("schema ID is empty or already registered"));
        }
        self.registrations.insert(
            descriptor.id.clone(),
            Registration {
                descriptor,
                normalize: Arc::new(normalize),
            },
        );
        Ok(())
    }
    pub fn schema(&self, id: &str) -> Result<SchemaDescriptor> {
        self.registrations
            .get(id)
            .map(|r| r.descriptor.clone())
            .ok_or(Error::UnknownSchema)
    }
    pub fn schemas(
        &self,
        limit: Option<i64>,
        cursor: Option<&str>,
    ) -> Result<Page<SchemaDescriptor, String>> {
        page_limit(limit)?;
        let mut values: Vec<_> = self
            .registrations
            .values()
            .filter(|r| cursor.is_none_or(|cursor| r.descriptor.id.as_str() > cursor))
            .map(|r| {
                let mut d = r.descriptor.clone();
                d.input_schema = None;
                d
            })
            .take(limit.map_or(usize::MAX, |n| n as usize + 1))
            .collect();
        let next_cursor = truncate(&mut values, limit, |d| d.id.clone());
        Ok(Page {
            items: values,
            next_cursor,
        })
    }
    pub fn normalize(&self, id: &str, value: Value) -> Result<Value> {
        if !value.is_object() {
            return Err(Error::Invalid("value must be an object"));
        }
        let normalized = (self
            .registrations
            .get(id)
            .ok_or(Error::UnknownSchema)?
            .normalize)(value)?;
        if !normalized.is_object() {
            return Err(Error::Invalid("normalized value must be an object"));
        }
        Ok(normalized)
    }
}
fn page_limit(limit: Option<i64>) -> Result<()> {
    if limit.is_some_and(|n| n <= 0 || n == i64::MAX) {
        Err(Error::Invalid("invalid page limit"))
    } else {
        Ok(())
    }
}
fn truncate<T>(
    values: &mut Vec<T>,
    limit: Option<i64>,
    key: impl Fn(&T) -> String,
) -> Option<String> {
    if limit.is_some_and(|n| values.len() > n as usize) {
        values.pop();
        values.last().map(key)
    } else {
        None
    }
}
const COLUMNS: &str = "key,schema AS schema_id,value,created_at,updated_at";
impl Core {
    pub async fn config_get(&self, key: &str) -> Result<Option<ConfigRecord>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM inkcre.configs WHERE key=$1"
        ))
        .bind(key)
        .fetch_optional(self.pool())
        .await?)
    }
    pub async fn config_list(
        &self,
        limit: Option<i64>,
        cursor: Option<&str>,
    ) -> Result<Page<ConfigRecord, String>> {
        page_limit(limit)?;
        let mut items:Vec<ConfigRecord>=sqlx::query_as(&format!("SELECT {COLUMNS} FROM inkcre.configs WHERE ($1::text IS NULL OR key>$1) ORDER BY key LIMIT $2")).bind(cursor).bind(limit.map(|n|n+1)).fetch_all(self.pool()).await?;
        let next_cursor = truncate(&mut items, limit, |r| r.key.clone());
        Ok(Page { items, next_cursor })
    }
    pub async fn config_replace(
        &self,
        registry: &ConfigRegistry,
        key: &str,
        schema: &str,
        value: Value,
    ) -> Result<(ConfigRecord, bool)> {
        let value = registry.normalize(schema, value)?;
        let mut tx = self.pool().begin().await?;
        let inserted:Option<ConfigRecord>=sqlx::query_as(&format!("INSERT INTO inkcre.configs(key,schema,value) VALUES($1,$2,$3) ON CONFLICT(key) DO NOTHING RETURNING {COLUMNS}")).bind(key).bind(schema).bind(&value).fetch_optional(&mut *tx).await?;
        let created = inserted.is_some();
        let record =
            match inserted {
                Some(record) => record,
                None => sqlx::query_as(&format!(
                    "UPDATE inkcre.configs SET schema=$2,value=$3 WHERE key=$1 RETURNING {COLUMNS}"
                ))
                .bind(key)
                .bind(schema)
                .bind(value)
                .fetch_one(&mut *tx)
                .await?,
            };
        tx.commit().await?;
        Ok((record, created))
    }
    pub async fn config_patch(
        &self,
        registry: &ConfigRegistry,
        key: &str,
        patch: Value,
    ) -> Result<ConfigRecord> {
        let patch = patch
            .as_object()
            .ok_or(Error::Invalid("patch must be an object"))?;
        let mut tx = self.pool().begin().await?;
        let mut record: ConfigRecord = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM inkcre.configs WHERE key=$1 FOR UPDATE"
        ))
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound)?;
        record
            .value
            .as_object_mut()
            .ok_or(Error::Invalid("persisted config must be an object"))?
            .extend(patch.clone());
        let value = registry.normalize(&record.schema_id, record.value)?;
        let result = sqlx::query_as(&format!(
            "UPDATE inkcre.configs SET value=$2 WHERE key=$1 RETURNING {COLUMNS}"
        ))
        .bind(key)
        .bind(value)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result)
    }
    pub async fn config_delete(&self, key: &str) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM inkcre.configs WHERE key=$1")
            .bind(key)
            .execute(self.pool())
            .await?
            .rows_affected()
            > 0)
    }
}
