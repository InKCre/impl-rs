//! Bounded metadata-only queries, without reading content or configuration.
use crate::{Core, Error, Result};
use serde_json::Value;
impl Core {
    pub async fn discover_storages(
        &self,
        after: Option<i64>,
        limit: i64,
    ) -> Result<Vec<(i32, String, Option<String>)>> {
        bounds(limit)?;
        Ok(sqlx::query_as("SELECT id,type,nickname FROM inkcre.storages WHERE ($1::bigint IS NULL OR id>$1) ORDER BY id LIMIT $2").bind(after).bind(limit+1).fetch_all(self.pool()).await?)
    }
    pub async fn discover_types(
        &self,
        collection: &str,
        after: Option<&str>,
        limit: i64,
    ) -> Result<Vec<(String, Option<String>)>> {
        bounds(limit)?;
        let sql = match collection {
            "source_types" => {
                "SELECT id,description FROM inkcre.sources_types WHERE ($1::text IS NULL OR id>$1) ORDER BY id LIMIT $2"
            }
            "storage_types" => {
                "SELECT id,description FROM inkcre.storage_types WHERE ($1::text IS NULL OR id>$1) ORDER BY id LIMIT $2"
            }
            _ => return Err(Error::InvalidInput("unknown discovery collection".into())),
        };
        Ok(sqlx::query_as(sql)
            .bind(after)
            .bind(limit + 1)
            .fetch_all(self.pool())
            .await?)
    }
    pub async fn discover_blocks(
        &self,
        resolver: Option<&str>,
        storage: Option<i32>,
        after: Option<i64>,
        limit: i64,
    ) -> Result<Vec<Value>> {
        bounds(limit)?;
        Ok(sqlx::query_scalar("SELECT jsonb_build_object('id',id,'entity','block:'||id,'resolver',resolver,'storage',storage,'created_at',created_at,'updated_at',updated_at) FROM inkcre.blocks WHERE ($1::text IS NULL OR resolver=$1) AND ($2::integer IS NULL OR storage=$2) AND ($3::bigint IS NULL OR id>$3) ORDER BY id LIMIT $4").bind(resolver).bind(storage).bind(after).bind(limit+1).fetch_all(self.pool()).await?)
    }
}
fn bounds(limit: i64) -> Result<()> {
    if !(1..=100).contains(&limit) {
        return Err(Error::InvalidInput(
            "discovery limit must be 1..=100".into(),
        ));
    }
    Ok(())
}
