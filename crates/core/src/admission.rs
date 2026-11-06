use crate::{Admission, Core, Error, Result};

pub const CONTRACT_REVISION: &str = "extension-distribution-open-v1";

impl Core {
    /// Read-only admission checks use runtime authority, never migration credentials.
    pub async fn admission(&self) -> Result<Admission> {
        let mut transaction = self.pool().begin().await?;
        sqlx::query("SET TRANSACTION READ ONLY")
            .execute(&mut *transaction)
            .await?;
        let (revision, environment, owner): (String, String, String) = sqlx::query_as(
            "SELECT contract_revision, environment, owner_role FROM inkcre_internal.contract_state WHERE singleton",
        ).fetch_optional(&mut *transaction).await
            .map_err(|_| Error::Admission("contract state is unavailable".into()))?
            .ok_or_else(|| Error::Admission("contract state is missing".into()))?;
        if revision != CONTRACT_REVISION {
            return Err(Error::Admission("unsupported contract revision".into()));
        }
        let (login, inherit, privileged, member, is_owner): (bool, bool, bool, bool, bool) = sqlx::query_as(
            "SELECT rolcanlogin, rolinherit, rolsuper OR rolcreatedb OR rolcreaterole OR rolreplication OR rolbypassrls, pg_has_role(current_user,'authenticated','USAGE'), current_user=$1 FROM pg_roles WHERE rolname=current_user",
        ).bind(owner).fetch_one(&mut *transaction).await?;
        if !login || !inherit || privileged || !member || is_owner {
            return Err(Error::Admission(
                "a non-administrative native Peer login inheriting authenticated is required"
                    .into(),
            ));
        }
        let required = [
            ("blocks", "id"),
            ("blocks", "resolver"),
            ("blocks", "content"),
            ("blocks", "storage"),
            ("blocks", "created_at"),
            ("blocks", "updated_at"),
            ("relations", "id"),
            ("relations", "from_"),
            ("relations", "to_"),
            ("relations", "content"),
            ("relations", "updated_at"),
            ("storage_blobs", "id"),
            ("storage_blobs", "data"),
            ("storages", "id"),
            ("storages", "type"),
            ("storages", "config"),
            ("storages", "nickname"),
            ("peers", "id"),
            ("peers", "name"),
            ("peers", "application_version"),
            ("peers", "labels"),
            ("peers", "config"),
            ("peers", "config_schema"),
            ("peers", "capabilities"),
            ("peers", "lease_expires_at"),
            ("peers", "created_at"),
            ("peers", "updated_at"),
            ("jobs", "id"),
            ("jobs", "type"),
            ("jobs", "parameters"),
            ("jobs", "state"),
            ("jobs", "status"),
            ("jobs", "abort_requested"),
            ("jobs", "submission_traceparent"),
            ("jobs", "submission_tracestate"),
            ("jobs", "timeout_seconds"),
            ("jobs", "created_at"),
            ("jobs", "started_at"),
            ("jobs", "closed_at"),
            ("job_types", "id"),
            ("job_types", "description"),
            ("job_types", "parameters_schema"),
            ("job_types", "default_timeout_seconds"),
            ("extensions", "name"),
            ("extensions", "nickname"),
            ("extensions", "config"),
            ("extensions", "state"),
            ("extension_peer_participations", "name"),
            ("extension_peer_participations", "peer_id"),
            ("extension_peer_participations", "distribution"),
            ("extension_peer_participations", "version"),
            ("extension_peer_participations", "enabled"),
            ("extension_peer_participations", "config_schema"),
        ];
        let columns: Vec<(String, String)> = sqlx::query_as(
            "SELECT table_name,column_name FROM information_schema.columns WHERE table_schema='inkcre'",
        ).fetch_all(&mut *transaction).await?;
        if required
            .iter()
            .any(|(table, column)| !columns.iter().any(|(t, c)| t == table && c == column))
        {
            return Err(Error::Admission(
                "required protocol columns are unavailable".into(),
            ));
        }
        let tables = [
            "blocks",
            "relations",
            "storage_blobs",
            "storages",
            "storage_types",
            "peers",
            "jobs",
            "job_types",
            "extensions",
            "extension_peer_participations",
        ];
        let names: Vec<_> = tables.iter().map(|name| format!("inkcre.{name}")).collect();
        let allowed:bool=sqlx::query_scalar("SELECT has_schema_privilege(current_user,'inkcre','USAGE') AND bool_and(has_table_privilege(current_user,name,'SELECT') AND has_table_privilege(current_user,name,'INSERT') AND has_table_privilege(current_user,name,'DELETE') AND (name='inkcre.extensions' OR has_table_privilege(current_user,name,'UPDATE'))) FROM unnest($1::text[]) AS relation(name)")
            .bind(names).fetch_one(&mut *transaction).await?;
        let (config,state,sequence):(bool,bool,bool)=sqlx::query_as("SELECT has_column_privilege(current_user,'inkcre.extensions','config','UPDATE'),has_column_privilege(current_user,'inkcre.extensions','state','UPDATE'),(SELECT bool_and(COALESCE(has_sequence_privilege(current_user,pg_get_serial_sequence(name,'id'),'USAGE'),false)) FROM unnest(ARRAY['inkcre.blocks','inkcre.relations','inkcre.jobs']) AS relation(name))")
            .fetch_one(&mut *transaction).await?;
        if !allowed || !config || !state || !sequence {
            return Err(Error::Admission(
                "required protocol privileges are unavailable".into(),
            ));
        }
        for signature in [
            "inkcre.renew_peer_lease(uuid,integer)",
            "inkcre.create_storage_blob(bytea)",
            "inkcre.read_storage_blob(uuid)",
        ] {
            let allowed: bool = sqlx::query_scalar("SELECT COALESCE(has_function_privilege(current_user,to_regprocedure($1),'EXECUTE'),false)")
                .bind(signature).fetch_one(&mut *transaction).await?;
            if !allowed {
                return Err(Error::Admission(
                    "required protocol functions are unavailable".into(),
                ));
            }
        }
        let migration_heads: Vec<String> = sqlx::query_scalar(
            "SELECT version_num::text FROM public.alembic_version ORDER BY version_num",
        )
        .fetch_all(&mut *transaction)
        .await?;
        if migration_heads.len() != 1
            || !["runtime", "development", "preview", "production"].contains(&environment.as_str())
        {
            return Err(Error::Admission(
                "database lifecycle identity is invalid".into(),
            ));
        }
        transaction.rollback().await?;
        Ok(Admission {
            revision,
            migration_heads,
            environment,
        })
    }
}
