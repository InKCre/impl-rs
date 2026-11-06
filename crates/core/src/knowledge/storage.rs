use super::{Error, Result, StorageDescriptor, StorageHandler, WritableStorageHandler};
use crate::{Core, Storage};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgConnection;
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

pub(crate) fn handlers() -> Vec<Arc<dyn StorageHandler>> {
    vec![Arc::new(PostgreSqlStorage), Arc::new(HttpStorage)]
}

#[derive(Serialize, Deserialize)]
struct PostgreSqlPointer {
    blob_id: Uuid,
}

fn blob_id(pointer: &str) -> Result<Uuid> {
    serde_json::from_str::<PostgreSqlPointer>(pointer)
        .map(|value| value.blob_id)
        .map_err(|_| Error::InvalidInput("invalid PostgreSQL blob pointer"))
}

pub struct PostgreSqlStorage;

#[async_trait]
impl StorageHandler for PostgreSqlStorage {
    fn descriptor(&self) -> StorageDescriptor {
        StorageDescriptor {
            id: "postgresql_binary".into(),
            description: "Deployment-owned PostgreSQL bytes".into(),
            config_schema: json!({"type":"object","additionalProperties":false}),
            writable: true,
        }
    }
    async fn read(&self, core: &Core, _storage: &Storage, pointer: &str) -> Result<Vec<u8>> {
        core.blob_read(blob_id(pointer)?)
            .await?
            .ok_or_else(|| crate::Error::NotFound("storage blob").into())
    }
    async fn read_in(
        &self,
        _core: &Core,
        connection: &mut PgConnection,
        _storage: &Storage,
        pointer: &str,
    ) -> Result<Vec<u8>> {
        sqlx::query_scalar("SELECT data FROM inkcre.storage_blobs WHERE id=$1")
            .bind(blob_id(pointer)?)
            .fetch_optional(connection)
            .await?
            .ok_or_else(|| crate::Error::NotFound("storage blob").into())
    }
    fn writable(&self) -> Option<&dyn WritableStorageHandler> {
        Some(self)
    }
}

#[async_trait]
impl WritableStorageHandler for PostgreSqlStorage {
    async fn create(
        &self,
        _core: &Core,
        connection: &mut PgConnection,
        _storage: &Storage,
        data: &[u8],
    ) -> Result<String> {
        let id: Uuid =
            sqlx::query_scalar("INSERT INTO inkcre.storage_blobs(data) VALUES($1) RETURNING id")
                .bind(data)
                .fetch_one(connection)
                .await?;
        Ok(json!({"blob_id":id}).to_string())
    }
    async fn update(
        &self,
        _core: &Core,
        connection: &mut PgConnection,
        _storage: &Storage,
        pointer: &str,
        data: &[u8],
    ) -> Result<bool> {
        Ok(
            sqlx::query("UPDATE inkcre.storage_blobs SET data=$2 WHERE id=$1")
                .bind(blob_id(pointer)?)
                .bind(data)
                .execute(connection)
                .await?
                .rows_affected()
                > 0,
        )
    }
    async fn delete(
        &self,
        _core: &Core,
        connection: &mut PgConnection,
        _storage: &Storage,
        pointer: &str,
    ) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM inkcre.storage_blobs WHERE id=$1")
            .bind(blob_id(pointer)?)
            .execute(connection)
            .await?
            .rows_affected()
            > 0)
    }
}

/// Configuration is parsed at execution; persisted-row reads do not require this handler.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpStorageConfig {
    pub timeout: u64,
    pub follow_redirects: bool,
    pub max_response_bytes: usize,
}

impl Default for HttpStorageConfig {
    fn default() -> Self {
        Self {
            timeout: 30,
            follow_redirects: true,
            max_response_bytes: 64 * 1024 * 1024,
        }
    }
}

pub struct HttpStorage;

fn http_url(pointer: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(pointer.trim())
        .map_err(|_| Error::InvalidInput("invalid HTTP pointer"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(Error::InvalidInput(
            "HTTP pointer must use http:// or https://",
        ));
    }
    Ok(url)
}

#[async_trait]
impl StorageHandler for HttpStorage {
    fn descriptor(&self) -> StorageDescriptor {
        StorageDescriptor {
            id: "http".into(),
            description: "Bounded HTTP(S) bytes without content interpretation".into(),
            config_schema: json!({"type":"object","properties":{"timeout":{"type":"integer","minimum":1,"default":30},"follow_redirects":{"type":"boolean","default":true},"max_response_bytes":{"type":"integer","minimum":1,"default":67108864}},"additionalProperties":false}),
            writable: false,
        }
    }
    async fn read(&self, _core: &Core, storage: &Storage, pointer: &str) -> Result<Vec<u8>> {
        let config: HttpStorageConfig = serde_json::from_value(storage.config.clone())
            .map_err(|_| Error::InvalidInput("invalid HTTP Storage configuration"))?;
        if config.timeout == 0 || config.max_response_bytes == 0 {
            return Err(Error::InvalidInput("HTTP Storage limits must be positive"));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout))
            .redirect(if config.follow_redirects {
                reqwest::redirect::Policy::limited(10)
            } else {
                reqwest::redirect::Policy::none()
            })
            .build()
            .map_err(|_| Error::Storage("HTTP client construction failed"))?;
        let mut response = client
            .get(http_url(pointer)?)
            .send()
            .await
            .map_err(|_| Error::Storage("HTTP retrieval failed"))?
            .error_for_status()
            .map_err(|_| Error::Storage("HTTP retrieval returned an error status"))?;
        if !response.status().is_success() {
            return Err(Error::Storage(
                "HTTP retrieval did not return successful content",
            ));
        }
        if response
            .content_length()
            .is_some_and(|length| length > config.max_response_bytes as u64)
        {
            return Err(Error::Limit("HTTP response declares too many bytes"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Error::Storage("HTTP body retrieval failed"))?
        {
            if chunk.len() > config.max_response_bytes.saturating_sub(bytes.len()) {
                return Err(Error::Limit("HTTP response exceeds configured bytes"));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
    fn transfer_url(&self, _storage: &Storage, pointer: &str) -> Result<Option<String>> {
        Ok(Some(http_url(pointer)?.to_string()))
    }
}
