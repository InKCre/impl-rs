//! 部署级 Cron 只物化当前分钟的普通 Job，不执行业务，也不补跑错过的发生。

use std::time::Duration;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use croner::parser::CronParser;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{FromRow, Postgres, Transaction};
use tokio::{sync::watch, task::JoinHandle, time::MissedTickBehavior};

use crate::{Core, Error, Job, JobInput, JobStatus, JobType, Page, Result};

const COLUMNS: &str = "id,schedule,enabled,job_type,job_parameters,job_timeout_seconds,last_job,last_scheduled_for,created_at,updated_at";
const CONFIG_KEY: &str = "core.cron";
const CONFIG_SCHEMA: &str = "core.cron.config.v1";

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Cron {
    pub id: i64,
    pub schedule: String,
    pub enabled: bool,
    pub job_type: String,
    pub job_parameters: Value,
    pub job_timeout_seconds: Option<i32>,
    pub last_job: Option<i64>,
    pub last_scheduled_for: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CronInput {
    pub schedule: String,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
    pub job_type: String,
    #[serde(default = "crate::models::empty_object")]
    pub job_parameters: Value,
    pub job_timeout_seconds: Option<i32>,
}
fn enabled_default() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CronPatch {
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub schedule: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub enabled: Option<bool>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub job_type: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub job_parameters: Option<Value>,
    #[serde(
        default,
        deserialize_with = "nullable",
        skip_serializing_if = "Option::is_none"
    )]
    pub job_timeout_seconds: Option<Option<i32>>,
}
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
fn nullable<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<Option<i32>>, D::Error> {
    Option::<i32>::deserialize(d).map(Some)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CronConfig {
    #[serde(default = "utc_default")]
    pub timezone: String,
}
fn utc_default() -> String {
    "UTC".into()
}
impl Default for CronConfig {
    fn default() -> Self {
        Self {
            timezone: utc_default(),
        }
    }
}
impl CronConfig {
    pub fn zone(&self) -> Result<Tz> {
        self.timezone.parse().map_err(|_| {
            Error::InvalidInput("Cron timezone must be an installed IANA timezone".into())
        })
    }
}

impl Core {
    pub async fn cron_config_get(&self) -> Result<CronConfig> {
        let row: Option<(String, Value)> =
            sqlx::query_as("SELECT schema,value FROM inkcre.configs WHERE key=$1")
                .bind(CONFIG_KEY)
                .fetch_optional(self.pool())
                .await?;
        let Some((schema, value)) = row else {
            return Ok(CronConfig::default());
        };
        if schema != CONFIG_SCHEMA {
            return Err(Error::Unsupported(
                "unknown Cron configuration schema".into(),
            ));
        }
        let timezone = value
            .get("timezone")
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| Error::InvalidInput("invalid persisted Cron timezone".into()))
            })
            .transpose()?
            .unwrap_or("UTC");
        let config = CronConfig {
            timezone: timezone.into(),
        };
        config.zone()?;
        Ok(config)
    }

    pub async fn cron_config_set(&self, config: &CronConfig) -> Result<()> {
        config.zone()?;
        let value = serde_json::to_value(config)
            .map_err(|_| Error::InvalidInput("invalid Cron configuration".into()))?;
        sqlx::query("INSERT INTO inkcre.configs(key,schema,value) VALUES($1,$2,$3) ON CONFLICT(key) DO UPDATE SET schema=EXCLUDED.schema,value=EXCLUDED.value")
            .bind(CONFIG_KEY).bind(CONFIG_SCHEMA).bind(value).execute(self.pool()).await?;
        Ok(())
    }

    pub async fn cron_get(&self, id: i64) -> Result<Option<Cron>> {
        Ok(
            sqlx::query_as(&format!("SELECT {COLUMNS} FROM inkcre.crons WHERE id=$1"))
                .bind(id)
                .fetch_optional(self.pool())
                .await?,
        )
    }

    pub async fn crons(&self, limit: i64, cursor: Option<i64>) -> Result<Page<Cron, i64>> {
        crate::graph::page_limit(limit)?;
        let mut items:Vec<Cron>=sqlx::query_as(&format!("SELECT {COLUMNS} FROM inkcre.crons WHERE ($1::bigint IS NULL OR id>$1) ORDER BY id LIMIT $2"))
            .bind(cursor).bind(limit+1).fetch_all(self.pool()).await?;
        let next_cursor = if items.len() > limit as usize {
            items.truncate(limit as usize);
            items.last().map(|item| item.id)
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }

    pub async fn cron_create(&self, input: CronInput) -> Result<Cron> {
        let mut tx = self.pool().begin().await?;
        validate_template(&mut tx, &input).await?;
        let cron=sqlx::query_as(&format!("INSERT INTO inkcre.crons(schedule,enabled,job_type,job_parameters,job_timeout_seconds) VALUES($1,$2,$3,$4,$5) RETURNING {COLUMNS}"))
            .bind(input.schedule).bind(input.enabled).bind(input.job_type).bind(input.job_parameters).bind(input.job_timeout_seconds).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(cron)
    }

    pub async fn cron_patch(&self, id: i64, patch: CronPatch) -> Result<Cron> {
        let mut tx = self.pool().begin().await?;
        let row: Cron = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM inkcre.crons WHERE id=$1 FOR UPDATE"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound("Cron"))?;
        let candidate = CronInput {
            schedule: patch.schedule.unwrap_or(row.schedule),
            enabled: patch.enabled.unwrap_or(row.enabled),
            job_type: patch.job_type.unwrap_or(row.job_type),
            job_parameters: patch.job_parameters.unwrap_or(row.job_parameters),
            job_timeout_seconds: patch.job_timeout_seconds.unwrap_or(row.job_timeout_seconds),
        };
        validate_template(&mut tx, &candidate).await?;
        let cron=sqlx::query_as(&format!("UPDATE inkcre.crons SET schedule=$2,enabled=$3,job_type=$4,job_parameters=$5,job_timeout_seconds=$6 WHERE id=$1 RETURNING {COLUMNS}"))
            .bind(id).bind(candidate.schedule).bind(candidate.enabled).bind(candidate.job_type).bind(candidate.job_parameters).bind(candidate.job_timeout_seconds).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok(cron)
    }

    pub async fn cron_delete(&self, id: i64) -> Result<bool> {
        Ok(sqlx::query("DELETE FROM inkcre.crons WHERE id=$1")
            .bind(id)
            .execute(self.pool())
            .await?
            .rows_affected()
            > 0)
    }

    /// Explicit immediate execution is independent of enabled intent and occurrence progress.
    pub async fn cron_run_now(&self, id: i64) -> Result<Job> {
        let mut tx = self.pool().begin().await?;
        let row: Cron = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM inkcre.crons WHERE id=$1 FOR SHARE"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound("Cron"))?;
        let job = Self::job_submit_in(&mut tx, job_input(row)).await?;
        tx.commit().await?;
        Ok(job)
    }

    /// One locked occurrence; concurrent Peers either observe its progress or skip its lock.
    pub async fn cron_materialize(&self, id: i64) -> Result<Option<Job>> {
        let timezone = self.cron_config_get().await?.zone()?;
        let mut tx = self.pool().begin().await?;
        let row: Option<Cron> = sqlx::query_as(&format!(
            "SELECT {COLUMNS} FROM inkcre.crons WHERE id=$1 FOR UPDATE SKIP LOCKED"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row.filter(|row| row.enabled) else {
            return Ok(None);
        };
        let now: DateTime<Utc> = sqlx::query_scalar("SELECT statement_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        let occurrence = DateTime::<Utc>::from_timestamp(now.timestamp().div_euclid(60) * 60, 0)
            .ok_or_else(|| {
                Error::InvalidInput("database clock is outside the supported Cron range".into())
            })?;
        let pattern = parse_schedule(&row.schedule)?;
        let matches = pattern
            .is_time_matching(&occurrence.with_timezone(&timezone))
            .map_err(|_| {
                Error::InvalidInput("persisted Cron schedule cannot be evaluated".into())
            })?;
        if !matches || row.last_scheduled_for == Some(occurrence) {
            return Ok(None);
        }
        if let Some(last_job) = row.last_job {
            let status: Option<JobStatus> =
                sqlx::query_scalar("SELECT status::text FROM inkcre.jobs WHERE id=$1")
                    .bind(last_job)
                    .fetch_optional(&mut *tx)
                    .await?;
            if status
                .is_some_and(|status| matches!(status, JobStatus::Pending | JobStatus::Running))
            {
                return Ok(None);
            }
        }
        let job = Self::job_submit_in(&mut tx, job_input(row)).await?;
        sqlx::query("UPDATE inkcre.crons SET last_job=$2,last_scheduled_for=$3 WHERE id=$1")
            .bind(id)
            .bind(job.id)
            .bind(occurrence)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        tracing::info!(
            event = "cron.materialized",
            cron_id = id,
            job_id = job.id,
            "Cron occurrence created Job"
        );
        Ok(Some(job))
    }
}

fn job_input(row: Cron) -> JobInput {
    JobInput {
        type_: row.job_type,
        parameters: row.job_parameters,
        timeout_seconds: row.job_timeout_seconds,
        submission_traceparent: None,
        submission_tracestate: None,
    }
}

fn parse_schedule(schedule: &str) -> Result<croner::Cron> {
    if schedule.split_whitespace().count() != 5 || schedule.contains('+') {
        return Err(Error::InvalidInput(
            "Cron schedule must be a valid five-field UNIX expression".into(),
        ));
    }
    CronParser::builder()
        .dom_and_dow(false)
        .sloppy_ranges(true)
        .build()
        .parse(schedule)
        .map_err(|_| {
            Error::InvalidInput("Cron schedule must be a valid five-field UNIX expression".into())
        })
}

async fn validate_template(tx: &mut Transaction<'_, Postgres>, input: &CronInput) -> Result<()> {
    parse_schedule(&input.schedule)?;
    if !input.job_parameters.is_object()
        || input
            .job_timeout_seconds
            .is_some_and(|timeout| timeout <= 0)
    {
        return Err(Error::InvalidInput(
            "Cron requires object parameters and a positive timeout override".into(),
        ));
    }
    let contract:JobType=sqlx::query_as("SELECT id,description,parameters_schema,default_timeout_seconds FROM inkcre.job_types WHERE id=$1 FOR SHARE")
        .bind(&input.job_type).fetch_optional(&mut **tx).await?.ok_or(Error::NotFound("job type"))?;
    crate::models::validate_json(&contract.parameters_schema, &input.job_parameters)?;
    crate::source::validate_source_job_in(tx, &input.job_type, &input.job_parameters).await?;
    Ok(())
}

pub struct CronScheduler {
    core: Core,
    poll_interval: Duration,
}
pub struct CronHandle {
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}
impl CronHandle {
    pub async fn shutdown(mut self) -> Result<()> {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            task.await
                .map_err(|_| Error::Conflict("Cron scheduler task failed".into()))?;
        }
        Ok(())
    }
}
impl Drop for CronHandle {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

impl CronScheduler {
    pub fn new(core: Core, poll_interval: Duration) -> Result<Self> {
        if poll_interval.is_zero() {
            return Err(Error::InvalidInput(
                "Cron discovery interval must be positive".into(),
            ));
        }
        Ok(Self {
            core,
            poll_interval,
        })
    }

    pub async fn materialize(&self, id: i64) -> Result<Option<Job>> {
        self.core.cron_materialize(id).await
    }

    /// Enumerate durable intent, then lock each template in its own transaction.
    pub async fn check_due(&self) -> Result<usize> {
        let mut cursor = None;
        let mut created = 0;
        loop {
            let rows:Vec<i64>=sqlx::query_scalar("SELECT id FROM inkcre.crons WHERE enabled=true AND ($1::bigint IS NULL OR id>$1) ORDER BY id LIMIT 100")
                .bind(cursor).fetch_all(self.core.pool()).await?;
            let Some(last) = rows.last().copied() else {
                break;
            };
            for id in rows {
                match self.core.cron_materialize(id).await {
                    Ok(Some(_)) => created += 1,
                    Ok(None) => {}
                    Err(_) => tracing::warn!(
                        cron_id = id,
                        "Cron occurrence was not created; template or database unavailable"
                    ),
                }
            }
            cursor = Some(last);
        }
        Ok(created)
    }

    pub fn start(self) -> CronHandle {
        let (stop, mut receiver) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(self.poll_interval);
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _=receiver.changed()=>break,
                    _=tick.tick()=> {
                        tokio::select! {
                            _=receiver.changed()=>break,
                            result=self.check_due()=>if result.is_err() {tracing::warn!("Cron discovery unavailable");},
                        }
                    }
                }
            }
        });
        CronHandle {
            stop,
            task: Some(task),
        }
    }
}
