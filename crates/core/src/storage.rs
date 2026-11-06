use crate::{Core, Error, HydratedContent, Result, Storage};
use uuid::Uuid;

impl Core {
    pub async fn storages(&self) -> Result<Vec<Storage>> {
        Ok(
            sqlx::query_as("SELECT id,type,nickname,config FROM inkcre.storages ORDER BY id")
                .fetch_all(self.pool())
                .await?,
        )
    }
    pub async fn storage_get(&self, id: i32) -> Result<Option<Storage>> {
        Ok(
            sqlx::query_as("SELECT id,type,nickname,config FROM inkcre.storages WHERE id=$1")
                .bind(id)
                .fetch_optional(self.pool())
                .await?,
        )
    }
    pub async fn blob_create(&self, data: &[u8]) -> Result<Uuid> {
        Ok(
            sqlx::query_scalar("INSERT INTO inkcre.storage_blobs(data) VALUES($1) RETURNING id")
                .bind(data)
                .fetch_one(self.pool())
                .await?,
        )
    }
    pub async fn blob_read(&self, id: Uuid) -> Result<Option<Vec<u8>>> {
        Ok(
            sqlx::query_scalar("SELECT data FROM inkcre.storage_blobs WHERE id=$1")
                .bind(id)
                .fetch_optional(self.pool())
                .await?,
        )
    }
    pub async fn blob_update(&self, id: Uuid, data: &[u8]) -> Result<bool> {
        Ok(
            sqlx::query("UPDATE inkcre.storage_blobs SET data=$2 WHERE id=$1")
                .bind(id)
                .bind(data)
                .execute(self.pool())
                .await?
                .rows_affected()
                > 0,
        )
    }
    pub async fn blob_delete(&self, id: Uuid) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM inkcre.storage_blobs WHERE id=$1")
            .bind(id)
            .execute(self.pool())
            .await?
            .rows_affected()
            > 0)
    }
    /// Pointer grammar is owned by the PostgreSQL storage implementation.
    pub fn postgresql_blob_pointer(id: Uuid) -> String {
        serde_json::json!({"blob_id":id}).to_string()
    }
    /// Hydrate through this Core's exact Storage registry without loading a Resolver.
    pub async fn block_content(&self, id: i32) -> Result<HydratedContent> {
        let block = self.block_get(id).await?.ok_or(Error::NotFound("block"))?;
        self.knowledge()
            .actual_content(&block)
            .await
            .map_err(Into::into)
    }
}
