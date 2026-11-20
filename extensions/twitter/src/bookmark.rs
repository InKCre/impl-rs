use crate::{Result, capture::Provider, conflict, db, graph, invalid, schema::*};
use inkcre_core::{Core, source::Source};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Collect {
    #[serde(default)]
    full: bool,
    #[serde(default = "forty")]
    result_limit: usize,
}
fn forty() -> usize {
    40
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Backfill {
    #[serde(default)]
    skip: usize,
    #[serde(default = "two_hundred")]
    limit: usize,
    #[serde(default = "fifty")]
    page_size: usize,
}
fn two_hundred() -> usize {
    200
}
fn fifty() -> usize {
    50
}
async fn locked(c: &mut sqlx::PgConnection, source: &Source) -> Result<Value> {
    let (kind, config, state): (String, Value, Value) =
        sqlx::query_as("SELECT type,config,state FROM inkcre.sources WHERE id=$1 FOR UPDATE")
            .bind(source.id)
            .fetch_optional(c)
            .await
            .map_err(db)?
            .ok_or_else(|| conflict("Twitter Source missing"))?;
    if kind != SOURCE || config != source.config {
        return Err(conflict("Twitter Source configuration changed"));
    }
    if !state.is_object() {
        return Err(invalid("Twitter Source state must be an object"));
    }
    Ok(state)
}
pub async fn run(
    core: &Core,
    source: Source,
    job: i64,
    config: Value,
    backfill: bool,
) -> Result<Value> {
    let api = Provider::new(core).await?;
    let result = execute(core, source, job, config, backfill, &api).await;
    let closed = api.close().await;
    result.and_then(|state| closed.map(|_| state))
}
async fn execute(
    core: &Core,
    source: Source,
    job: i64,
    config: Value,
    backfill: bool,
    api: &Provider,
) -> Result<Value> {
    if backfill {
        return history(core, &source, job, api, config).await;
    }
    let config: Collect =
        serde_json::from_value(config).map_err(|_| invalid("Invalid bookmark collect config"))?;
    if !(5..=100).contains(&config.result_limit) {
        return Err(invalid("Invalid bookmark result limit"));
    }
    let mut state = core
        .job_get(job)
        .await
        .map_err(|_| conflict("Twitter Job read failed"))?
        .ok_or_else(|| conflict("Twitter Job missing"))?
        .state;
    let page = state["page"].as_str().map(str::to_owned);
    let response = api.bookmarks(page.as_deref(), config.result_limit).await?;
    let mut tx = core.pool().begin().await.map_err(db)?;
    let mut source_state = locked(&mut tx, &source).await?;
    api.guard_in(&mut tx).await?;
    let selected = if !config.full {
        let latest = source_state["latest_tweet_id"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| {
                source_state["latest_tweet_id"]
                    .as_i64()
                    .map(|id| id.to_string())
            })
            .unwrap_or_default();
        let end = response
            .primary
            .iter()
            .position(|id| id == &latest)
            .unwrap_or(response.primary.len());
        &response.primary[..end]
    } else {
        &response.primary
    };
    if !selected.is_empty() {
        graph::persist(&mut tx, &response.batch.selected(selected)?, true).await?;
    }
    if !config.full
        && let Some(latest) = response.primary.first()
    {
        source_state["latest_tweet_id"] = json!(latest);
        sqlx::query("UPDATE inkcre.sources SET state=$2 WHERE id=$1")
            .bind(source.id)
            .bind(source_state)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
    }
    tx.commit().await.map_err(db)?;
    if config.full
        && let Some(next) = response.next.filter(|next| Some(next) != page.as_ref())
    {
        state["page"] = json!(next)
    }
    Ok(state)
}
async fn history(
    core: &Core,
    source: &Source,
    job: i64,
    api: &Provider,
    value: Value,
) -> Result<Value> {
    let config: Backfill =
        serde_json::from_value(value).map_err(|_| invalid("Invalid bookmark backfill config"))?;
    if config.skip > 10000
        || !(1..=1000).contains(&config.limit)
        || !(1..=100).contains(&config.page_size)
    {
        return Err(invalid("Invalid bookmark backfill range"));
    }
    let mut page = None;
    let (mut scanned, mut saved, mut pages) = (0, 0, 0);
    let end = config.skip + config.limit;
    let mut cursors = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut state = json!({"scanned":0,"saved":0,"pages":0,"exhausted":false});
    core.job_patch_state(job, &state)
        .await
        .map_err(|_| conflict("Twitter Job report failed"))?;
    while scanned < end {
        let response = api
            .bookmarks(page.as_deref(), config.page_size.min(end - scanned))
            .await?;
        let ids = &response.primary;
        let unique: BTreeSet<_> = ids.iter().cloned().collect();
        if unique.len() != ids.len() || !seen.is_disjoint(&unique) {
            return Err(conflict(
                "Bookmark ordering changed during backfill; retry explicitly",
            ));
        }
        if response
            .next
            .as_ref()
            .is_some_and(|next| cursors.contains(next))
        {
            return Err(conflict("X repeated bookmark pagination token"));
        }
        if ids.is_empty() && response.next.as_ref().is_some_and(|next| !next.is_empty()) {
            return Err(conflict("X returned an empty page with continuation"));
        }
        let start = config.skip.saturating_sub(scanned).min(ids.len());
        let stop = (end - scanned).min(ids.len());
        let selected = &ids[start..stop];
        if !selected.is_empty() {
            let mut tx = core.pool().begin().await.map_err(db)?;
            locked(&mut tx, source).await?;
            api.guard_in(&mut tx).await?;
            graph::persist(&mut tx, &response.batch.selected(selected)?, true).await?;
            tx.commit().await.map_err(db)?;
        }
        seen.extend(unique);
        scanned += ids.len();
        saved += selected.len();
        pages += 1;
        page = response.next.filter(|next| !next.is_empty());
        state = json!({"scanned":scanned,"saved":saved,"pages":pages,"exhausted":page.is_none(),"next_page":page});
        core.job_patch_state(job, &state)
            .await
            .map_err(|_| conflict("Twitter Job report failed"))?;
        if let Some(next) = &page {
            cursors.insert(next.clone());
        } else {
            break;
        }
    }
    Ok(state)
}
