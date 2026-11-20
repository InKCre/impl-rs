//! DAV property discovery and projection share one authority with REPORT responses.
use crate::{
    Error, Result,
    graph::{Collection, Graph, Resource},
    protocol::{
        CAL, CARD, DAV, collection_etag, element, esc, etag, fail, header, href, multistatus,
        split, status_text, token, xml,
    },
    schema::*,
};
use inkcre_extension_sdk::{HttpRequest, HttpResponse};
use roxmltree::{Document, Node};
#[derive(Clone)]
pub(crate) struct PropertySelection {
    names: Vec<(String, String)>,
    names_only: bool,
    explicit: bool,
    pub(crate) data_override: Option<String>,
}
pub(crate) fn selection(document: Option<&Document<'_>>) -> PropertySelection {
    let all = vec![
        (DAV, "displayname"),
        (DAV, "resourcetype"),
        (DAV, "getetag"),
        (DAV, "getlastmodified"),
        (DAV, "getcontenttype"),
        (DAV, "getcontentlength"),
        (DAV, "current-user-principal"),
        (DAV, "principal-URL"),
        (DAV, "owner"),
        (DAV, "current-user-privilege-set"),
        (DAV, "supported-report-set"),
        (DAV, "sync-token"),
        (CAL, "calendar-home-set"),
        (CARD, "addressbook-home-set"),
        (CAL, "supported-calendar-component-set"),
    ]
    .into_iter()
    .map(|(ns, name)| (ns.into(), name.into()))
    .collect();
    let prop = document.and_then(|d| {
        d.root_element()
            .children()
            .find(|n| n.has_tag_name((DAV, "prop")))
    });
    PropertySelection {
        data_override: None,
        explicit: prop.is_some(),
        names: prop
            .map(|p| {
                p.children()
                    .filter(Node::is_element)
                    .map(|n| {
                        (
                            n.tag_name().namespace().unwrap_or("").into(),
                            n.tag_name().name().into(),
                        )
                    })
                    .collect()
            })
            .unwrap_or(all),
        names_only: document.is_some_and(|d| {
            d.root_element()
                .children()
                .any(|n| n.has_tag_name((DAV, "propname")))
        }),
    }
}
fn collection_property(
    path: &str,
    collection: Option<&Collection>,
    ns: &str,
    name: &str,
) -> Option<String> {
    let props = collection.map(|c| &c.content.props);
    let tag = props.and_then(|p| p.get("tag")).map(String::as_str);
    let principal = path == "inkcre";
    let root = path.is_empty();
    let contact = tag == Some("VADDRESSBOOK");
    match (ns, name) {
        (DAV, "resourcetype") => {
            let resource_type = if principal {
                "<D:collection/><D:principal/>"
            } else if tag == Some("VCALENDAR") {
                "<D:collection/><C:calendar/>"
            } else if contact {
                "<D:collection/><CR:addressbook/>"
            } else {
                "<D:collection/>"
            };
            Some(resource_type.into())
        }
        (DAV, "displayname") => {
            let fallback = if principal { "inkcre" } else { path };
            Some(esc(props
                .and_then(|p| p.get("D:displayname"))
                .map(String::as_str)
                .unwrap_or(fallback)))
        }
        (DAV, "current-user-principal" | "principal-URL" | "owner") => {
            Some("<D:href>/caldav/inkcre/</D:href>".into())
        }
        (CAL, "calendar-home-set") | (CARD, "addressbook-home-set") if principal || root => {
            Some("<D:href>/caldav/inkcre/</D:href>".into())
        }
        (DAV, "current-user-privilege-set") => Some("<D:privilege><D:all/></D:privilege>".into()),
        (DAV, "getetag") => collection.map(collection_etag).map(|v| esc(&v)),
        (DAV, "sync-token") => collection
            .filter(|_| tag.is_some())
            .map(token)
            .map(|v| esc(&v)),
        (DAV, "getlastmodified") => {
            collection.map(|c| c.modified.format("%a, %d %b %Y %H:%M:%S GMT").to_string())
        }
        (DAV, "getcontenttype") if tag.is_some() => Some(
            if contact {
                "text/vcard"
            } else {
                "text/calendar"
            }
            .into(),
        ),
        (CAL, "supported-calendar-component-set") if tag == Some("VCALENDAR") => {
            let components = props
                .and_then(|p| p.get("C:supported-calendar-component-set"))
                .map(String::as_str)
                .unwrap_or("VEVENT,VTODO,VJOURNAL");
            Some(
                components
                    .split(',')
                    .map(|name| format!("<C:comp name=\"{}\"/>", esc(name)))
                    .collect(),
            )
        }
        (CAL, "supported-calendar-data") if tag == Some("VCALENDAR") => {
            Some("<C:calendar-data content-type=\"text/calendar\" version=\"2.0\"/>".into())
        }
        (CARD, "supported-address-data") if contact => {
            Some("<CR:address-data-type content-type=\"text/vcard\" version=\"3.0\"/>".into())
        }
        (CAL, "max-resource-size") | (CARD, "max-resource-size") => {
            Some((8 * 1024 * 1024).to_string())
        }
        (DAV, "supported-report-set") => {
            let (prefix, family) = if contact {
                ("CR", "addressbook")
            } else {
                ("C", "calendar")
            };
            Some(format!(
                "<D:supported-report><D:report><D:sync-collection/></D:report></D:supported-report><D:supported-report><D:report><{prefix}:{family}-query/></D:report></D:supported-report><D:supported-report><D:report><{prefix}:{family}-multiget/></D:report></D:supported-report>"
            ))
        }
        _ => {
            let key = match ns {
                DAV => format!("D:{name}"),
                CAL => format!("C:{name}"),
                CARD => format!("CR:{name}"),
                _ => format!("{{{ns}}}{name}"),
            };
            props.and_then(|p| p.get(&key)).map(|v| esc(v))
        }
    }
}

fn item_property(item: &Resource, ns: &str, name: &str) -> Option<String> {
    match (ns, name) {
        (DAV, "getetag") => Some(esc(&etag(&item.content.text))),
        (DAV, "getcontenttype") => Some(
            if item.resolver == CONTACT {
                "text/vcard; charset=utf-8"
            } else {
                "text/calendar; charset=utf-8"
            }
            .into(),
        ),
        (DAV, "getcontentlength") => Some(item.content.text.len().to_string()),
        (DAV, "getlastmodified") => Some(
            item.modified
                .format("%a, %d %b %Y %H:%M:%S GMT")
                .to_string(),
        ),
        (DAV, "resourcetype") => Some(String::new()),
        (CAL, "calendar-data") if item.resolver == CALENDAR => Some(esc(&item.content.text)),
        (CARD, "address-data") if item.resolver == CONTACT => Some(esc(&item.content.text)),
        _ => None,
    }
}
pub(crate) fn prop_response(
    path: &str,
    collection: Option<&Collection>,
    item: Option<&Resource>,
    selection: &PropertySelection,
) -> String {
    let mut ok = String::new();
    let mut missing = String::new();
    let mut names = selection.names.clone();
    if !selection.explicit
        && let Some(collection) = collection
    {
        for key in collection
            .content
            .props
            .keys()
            .filter(|key| key.as_str() != "tag")
        {
            let qualified = if let Some(name) = key.strip_prefix("D:") {
                Some((DAV.to_owned(), name.to_owned()))
            } else if let Some(name) = key.strip_prefix("C:") {
                Some((CAL.to_owned(), name.to_owned()))
            } else if let Some(name) = key.strip_prefix("CR:") {
                Some((CARD.to_owned(), name.to_owned()))
            } else {
                key.strip_prefix('{')
                    .and_then(|key| key.split_once('}'))
                    .map(|(ns, name)| (ns.to_owned(), name.to_owned()))
            };
            if let Some(qualified) = qualified
                && !names.contains(&qualified)
            {
                names.push(qualified);
            }
        }
    }
    for (ns, name) in &names {
        let value = if let Some(item) = item {
            if (ns == CAL && name == "calendar-data") || (ns == CARD && name == "address-data") {
                selection
                    .data_override
                    .as_ref()
                    .map(|text| esc(text))
                    .or_else(|| item_property(item, ns, name))
            } else {
                item_property(item, ns, name)
            }
        } else {
            collection_property(path, collection, ns, name)
        };
        if let Some(value) = value {
            ok.push_str(&element(
                ns,
                name,
                if selection.names_only { "" } else { &value },
            ));
        } else if selection.explicit {
            missing.push_str(&element(ns, name, ""));
        }
    }
    let mut body = format!(
        "<D:response><D:href>{}</D:href>",
        esc(&href(path, item.is_none()))
    );
    for (status, props) in [(200, ok), (404, missing)] {
        if !props.is_empty() {
            body.push_str(&format!("<D:propstat><D:prop>{props}</D:prop><D:status>HTTP/1.1 {status} {}</D:status></D:propstat>",status_text(status)));
        }
    }
    body.push_str("</D:response>");
    body
}
pub(crate) fn propfind(graph: &Graph, request: &HttpRequest, path: &str) -> Result<HttpResponse> {
    let document = xml(request)?;
    let selection = selection(document.as_ref());
    let depth = header(request, "depth").unwrap_or("infinity");
    if !["0", "1", "infinity"].contains(&depth) {
        return Err(Error::Invalid("invalid DAV Depth"));
    }
    let mut body = String::new();
    if path.is_empty() || graph.collections.contains_key(path) {
        let collection = graph.collections.get(path);
        body.push_str(&prop_response(path, collection, None, &selection));
        if depth != "0" {
            if let Some(collection) = collection {
                for (href, item) in &collection.items {
                    body.push_str(&prop_response(
                        &format!("{path}/{href}"),
                        None,
                        Some(item),
                        &selection,
                    ));
                }
            }
            for (child, c) in &graph.collections {
                if child != path
                    && if depth == "infinity" {
                        path.is_empty() || child.starts_with(&format!("{path}/"))
                    } else {
                        split(child).0 == path
                    }
                {
                    body.push_str(&prop_response(child, Some(c), None, &selection));
                    if depth == "infinity" {
                        for (href, item) in &c.items {
                            body.push_str(&prop_response(
                                &format!("{child}/{href}"),
                                None,
                                Some(item),
                                &selection,
                            ));
                        }
                    }
                }
            }
        }
    } else {
        let (parent, href) = split(path);
        let Some(item) = graph
            .collections
            .get(parent)
            .and_then(|c| c.items.get(href))
        else {
            return Ok(fail(404, "DAV resource not found"));
        };
        body.push_str(&prop_response(path, None, Some(item), &selection));
    }
    Ok(multistatus(body))
}
