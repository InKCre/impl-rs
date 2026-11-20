//! Profile-scoped semantic projections and cosine retrieval.
use crate::{
    Core, Error, Job, JobType, Page, Result,
    ai::{Ai, Requirement},
    knowledge::TextOptions,
    lexical::BlockFilters,
    worker::{JobContext, JobHandler},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{FromRow, Row};
use std::{collections::BTreeSet, sync::Arc};
pub const SEMANTIC_RETRIEVAL_CAPABILITY: &str = "core.semantic_retrieval.v1";
pub const CAPABILITY: &str = "core.semantic_retrieval.v1";
#[derive(Clone, Serialize, Deserialize, FromRow)]
pub struct Profile {
    pub id: i64,
    pub name: Option<String>,
    pub ai_model: i64,
    pub dimensions: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
#[derive(schemars::JsonSchema, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Options {
    #[schemars(range(min = 1, max = 100))]
    pub limit: i64,
    pub min_score: Option<f64>,
    pub filters: Option<BlockFilters>,
    pub entity_types: BTreeSet<String>,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            limit: 20,
            min_score: None,
            filters: None,
            entity_types: BTreeSet::from(["block".into(), "relation".into()]),
        }
    }
}
#[derive(schemars::JsonSchema, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub query: String,
    pub profile: Option<i64>,
    #[serde(default)]
    pub options: Options,
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        if self.query.trim().is_empty()
            || !(1..=100).contains(&self.options.limit)
            || self
                .options
                .min_score
                .is_some_and(|v| !v.is_finite() || !(-1.0..=1.0).contains(&v))
            || self.options.entity_types.is_empty()
            || self
                .options
                .entity_types
                .iter()
                .any(|t| t != "block" && t != "relation")
        {
            return Err(Error::InvalidInput(
                "invalid semantic retrieval options".into(),
            ));
        }
        Ok(())
    }
}
#[derive(Deserialize, Serialize)]
pub struct Match {
    #[serde(rename = "type")]
    pub kind: String,
    pub entity: Value,
    pub score: f64,
}
#[derive(Deserialize, Serialize)]
pub struct Retrieval {
    pub profile: i64,
    pub metric: String,
    pub matches: Vec<Match>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaintenanceOptions {
    pub max_embeddings: usize,
    pub batch_size: usize,
    pub scan_page_size: i64,
    pub diagnostic_limit: usize,
}
impl Default for MaintenanceOptions {
    fn default() -> Self {
        Self {
            max_embeddings: 100,
            batch_size: 20,
            scan_page_size: 100,
            diagnostic_limit: 20,
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parameters {
    pub profile: Option<i64>,
    #[serde(default)]
    pub options: MaintenanceOptions,
}
#[derive(Serialize)]
pub struct Report {
    pub profile: i64,
    pub embedded: usize,
    pub unavailable: usize,
    pub failed: usize,
    pub diagnostics: Vec<Value>,
}
fn ai_error(_: crate::ai::Error) -> Error {
    Error::Unavailable("AI embedding execution")
}
fn vector(vector: &[f32]) -> String {
    format!(
        "[{}]",
        vector
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}
impl Core {
    pub async fn semantic_profile(&self, id: i64) -> Result<Option<Profile>> {
        Ok(sqlx::query_as("SELECT id,name,ai_model,dimensions,created_at,updated_at FROM inkcre.embedding_profiles WHERE id=$1").bind(id).fetch_optional(self.pool()).await?)
    }
    pub async fn semantic_profiles(
        &self,
        limit: i64,
        cursor: Option<i64>,
    ) -> Result<Page<Profile, i64>> {
        if limit <= 0 || limit == i64::MAX {
            return Err(Error::InvalidInput("invalid profile page limit".into()));
        }
        let mut items:Vec<Profile>=sqlx::query_as("SELECT id,name,ai_model,dimensions,created_at,updated_at FROM inkcre.embedding_profiles WHERE ($1::bigint IS NULL OR id>$1) ORDER BY id LIMIT $2").bind(cursor).bind(limit+1).fetch_all(self.pool()).await?;
        let next_cursor = if items.len() > limit as usize {
            items.pop();
            items.last().map(|p| p.id)
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }
    async fn selected_profile(&self, id: Option<i64>) -> Result<Profile> {
        let id = match id {
            Some(id) => id,
            None => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Config {
                    default_profile: Option<i64>,
                }
                let row: Option<(String, Value)> = sqlx::query_as(
                    "SELECT schema,value FROM inkcre.configs WHERE key='semantic_retrieval'",
                )
                .fetch_optional(self.pool())
                .await?;
                let Some((schema, value)) = row else {
                    return Err(Error::Unavailable("semantic retrieval default profile"));
                };
                if schema != "core.semantic_retrieval.config.v1" {
                    return Err(Error::InvalidInput("invalid semantic config schema".into()));
                }
                let config: Config = serde_json::from_value(value)
                    .map_err(|_| Error::InvalidInput("invalid semantic config".into()))?;
                config
                    .default_profile
                    .ok_or(Error::Unavailable("semantic retrieval default profile"))?
            }
        };
        self.semantic_profile(id)
            .await?
            .ok_or(Error::NotFound("embedding profile"))
    }
    pub async fn semantic_can_maintain(&self, id: Option<i64>) -> Result<bool> {
        let profile = match self.selected_profile(id).await {
            Ok(p) => p,
            Err(Error::NotFound(_) | Error::Unavailable(_)) => return Ok(false),
            Err(e) => return Err(e),
        };
        Ai::new(self.clone())
            .map_err(ai_error)?
            .can_execute(profile.ai_model, &Requirement::embedding())
            .await
            .map_err(ai_error)
    }
    pub async fn semantic_retrieve(&self, request: Request) -> Result<Retrieval> {
        request.validate()?;
        let options = request.options;
        let profile = self.selected_profile(request.profile).await?;
        let vectors = Ai::new(self.clone())
            .map_err(ai_error)?
            .embed(
                profile.ai_model,
                &[request.query],
                profile.dimensions as usize,
            )
            .await
            .map_err(ai_error)?;
        let query = vector(&vectors[0]);
        let mut matches = Vec::new();
        let resolver = options
            .filters
            .as_ref()
            .and_then(|f| f.resolver.as_ref())
            .map(|f| f.eq.as_str());
        let storage = options
            .filters
            .as_ref()
            .and_then(|f| f.storage.as_ref())
            .map(|f| f.eq);
        for kind in ["block", "relation"] {
            if !options.entity_types.contains(kind)
                || (kind == "relation" && (resolver.is_some() || storage.is_some()))
            {
                continue;
            }
            let sql = if kind == "block" {
                "SELECT to_jsonb(b) AS entity,1-(e.embedding <=> $1::vector) AS score FROM inkcre.blocks b JOIN inkcre.block_embeddings e ON e.block=b.id WHERE e.profile=$2 AND e.updated_at >= $3 AND e.updated_at >= b.updated_at AND vector_dims(e.embedding)=$4 AND vector_norm(e.embedding)>0 AND ($5::text IS NULL OR b.resolver=$5) AND ($6::integer IS NULL OR b.storage=$6) AND ($7::float8 IS NULL OR 1-(e.embedding <=> $1::vector)>=$7) ORDER BY e.embedding <=> $1::vector,b.id LIMIT $8"
            } else {
                "SELECT to_jsonb(b) AS entity,1-(e.embedding <=> $1::vector) AS score FROM inkcre.relations b JOIN inkcre.relation_embeddings e ON e.relation=b.id JOIN inkcre.blocks f ON f.id=b.from_ JOIN inkcre.blocks t ON t.id=b.to_ WHERE e.profile=$2 AND e.updated_at >= $3 AND e.updated_at >= b.updated_at AND e.updated_at >= f.updated_at AND e.updated_at >= t.updated_at AND vector_dims(e.embedding)=$4 AND vector_norm(e.embedding)>0 AND $5::text IS NULL AND $6::integer IS NULL AND ($7::float8 IS NULL OR 1-(e.embedding <=> $1::vector)>=$7) ORDER BY e.embedding <=> $1::vector,b.id LIMIT $8"
            };
            for row in sqlx::query(sql)
                .bind(&query)
                .bind(profile.id)
                .bind(profile.updated_at)
                .bind(profile.dimensions)
                .bind(resolver)
                .bind(storage)
                .bind(options.min_score)
                .bind(options.limit)
                .fetch_all(self.pool())
                .await?
            {
                let score: f64 = row.try_get("score")?;
                if !score.is_finite() {
                    return Err(Error::InvalidInput("nonfinite cosine score".into()));
                }
                matches.push(Match {
                    kind: kind.into(),
                    entity: row.try_get("entity")?,
                    score: score.clamp(-1.0, 1.0),
                });
            }
        }
        matches.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then(a.kind.cmp(&b.kind))
                .then(a.entity["id"].as_i64().cmp(&b.entity["id"].as_i64()))
        });
        matches.truncate(options.limit as usize);
        Ok(Retrieval {
            profile: profile.id,
            metric: "cosine".into(),
            matches,
        })
    }
    pub async fn semantic_maintain(
        &self,
        id: Option<i64>,
        options: MaintenanceOptions,
        rebuild: bool,
    ) -> Result<Report> {
        if options.max_embeddings == 0 || options.batch_size == 0 || options.scan_page_size <= 0 {
            return Err(Error::InvalidInput("invalid maintenance bounds".into()));
        }
        let profile = self.selected_profile(id).await?;
        let ai = Ai::new(self.clone()).map_err(ai_error)?;
        let cutoff: DateTime<Utc> = sqlx::query_scalar("SELECT statement_timestamp()")
            .fetch_one(self.pool())
            .await?;
        let mut report = Report {
            profile: profile.id,
            embedded: 0,
            unavailable: 0,
            failed: 0,
            diagnostics: Vec::new(),
        };
        let mut batch = Vec::new();
        for kind in ["block", "relation"] {
            let mut cursor = 0;
            loop {
                let sql = if kind == "block" {
                    "SELECT b.id,to_jsonb(b) AS entity,(e.block IS NULL OR CASE WHEN $5 THEN e.updated_at<$4 ELSE e.updated_at<$2 OR e.updated_at<b.updated_at OR vector_dims(e.embedding)<>$3 END) AS stale FROM inkcre.blocks b LEFT JOIN inkcre.block_embeddings e ON e.block=b.id AND e.profile=$1 WHERE b.id>$6 ORDER BY b.id LIMIT $7"
                } else {
                    "SELECT b.id,to_jsonb(b) AS entity,(e.relation IS NULL OR CASE WHEN $5 THEN e.updated_at<$4 ELSE e.updated_at<$2 OR e.updated_at<b.updated_at OR e.updated_at<f.updated_at OR e.updated_at<t.updated_at OR vector_dims(e.embedding)<>$3 END) AS stale FROM inkcre.relations b JOIN inkcre.blocks f ON f.id=b.from_ JOIN inkcre.blocks t ON t.id=b.to_ LEFT JOIN inkcre.relation_embeddings e ON e.relation=b.id AND e.profile=$1 WHERE b.id>$6 ORDER BY b.id LIMIT $7"
                };
                let rows = sqlx::query(sql)
                    .bind(profile.id)
                    .bind(profile.updated_at)
                    .bind(profile.dimensions)
                    .bind(cutoff)
                    .bind(rebuild)
                    .bind(cursor)
                    .bind(options.scan_page_size)
                    .fetch_all(self.pool())
                    .await?;
                let exhausted = rows.len() < options.scan_page_size as usize;
                for row in rows {
                    let entity: i32 = row.try_get("id")?;
                    cursor = entity;
                    if !row.try_get::<bool, _>("stale")? {
                        continue;
                    }
                    let projection = if kind == "block" {
                        self.knowledge()
                            .text(entity, TextOptions::default())
                            .await
                            .map_err(|error| match error {
                                crate::knowledge::Error::UnknownResolver(_) => "unknown_resolver",
                                crate::knowledge::Error::Unsupported(_) => "unsupported_text",
                                _ => "projection_failed",
                            })
                    } else {
                        let value: Value = row.try_get("entity")?;
                        self.relation_projection(&value)
                            .await
                            .map_err(|error| match error {
                                crate::knowledge::Error::UnknownResolver(_) => "unknown_resolver",
                                crate::knowledge::Error::Unsupported(_) => "unsupported_text",
                                _ => "projection_failed",
                            })
                    };
                    match projection {
                        Ok(Some(text)) if !text.trim().is_empty() => {
                            batch.push((kind, entity, text))
                        }
                        outcome => {
                            let (label, reason) = match outcome {
                                Err("projection_failed") => {
                                    report.failed += 1;
                                    ("failed", "projection_failed")
                                }
                                Err(reason) => {
                                    report.unavailable += 1;
                                    ("unavailable", reason)
                                }
                                Ok(_) => {
                                    report.unavailable += 1;
                                    ("unavailable", "empty_projection")
                                }
                            };
                            if report.diagnostics.len() < options.diagnostic_limit {
                                report.diagnostics.push(json!({"entity_type":kind,"entity":entity,"outcome":label,"reason":reason}));
                            }
                        }
                    }
                    let batch_ready = batch.len() >= options.batch_size
                        || report.embedded + batch.len() >= options.max_embeddings;
                    if batch_ready {
                        let succeeded = self
                            .semantic_flush(
                                &ai,
                                &profile,
                                &mut batch,
                                &mut report,
                                options.diagnostic_limit,
                            )
                            .await?;
                        if !succeeded || report.embedded >= options.max_embeddings {
                            return Ok(report);
                        }
                    }
                }
                if exhausted {
                    break;
                }
            }
        }
        self.semantic_flush(
            &ai,
            &profile,
            &mut batch,
            &mut report,
            options.diagnostic_limit,
        )
        .await?;
        Ok(report)
    }
    async fn relation_projection(&self, value: &Value) -> crate::knowledge::Result<Option<String>> {
        let Some(from) = value["from_"].as_i64() else {
            return Ok(None);
        };
        let Some(to) = value["to_"].as_i64() else {
            return Ok(None);
        };
        let content = value["content"].as_str().unwrap_or("");
        if content.trim().is_empty() {
            return Ok(None);
        }
        let a = self.knowledge().label(from as i32, false).await?;
        let b = self.knowledge().label(to as i32, false).await?;
        Ok((!a.trim().is_empty() && !b.trim().is_empty())
            .then(|| format!("subject:\n{a}\nproperty:\n{content}\nvalue:\n{b}")))
    }
    async fn semantic_flush(
        &self,
        ai: &Ai,
        profile: &Profile,
        batch: &mut Vec<(&str, i32, String)>,
        report: &mut Report,
        diagnostic_limit: usize,
    ) -> Result<bool> {
        if batch.is_empty() {
            return Ok(true);
        }
        let current = std::mem::take(batch);
        let texts = current
            .iter()
            .map(|(_, _, text)| text.clone())
            .collect::<Vec<_>>();
        let vectors = match ai
            .embed(profile.ai_model, &texts, profile.dimensions as usize)
            .await
        {
            Ok(v) => v,
            Err(_) => {
                for (kind, id, _) in current {
                    report.failed += 1;
                    if report.diagnostics.len() < diagnostic_limit {
                        report.diagnostics.push(json!({"entity_type":kind,"entity":id,"outcome":"failed","reason":"embedding_failed"}));
                    }
                }
                return Ok(false);
            }
        };
        let mut tx = self.pool().begin().await?;
        for ((kind, id, _), embedding) in current.iter().zip(vectors) {
            let sql = if *kind == "block" {
                "INSERT INTO inkcre.block_embeddings(profile,block,embedding) VALUES($1,$2,$3::vector) ON CONFLICT(profile,block) DO UPDATE SET embedding=EXCLUDED.embedding,updated_at=statement_timestamp()"
            } else {
                "INSERT INTO inkcre.relation_embeddings(profile,relation,embedding) VALUES($1,$2,$3::vector) ON CONFLICT(profile,relation) DO UPDATE SET embedding=EXCLUDED.embedding,updated_at=statement_timestamp()"
            };
            sqlx::query(sql)
                .bind(profile.id)
                .bind(id)
                .bind(vector(&embedding))
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        report.embedded += current.len();
        Ok(true)
    }
}
struct MaintenanceHandler {
    rebuild: bool,
}
#[async_trait::async_trait]
impl JobHandler for MaintenanceHandler {
    fn job_type(&self) -> JobType {
        JobType {
            id: format!(
                "core.semantic_retrieval.{}.v1",
                if self.rebuild { "rebuild" } else { "maintain" }
            ),
            description: (if self.rebuild {
                "Rebuild semantic embedding records present before invocation."
            } else {
                "Maintain missing or stale semantic embedding records."
            }
            .into()),
            parameters_schema: serde_json::from_str(include_str!(
                "semantic-maintenance.schema.json"
            ))
            .expect("bundled semantic maintenance schema must be valid JSON"),
            default_timeout_seconds: if self.rebuild { 1800 } else { 900 },
        }
    }
    async fn can_handle(&self, core: &Core, job: &Job) -> Result<bool> {
        let p: Parameters = serde_json::from_value(job.parameters.clone())
            .map_err(|_| Error::InvalidInput("invalid semantic job parameters".into()))?;
        core.semantic_can_maintain(p.profile).await
    }
    async fn handle(&self, context: JobContext, job: Job) -> Result<()> {
        let p: Parameters = serde_json::from_value(job.parameters)
            .map_err(|_| Error::InvalidInput("invalid semantic job parameters".into()))?;
        let report = context
            .core()
            .semantic_maintain(p.profile, p.options, self.rebuild)
            .await?;
        context
            .patch_state(
                serde_json::to_value(report)
                    .map_err(|_| Error::InvalidInput("invalid semantic report".into()))?,
            )
            .await
    }
}
pub fn job_handlers() -> Vec<Arc<dyn JobHandler>> {
    vec![
        Arc::new(MaintenanceHandler { rebuild: false }),
        Arc::new(MaintenanceHandler { rebuild: true }),
    ]
}
