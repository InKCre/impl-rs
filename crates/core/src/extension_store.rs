use crate::{Core, Error, Extension, Participation, Result};
use serde_json::Value;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

pub(crate) fn extension_name(name: &str) -> Result<()> {
    let parts: Vec<_> = name.split('/').collect();
    let valid = parts.len() == 2
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.len() <= 64
                && part
                    .bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
                && part.as_bytes()[0] != b'-'
                && part.as_bytes()[part.len() - 1] != b'-'
        });
    if !valid {
        return Err(Error::InvalidInput(
            "extension must be a canonical namespace/name".into(),
        ));
    }
    Ok(())
}

async fn lock(tx: &mut Transaction<'_, Postgres>, name: &str) -> Result<()> {
    let locked: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtextextended($1,0))")
            .bind(name)
            .fetch_one(&mut **tx)
            .await?;
    if !locked {
        return Err(Error::Conflict(
            "another extension operation is active".into(),
        ));
    }
    Ok(())
}

impl Core {
    pub async fn extensions(&self) -> Result<Vec<Extension>> {
        Ok(
            sqlx::query_as(
                "SELECT name,nickname,config,state FROM inkcre.extensions ORDER BY name",
            )
            .fetch_all(self.pool())
            .await?,
        )
    }
    pub async fn extension_get(&self, name: &str) -> Result<Option<Extension>> {
        Ok(
            sqlx::query_as(
                "SELECT name,nickname,config,state FROM inkcre.extensions WHERE name=$1",
            )
            .bind(name)
            .fetch_optional(self.pool())
            .await?,
        )
    }
    /// Ensure the deployment-wide identity without replacing config or producer state.
    pub async fn extension_ensure(&self, name: &str) -> Result<Extension> {
        extension_name(name)?;
        let mut tx = self.pool().begin().await?;
        lock(&mut tx, name).await?;
        sqlx::query("INSERT INTO inkcre.extensions(name) VALUES($1) ON CONFLICT(name) DO NOTHING")
            .bind(name)
            .execute(&mut *tx)
            .await?;
        let extension = sqlx::query_as(
            "SELECT name,nickname,config,state FROM inkcre.extensions WHERE name=$1",
        )
        .bind(name)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(extension)
    }
    /// Structural patch only. Use `extension_patch_config_for_peer` for exact-schema admission.
    pub async fn extension_patch_config(&self, name: &str, patch: &Value) -> Result<Extension> {
        self.patch_config(name, None, patch).await
    }
    /// Validate against this Peer's exact selected Distribution; never merge schemas.
    pub async fn extension_patch_config_for_peer(
        &self,
        name: &str,
        peer_id: Uuid,
        patch: &Value,
    ) -> Result<Extension> {
        self.patch_config(name, Some(peer_id), patch).await
    }
    async fn patch_config(
        &self,
        name: &str,
        peer_id: Option<Uuid>,
        patch: &Value,
    ) -> Result<Extension> {
        let patch = patch.as_object().ok_or_else(|| {
            Error::InvalidInput("extension config patch must be an object".into())
        })?;
        let mut tx = self.pool().begin().await?;
        lock(&mut tx, name).await?;
        let mut extension: Extension = sqlx::query_as(
            "SELECT name,nickname,config,state FROM inkcre.extensions WHERE name=$1 FOR UPDATE",
        )
        .bind(name)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound("extension"))?;
        let config = extension.config.as_object_mut().ok_or_else(|| {
            Error::InvalidInput("persisted extension config is not an object".into())
        })?;
        config.extend(patch.clone());
        if let Some(peer_id) = peer_id {
            let selected:Participation=sqlx::query_as("SELECT name,peer_id,distribution,version,enabled,config_schema FROM inkcre.extension_peer_participations WHERE name=$1 AND peer_id=$2 FOR SHARE")
                .bind(name).bind(peer_id).fetch_optional(&mut *tx).await?.ok_or(Error::NotFound("extension participation"))?;
            let schema = selected.config_schema.ok_or_else(|| {
                Error::Conflict(
                    "the exact selected distribution has no resolved configuration schema".into(),
                )
            })?;
            crate::models::validate_json(&schema, &extension.config)?;
        }
        sqlx::query("UPDATE inkcre.extensions SET config=$2 WHERE name=$1")
            .bind(name)
            .bind(&extension.config)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(extension)
    }
    /// Replace the complete producer object only when the caller owns it exclusively.
    /// Concurrent producers should use `extension_patch_state` to preserve other keys.
    pub async fn extension_set_state(&self, name: &str, state: &Value) -> Result<()> {
        if !state.is_object() {
            return Err(Error::InvalidInput(
                "extension state must be an object".into(),
            ));
        }
        let affected = sqlx::query("UPDATE inkcre.extensions SET state=$2 WHERE name=$1")
            .bind(name)
            .bind(state)
            .execute(self.pool())
            .await?
            .rows_affected();
        if affected == 0 {
            return Err(Error::NotFound("extension"));
        }
        Ok(())
    }
    /// Atomically merge top-level producer keys without copying an old state snapshot.
    pub async fn extension_patch_state(&self, name: &str, patch: &Value) -> Result<()> {
        if !patch.is_object() {
            return Err(Error::InvalidInput(
                "extension state patch must be an object".into(),
            ));
        }
        let affected =
            sqlx::query("UPDATE inkcre.extensions SET state=state||$2::jsonb WHERE name=$1")
                .bind(name)
                .bind(patch)
                .execute(self.pool())
                .await?
                .rows_affected();
        if affected == 0 {
            return Err(Error::NotFound("extension"));
        }
        Ok(())
    }
    pub async fn participations(&self, peer_id: Uuid) -> Result<Vec<Participation>> {
        Ok(sqlx::query_as("SELECT name,peer_id,distribution,version,enabled,config_schema FROM inkcre.extension_peer_participations WHERE peer_id=$1 ORDER BY name").bind(peer_id).fetch_all(self.pool()).await?)
    }
    pub async fn participation_get(
        &self,
        name: &str,
        peer_id: Uuid,
    ) -> Result<Option<Participation>> {
        Ok(sqlx::query_as("SELECT name,peer_id,distribution,version,enabled,config_schema FROM inkcre.extension_peer_participations WHERE name=$1 AND peer_id=$2").bind(name).bind(peer_id).fetch_optional(self.pool()).await?)
    }
    pub async fn participation_select(
        &self,
        name: &str,
        peer_id: Uuid,
        distribution: &str,
        version: &str,
        config_schema: Option<&Value>,
    ) -> Result<Participation> {
        extension_name(name)?;
        let version_parsed = semver::Version::parse(version)
            .map_err(|_| Error::InvalidInput("extension version must be semver".into()))?;
        if !version_parsed.build.is_empty()
            || distribution.trim().is_empty()
            || config_schema.is_some_and(|schema| !schema.is_object())
        {
            return Err(Error::InvalidInput(
                "invalid native distribution contract".into(),
            ));
        }
        if let Some(schema) = config_schema {
            jsonschema::options()
                .with_draft(jsonschema::Draft::Draft202012)
                .build(schema)
                .map_err(|_| {
                    Error::InvalidInput("invalid extension configuration schema".into())
                })?;
        }
        let mut tx = self.pool().begin().await?;
        lock(&mut tx, name).await?;
        sqlx::query("INSERT INTO inkcre.extensions(name) VALUES($1) ON CONFLICT(name) DO NOTHING")
            .bind(name)
            .execute(&mut *tx)
            .await?;
        let existing:Option<Participation>=sqlx::query_as("SELECT name,peer_id,distribution,version,enabled,config_schema FROM inkcre.extension_peer_participations WHERE name=$1 AND peer_id=$2 FOR UPDATE")
            .bind(name).bind(peer_id).fetch_optional(&mut *tx).await?;
        if existing.as_ref().is_some_and(|p| {
            p.enabled && (p.distribution.as_deref() != Some(distribution) || p.version != version)
        }) {
            return Err(Error::Conflict(
                "disable the extension before changing its exact selection".into(),
            ));
        }
        let participation=sqlx::query_as("INSERT INTO inkcre.extension_peer_participations(name,peer_id,distribution,version,config_schema) VALUES($1,$2,$3,$4,$5) ON CONFLICT(name,peer_id) DO UPDATE SET config_schema=CASE WHEN extension_peer_participations.distribution=EXCLUDED.distribution AND extension_peer_participations.version=EXCLUDED.version THEN COALESCE(EXCLUDED.config_schema,extension_peer_participations.config_schema) ELSE EXCLUDED.config_schema END,distribution=EXCLUDED.distribution,version=EXCLUDED.version RETURNING name,peer_id,distribution,version,enabled,config_schema")
            .bind(name).bind(peer_id).bind(distribution).bind(version).bind(config_schema).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(participation)
    }
    /// The host may publish a schema only for the exact selection it resolved.
    pub async fn participation_update_schema(
        &self,
        name: &str,
        peer_id: Uuid,
        distribution: &str,
        version: &str,
        schema: Value,
    ) -> Result<bool> {
        if !schema.is_object() {
            return Err(Error::InvalidInput(
                "config schema must be an object".into(),
            ));
        }
        Ok(sqlx::query("UPDATE inkcre.extension_peer_participations SET config_schema=$5 WHERE name=$1 AND peer_id=$2 AND distribution=$3 AND version=$4 AND enabled=true")
            .bind(name).bind(peer_id).bind(distribution).bind(version).bind(schema).execute(self.pool()).await?.rows_affected()>0)
    }
    pub async fn participation_set_enabled(
        &self,
        name: &str,
        peer_id: Uuid,
        enabled: bool,
    ) -> Result<Participation> {
        let mut tx = self.pool().begin().await?;
        lock(&mut tx, name).await?;
        let existing:Participation=sqlx::query_as("SELECT name,peer_id,distribution,version,enabled,config_schema FROM inkcre.extension_peer_participations WHERE name=$1 AND peer_id=$2 FOR UPDATE")
            .bind(name).bind(peer_id).fetch_optional(&mut *tx).await?.ok_or(Error::NotFound("extension participation"))?;
        if enabled {
            if existing.distribution.is_none() {
                return Err(Error::Conflict(
                    "resolve an exact distribution before enabling".into(),
                ));
            }
            let config: Value =
                sqlx::query_scalar("SELECT config FROM inkcre.extensions WHERE name=$1 FOR UPDATE")
                    .bind(name)
                    .fetch_one(&mut *tx)
                    .await?;
            if let Some(schema) = existing.config_schema.as_ref() {
                crate::models::validate_json(schema, &config)?;
            }
        }
        let row=sqlx::query_as("UPDATE inkcre.extension_peer_participations SET enabled=$3 WHERE name=$1 AND peer_id=$2 RETURNING name,peer_id,distribution,version,enabled,config_schema")
            .bind(name).bind(peer_id).bind(enabled).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(row)
    }
    /// Unselect is Peer-local and preserves shared identity, config, and producer state.
    pub async fn participation_delete(&self, name: &str, peer_id: Uuid) -> Result<bool> {
        let mut tx = self.pool().begin().await?;
        lock(&mut tx, name).await?;
        let row:Option<bool>=sqlx::query_scalar("SELECT enabled FROM inkcre.extension_peer_participations WHERE name=$1 AND peer_id=$2 FOR UPDATE").bind(name).bind(peer_id).fetch_optional(&mut *tx).await?;
        if row == Some(true) {
            return Err(Error::Conflict(
                "disable the extension before unselecting".into(),
            ));
        }
        let deleted = sqlx::query(
            "DELETE FROM inkcre.extension_peer_participations WHERE name=$1 AND peer_id=$2",
        )
        .bind(name)
        .bind(peer_id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        tx.commit().await?;
        Ok(deleted)
    }
}
