use chrono::{DateTime, Utc};
use inkcre_core::{Block, Core, Relation, source::Source};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use std::collections::HashMap;

use crate::{Error, Result, schema::*};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReconcileResult {
    pub block_id: i32,
    pub action: String,
    pub alternate_url_changed: bool,
}

const BLOCK: &str = "id,resolver,content,storage,created_at,updated_at";

async fn create_block(
    tx: &mut Transaction<'_, Postgres>,
    resolver: &str,
    content: String,
) -> Result<Block> {
    Ok(sqlx::query_as(&format!(
        "INSERT INTO inkcre.blocks(resolver,content) VALUES($1,$2) RETURNING {BLOCK}"
    ))
    .bind(resolver)
    .bind(content)
    .fetch_one(&mut **tx)
    .await?)
}

async fn replace_content<T: Serialize>(
    tx: &mut Transaction<'_, Postgres>,
    id: i32,
    content: &T,
) -> Result<()> {
    sqlx::query("UPDATE inkcre.blocks SET content=$2 WHERE id=$1")
        .bind(id)
        .bind(serde_json::to_string(content)?)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn relation(
    tx: &mut Transaction<'_, Postgres>,
    from: i32,
    to: i32,
    content: &str,
) -> Result<i32> {
    Ok(sqlx::query_scalar(
        "INSERT INTO inkcre.relations(from_,to_,content) VALUES($1,$2,$3) RETURNING id",
    )
    .bind(from)
    .bind(to)
    .bind(content)
    .fetch_one(&mut **tx)
    .await?)
}

async fn reconcile_feed_in(
    tx: &mut Transaction<'_, Postgres>,
    source: &Source,
    canonical: &CanonicalFeed,
) -> Result<ReconcileResult> {
    let candidates: Vec<Block> = sqlx::query_as(&format!("SELECT {BLOCK} FROM inkcre.blocks WHERE resolver=$1 AND content::jsonb->>'source_instance_id'=$2 FOR UPDATE"))
        .bind(FEED_RESOLVER).bind(source.id.to_string()).fetch_all(&mut **tx).await?;
    let mut matched = None;
    for block in candidates {
        let persisted: CanonicalFeed = serde_json::from_str(&block.content)
            .map_err(|_| Error::Integrity("invalid persisted feed content"))?;
        if persisted.identity() == canonical.identity() {
            if matched.is_some() {
                return Err(Error::Integrity("feed identity has multiple exact roots"));
            }
            matched = Some((block, persisted));
        }
    }
    let result = if let Some((block, persisted)) = matched {
        let changed = persisted != *canonical;
        if changed {
            replace_content(tx, block.id, canonical).await?;
        }
        ReconcileResult {
            block_id: block.id,
            action: if changed { "updated" } else { "unchanged" }.into(),
            alternate_url_changed: false,
        }
    } else {
        let block = create_block(tx, FEED_RESOLVER, serde_json::to_string(canonical)?).await?;
        ReconcileResult {
            block_id: block.id,
            action: "created".into(),
            alternate_url_changed: false,
        }
    };
    Ok(result)
}

async fn item_roots(
    tx: &mut Transaction<'_, Postgres>,
    feed: i32,
) -> Result<Vec<(Block, CanonicalItem)>> {
    let records:Vec<Block>=sqlx::query_as("SELECT b.id,b.resolver,b.content,b.storage,b.created_at,b.updated_at FROM inkcre.relations r JOIN inkcre.blocks b ON b.id=r.from_ WHERE r.to_=$1 AND r.content='feed' ORDER BY r.id")
        .bind(feed).fetch_all(&mut **tx).await?;
    let mut roots = Vec::new();
    for block in records {
        if block.resolver != ITEM_RESOLVER {
            return Err(Error::Integrity(
                "feed membership originates at another Resolver",
            ));
        }
        let persisted: CanonicalItem = serde_json::from_str(&block.content)
            .map_err(|_| Error::Integrity("invalid persisted feed item"))?;
        persisted.validate()?;
        roots.push((block, persisted));
    }
    Ok(roots)
}

async fn reconcile_items_in(
    tx: &mut Transaction<'_, Postgres>,
    feed: i32,
    items: &[&ParsedItem],
) -> Result<Vec<ReconcileResult>> {
    let resolver: Option<String> =
        sqlx::query_scalar("SELECT resolver FROM inkcre.blocks WHERE id=$1 FOR UPDATE")
            .bind(feed)
            .fetch_optional(&mut **tx)
            .await?;
    if resolver.as_deref() != Some(FEED_RESOLVER) {
        return Err(Error::Integrity(
            "feed root disappeared or changed Resolver",
        ));
    }
    let mut identities = HashMap::new();
    for (block, persisted) in item_roots(tx, feed).await? {
        if let Some(identity) = persisted.identity()
            && identities
                .insert(identity.to_string(), (block, persisted))
                .is_some()
        {
            return Err(Error::Integrity("feed has duplicate exact item identity"));
        }
    }
    let mut results = Vec::with_capacity(items.len());
    for item in items {
        let canonical = &item.item;
        canonical.validate()?;
        let identity = canonical.identity().map(|value| value.to_string());
        let matched = identity
            .as_ref()
            .and_then(|identity| identities.get(identity));
        let (block, result) = if let Some((block, persisted)) = matched {
            let changed = persisted != canonical;
            if changed {
                replace_content(tx, block.id, canonical).await?;
            }
            (
                block.clone(),
                ReconcileResult {
                    block_id: block.id,
                    action: if changed { "updated" } else { "unchanged" }.into(),
                    alternate_url_changed: persisted.alternate_url != canonical.alternate_url,
                },
            )
        } else {
            let block = create_block(tx, ITEM_RESOLVER, serde_json::to_string(canonical)?).await?;
            relation(tx, block.id, feed, "feed").await?;
            let result = ReconcileResult {
                block_id: block.id,
                action: "created".into(),
                alternate_url_changed: true,
            };
            (block, result)
        };
        reconcile_enclosures(tx, result.block_id, &item.enclosures).await?;
        if let Some(identity) = identity {
            identities.insert(identity, (block, canonical.clone()));
        }
        results.push(result);
    }
    Ok(results)
}

pub struct PrimaryResult {
    pub feed: ReconcileResult,
    pub items: Vec<(usize, ReconcileResult)>,
    pub skipped: Vec<Value>,
}

pub struct SnapshotCheckpoint {
    pub observed_at: DateTime<Utc>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// 短事务内串行化Source、feed身份、全部primary graph及其水位线。
/// 网络和可重建的enrichment不在此事务内；旧快照不能覆盖并发已提交的新状态。
pub async fn reconcile_snapshot(
    core: &Core,
    source: &Source,
    config: &FeedConfig,
    previous: &FeedState,
    snapshot: &Snapshot,
    checkpoint: SnapshotCheckpoint,
) -> Result<PrimaryResult> {
    let mut tx = core.pool().begin().await?;
    let current: Option<(Value, Value)> =
        sqlx::query_as("SELECT config,state FROM inkcre.sources WHERE id=$1 FOR UPDATE")
            .bind(source.id)
            .fetch_optional(&mut *tx)
            .await?;
    if current.as_ref() != Some(&(source.config.clone(), source.state.clone())) {
        return Err(Error::Conflict(
            "Source configuration or checkpoint changed during retrieval",
        ));
    }
    let feed = reconcile_feed_in(&mut tx, source, &snapshot.feed).await?;
    let admission = if previous.snapshot_feed_block_id == Some(feed.block_id) {
        previous.clone()
    } else {
        FeedState::default()
    };
    let mut indices = Vec::new();
    let mut admitted = Vec::new();
    let mut skipped = Vec::new();
    for (index, item) in snapshot.items.iter().enumerate() {
        let skip = if item.item.identity().is_some() {
            None
        } else if config.unidentified_item_behavior == "discard" {
            Some("unidentified_item_discarded")
        } else if item
            .item
            .source_time()
            .zip(admission.last_successful_contentful_snapshot_observed_at)
            .is_some_and(|(time, watermark)| time <= watermark)
        {
            Some("unidentified_item_before_watermark")
        } else {
            None
        };
        if let Some(code) = skip {
            skipped.push(json!({"scope":"item","code":code,"item_index":index}));
        } else {
            indices.push(index);
            admitted.push(item);
        }
    }
    let items = reconcile_items_in(&mut tx, feed.block_id, &admitted).await?;
    let state = FeedState {
        etag: checkpoint.etag,
        last_modified: checkpoint.last_modified,
        last_successful_contentful_snapshot_observed_at: Some(checkpoint.observed_at),
        snapshot_configured_url: Some(config.feed_url.clone()),
        snapshot_feed_block_id: Some(feed.block_id),
    };
    sqlx::query("UPDATE inkcre.sources SET state=state || $2 WHERE id=$1")
        .bind(source.id)
        .bind(serde_json::to_value(state)?)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(PrimaryResult {
        feed,
        items: indices.into_iter().zip(items).collect(),
        skipped,
    })
}

async fn reconcile_enclosures(
    tx: &mut Transaction<'_, Postgres>,
    item: i32,
    canonical: &[CanonicalEnclosure],
) -> Result<()> {
    let records:Vec<(i32,i32,String,String)>=sqlx::query_as("SELECT r.id,b.id,b.resolver,b.content FROM inkcre.relations r JOIN inkcre.blocks b ON b.id=r.to_ WHERE r.from_=$1 AND r.content='enclosure' ORDER BY r.id")
        .bind(item).fetch_all(&mut **tx).await?;
    let mut existing = Vec::new();
    for (relation, block, resolver, content) in records {
        if resolver != ENCLOSURE_RESOLVER {
            return Err(Error::Integrity(
                "enclosure relation targets another Resolver",
            ));
        }
        let value: CanonicalEnclosure = serde_json::from_str(&content)
            .map_err(|_| Error::Integrity("invalid persisted enclosure metadata"))?;
        existing.push((relation, block, value));
    }
    let mut retained = Vec::new();
    for value in canonical {
        let old = existing
            .iter()
            .position(|(relation, _, old)| old.url == value.url && !retained.contains(relation));
        if let Some(index) = old {
            let (relation, block, old) = &existing[index];
            if old != value {
                replace_content(tx, *block, value).await?;
            }
            retained.push(*relation);
        } else {
            let block = create_block(tx, ENCLOSURE_RESOLVER, serde_json::to_string(value)?).await?;
            retained.push(relation(tx, item, block.id, "enclosure").await?);
        }
    }
    let remove: Vec<i32> = existing
        .iter()
        .filter(|(id, _, _)| !retained.contains(id))
        .map(|(id, _, _)| *id)
        .collect();
    if !remove.is_empty() {
        sqlx::query("DELETE FROM inkcre.relations WHERE id=ANY($1)")
            .bind(remove)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

pub async fn outgoing(core: &Core, id: i32, content: &str) -> Result<Vec<Relation>> {
    Ok(sqlx::query_as("SELECT id,from_,to_,content,updated_at FROM inkcre.relations WHERE from_=$1 AND content=$2 ORDER BY id")
        .bind(id).bind(content).fetch_all(core.pool()).await?)
}

pub async fn single_outgoing(core: &Core, id: i32, content: &str) -> Result<Option<i32>> {
    let relations = outgoing(core, id, content).await?;
    if relations.len() > 1 {
        return Err(Error::Integrity(
            "a single-valued feed projection is ambiguous",
        ));
    }
    relations
        .first()
        .map(|edge| {
            edge.to_
                .ok_or(Error::Integrity("feed projection has no endpoint"))
        })
        .transpose()
}
