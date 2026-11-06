use crate::{Core, Error, Job, JobInput, JobStatus, JobType, Page, Result};
use serde_json::Value;

const JOB_COLUMNS: &str = "id,type,parameters,state,timeout_seconds,status::text AS status,created_at,started_at,closed_at,abort_requested,submission_traceparent,submission_tracestate";

impl Core {
    pub async fn job_type_get(&self, id: &str) -> Result<Option<JobType>> {
        Ok(sqlx::query_as("SELECT id,description,parameters_schema,default_timeout_seconds FROM inkcre.job_types WHERE id=$1").bind(id).fetch_optional(self.pool()).await?)
    }
    pub async fn job_types(
        &self,
        limit: i64,
        cursor: Option<&str>,
    ) -> Result<Page<JobType, String>> {
        crate::graph::page_limit(limit)?;
        let mut items:Vec<JobType>=sqlx::query_as("SELECT id,description,parameters_schema,default_timeout_seconds FROM inkcre.job_types WHERE ($1::text IS NULL OR id>$1) ORDER BY id LIMIT $2")
            .bind(cursor).bind(limit+1).fetch_all(self.pool()).await?;
        let next_cursor = if items.len() > limit as usize {
            items.truncate(limit as usize);
            items.last().map(|x| x.id.clone())
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }
    /// Publish only exact Job types owned by this runtime or its extensions.
    pub async fn job_type_upsert(&self, job_type: &JobType) -> Result<()> {
        if job_type.id.trim().is_empty()
            || job_type.default_timeout_seconds <= 0
            || !job_type.parameters_schema.is_object()
        {
            return Err(Error::InvalidInput("invalid job type contract".into()));
        }
        jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .build(&job_type.parameters_schema)
            .map_err(|_| Error::InvalidInput("invalid job parameters schema".into()))?;
        sqlx::query("INSERT INTO inkcre.job_types(id,description,parameters_schema,default_timeout_seconds) VALUES($1,$2,$3,$4) ON CONFLICT(id) DO UPDATE SET description=EXCLUDED.description,parameters_schema=EXCLUDED.parameters_schema,default_timeout_seconds=EXCLUDED.default_timeout_seconds")
            .bind(&job_type.id).bind(&job_type.description).bind(&job_type.parameters_schema).bind(job_type.default_timeout_seconds).execute(self.pool()).await?;
        Ok(())
    }
    /// Accept any catalogued exact type, independently of local handler availability.
    pub async fn job_submit(&self, input: JobInput) -> Result<Job> {
        if !input.parameters.is_object() {
            return Err(Error::InvalidInput(
                "job parameters must be an object".into(),
            ));
        }
        if [&input.submission_traceparent, &input.submission_tracestate]
            .iter()
            .any(|x| x.as_ref().is_some_and(|s| s.len() > 512))
        {
            return Err(Error::InvalidInput(
                "submission trace values exceed capacity".into(),
            ));
        }
        let mut tx = self.pool().begin().await?;
        let job_type:JobType=sqlx::query_as("SELECT id,description,parameters_schema,default_timeout_seconds FROM inkcre.job_types WHERE id=$1 FOR SHARE")
            .bind(&input.type_).fetch_optional(&mut *tx).await?.ok_or(Error::NotFound("job type"))?;
        crate::models::validate_json(&job_type.parameters_schema, &input.parameters)?;
        let timeout = input
            .timeout_seconds
            .unwrap_or(job_type.default_timeout_seconds);
        if timeout <= 0 {
            return Err(Error::InvalidInput("job timeout must be positive".into()));
        }
        let job=sqlx::query_as(&format!("INSERT INTO inkcre.jobs(type,parameters,timeout_seconds,submission_traceparent,submission_tracestate) VALUES($1,$2,$3,$4,$5) RETURNING {JOB_COLUMNS}"))
            .bind(input.type_).bind(input.parameters).bind(timeout).bind(input.submission_traceparent).bind(input.submission_tracestate).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(job)
    }
    pub async fn job_get(&self, id: i64) -> Result<Option<Job>> {
        Ok(sqlx::query_as(&format!(
            "SELECT {JOB_COLUMNS} FROM inkcre.jobs WHERE id=$1"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await?)
    }
    pub async fn jobs(
        &self,
        limit: i64,
        cursor: Option<i64>,
        type_: Option<&str>,
        status: Option<JobStatus>,
    ) -> Result<Page<Job, i64>> {
        crate::graph::page_limit(limit)?;
        let mut items:Vec<Job>=sqlx::query_as(&format!("SELECT {JOB_COLUMNS} FROM inkcre.jobs WHERE ($1::bigint IS NULL OR id<$1) AND ($2::text IS NULL OR type=$2) AND ($3::text IS NULL OR status::text=$3) ORDER BY id DESC LIMIT $4"))
            .bind(cursor).bind(type_).bind(status).bind(limit+1).fetch_all(self.pool()).await?;
        let next_cursor = if items.len() > limit as usize {
            items.truncate(limit as usize);
            items.last().map(|x| x.id)
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }
    /// The caller must first decide it can execute this exact command.
    pub async fn job_claim(&self, id: i64) -> Result<Option<Job>> {
        Ok(sqlx::query_as(&format!("UPDATE inkcre.jobs SET status='running' WHERE id=$1 AND status='pending' RETURNING {JOB_COLUMNS}"))
            .bind(id).fetch_optional(self.pool()).await?)
    }
    pub async fn job_finish(&self, id: i64, status: JobStatus, state: &Value) -> Result<bool> {
        if !status.terminal() || !state.is_object() {
            return Err(Error::InvalidInput(
                "terminal job status and object state are required".into(),
            ));
        }
        Ok(sqlx::query("UPDATE inkcre.jobs SET status=$2::text::inkcre.jobstatus,state=$3 WHERE id=$1 AND status='running'")
            .bind(id).bind(status).bind(state).execute(self.pool()).await?.rows_affected()>0)
    }
    pub async fn job_abort(&self, id: i64) -> Result<Job> {
        let mut tx = self.pool().begin().await?;
        sqlx::query("UPDATE inkcre.jobs SET abort_requested=true,status=CASE WHEN status='pending' THEN 'aborted'::inkcre.jobstatus ELSE status END WHERE id=$1 AND status IN ('pending','running')")
            .bind(id).execute(&mut *tx).await?;
        let job = sqlx::query_as(&format!(
            "SELECT {JOB_COLUMNS} FROM inkcre.jobs WHERE id=$1"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound("job"))?;
        tx.commit().await?;
        Ok(job)
    }
    pub async fn job_abort_requested(&self, id: i64) -> Result<bool> {
        sqlx::query_scalar("SELECT abort_requested FROM inkcre.jobs WHERE id=$1")
            .bind(id)
            .fetch_optional(self.pool())
            .await?
            .ok_or(Error::NotFound("job"))
    }
    pub async fn jobs_expire_overdue(&self) -> Result<u64> {
        Ok(sqlx::query("UPDATE inkcre.jobs SET status='timed_out' WHERE status='running' AND started_at+timeout_seconds*interval '1 second'<=statement_timestamp()")
            .execute(self.pool()).await?.rows_affected())
    }
    /// Remove only a terminal/pending record; executing work must first be aborted.
    pub async fn job_delete(&self, id: i64) -> Result<bool> {
        let result = sqlx::query("DELETE FROM inkcre.jobs WHERE id=$1 AND status<>'running'")
            .bind(id)
            .execute(self.pool())
            .await?;
        if result.rows_affected() == 0 && self.job_get(id).await?.is_some() {
            return Err(Error::Conflict("running job cannot be deleted".into()));
        }
        Ok(result.rows_affected() > 0)
    }
    pub async fn job_type_delete(&self, id: &str) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM inkcre.job_types WHERE id=$1")
            .bind(id)
            .execute(self.pool())
            .await?
            .rows_affected()
            > 0)
    }
}
