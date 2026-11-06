use std::time::Duration;

use chrono::Utc;
use inkcre_core::{Core, source::Source};
use reqwest::{
    Client, StatusCode,
    header::{CONTENT_TYPE, ETAG, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED},
};
use serde_json::{Value, json};

use crate::{Error, Result, enrichment, parser, reconcile, schema::*};

pub struct Response {
    pub status: StatusCode,
    pub body: Vec<u8>,
    pub effective_url: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
}

pub async fn fetch(
    config: &FeedConfig,
    url: &str,
    max_bytes: usize,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> Result<Response> {
    require_http(url)?;
    let client = Client::builder()
        .timeout(Duration::from_secs(config.request_timeout_seconds))
        .user_agent(&config.user_agent)
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()
        .map_err(|_| Error::Invalid("RSS HTTP client configuration is invalid"))?;
    let mut request = client.get(url);
    if let Some(value) = etag {
        request = request.header(IF_NONE_MATCH, value);
    }
    if let Some(value) = last_modified {
        request = request.header(IF_MODIFIED_SINCE, value);
    }
    let mut response = request
        .send()
        .await
        .map_err(|_| Error::Http("RSS HTTP request failed"))?;
    let status = response.status();
    if status != StatusCode::NOT_MODIFIED && !status.is_success() {
        return Err(Error::Http("RSS HTTP resource returned an error status"));
    }
    let effective_url = response.url().to_string();
    let etag = response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .or_else(|| {
            (status == StatusCode::NOT_MODIFIED)
                .then(|| etag.map(str::to_owned))
                .flatten()
        });
    let last_modified = response
        .headers()
        .get(LAST_MODIFIED)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .or_else(|| {
            (status == StatusCode::NOT_MODIFIED)
                .then(|| last_modified.map(str::to_owned))
                .flatten()
        });
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut body = Vec::new();
    if status != StatusCode::NOT_MODIFIED {
        if response
            .content_length()
            .is_some_and(|length| length > max_bytes as u64)
        {
            return Err(Error::Http("RSS HTTP resource declares too many bytes"));
        }
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Error::Http("RSS HTTP body retrieval failed"))?
        {
            if chunk.len() > max_bytes.saturating_sub(body.len()) {
                return Err(Error::Http(
                    "RSS HTTP resource exceeds its configured byte limit",
                ));
            }
            body.extend_from_slice(&chunk);
        }
    }
    Ok(Response {
        status,
        body,
        effective_url,
        etag,
        last_modified,
        content_type,
    })
}

pub async fn run(
    core: &Core,
    source: Source,
    family: Family,
    job: i64,
    job_config: CollectConfig,
) -> Result<Value> {
    let config = FeedConfig::parse(&source.config)?;
    let previous: FeedState = serde_json::from_value(source.state.clone())
        .map_err(|_| Error::Invalid("invalid persisted RSS checkpoint"))?;
    let scoped = previous.snapshot_configured_url.as_deref() == Some(&config.feed_url);
    core.job_patch_state(
        job,
        &json!({"family":family,"feed_url":config.feed_url,"diagnostics":[]}),
    )
    .await?;
    let response = fetch(
        &config,
        &config.feed_url,
        config.max_feed_bytes,
        if scoped {
            previous.etag.as_deref()
        } else {
            None
        },
        if scoped {
            previous.last_modified.as_deref()
        } else {
            None
        },
    )
    .await?;
    if response.status == StatusCode::NOT_MODIFIED {
        if !scoped {
            return Err(Error::Http(
                "feed returned 304 without a matching successful snapshot",
            ));
        }
        let patch = json!({"etag":response.etag,"last_modified":response.last_modified});
        let changed = sqlx::query(
            "UPDATE inkcre.sources SET state=state || $4 WHERE id=$1 AND config=$2 AND state=$3",
        )
        .bind(source.id)
        .bind(&source.config)
        .bind(&source.state)
        .bind(patch)
        .execute(core.pool())
        .await?
        .rows_affected();
        if changed == 0 {
            return Err(Error::Conflict(
                "Source changed during conditional retrieval",
            ));
        }
        return Ok(
            json!({"not_modified":true,"items":{"created":0,"updated":0,"unchanged":0,"skipped":0}}),
        );
    }
    let observed = Utc::now();
    let body = response.body;
    let configured_url = config.feed_url.clone();
    let effective_url = response.effective_url;
    let content_type = response.content_type;
    let source_id = source.id;
    let snapshot = tokio::task::spawn_blocking(move || {
        parser::parse_snapshot(
            &body,
            source_id,
            family,
            &configured_url,
            &effective_url,
            content_type.as_deref(),
        )
    })
    .await
    .map_err(|_| Error::Invalid("feed parser task failed"))??;
    let primary = reconcile::reconcile_snapshot(
        core,
        &source,
        &config,
        &previous,
        &snapshot,
        reconcile::SnapshotCheckpoint {
            observed_at: observed,
            etag: response.etag,
            last_modified: response.last_modified,
        },
    )
    .await?;
    let mut counts = json!({"created":0,"updated":0,"unchanged":0,"skipped":primary.skipped.len()});
    for (_, item) in &primary.items {
        let key = &item.action;
        let value = counts.get(key).and_then(Value::as_u64).unwrap_or(0);
        counts[key] = json!(value + 1);
    }
    let ids: Vec<i32> = primary
        .items
        .iter()
        .map(|(_, item)| item.block_id)
        .collect();
    let mut diagnostics = snapshot.diagnostics;
    diagnostics.extend(primary.skipped);
    let mut result = json!({"snapshot_observed_at":observed,"feed":primary.feed,"items":counts,"primary_item_block_ids":ids,"diagnostics":diagnostics});
    // primary graph与cursor已经同事务落地；后续可重建的enrichment不阻塞来源进度。
    core.job_patch_state(job, &result).await?;
    let fetch_full_text = job_config.fetch_full_text.unwrap_or(config.fetch_full_text);
    let download_enclosures = job_config
        .download_enclosures
        .unwrap_or(config.download_enclosures);
    let target_storage = job_config
        .target_storage_id
        .or(source.storage)
        .unwrap_or(config.target_storage_id);
    let mut enrichment_counts = json!({"full_text_created":0,"full_text_existing":0,"full_text_updated":0,"full_text_unavailable":0,"enclosure_created":0,"enclosure_existing":0,"failed":0});
    for (index, item) in &primary.items {
        if fetch_full_text {
            match enrichment::full_text(core, item.block_id, item.alternate_url_changed, false)
                .await
            {
                Ok(value) => increment(
                    &mut enrichment_counts,
                    &format!("full_text_{}", value.status),
                ),
                Err(_) => {
                    increment(&mut enrichment_counts, "failed");
                    result["diagnostics"].as_array_mut().ok_or(Error::Invalid("invalid diagnostics state"))?.push(json!({"scope":"full_text","code":"enrichment_failed","item_index":index}));
                }
            }
        }
        if download_enclosures {
            for edge in reconcile::outgoing(core, item.block_id, "enclosure").await? {
                let Some(enclosure) = edge.to_ else {
                    continue;
                };
                match enrichment::enclosure(core, enclosure, target_storage).await {
                    Ok(value) => increment(
                        &mut enrichment_counts,
                        &format!("enclosure_{}", value.status),
                    ),
                    Err(_) => {
                        increment(&mut enrichment_counts, "failed");
                        result["diagnostics"].as_array_mut().ok_or(Error::Invalid("invalid diagnostics state"))?.push(json!({"scope":"enclosure","code":"materialization_failed","item_index":index,"enclosure_block_id":enclosure}));
                    }
                }
            }
        }
    }
    result["enrichment"] = json!({"fetch_full_text":fetch_full_text,"download_enclosures":download_enclosures,"target_storage_id":target_storage,"results":enrichment_counts});
    Ok(result)
}

fn increment(counts: &mut Value, key: &str) {
    let value = counts.get(key).and_then(Value::as_u64).unwrap_or(0);
    counts[key] = json!(value + 1);
}
