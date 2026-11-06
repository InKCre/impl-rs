use dom_smoothie::{Config, Readability, TextMode};
use inkcre_core::Core;
use serde::Serialize;
use sqlx::{Postgres, Transaction};

use crate::{Error, Result, collect, reconcile, schema::*};

#[derive(Serialize)]
pub struct FullTextResult {
    pub item_block_id: i32,
    pub content_block_id: Option<i32>,
    pub status: String,
}
#[derive(Serialize)]
pub struct MaterializationResult {
    pub enclosure_block_id: i32,
    pub content_block_id: Option<i32>,
    pub status: String,
    pub resolver_id: Option<String>,
    pub error: Option<String>,
}

async fn config_for_item(core: &Core, item: i32) -> Result<FeedConfig> {
    let feed = reconcile::single_outgoing(core, item, "feed")
        .await?
        .ok_or(Error::Integrity("feed item has no feed membership"))?;
    let block = core
        .block_get(feed)
        .await?
        .ok_or(Error::Integrity("feed item references a missing feed"))?;
    if block.resolver != FEED_RESOLVER {
        return Err(Error::Integrity("feed item references another Resolver"));
    }
    let feed: CanonicalFeed = serde_json::from_str(&block.content)
        .map_err(|_| Error::Integrity("invalid feed content"))?;
    let source = core
        .source_get(feed.source_instance_id)
        .await?
        .ok_or(Error::Invalid("Source configuration no longer exists"))?;
    FeedConfig::parse(&source.config)
}

async fn config_for_enclosure(core: &Core, enclosure: i32) -> Result<FeedConfig> {
    let owners:Vec<i32>=sqlx::query_scalar("SELECT from_ FROM inkcre.relations WHERE to_=$1 AND content='enclosure' AND from_ IS NOT NULL ORDER BY id")
        .bind(enclosure).fetch_all(core.pool()).await?;
    if owners.len() != 1 {
        return Err(Error::Integrity(
            "enclosure must have exactly one feed item owner",
        ));
    }
    config_for_item(core, owners[0]).await
}

async fn one_in(tx: &mut Transaction<'_, Postgres>, id: i32, content: &str) -> Result<Option<i32>> {
    let records: Vec<Option<i32>> = sqlx::query_scalar(
        "SELECT to_ FROM inkcre.relations WHERE from_=$1 AND content=$2 ORDER BY id",
    )
    .bind(id)
    .bind(content)
    .fetch_all(&mut **tx)
    .await?;
    if records.len() > 1 {
        return Err(Error::Integrity(
            "single-valued feed projection is ambiguous",
        ));
    }
    records
        .first()
        .map(|id| id.ok_or(Error::Integrity("feed projection has no endpoint")))
        .transpose()
}

pub async fn full_text(
    core: &Core,
    item_id: i32,
    refresh: bool,
    require_enabled: bool,
) -> Result<FullTextResult> {
    let block = core
        .block_get(item_id)
        .await?
        .ok_or(Error::Invalid("feed item does not exist"))?;
    if block.resolver != ITEM_RESOLVER {
        return Err(Error::Integrity("full-text owner is not a feed item"));
    }
    let item: CanonicalItem = serde_json::from_str(&block.content)
        .map_err(|_| Error::Integrity("invalid feed item content"))?;
    item.validate()?;
    if !refresh
        && let Some(existing) = reconcile::single_outgoing(core, item_id, "full_text").await?
    {
        return Ok(FullTextResult {
            item_block_id: item_id,
            content_block_id: Some(existing),
            status: "existing".into(),
        });
    }
    let config = config_for_item(core, item_id).await?;
    let unavailable = || FullTextResult {
        item_block_id: item_id,
        content_block_id: None,
        status: "unavailable".into(),
    };
    if require_enabled && !config.fetch_full_text {
        return Ok(unavailable());
    }
    let Some(url) = item.alternate_url.clone() else {
        return Ok(unavailable());
    };
    let response = collect::fetch(&config, &url, config.max_article_bytes, None, None).await?;
    let html = inkcre_core::knowledge::decode_html_bytes(&response.body)?;
    let text = tokio::task::spawn_blocking(move || {
        let cfg = Config {
            text_mode: TextMode::Formatted,
            max_elements_to_parse: 100_000,
            ..Default::default()
        };
        Readability::new(html, Some(&url), Some(cfg))
            .ok()?
            .parse()
            .ok()
            .map(|article| article.text_content.to_string())
            .filter(|text| !text.trim().is_empty())
    })
    .await
    .map_err(|_| Error::Invalid("article extraction task failed"))?;
    let Some(text) = text else {
        return Ok(unavailable());
    };
    let mut tx = core.pool().begin().await?;
    let current: Option<(String, String)> =
        sqlx::query_as("SELECT resolver,content FROM inkcre.blocks WHERE id=$1 FOR UPDATE")
            .bind(item_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((resolver, content)) = current else {
        return Err(Error::Integrity("feed item disappeared during enrichment"));
    };
    if resolver != ITEM_RESOLVER {
        return Err(Error::Integrity(
            "feed item changed Resolver during enrichment",
        ));
    }
    let current: CanonicalItem = serde_json::from_str(&content)
        .map_err(|_| Error::Integrity("feed item changed to invalid content"))?;
    if current.alternate_url != item.alternate_url {
        return Err(Error::Conflict("feed item URL changed during enrichment"));
    }
    let (id, status) = if let Some(existing) = one_in(&mut tx, item_id, "full_text").await? {
        let child: Option<(String, String)> =
            sqlx::query_as("SELECT resolver,content FROM inkcre.blocks WHERE id=$1")
                .bind(existing)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((resolver, content)) = child else {
            return Err(Error::Integrity("full-text child is missing"));
        };
        if resolver != "core.text.v1" {
            return Err(Error::Integrity("full-text child has another Resolver"));
        }
        if refresh && content != text {
            sqlx::query("UPDATE inkcre.blocks SET content=$2 WHERE id=$1")
                .bind(existing)
                .bind(text)
                .execute(&mut *tx)
                .await?;
            (existing, "updated")
        } else {
            (existing, "existing")
        }
    } else {
        let child: i32 = sqlx::query_scalar(
            "INSERT INTO inkcre.blocks(resolver,content) VALUES('core.text.v1',$1) RETURNING id",
        )
        .bind(text)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO inkcre.relations(from_,to_,content) VALUES($1,$2,'full_text')")
            .bind(item_id)
            .bind(child)
            .execute(&mut *tx)
            .await?;
        (child, "created")
    };
    tx.commit().await?;
    Ok(FullTextResult {
        item_block_id: item_id,
        content_block_id: Some(id),
        status: status.into(),
    })
}

fn filename_media_type(url: &str) -> Option<&'static str> {
    let url = url::Url::parse(url).ok()?;
    let extension = url.path().rsplit('.').next()?.to_ascii_lowercase();
    match extension.as_str() {
        "txt" => Some("text/plain"),
        "html" | "htm" => Some("text/html"),
        "md" | "markdown" => Some("text/markdown"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "pdf" => Some("application/pdf"),
        "epub" => Some("application/epub+zip"),
        "zip" => Some("application/zip"),
        "mp3" => Some("audio/mpeg"),
        "mp4" => Some("video/mp4"),
        _ => None,
    }
}

pub async fn enclosure(core: &Core, id: i32, target_storage: i32) -> Result<MaterializationResult> {
    let block = core
        .block_get(id)
        .await?
        .ok_or(Error::Invalid("enclosure does not exist"))?;
    if block.resolver != ENCLOSURE_RESOLVER {
        return Err(Error::Integrity("enclosure metadata has another Resolver"));
    }
    let metadata: CanonicalEnclosure = serde_json::from_str(&block.content)
        .map_err(|_| Error::Integrity("invalid enclosure metadata"))?;
    if let Some(existing) = reconcile::single_outgoing(core, id, "content").await? {
        let child = core
            .block_get(existing)
            .await?
            .ok_or(Error::Integrity("enclosure content child is missing"))?;
        return Ok(MaterializationResult {
            enclosure_block_id: id,
            content_block_id: Some(existing),
            status: "existing".into(),
            resolver_id: Some(child.resolver),
            error: None,
        });
    }
    let config = config_for_enclosure(core, id).await?;
    let response = collect::fetch(
        &config,
        &metadata.url,
        config.max_enclosure_bytes,
        None,
        None,
    )
    .await?;
    let observed = response.content_type.as_deref();
    let declared = metadata.declared_media_type.as_deref();
    let first = if metadata.family == Family::Rss {
        [declared, observed]
    } else {
        [observed, declared]
    };
    let candidates = [
        first[0],
        first[1],
        filename_media_type(&metadata.url),
        inkcre_core::knowledge::detect_media_type(&response.body),
    ];
    let knowledge = core.knowledge();
    let mut resolver = "core.file.v1".to_owned();
    for candidate in candidates {
        if let Some(id) = knowledge.match_media_type(candidate)? {
            resolver = id;
            break;
        }
    }
    let mut tx = core.pool().begin().await?;
    let current: Option<(String, String)> =
        sqlx::query_as("SELECT resolver,content FROM inkcre.blocks WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((current_resolver, content)) = current else {
        return Err(Error::Integrity("enclosure disappeared during retrieval"));
    };
    if current_resolver != ENCLOSURE_RESOLVER
        || serde_json::from_str::<CanonicalEnclosure>(&content)
            .map_err(|_| Error::Integrity("invalid updated enclosure"))?
            .url
            != metadata.url
    {
        return Err(Error::Conflict("enclosure changed during retrieval"));
    }
    if let Some(existing) = one_in(&mut tx, id, "content").await? {
        let child: Option<String> =
            sqlx::query_scalar("SELECT resolver FROM inkcre.blocks WHERE id=$1")
                .bind(existing)
                .fetch_optional(&mut *tx)
                .await?;
        let child = child.ok_or(Error::Integrity("enclosure content child is missing"))?;
        tx.rollback().await?;
        return Ok(MaterializationResult {
            enclosure_block_id: id,
            content_block_id: Some(existing),
            status: "existing".into(),
            resolver_id: Some(child),
            error: None,
        });
    }
    let pointer = knowledge
        .storage_create(&mut tx, target_storage, &response.body)
        .await?;
    let child: i32 = sqlx::query_scalar(
        "INSERT INTO inkcre.blocks(resolver,content,storage) VALUES($1,$2,$3) RETURNING id",
    )
    .bind(&resolver)
    .bind(pointer)
    .bind(target_storage)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO inkcre.relations(from_,to_,content) VALUES($1,$2,'content')")
        .bind(id)
        .bind(child)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(MaterializationResult {
        enclosure_block_id: id,
        content_block_id: Some(child),
        status: "created".into(),
        resolver_id: Some(resolver),
        error: None,
    })
}
