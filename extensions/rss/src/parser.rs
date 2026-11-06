use chrono::{DateTime, Timelike, Utc};
use feed_rs::{model::FeedType, parser::Builder};
use roxmltree::{Document, Node};
use serde_json::json;
use url::Url;

use crate::{Error, Result, schema::*};

fn text(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}
fn child_text(node: Node<'_, '_>, name: &str) -> Option<String> {
    node.children()
        .find(|child| child.is_element() && child.tag_name().name() == name)
        .and_then(|node| text(node.text()))
}

fn seconds(value: Option<DateTime<Utc>>) -> Option<DateTime<Utc>> {
    value.and_then(|value| value.with_nanosecond(0))
}

fn base_url(node: Node<'_, '_>, fallback: &str) -> String {
    let ancestors: Vec<_> = node.ancestors().filter(Node::is_element).collect();
    let mut base = fallback.to_owned();
    for ancestor in ancestors.into_iter().rev() {
        if let Some(value) = ancestor.attribute(("http://www.w3.org/XML/1998/namespace", "base"))
            && let Some(resolved) = address(value, &base)
        {
            base = resolved;
        }
    }
    base
}

fn address(value: &str, base: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    // 原生绝对地址保留作者字符串，避免额外URL规范化成为身份迁移。
    if Url::parse(value).is_ok() {
        return Some(value.into());
    }
    Url::parse(base)
        .ok()?
        .join(value)
        .ok()
        .map(|url| url.to_string())
}

fn alternate(node: Node<'_, '_>, base: &str) -> Option<String> {
    node.children()
        .filter(|child| child.is_element() && child.tag_name().name() == "link")
        .find_map(|link| {
            if let Some(href) = link.attribute("href") {
                if link.attribute("rel").unwrap_or("alternate") == "alternate" {
                    address(href, &base_url(link, base))
                } else {
                    None
                }
            } else {
                link.text()
                    .and_then(|value| address(value, &base_url(link, base)))
            }
        })
}

fn self_url(node: Node<'_, '_>, base: &str) -> Option<String> {
    node.children()
        .find(|child| {
            child.is_element()
                && child.tag_name().name() == "link"
                && child.attribute("rel") == Some("self")
        })
        .and_then(|link| {
            link.attribute("href")
                .and_then(|href| address(href, &base_url(link, base)))
        })
}

fn native_id(node: Node<'_, '_>, family: Family, base: &str) -> Option<String> {
    let name = if family == Family::Atom { "id" } else { "guid" };
    if let Some(id) = node
        .children()
        .find(|child| child.is_element() && child.tag_name().name() == name)
    {
        let value = text(id.text())?;
        // feedparser 的 Atom id 与 permalink GUID 按 xml:base 还原；
        // isPermaLink=false 的 GUID 是不透明标识，不能规范化为 URL。
        return if family == Family::Atom || id.attribute("isPermaLink").unwrap_or("true") == "true"
        {
            address(&value, &base_url(id, base))
        } else {
            Some(value)
        };
    }
    (family == Family::Rss)
        .then(|| text(node.attribute(("http://www.w3.org/1999/02/22-rdf-syntax-ns#", "about"))))
        .flatten()
}

pub fn decode_xml(body: &[u8], content_type: Option<&str>) -> Result<String> {
    if let Some((encoding, length)) = encoding_rs::Encoding::for_bom(body) {
        return encoding
            .decode_without_bom_handling_and_without_replacement(&body[length..])
            .map(|text| text.into_owned())
            .ok_or(Error::Invalid("invalid declared XML encoding"));
    }
    let prefix: String = body
        .iter()
        .take(512)
        .map(|value| char::from(*value))
        .collect();
    let declaration = prefix
        .strip_prefix("<?xml")
        .and_then(|part| part.split_once("?>"))
        .map(|(part, _)| part);
    let declared = declaration
        .and_then(|part| part.split_once("encoding"))
        .and_then(|(_, value)| value.split_once('='))
        .and_then(|(_, value)| {
            let value = value.trim_start();
            let quote = value.chars().next()?;
            matches!(quote, '\'' | '"')
                .then(|| value[1..].split(quote).next().unwrap_or("").to_owned())
        });
    let header = content_type.and_then(|value| {
        value.split(';').find_map(|part| {
            let (name, value) = part.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case("charset")
                .then(|| value.trim().trim_matches(['\'', '"']).to_owned())
        })
    });
    let charset = declared.or(header);
    let encoding = charset
        .as_deref()
        .map(|value| {
            encoding_rs::Encoding::for_label(value.as_bytes())
                .ok_or(Error::Invalid("unknown XML charset"))
        })
        .transpose()?
        .unwrap_or(encoding_rs::UTF_8);
    encoding
        .decode_without_bom_handling_and_without_replacement(body)
        .map(|text| text.into_owned())
        .ok_or(Error::Invalid("invalid XML character encoding"))
}

pub fn parse_snapshot(
    body: &[u8],
    source: i32,
    family: Family,
    configured_url: &str,
    effective_url: &str,
    content_type: Option<&str>,
) -> Result<Snapshot> {
    let xml = decode_xml(body, content_type)?;
    let xml = xml.trim_start_matches('\u{feff}');
    // 数据已归一为UTF-8，移除旧编码声明后再交给feed-rs，防止二次解码。
    let parse_xml = if xml.starts_with("<?xml") {
        xml.split_once("?>").map_or(xml, |(_, body)| body)
    } else {
        xml
    };
    let document = Document::parse(parse_xml)
        .map_err(|_| Error::Invalid("feed document is not usable XML"))?;
    let root = document.root_element();
    let actual_family = if root.tag_name().name() == "feed" {
        Family::Atom
    } else if matches!(root.tag_name().name(), "rss" | "RDF") {
        Family::Rss
    } else {
        return Err(Error::Invalid("unsupported feed family"));
    };
    if actual_family != family {
        return Err(Error::Invalid(
            "feed family differs from configured Source type",
        ));
    }
    let channel = if family == Family::Atom {
        root
    } else {
        root.children()
            .find(|node| node.is_element() && node.tag_name().name() == "channel")
            .ok_or(Error::Invalid("RSS document has no channel"))?
    };
    let raw_items: Vec<_> = if family == Family::Atom {
        root.children()
            .filter(|node| node.is_element() && node.tag_name().name() == "entry")
            .collect()
    } else if root.tag_name().name() == "RDF" {
        root.children()
            .filter(|node| node.is_element() && node.tag_name().name() == "item")
            .collect()
    } else {
        channel
            .children()
            .filter(|node| node.is_element() && node.tag_name().name() == "item")
            .collect()
    };
    let parsed = Builder::new()
        .base_uri(Some(effective_url))
        .id_generator(|_, _, _| String::new())
        .sanitize_content(false)
        .build()
        .parse(parse_xml.as_bytes())
        .map_err(|_| Error::Invalid("feed parser could not restore a usable document"))?;
    let parsed_family = match parsed.feed_type {
        FeedType::Atom => Family::Atom,
        FeedType::RSS0 | FeedType::RSS1 | FeedType::RSS2 => Family::Rss,
        _ => return Err(Error::Invalid("unsupported parsed feed family")),
    };
    if parsed_family != family {
        return Err(Error::Invalid(
            "parsed family differs from configured Source type",
        ));
    }
    if raw_items.len() != parsed.entries.len() {
        return Err(Error::Invalid(
            "feed entries cannot be aligned with authored protocol metadata",
        ));
    }
    let source_native_id =
        child_text(channel, "id").and_then(|id| address(&id, &base_url(channel, effective_url)));
    let home_url = alternate(channel, effective_url).or_else(|| source_native_id.clone());
    let feed = CanonicalFeed {
        source_instance_id: source,
        family,
        configured_url: configured_url.into(),
        source_native_id,
        declared_self_url: self_url(channel, effective_url),
        title: parsed
            .title
            .as_ref()
            .and_then(|value| text(Some(&value.content))),
        home_url,
        description: parsed
            .description
            .as_ref()
            .and_then(|value| text(Some(&value.content))),
        language: parsed.language,
        authored_updated_at: seconds(parsed.updated),
    };
    let mut diagnostics = Vec::new();
    let mut items = Vec::new();
    for (index, (entry, raw)) in parsed.entries.into_iter().zip(raw_items).enumerate() {
        let native = native_id(raw, family, effective_url);
        let mut categories = Vec::new();
        for category in entry.categories {
            let value = text(Some(&category.term)).or_else(|| text(category.label.as_deref()));
            let Some(value) = value else { continue };
            if !value.is_empty() && !categories.contains(&value) {
                categories.push(value);
            }
        }
        let item = CanonicalItem {
            source_native_id_kind: native.as_ref().map(|_| {
                if family == Family::Atom {
                    "atom_id"
                } else {
                    "rss_guid"
                }
                .into()
            }),
            source_native_id: native,
            alternate_url: alternate(raw, effective_url),
            title: entry.title.and_then(|value| text(Some(&value.content))),
            summary: entry.summary.and_then(|value| text(Some(&value.content))),
            authored_content: entry
                .content
                .as_ref()
                .and_then(|value| text(value.body.as_deref())),
            authored_content_type: entry
                .content
                .as_ref()
                .filter(|value| value.body.is_some())
                .map(|value| value.content_type.to_string()),
            authored_published_at: seconds(entry.published),
            authored_updated_at: seconds(entry.updated),
            authors: entry
                .authors
                .into_iter()
                .map(|author| FeedAuthor {
                    name: text(author.name.as_deref()),
                    email: text(author.email.as_deref()),
                    url: text(author.uri.as_deref()),
                })
                .filter(|author| {
                    author.name.is_some() || author.email.is_some() || author.url.is_some()
                })
                .collect(),
            categories,
        };
        if item.validate().is_err() {
            diagnostics.push(json!({"scope":"item","code":"malformed_item","item_index":index}));
            continue;
        }
        let mut enclosures = Vec::new();
        for node in raw.children().filter(|node| node.is_element()) {
            let native_enclosure = family == Family::Rss && node.tag_name().name() == "enclosure";
            let is_enclosure = native_enclosure
                || (node.tag_name().name() == "link" && node.attribute("rel") == Some("enclosure"));
            if !is_enclosure {
                continue;
            }
            let raw_url = node.attribute(if native_enclosure { "url" } else { "href" });
            let Some(url) =
                raw_url.and_then(|value| address(value, &base_url(node, effective_url)))
            else {
                diagnostics
                    .push(json!({"scope":"enclosure","code":"missing_url","item_index":index}));
                continue;
            };
            if require_http(&url).is_err() {
                diagnostics.push(
                    json!({"scope":"enclosure","code":"invalid_metadata","item_index":index}),
                );
                continue;
            }
            let declared_length = match node.attribute("length") {
                Some(length) if length.trim().starts_with('-') => {
                    diagnostics.push(
                        json!({"scope":"enclosure","code":"invalid_metadata","item_index":index}),
                    );
                    continue;
                }
                Some(length) => match length.trim().parse::<u64>() {
                    Ok(length) => Some(length),
                    Err(_) => {
                        diagnostics.push(json!({"scope":"enclosure","code":"invalid_declared_length","item_index":index}));
                        None
                    }
                },
                None => None,
            };
            enclosures.push(CanonicalEnclosure {
                family,
                url,
                declared_media_type: text(node.attribute("type")),
                declared_length,
                title: text(node.attribute("title")),
            });
        }
        items.push(ParsedItem { item, enclosures });
    }
    Ok(Snapshot {
        feed,
        items,
        diagnostics,
    })
}
