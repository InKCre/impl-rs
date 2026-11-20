//! DAV method dispatch owns protocol authentication, conditions and publication semantics.
use crate::{
    Error, Result, codec,
    graph::{self, Collection, Graph, Resource},
    schema::*,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::Utc;
use inkcre_core::Core;
use inkcre_extension_sdk::{HttpBody, HttpHeader, HttpRequest, HttpResponse};
use roxmltree::{Document, Node};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use subtle::ConstantTimeEq;

pub const METHODS: [&str; 11] = [
    "OPTIONS",
    "PROPFIND",
    "REPORT",
    "GET",
    "HEAD",
    "PUT",
    "DELETE",
    "MKCOL",
    "MKCALENDAR",
    "PROPPATCH",
    "MOVE",
];
pub(crate) const DAV: &str = "DAV:";
pub(crate) const CAL: &str = "urn:ietf:params:xml:ns:caldav";
pub(crate) const CARD: &str = "urn:ietf:params:xml:ns:carddav";
const SYNC: &str = "https://inkcre.dev/caldav/sync/";
pub fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .and_then(|h| std::str::from_utf8(&h.value).ok())
}
pub fn response(status: u16, body: impl Into<Vec<u8>>, mime: &str) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![HttpHeader {
            name: "content-type".into(),
            value: mime.as_bytes().to_vec(),
        }],
        body: HttpBody(body.into()),
    }
}
fn extra(response: &mut HttpResponse, name: &str, value: impl AsRef<[u8]>) {
    response.headers.push(HttpHeader {
        name: name.into(),
        value: value.as_ref().to_vec(),
    });
}
pub(crate) fn fail(status: u16, message: &str) -> HttpResponse {
    response(status, message.as_bytes(), "text/plain; charset=utf-8")
}
pub(crate) fn xml_error(status: u16, namespace: &str, name: &str) -> HttpResponse {
    response(
        status,
        format!(
            "<?xml version=\"1.0\"?><D:error xmlns:D=\"DAV:\"><X:{name} xmlns:X=\"{}\"/></D:error>",
            esc(namespace)
        ),
        "application/xml; charset=utf-8",
    )
}
fn unauthorized() -> HttpResponse {
    let mut r = fail(401, "Invalid DAV credentials");
    extra(
        &mut r,
        "www-authenticate",
        r#"Basic realm="InKCre", charset="UTF-8""#,
    );
    r
}
async fn authenticated(core: &Core, request: &HttpRequest) -> Result<bool> {
    let Some(value) = header(request, "authorization") else {
        return Ok(false);
    };
    let Some((scheme, encoded)) = value.split_once(' ') else {
        return Ok(false);
    };
    if !scheme.eq_ignore_ascii_case("basic") {
        return Ok(false);
    }
    let Ok(decoded) = STANDARD.decode(encoded) else {
        return Ok(false);
    };
    let Ok(decoded) = String::from_utf8(decoded) else {
        return Ok(false);
    };
    let Some((user, password)) = decoded.split_once(':') else {
        return Ok(false);
    };
    let Some(extension) = core.extension_get(NAME).await? else {
        return Ok(false);
    };
    let Ok(config) = serde_json::from_value::<Config>(extension.config) else {
        return Ok(false);
    };
    if config.validate().is_err() {
        return Ok(false);
    }
    Ok(user == "inkcre"
        && config
            .password
            .is_some_and(|configured| bool::from(configured.as_bytes().ct_eq(password.as_bytes()))))
}
pub async fn handle(core: &Core, request: HttpRequest) -> Result<HttpResponse> {
    if !authenticated(core, &request).await? {
        return Ok(unauthorized());
    }
    if request.body.0.len() > 16 * 1024 * 1024 {
        return Ok(fail(413, "DAV request exceeds 16 MiB"));
    }
    if !METHODS.contains(&request.method.as_str()) {
        let mut r = fail(405, "DAV method not allowed");
        extra(&mut r, "allow", METHODS.join(", "));
        return Ok(r);
    }
    let path = request
        .path
        .strip_prefix("/caldav")
        .ok_or(Error::Invalid("invalid DAV route"))?
        .trim_matches('/');
    if path.split('/').any(|part| part == "." || part == "..")
        || (!path.is_empty() && path != "inkcre" && !path.starts_with("inkcre/"))
    {
        return Ok(fail(403, "DAV path is outside the authenticated principal"));
    }
    if request.method == "OPTIONS" {
        let mut r = response(200, vec![], "text/plain");
        extra(
            &mut r,
            "dav",
            "1, 2, 3, calendar-access, addressbook, extended-mkcol",
        );
        extra(&mut r, "allow", METHODS.join(", "));
        return Ok(r);
    }
    // Predefined personal collections are created lazily, just as the canonical DAV storage.
    ensure_defaults(core).await?;
    let write = matches!(
        request.method.as_str(),
        "PUT" | "DELETE" | "MKCOL" | "MKCALENDAR" | "PROPPATCH" | "MOVE"
    );
    let mut tx = graph::transaction(core, write).await?;
    let mut graph = Graph::load(&mut tx, write).await?;
    let result = dispatch(&mut graph, &request, path);
    let response = match result {
        Ok(r) => r,
        Err(Error::Invalid(message)) => fail(400, message),
        Err(error) => return Err(error),
    };
    if write && response.status < 400 {
        graph.save(core, &mut tx).await?;
    }
    tx.commit().await?;
    Ok(response)
}
async fn ensure_defaults(core: &Core) -> Result<()> {
    let initialized: i64 = sqlx::query_scalar("SELECT count(DISTINCT content::jsonb->>'path') FROM inkcre.blocks WHERE resolver=ANY($1) AND content::jsonb->>'path'=ANY($2)")
        .bind([COLLECTION,LEGACY_COLLECTION]).bind(["inkcre","inkcre/calendar","inkcre/tasks","inkcre/contacts"]).fetch_one(core.pool()).await?;
    if initialized == 4 {
        return Ok(());
    }
    let mut tx = graph::transaction(core, true).await?;
    let mut graph = Graph::load(&mut tx, true).await?;
    let mut changed = false;
    for (path, tag, label, component) in [
        ("inkcre", "", "", ""),
        ("inkcre/calendar", "VCALENDAR", "InKCre 日历", "VEVENT"),
        ("inkcre/tasks", "VCALENDAR", "InKCre 任务", "VTODO"),
        ("inkcre/contacts", "VADDRESSBOOK", "InKCre 联系人", ""),
    ] {
        if graph.collections.contains_key(path) {
            continue;
        }
        // A published collection deleted by a client stays withdrawn instead of reappearing.
        let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM inkcre.blocks WHERE resolver=ANY($1) AND content::jsonb->>'path'=$2)").bind([COLLECTION,LEGACY_COLLECTION]).bind(path).fetch_one(tx.connection()).await?;
        if exists {
            continue;
        }
        let mut props = BTreeMap::new();
        if !tag.is_empty() {
            props.insert("tag".into(), tag.into());
            props.insert("D:displayname".into(), label.into());
        }
        if !component.is_empty() {
            props.insert(
                "C:supported-calendar-component-set".into(),
                component.into(),
            );
        }
        graph.collections.insert(
            path.into(),
            Collection {
                id: None,
                content: CollectionContent {
                    path: path.into(),
                    props,
                    published: true,
                },
                items: BTreeMap::new(),
                modified: Utc::now(),
            },
        );
        changed = true;
    }
    if changed {
        graph.save(core, &mut tx).await?;
    }
    tx.commit().await?;
    Ok(())
}
fn dispatch(graph: &mut Graph, request: &HttpRequest, path: &str) -> Result<HttpResponse> {
    match request.method.as_str() {
        "GET" | "HEAD" => get(graph, request, path),
        "PROPFIND" => crate::properties::propfind(graph, request, path),
        "REPORT" => crate::report::report(graph, request, path),
        "PUT" => put(graph, request, path),
        "DELETE" => delete(graph, request, path),
        "MKCOL" | "MKCALENDAR" => mkcol(graph, request, path),
        "PROPPATCH" => proppatch(graph, request, path),
        "MOVE" => move_resource(graph, request, path),
        _ => Ok(fail(405, "DAV method not allowed")),
    }
}
pub(crate) fn split(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}
pub(crate) fn etag(text: &str) -> String {
    format!("\"{:x}\"", Sha256::digest(text.as_bytes()))
}
pub(crate) fn collection_etag(collection: &Collection) -> String {
    let mut content = serde_json::to_string(&collection.content.props).unwrap_or_default();
    for (href, item) in &collection.items {
        content.push_str(href);
        content.push_str(&item.content.text)
    }
    etag(&content)
}
pub(crate) fn token(collection: &Collection) -> String {
    format!("{SYNC}{}", collection_etag(collection).trim_matches('"'))
}
fn condition(request: &HttpRequest, current: Option<&str>, strict: bool) -> bool {
    if let Some(value) = header(request, "if-match") {
        if current.is_none()
            || !(value == "*" || value.split(',').map(str::trim).any(|v| Some(v) == current))
        {
            return false;
        }
    } else if strict && current.is_some() {
        return false;
    }
    if let Some(value) = header(request, "if-none-match")
        && current.is_some_and(|current| {
            value == "*" || value.split(',').map(str::trim).any(|v| v == current)
        })
    {
        return false;
    }
    true
}
fn get(graph: &Graph, request: &HttpRequest, path: &str) -> Result<HttpResponse> {
    let (parent, href) = split(path);
    let (item, collection) = if let Some(c) = graph.collections.get(path) {
        (None, Some(c))
    } else {
        (
            graph
                .collections
                .get(parent)
                .and_then(|c| c.items.get(href)),
            None,
        )
    };
    let (body, mime, tag, modified) = if let Some(item) = item {
        (
            item.content.text.clone(),
            if item.resolver == CONTACT {
                "text/vcard; charset=utf-8"
            } else {
                "text/calendar; charset=utf-8"
            },
            etag(&item.content.text),
            item.modified,
        )
    } else if let Some(collection) = collection {
        if !collection.content.props.contains_key("tag") {
            return Ok(fail(403, "DAV principal cannot be downloaded"));
        }
        let contact = collection
            .content
            .props
            .get("tag")
            .is_some_and(|v| v == "VADDRESSBOOK");
        let text = if contact {
            collection
                .items
                .values()
                .map(|r| r.content.text.as_str())
                .collect::<String>()
        } else {
            let mut calendar=codec::Component{name:"VCALENDAR".into(),properties:codec::parse("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//InKCre//CalDAV Rust//EN\r\nEND:VCALENDAR\r\n")?.properties,children:vec![]};
            for item in collection.items.values() {
                calendar
                    .children
                    .extend(codec::parse(&item.content.text)?.children)
            }
            calendar.wire()
        };
        (
            text,
            if contact {
                "text/vcard; charset=utf-8"
            } else {
                "text/calendar; charset=utf-8"
            },
            collection_etag(collection),
            collection.modified,
        )
    } else {
        return Ok(fail(404, "DAV resource not found"));
    };
    if header(request, "if-none-match")
        .is_some_and(|v| v == "*" || v.split(',').map(str::trim).any(|v| v == tag))
    {
        let mut r = response(304, vec![], mime);
        extra(&mut r, "etag", tag);
        return Ok(r);
    }
    if !condition(request, Some(&tag), false) {
        return Ok(fail(412, "DAV precondition failed"));
    }
    let length = body.len();
    let mut r = response(
        200,
        if request.method == "HEAD" {
            vec![]
        } else {
            body.into_bytes()
        },
        mime,
    );
    extra(&mut r, "etag", tag);
    extra(
        &mut r,
        "last-modified",
        modified.format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
    );
    extra(&mut r, "content-length", length.to_string());
    Ok(r)
}
fn put(graph: &mut Graph, request: &HttpRequest, path: &str) -> Result<HttpResponse> {
    if graph.collections.contains_key(path) {
        return Ok(fail(405, "DAV collection overwrite is disabled"));
    }
    if request.body.0.len() > 8 * 1024 * 1024 {
        return Ok(fail(413, "DAV resource exceeds 8 MiB"));
    }
    let (parent, href) = split(path);
    let Some(collection) = graph.collections.get_mut(parent) else {
        return Ok(fail(409, "DAV parent collection is missing"));
    };
    let Some(kind) = collection.content.props.get("tag") else {
        return Ok(fail(409, "DAV resource requires a calendar or addressbook"));
    };
    let text = std::str::from_utf8(&request.body.0)
        .map_err(|_| Error::Invalid("DAV resource must be UTF-8"))?;
    codec::validate(text, kind)?;
    let (remainder, props) = codec::extract(text)?;
    let text = codec::compose(&remainder, &props)?;
    let current = collection.items.get(href);
    let old_tag = current.map(|r| etag(&r.content.text));
    if !condition(request, old_tag.as_deref(), true) {
        return Ok(fail(412, "DAV precondition failed"));
    }
    let uid = codec::uid(&text)?;
    if collection
        .items
        .iter()
        .any(|(other, r)| other != href && codec::uid(&r.content.text).ok().as_ref() == Some(&uid))
    {
        return Ok(xml_error(
            409,
            if kind == "VADDRESSBOOK" { CARD } else { CAL },
            "no-uid-conflict",
        ));
    }
    if let Some(current) = current
        && codec::uid(&current.content.text)? != uid
    {
        return Ok(xml_error(
            403,
            if kind == "VADDRESSBOOK" { CARD } else { CAL },
            "no-uid-conflict",
        ));
    }
    let existed = current.is_some();
    let resource = Resource {
        id: current.and_then(|r| r.id),
        resolver: if kind == "VADDRESSBOOK" {
            CONTACT
        } else {
            CALENDAR
        }
        .into(),
        content: ResourceContent {
            href: href.into(),
            text: text.clone(),
        },
        modified: Utc::now(),
    };
    collection.items.insert(href.into(), resource);
    collection.modified = Utc::now();
    let mut r = response(if existed { 204 } else { 201 }, vec![], "text/plain");
    extra(&mut r, "etag", etag(&text));
    Ok(r)
}
fn delete(graph: &mut Graph, request: &HttpRequest, path: &str) -> Result<HttpResponse> {
    if path == "inkcre" || path.is_empty() {
        return Ok(fail(403, "DAV principal cannot be deleted"));
    }
    if let Some(collection) = graph.collections.get(path) {
        let tag = collection_etag(collection);
        if !condition(request, Some(&tag), true) {
            return Ok(fail(412, "DAV precondition failed"));
        }
        graph
            .collections
            .retain(|key, _| key != path && !key.starts_with(&format!("{path}/")));
        return Ok(response(200, vec![], "text/plain"));
    }
    let (parent, href) = split(path);
    let Some(collection) = graph.collections.get_mut(parent) else {
        return Ok(fail(404, "DAV resource not found"));
    };
    let Some(item) = collection.items.get(href) else {
        return Ok(fail(404, "DAV resource not found"));
    };
    if !condition(request, Some(&etag(&item.content.text)), true) {
        return Ok(fail(412, "DAV precondition failed"));
    }
    collection.items.remove(href);
    collection.modified = Utc::now();
    Ok(response(200, vec![], "text/plain"))
}
pub(crate) fn xml(request: &HttpRequest) -> Result<Option<Document<'_>>> {
    if request.body.0.is_empty() {
        return Ok(None);
    }
    let text = std::str::from_utf8(&request.body.0)
        .map_err(|_| Error::Invalid("DAV XML must be UTF-8"))?;
    if text.contains("<!DOCTYPE") || text.contains("<!ENTITY") {
        return Err(Error::Invalid("DAV XML entities are disabled"));
    }
    Ok(Some(
        Document::parse(text).map_err(|_| Error::Invalid("invalid DAV XML"))?,
    ))
}
pub(crate) fn prop_key(node: Node<'_, '_>) -> String {
    let ns = node.tag_name().namespace().unwrap_or("");
    let name = node.tag_name().name();
    match ns {
        DAV => format!("D:{name}"),
        CAL => format!("C:{name}"),
        CARD => format!("CR:{name}"),
        _ => format!("{{{ns}}}{name}"),
    }
}
fn decode_props(document: Option<&Document<'_>>) -> Result<BTreeMap<String, String>> {
    let mut props = BTreeMap::new();
    let Some(document) = document else {
        return Ok(props);
    };
    for prop in document
        .descendants()
        .filter(|n| n.is_element() && n.tag_name().name() == "prop")
        .flat_map(|p| p.children().filter(Node::is_element))
    {
        let key = prop_key(prop);
        match key.as_str() {
            "D:resourcetype" => {
                if prop.children().any(|n| n.has_tag_name((CAL, "calendar"))) {
                    props.insert("tag".into(), "VCALENDAR".into());
                } else if prop
                    .children()
                    .any(|n| n.has_tag_name((CARD, "addressbook")))
                {
                    props.insert("tag".into(), "VADDRESSBOOK".into());
                }
            }
            "C:supported-calendar-component-set" => {
                props.insert(
                    key,
                    prop.children()
                        .filter(|n| n.has_tag_name((CAL, "comp")))
                        .filter_map(|n| n.attribute("name"))
                        .collect::<Vec<_>>()
                        .join(","),
                );
            }
            _ => {
                props.insert(key, prop.text().unwrap_or("").into());
            }
        }
    }
    Ok(props)
}
fn mkcol(graph: &mut Graph, request: &HttpRequest, path: &str) -> Result<HttpResponse> {
    if graph.collections.contains_key(path) {
        return Ok(fail(405, "DAV collection already exists"));
    }
    let (parent, _) = split(path);
    if !graph.collections.contains_key(parent) {
        return Ok(fail(409, "DAV parent collection is missing"));
    }
    if graph.collections[parent].content.props.contains_key("tag") {
        return Ok(fail(
            403,
            "DAV nested calendar collections are not supported",
        ));
    }
    let document = xml(request)?;
    let mut props = decode_props(document.as_ref())?;
    if request.method == "MKCALENDAR" {
        props.insert("tag".into(), "VCALENDAR".into());
    }
    if props
        .get("tag")
        .is_some_and(|v| v != "VCALENDAR" && v != "VADDRESSBOOK")
    {
        return Ok(fail(400, "invalid DAV collection type"));
    }
    graph.collections.insert(
        path.into(),
        Collection {
            id: None,
            content: CollectionContent {
                path: path.into(),
                props,
                published: true,
            },
            items: BTreeMap::new(),
            modified: Utc::now(),
        },
    );
    Ok(response(201, vec![], "text/plain"))
}
fn proppatch(graph: &mut Graph, request: &HttpRequest, path: &str) -> Result<HttpResponse> {
    let Some(collection) = graph.collections.get_mut(path) else {
        return Ok(fail(404, "DAV collection not found"));
    };
    let document = xml(request)?.ok_or(Error::Invalid("PROPPATCH requires XML"))?;
    let mut changed = collection.content.props.clone();
    let mut statuses = Vec::new();
    for operation in document.root_element().children().filter(Node::is_element) {
        let remove = operation.has_tag_name((DAV, "remove"));
        if !remove && !operation.has_tag_name((DAV, "set")) {
            return Err(Error::Invalid("invalid PROPPATCH operation"));
        }
        for prop in operation
            .children()
            .filter(|n| n.has_tag_name((DAV, "prop")))
            .flat_map(|n| n.children().filter(Node::is_element))
        {
            let key = prop_key(prop);
            let protected = matches!(
                key.as_str(),
                "D:resourcetype"
                    | "D:getetag"
                    | "D:sync-token"
                    | "D:current-user-principal"
                    | "D:owner"
            );
            statuses.push((
                prop.tag_name().namespace().unwrap_or("").to_owned(),
                prop.tag_name().name().to_owned(),
                if protected { 403 } else { 200 },
            ));
            if !protected {
                if remove {
                    changed.remove(&key);
                } else if key == "C:supported-calendar-component-set" {
                    changed.insert(
                        key,
                        prop.children()
                            .filter_map(|n| n.attribute("name"))
                            .collect::<Vec<_>>()
                            .join(","),
                    );
                } else {
                    changed.insert(key, prop.text().unwrap_or("").into());
                }
            }
        }
    }
    let failed = statuses.iter().any(|(_, _, status)| *status != 200);
    if !failed {
        collection.content.props = changed;
        collection.modified = Utc::now();
    }
    let mut body = String::new();
    for (ns, name, status) in statuses {
        let status = if failed && status == 200 { 424 } else { status };
        body.push_str(&format!(
            "<D:propstat><D:prop>{}</D:prop><D:status>HTTP/1.1 {status} {}</D:status></D:propstat>",
            element(&ns, &name, ""),
            status_text(status)
        ));
    }
    Ok(multistatus(format!(
        "<D:response><D:href>{}</D:href>{body}</D:response>",
        esc(&href(path, true))
    )))
}
fn move_resource(graph: &mut Graph, request: &HttpRequest, path: &str) -> Result<HttpResponse> {
    let Some(destination) = header(request, "destination") else {
        return Err(Error::Invalid("MOVE requires Destination"));
    };
    let destination = if let Ok(url) = url::Url::parse(destination) {
        if header(request, "host").is_some_and(|host| {
            url.host_str()
                .is_none_or(|name| host.split(':').next() != Some(name))
        }) {
            return Ok(fail(502, "DAV Destination must use this server"));
        }
        url.path().to_owned()
    } else {
        destination.into()
    };
    let destination = String::from_utf8(codec::percent_bytes(&destination)?)
        .map_err(|_| Error::Invalid("invalid DAV Destination encoding"))?;
    let Some(destination) = destination.strip_prefix("/caldav/") else {
        return Ok(fail(502, "DAV Destination must use this service"));
    };
    if !destination.starts_with("inkcre/") || destination.split('/').any(|p| p == "." || p == "..")
    {
        return Ok(fail(403, "DAV Destination is outside this principal"));
    }
    let (parent, name) = split(path);
    let (target_path, target_href) = split(destination);
    let Some(source) = graph
        .collections
        .get(parent)
        .and_then(|c| c.items.get(name))
        .cloned()
    else {
        return Ok(fail(404, "DAV source not found"));
    };
    if !condition(request, Some(&etag(&source.content.text)), true) {
        return Ok(fail(412, "DAV precondition failed"));
    }
    let Some(target) = graph.collections.get(target_path) else {
        return Ok(fail(409, "DAV destination collection not found"));
    };
    let expected = if target
        .content
        .props
        .get("tag")
        .is_some_and(|v| v == "VADDRESSBOOK")
    {
        CONTACT
    } else {
        CALENDAR
    };
    if source.resolver != expected {
        return Ok(fail(403, "DAV destination collection has a different type"));
    }
    let existed = target.items.contains_key(target_href);
    if existed && header(request, "overwrite") == Some("F") {
        return Ok(fail(412, "DAV destination exists"));
    }
    let uid = codec::uid(&source.content.text)?;
    if target.items.iter().any(|(key, r)| {
        key != target_href
            && r.id != source.id
            && codec::uid(&r.content.text).ok().as_ref() == Some(&uid)
    }) {
        return Ok(xml_error(409, CAL, "no-uid-conflict"));
    }
    let mut source = graph
        .collections
        .get_mut(parent)
        .expect("source collection")
        .items
        .remove(name)
        .expect("source resource");
    source.content.href = target_href.into();
    source.modified = Utc::now();
    let target = graph
        .collections
        .get_mut(target_path)
        .expect("target collection");
    target.items.insert(target_href.into(), source);
    target.modified = Utc::now();
    Ok(response(
        if existed { 204 } else { 201 },
        vec![],
        "text/plain",
    ))
}
pub fn esc(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
pub(crate) fn href(path: &str, collection: bool) -> String {
    let mut url = url::Url::parse("http://dav.invalid/caldav/").expect("static DAV URL");
    {
        let mut segments = url.path_segments_mut().expect("hierarchical DAV URL");
        for part in path.split('/').filter(|p| !p.is_empty()) {
            segments.push(part);
        }
        if collection && !path.is_empty() {
            segments.push("");
        }
    }
    url.path().to_owned()
}
pub(crate) fn element(namespace: &str, name: &str, content: &str) -> String {
    format!(
        "<X:{name} xmlns:X=\"{}\">{content}</X:{name}>",
        esc(namespace)
    )
}
pub(crate) fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        424 => "Failed Dependency",
        _ => "Error",
    }
}
pub(crate) fn multistatus(content: String) -> HttpResponse {
    response(
        207,
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"{CAL}\" xmlns:CR=\"{CARD}\">{content}</D:multistatus>"
        ),
        "application/xml; charset=utf-8",
    )
}
