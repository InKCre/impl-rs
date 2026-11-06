use crate::{Capability, Core, Error, Page, Peer, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::BTreeSet;
use uuid::Uuid;

const PEER_COLUMNS: &str = "id,name,application_version,labels,config,config_schema,capabilities,lease_expires_at,created_at,updated_at,COALESCE(lease_expires_at>statement_timestamp(),false) AS lease_active";

impl Core {
    /// Refresh application-owned facts while preserving Human name/config/labels.
    pub async fn peer_register(
        &self,
        id: Uuid,
        name: &str,
        application_version: &str,
        config_schema: &Value,
    ) -> Result<Peer> {
        if id.is_nil()
            || name.trim().is_empty()
            || application_version.trim().is_empty()
            || !config_schema.is_object()
        {
            return Err(Error::InvalidInput(
                "peer identity, name, application version and object schema are required".into(),
            ));
        }
        Ok(sqlx::query_as(&format!("INSERT INTO inkcre.peers(id,name,application_version,config_schema) VALUES($1,$2,$3,$4) ON CONFLICT(id) DO UPDATE SET application_version=EXCLUDED.application_version,config_schema=EXCLUDED.config_schema RETURNING {PEER_COLUMNS}"))
            .bind(id).bind(name).bind(application_version).bind(config_schema).fetch_one(self.pool()).await?)
    }
    pub async fn peer_get(&self, id: Uuid) -> Result<Option<Peer>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {PEER_COLUMNS} FROM inkcre.peers WHERE id=$1"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await?)
    }
    pub async fn peer_list(&self, limit: i64, cursor: Option<Uuid>) -> Result<Page<Peer, Uuid>> {
        crate::graph::page_limit(limit)?;
        let mut items:Vec<Peer>=sqlx::query_as(&format!("SELECT {PEER_COLUMNS} FROM inkcre.peers WHERE ($1::uuid IS NULL OR id>$1) ORDER BY id LIMIT $2"))
            .bind(cursor).bind(limit+1).fetch_all(self.pool()).await?;
        let next_cursor = if items.len() > limit as usize {
            items.truncate(limit as usize);
            items.last().map(|x| x.id)
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }
    pub async fn peer_publish_capabilities(
        &self,
        id: Uuid,
        capabilities: &[Capability],
    ) -> Result<()> {
        let mut names = BTreeSet::new();
        for capability in capabilities {
            if capability.id.trim().is_empty()
                || !names.insert(&capability.id)
                || capability.inbound.protocol.trim().is_empty()
                || !capability.inbound.parameters.is_object()
            {
                return Err(Error::InvalidInput(
                    "capability IDs must be unique and inbound parameters must be objects".into(),
                ));
            }
        }
        let mut capabilities = capabilities.to_vec();
        capabilities.sort_by(|a, b| a.id.cmp(&b.id));
        let value = serde_json::to_value(capabilities)
            .map_err(|_| Error::InvalidInput("invalid capability snapshot".into()))?;
        let changed = sqlx::query("UPDATE inkcre.peers SET capabilities=$2 WHERE id=$1")
            .bind(id)
            .bind(value)
            .execute(self.pool())
            .await?
            .rows_affected();
        if changed == 0 {
            return Err(Error::NotFound("peer"));
        }
        Ok(())
    }
    pub async fn peer_renew_lease(&self, id: Uuid, ttl_seconds: i32) -> Result<DateTime<Utc>> {
        if ttl_seconds <= 0 {
            return Err(Error::InvalidInput(
                "peer lease TTL must be positive".into(),
            ));
        }
        if self.peer_get(id).await?.is_none() {
            return Err(Error::NotFound("peer"));
        }
        Ok(sqlx::query_scalar("SELECT inkcre.renew_peer_lease($1,$2)")
            .bind(id)
            .bind(ttl_seconds)
            .fetch_one(self.pool())
            .await?)
    }
    pub async fn peer_clear_lease(&self, id: Uuid) -> Result<()> {
        sqlx::query("UPDATE inkcre.peers SET lease_expires_at=NULL WHERE id=$1")
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(())
    }
    pub async fn peer_delete(&self, id: Uuid) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM inkcre.peers WHERE id=$1")
            .bind(id)
            .execute(self.pool())
            .await?
            .rows_affected()
            > 0)
    }
}
