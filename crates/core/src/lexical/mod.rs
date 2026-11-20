//! Rebuildable Block lexical projections and PostgreSQL ranking.
use crate::{
    Block, Core, Error, Job, JobType, Result,
    knowledge::{TextOptions, TextProjectionContext},
    worker::{JobContext, JobHandler},
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use std::sync::Arc;
use unicode_casefold::UnicodeCaseFold;

pub const CAPABILITY: &str = "core.feature_retrieval.lexical.v1";
pub const MAINTAIN_JOB: &str = "core.feature_retrieval.lexical.maintain.v1";
pub const REBUILD_JOB: &str = "core.feature_retrieval.lexical.rebuild.v1";
#[derive(schemars::JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Equality<T> {
    pub eq: T,
}
fn predicate<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> std::result::Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
#[derive(schemars::JsonSchema, Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockFilters {
    #[serde(
        default,
        deserialize_with = "predicate",
        skip_serializing_if = "Option::is_none"
    )]
    pub resolver: Option<Equality<String>>,
    #[serde(
        default,
        deserialize_with = "predicate",
        skip_serializing_if = "Option::is_none"
    )]
    pub storage: Option<Equality<i32>>,
}
#[derive(schemars::JsonSchema, Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LexicalRequest {
    pub query: String,
    #[serde(default = "default_limit")]
    #[schemars(range(min = 1, max = 20))]
    pub limit: i64,
    pub filters: Option<BlockFilters>,
}
fn default_limit() -> i64 {
    20
}
#[derive(Debug, Serialize, Deserialize)]
pub struct LexicalMatch {
    pub block: Block,
    pub label: String,
    pub excerpt: String,
    pub evidence: String,
    pub rank: f64,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct LexicalResult {
    pub matches: Vec<LexicalMatch>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MaintenanceOptions {
    pub max_records: usize,
    pub scan_page_size: usize,
    pub diagnostic_limit: usize,
}
impl Default for MaintenanceOptions {
    fn default() -> Self {
        Self {
            max_records: 100,
            scan_page_size: 100,
            diagnostic_limit: 20,
        }
    }
}
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct MaintenanceReport {
    pub indexed: usize,
    pub unavailable: usize,
    pub failed: usize,
    pub diagnostics: Vec<Diagnostic>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Diagnostic {
    pub block: i32,
    pub outcome: String,
    pub reason: String,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Parameters {
    #[serde(default)]
    options: MaintenanceOptions,
}
#[derive(FromRow)]
struct Ranked {
    id: i32,
    storage: Option<i32>,
    resolver: String,
    content: String,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    label: String,
    text: Option<String>,
    evidence: String,
    rank: f64,
}
impl LexicalRequest {
    pub fn validate(&self) -> Result<()> {
        if self.query.trim().is_empty() || !(1..=20).contains(&self.limit) {
            return Err(Error::InvalidInput(
                "nonempty lexical query and limit 1..=20 required".into(),
            ));
        }
        if self
            .filters
            .as_ref()
            .and_then(|f| f.resolver.as_ref())
            .is_some_and(|p| p.eq.trim().is_empty())
        {
            return Err(Error::InvalidInput("resolver.eq must be nonempty".into()));
        }
        Ok(())
    }
}
impl Core {
    pub async fn lexical_retrieve(&self, request: LexicalRequest) -> Result<LexicalResult> {
        let query = request.query.trim();
        request.validate()?;
        let resolver = request
            .filters
            .as_ref()
            .and_then(|f| f.resolver.as_ref())
            .map(|p| p.eq.as_str());
        let storage = request
            .filters
            .as_ref()
            .and_then(|f| f.storage.as_ref())
            .map(|p| p.eq);
        let pattern = format!(
            "%{}%",
            query
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let rows: Vec<Ranked> = sqlx::query_as(include_str!("search.sql"))
            .bind(query)
            .bind(pattern)
            .bind(resolver)
            .bind(storage)
            .bind(request.limit)
            .fetch_all(self.pool())
            .await?;
        Ok(LexicalResult {
            matches: rows
                .into_iter()
                .map(|r| LexicalMatch {
                    excerpt: excerpt(r.text.as_deref().unwrap_or(&r.label), query),
                    label: r.label,
                    evidence: r.evidence,
                    rank: r.rank,
                    block: Block {
                        id: r.id,
                        storage: r.storage,
                        resolver: r.resolver,
                        content: r.content,
                        created_at: r.created_at,
                        updated_at: r.updated_at,
                    },
                })
                .collect(),
        })
    }
    pub async fn lexical_maintain(
        &self,
        options: MaintenanceOptions,
        rebuild: bool,
    ) -> Result<MaintenanceReport> {
        if options.max_records == 0 || options.scan_page_size == 0 {
            return Err(Error::InvalidInput(
                "maintenance budgets must be positive".into(),
            ));
        }
        let cutoff: Option<DateTime<Utc>> = if rebuild {
            Some(
                sqlx::query_scalar("SELECT CURRENT_TIMESTAMP")
                    .fetch_one(self.pool())
                    .await?,
            )
        } else {
            None
        };
        let mut report = MaintenanceReport::default();
        let mut cursor = 0;
        let mut processed = 0;
        while processed < options.max_records {
            let size = options
                .scan_page_size
                .min(options.max_records - processed)
                .min(i64::MAX as usize) as i64;
            let blocks: Vec<Block> = sqlx::query_as(include_str!("candidates.sql"))
                .bind(cursor)
                .bind(cutoff)
                .bind(size)
                .fetch_all(self.pool())
                .await?;
            let exhausted = blocks.len() < (size as usize);
            let mut projections = Vec::new();
            for block in blocks {
                cursor = block.id;
                processed += 1;
                let projected = async {
                    let knowledge = self.knowledge();
                    let label = knowledge.label(block.id, false).await?;
                    let text = knowledge
                        .text(
                            block.id,
                            TextOptions {
                                refresh: false,
                                materialize_missing: true,
                                context: TextProjectionContext::Lexical,
                            },
                        )
                        .await?;
                    Ok::<_, crate::knowledge::Error>((label, text))
                }
                .await;
                match projected {
                    Ok((label, text)) if !label.trim().is_empty() => projections.push((
                        block.id,
                        label.trim().to_owned(),
                        text.and_then(|v| {
                            let v = v.trim().to_owned();
                            (!v.is_empty()).then_some(v)
                        }),
                    )),
                    Ok(_) => record(&mut report, &options, block.id, true, "empty_label"),
                    Err(crate::knowledge::Error::UnknownResolver(_)) => {
                        record(&mut report, &options, block.id, true, "unknown_resolver")
                    }
                    Err(crate::knowledge::Error::Unsupported(_)) => record(
                        &mut report,
                        &options,
                        block.id,
                        true,
                        "unsupported_lexical_text",
                    ),
                    Err(_) => record(&mut report, &options, block.id, false, "projection_failed"),
                }
            }
            if !projections.is_empty() {
                let write = async {
                    let mut tx = self.pool().begin().await?;
                    for (id, label, text) in &projections {
                        sqlx::query(include_str!("upsert.sql"))
                            .bind(id)
                            .bind(label)
                            .bind(text)
                            .execute(&mut *tx)
                            .await?;
                    }
                    tx.commit().await
                }
                .await;
                match write {
                    Ok(()) => report.indexed += projections.len(),
                    Err(_) => {
                        for (id, _, _) in projections {
                            record(&mut report, &options, id, false, "projection_write_failed");
                        }
                    }
                }
            }
            if exhausted {
                break;
            }
        }
        Ok(report)
    }
}
fn record(
    report: &mut MaintenanceReport,
    options: &MaintenanceOptions,
    block: i32,
    unavailable: bool,
    reason: &str,
) {
    let outcome = if unavailable {
        report.unavailable += 1;
        "unavailable"
    } else {
        report.failed += 1;
        "failed"
    };
    if report.diagnostics.len() < options.diagnostic_limit {
        report.diagnostics.push(Diagnostic {
            block,
            outcome: outcome.into(),
            reason: reason.into(),
        });
    }
}
fn excerpt(source: &str, query: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let folded: Vec<char> = source.case_fold().collect();
    let needle: Vec<char> = query.case_fold().collect();
    let locate = |needle: &[char]| {
        if needle.is_empty() {
            None
        } else {
            folded.windows(needle.len()).position(|part| part == needle)
        }
    };
    let position = locate(&needle)
        .or_else(|| {
            query
                .split_whitespace()
                .filter_map(|term| locate(&term.case_fold().collect::<Vec<_>>()))
                .min()
        })
        .unwrap_or(0);
    let start = position.saturating_sub(280 / 3).min(chars.len());
    let end = (start + 280).min(chars.len());
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        chars[start..end].iter().collect::<String>(),
        if end < chars.len() { "…" } else { "" }
    )
}
struct MaintenanceHandler {
    rebuild: bool,
}
pub fn job_handlers() -> Vec<Arc<dyn JobHandler>> {
    vec![
        Arc::new(MaintenanceHandler { rebuild: false }),
        Arc::new(MaintenanceHandler { rebuild: true }),
    ]
}
#[async_trait]
impl JobHandler for MaintenanceHandler {
    fn job_type(&self) -> JobType {
        JobType {
            id: if self.rebuild {
                REBUILD_JOB
            } else {
                MAINTAIN_JOB
            }
            .into(),
            description: if self.rebuild {
                "Rebuild Block lexical records present before invocation."
            } else {
                "Maintain missing or stale Block lexical projection records."
            }
            .into(),
            default_timeout_seconds: if self.rebuild { 1800 } else { 900 },
            parameters_schema: serde_json::from_str(include_str!("parameters.json"))
                .expect("static lexical contract schema"),
        }
    }
    async fn can_handle(&self, core: &Core, _job: &Job) -> Result<bool> {
        Ok(!core.knowledge().registry().resolvers()?.is_empty())
    }
    async fn handle(&self, context: JobContext, job: Job) -> Result<()> {
        let params: Parameters = serde_json::from_value(job.parameters)
            .map_err(|_| Error::InvalidInput("invalid lexical maintenance parameters".into()))?;
        let report = context
            .core()
            .lexical_maintain(params.options, self.rebuild)
            .await?;
        context
            .set_state(
                serde_json::to_value(report)
                    .map_err(|_| Error::InvalidInput("invalid maintenance report".into()))?,
            )
            .await
    }
}
