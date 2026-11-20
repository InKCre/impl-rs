//! CalDAV/CardDAV REPORT selection and bounded query evaluation.
use crate::{
    Error, Result, codec,
    graph::Graph,
    properties::{PropertySelection, prop_response, selection},
    protocol::{CAL, CARD, DAV, esc, fail, href, multistatus, token, xml, xml_error},
};
use inkcre_extension_sdk::{HttpRequest, HttpResponse};
use roxmltree::Node;
pub(crate) fn report(graph: &Graph, request: &HttpRequest, path: &str) -> Result<HttpResponse> {
    let Some(collection) = graph.collections.get(path) else {
        return Ok(fail(404, "DAV collection not found"));
    };
    let Some(document) = xml(request)? else {
        return Ok(multistatus(String::new()));
    };
    let root = document.root_element();
    let selection = selection(Some(&document));
    let name = root.tag_name().name();
    let tag = collection.content.props.get("tag").map(String::as_str);
    if (root.tag_name().namespace() == Some(CAL) && tag != Some("VCALENDAR"))
        || (root.tag_name().namespace() == Some(CARD) && tag != Some("VADDRESSBOOK"))
    {
        return Ok(xml_error(403, DAV, "supported-report"));
    }
    let mut body = String::new();
    if [
        "principal-search-property-set",
        "principal-property-search",
        "expand-property",
    ]
    .iter()
    .any(|name| root.has_tag_name((DAV, *name)))
    {
        return Ok(multistatus(body));
    }
    if root.has_tag_name((CAL, "free-busy-query")) {
        let range = root
            .children()
            .find(|n| n.has_tag_name((CAL, "time-range")))
            .ok_or(Error::Invalid("free-busy-query requires time-range"))?;
        return Ok(crate::protocol::response(
            200,
            crate::temporal::free_busy(
                collection.items.values().map(|r| r.content.text.clone()),
                range,
            )?
            .into_bytes(),
            "text/calendar; charset=utf-8",
        ));
    }
    match (root.tag_name().namespace().unwrap_or(""), name) {
        (DAV, "sync-collection") => {
            let previous = root
                .children()
                .find(|n| n.has_tag_name((DAV, "sync-token")))
                .and_then(|n| n.text())
                .unwrap_or("");
            let current = token(collection);
            if !previous.is_empty() && previous != current {
                return Ok(xml_error(403, DAV, "valid-sync-token"));
            }
            if previous.is_empty() {
                for (href, item) in &collection.items {
                    body.push_str(&item_response(
                        &format!("{path}/{href}"),
                        item,
                        &selection,
                        root,
                    )?);
                }
            }
            body.push_str(&format!("<D:sync-token>{}</D:sync-token>", esc(&current)));
        }
        (CAL, "calendar-multiget") | (CARD, "addressbook-multiget") => {
            for href_node in root.children().filter(|n| n.has_tag_name((DAV, "href"))) {
                let raw = href_node.text().unwrap_or("");
                let decoded = url::Url::parse(&format!("http://dav.invalid{raw}"))
                    .ok()
                    .map(|u| u.path().to_owned())
                    .unwrap_or_else(|| raw.into());
                let relative = decoded.strip_prefix("/caldav/").unwrap_or(&decoded);
                let resource = collection
                    .items
                    .iter()
                    .find(|(item_href, _)| {
                        href(&format!("{path}/{item_href}"), false) == decoded
                            || format!("{path}/{item_href}") == relative
                    })
                    .map(|(_, r)| r);
                if let Some(item) = resource {
                    body.push_str(&item_response(
                        &format!("{path}/{}", item.content.href),
                        item,
                        &selection,
                        root,
                    )?);
                } else {
                    body.push_str(&format!("<D:response><D:href>{}</D:href><D:status>HTTP/1.1 404 Not Found</D:status></D:response>",esc(raw)));
                }
            }
        }
        (CAL, "calendar-query") | (CARD, "addressbook-query") => {
            let filter = root.children().find(|n| n.tag_name().name() == "filter");
            for (href, item) in &collection.items {
                let document = crate::temporal::query_document(&codec::parse(&item.content.text)?);
                if filter
                    .map(|f| filter_matches(f, &document))
                    .transpose()?
                    .unwrap_or(true)
                {
                    body.push_str(&item_response(
                        &format!("{path}/{href}"),
                        item,
                        &selection,
                        root,
                    )?);
                }
            }
        }
        _ => return Ok(xml_error(403, DAV, "supported-report")),
    }
    Ok(multistatus(body))
}
fn filter_matches(filter: Node<'_, '_>, component: &codec::Component) -> Result<bool> {
    let mut results = Vec::new();
    for node in filter.children().filter(Node::is_element) {
        let result = match node.tag_name().name() {
            "comp-filter" => {
                let name = node.attribute("name").unwrap_or("");
                let mut candidates = Vec::new();
                if component.name == name {
                    candidates.push(component)
                }
                candidates.extend(component.children.iter().filter(|c| c.name == name));
                let absent = node
                    .children()
                    .any(|n| n.tag_name().name() == "is-not-defined");
                if absent {
                    candidates.is_empty()
                } else {
                    let mut matched = false;
                    for c in candidates {
                        if filter_matches(node, c)? {
                            matched = true;
                            break;
                        }
                    }
                    matched
                }
            }
            "prop-filter" => {
                let name = node.attribute("name").unwrap_or("");
                let properties: Vec<_> = component
                    .properties
                    .iter()
                    .filter(|p| p.name.eq_ignore_ascii_case(name))
                    .collect();
                let absent = node
                    .children()
                    .any(|n| n.tag_name().name() == "is-not-defined");
                if absent {
                    properties.is_empty()
                } else {
                    let mut matched = false;
                    for p in properties {
                        let mut matches = true;
                        for condition in node.children().filter(Node::is_element) {
                            match condition.tag_name().name() {
                                "text-match" => matches &= text_matches(condition, &p.text())?,
                                "param-filter" => {
                                    let values =
                                        p.params.get(condition.attribute("name").unwrap_or(""));
                                    let absent = condition
                                        .children()
                                        .any(|n| n.tag_name().name() == "is-not-defined");
                                    matches &= if absent {
                                        values.is_none()
                                    } else if let Some(values) = values {
                                        let mut matched = false;
                                        for value in values {
                                            let mut good = true;
                                            for matcher in condition
                                                .children()
                                                .filter(|n| n.tag_name().name() == "text-match")
                                            {
                                                good &= text_matches(matcher, value)?
                                            }
                                            matched |= good
                                        }
                                        matched
                                    } else {
                                        false
                                    }
                                }
                                "time-range" => {
                                    matches &= crate::temporal::matches(condition, component)?
                                }
                                _ => return Err(Error::Invalid("unsupported DAV property filter")),
                            }
                        }
                        if matches {
                            matched = true;
                            break;
                        }
                    }
                    matched
                }
            }
            "time-range" => crate::temporal::matches(node, component)?,
            "is-not-defined" => true,
            _ => return Err(Error::Invalid("unsupported DAV query filter")),
        };
        results.push(result)
    }
    let test = filter
        .attribute("test")
        .unwrap_or(if filter.has_tag_name((CARD, "filter")) {
            "anyof"
        } else {
            "allof"
        });
    if !["anyof", "allof"].contains(&test) {
        return Err(Error::Invalid("unsupported DAV filter test"));
    }
    Ok(if test == "anyof" {
        results.into_iter().any(|v| v)
    } else {
        results.into_iter().all(|v| v)
    })
}
fn text_matches(node: Node<'_, '_>, value: &str) -> Result<bool> {
    let collation = node.attribute("collation").unwrap_or("i;unicode-casemap");
    if !["i;unicode-casemap", "i;ascii-casemap", "i;octet"].contains(&collation) {
        return Err(Error::Invalid("unsupported DAV text collation"));
    }
    let search = node.text().unwrap_or("");
    let (value, search) = if collation == "i;octet" {
        (value.to_owned(), search.to_owned())
    } else {
        (value.to_lowercase(), search.to_lowercase())
    };
    let matched = match node.attribute("match-type").unwrap_or("contains") {
        "contains" => value.contains(&search),
        "equals" => value == search,
        "starts-with" => value.starts_with(&search),
        "ends-with" => value.ends_with(&search),
        _ => return Err(Error::Invalid("unsupported DAV text match type")),
    };
    Ok(if node.attribute("negate-condition") == Some("yes") {
        !matched
    } else {
        matched
    })
}

fn item_response(
    path: &str,
    item: &crate::graph::Resource,
    selection: &PropertySelection,
    request: Node<'_, '_>,
) -> Result<String> {
    let mut selected = selection.clone();
    if let Some(data) = request
        .children()
        .find(|n| n.has_tag_name((DAV, "prop")))
        .and_then(|p| {
            p.children().find(|n| {
                n.has_tag_name((CAL, "calendar-data")) || n.has_tag_name((CARD, "address-data"))
            })
        })
    {
        selected.data_override = crate::projection::data(&item.content.text, data)?;
    }
    Ok(prop_response(path, None, Some(item), &selected))
}
