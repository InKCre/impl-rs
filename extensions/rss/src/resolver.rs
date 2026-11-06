use inkcre_core::{
    Core,
    knowledge::{
        TextOptions, TextProjectionContext, decode_unicode_bytes, format_label, html_text,
    },
};
use inkcre_extension_sdk::{Content, ResolverOperation, ResolverRequest, TextContext};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{Error, Result, enrichment, schema::*};

fn decode<T: DeserializeOwned>(content: &Content) -> Result<T> {
    let text = match content {
        Content::Text(text) => text.clone(),
        Content::Bytes(bytes) => decode_unicode_bytes(bytes)?,
    };
    serde_json::from_str(&text).map_err(|_| Error::Integrity("invalid canonical RSS content"))
}
fn value<T: Serialize>(value: T) -> Result<Value> {
    Ok(serde_json::to_value(value)?)
}
fn optional_text(text: Option<String>) -> Value {
    text.map_or(Value::Null, Value::String)
}
fn join(parts: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    let parts: Vec<String> = parts
        .into_iter()
        .flatten()
        .filter(|value| !value.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

fn plain(value: Option<&str>, media: Option<&str>) -> Result<Option<String>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if media.is_some_and(|media| media.to_ascii_lowercase().contains("html"))
        || value.starts_with('<')
    {
        let text = html_text(value)?.trim().to_owned();
        return Ok((!text.is_empty()).then_some(text));
    }
    Ok(Some(value.into()))
}

fn edges(request: &ResolverRequest, kind: &str) -> Result<Vec<i32>> {
    request
        .input
        .relations
        .iter()
        .filter(|edge| edge.from_ == Some(request.input.block.id) && edge.content == kind)
        .map(|edge| {
            edge.to_
                .ok_or(Error::Integrity("RSS graph projection has no endpoint"))
        })
        .collect()
}

#[derive(Serialize)]
struct SolvedItem {
    root: CanonicalItem,
    feed_block_id: Option<i32>,
    enclosure_block_ids: Vec<i32>,
    full_text_block_id: Option<i32>,
}

async fn solved_item(
    core: &Core,
    request: &ResolverRequest,
    materialize: bool,
) -> Result<SolvedItem> {
    let root: CanonicalItem = decode(&request.input.content)?;
    root.validate()?;
    let feed = edges(request, "feed")?;
    let enclosures = edges(request, "enclosure")?;
    let full_text = edges(request, "full_text")?;
    if feed.len() > 1 || full_text.len() > 1 {
        return Err(Error::Integrity(
            "feed item has ambiguous graph-owned projections",
        ));
    }
    let mut full_text = full_text.first().copied();
    if full_text.is_none() && materialize {
        // 原始资料和已保存的图引用在来源配置撤回后仍可读取。按需获取失败不伪造正文。
        if let Ok(result) = enrichment::full_text(core, request.input.block.id, false, true).await {
            full_text = result.content_block_id;
        }
    }
    Ok(SolvedItem {
        root,
        feed_block_id: feed.first().copied(),
        enclosure_block_ids: enclosures,
        full_text_block_id: full_text,
    })
}

pub async fn read(core: &Core, request: ResolverRequest) -> Result<Value> {
    match request.resolver.as_str() {
        FEED_RESOLVER => {
            let feed: CanonicalFeed = decode(&request.input.content)?;
            match request.operation {
                ResolverOperation::Solve { .. } => value(feed),
                ResolverOperation::Text { .. } => {
                    Ok(optional_text(join([feed.title, feed.description])))
                }
                ResolverOperation::Label { .. } => Ok(Value::String(format_label(
                    "feed",
                    feed.title.as_deref().or(Some(&feed.configured_url)),
                    false,
                ))),
                ResolverOperation::Read { .. } => {
                    Err(Error::Invalid("RSS Resolver has no custom read methods"))
                }
            }
        }
        ITEM_RESOLVER => match &request.operation {
            ResolverOperation::Solve {
                materialize_missing,
                ..
            } => value(solved_item(core, &request, *materialize_missing).await?),
            ResolverOperation::Text {
                refresh,
                materialize_missing,
                context,
            } => {
                let solved = solved_item(core, &request, *materialize_missing).await?;
                let full_text = if matches!(context, TextContext::Default) {
                    if let Some(block) = solved.full_text_block_id {
                        core.knowledge()
                            .text(
                                block,
                                TextOptions {
                                    refresh: *refresh,
                                    materialize_missing: *materialize_missing,
                                    context: TextProjectionContext::Default,
                                },
                            )
                            .await?
                    } else {
                        None
                    }
                } else {
                    None
                };
                let parts = [
                    plain(solved.root.title.as_deref(), None)?,
                    plain(solved.root.summary.as_deref(), Some("text/html"))?,
                    full_text.or(plain(
                        solved.root.authored_content.as_deref(),
                        solved.root.authored_content_type.as_deref(),
                    )?),
                ];
                Ok(optional_text(join(parts)))
            }
            ResolverOperation::Label { .. } => {
                let item: CanonicalItem = decode(&request.input.content)?;
                item.validate()?;
                let authored = plain(
                    item.authored_content.as_deref(),
                    item.authored_content_type.as_deref(),
                )?;
                let identifier = item
                    .title
                    .as_deref()
                    .or(item.source_native_id.as_deref())
                    .or(item.alternate_url.as_deref())
                    .or(authored.as_deref());
                Ok(Value::String(format_label("feed item", identifier, true)))
            }
            ResolverOperation::Read { .. } => {
                Err(Error::Invalid("RSS Resolver has no custom read methods"))
            }
        },
        ENCLOSURE_RESOLVER => {
            let enclosure: CanonicalEnclosure = decode(&request.input.content)?;
            match request.operation {
                ResolverOperation::Solve { .. } => {
                    let ids = edges(&request, "content")?;
                    if ids.len() > 1 {
                        return Err(Error::Integrity(
                            "enclosure has more than one semantic child",
                        ));
                    }
                    Ok(json!({"root":enclosure,"content_block_id":ids.first()}))
                }
                ResolverOperation::Text { .. } => Ok(optional_text(enclosure.title)),
                ResolverOperation::Label { .. } => Ok(Value::String(format_label(
                    "feed enclosure",
                    enclosure.title.as_deref().or(Some(&enclosure.url)),
                    false,
                ))),
                ResolverOperation::Read { .. } => {
                    Err(Error::Invalid("RSS Resolver has no custom read methods"))
                }
            }
        }
        _ => Err(Error::Invalid("unknown RSS Resolver")),
    }
}
