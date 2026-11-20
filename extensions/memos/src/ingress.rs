use crate::{Error, Result, graph, schema::*, wire};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD},
};
use inkcre_core::{Core, HydratedContent};
use inkcre_extension_sdk::{
    HttpAuth, HttpBody, HttpHeader, HttpIngress, HttpPathMatch, HttpRequest, HttpResponse,
    HttpRoute, PeerHttpInbound,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub fn descriptor() -> HttpIngress {
    let mut routes = vec![
        HttpRoute {
            method: "GET".into(),
            path: "/memos/connection".into(),
            path_match: HttpPathMatch::Exact,
            auth: HttpAuth::Peer,
        },
        HttpRoute {
            method: "GET".into(),
            path: "/memos/api/v1/instance/profile".into(),
            path_match: HttpPathMatch::Exact,
            auth: HttpAuth::Public,
        },
    ];
    for method in ["GET", "POST", "PATCH", "DELETE"] {
        routes.push(HttpRoute {
            method: method.into(),
            path: "/memos/api/v1".into(),
            path_match: HttpPathMatch::Prefix,
            auth: HttpAuth::Protocol,
        });
    }
    routes.push(HttpRoute {
        method: "GET".into(),
        path: "/memos/file/attachments".into(),
        path_match: HttpPathMatch::Prefix,
        auth: HttpAuth::Protocol,
    });
    HttpIngress {
        routes,
        peer_inbounds: vec![PeerHttpInbound {
            capability: "memos.connection.v1".into(),
            method: "GET".into(),
            path: "/memos/connection".into(),
        }],
    }
}
fn response(status: u16, mut value: Value) -> HttpResponse {
    fn clean(value: &mut Value) {
        match value {
            Value::Object(object) => {
                object.retain(|_, value| !value.is_null());
                for child in object.values_mut() {
                    clean(child);
                }
            }
            Value::Array(values) => {
                for child in values {
                    clean(child);
                }
            }
            _ => (),
        }
    }
    clean(&mut value);
    HttpResponse {
        status,
        headers: vec![HttpHeader {
            name: "content-type".into(),
            value: b"application/json".to_vec(),
        }],
        body: HttpBody(serde_json::to_vec(&value).expect("JSON Value serialization")),
    }
}
pub fn error(error: Error) -> HttpResponse {
    let (status, message) = match error {
        Error::BadRequest(message) => (400, message),
        Error::Unauthorized => (401, "Invalid Memos personal access token"),
        Error::NotFound => (404, "Memos resource not found"),
        Error::Conflict(message) => (409, message),
        Error::Integrity(message) => (500, message),
        Error::Database => (500, "Memos database operation failed"),
        Error::Json => (500, "Memos canonical graph content is invalid"),
    };
    let mut result = response(status, json!({"detail":message}));
    if status == 401 {
        result.headers.push(HttpHeader {
            name: "www-authenticate".into(),
            value: b"Bearer".to_vec(),
        });
    }
    result
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    personal_access_token: Option<String>,
}
async fn authenticate(core: &Core, request: &HttpRequest) -> Result<()> {
    let presented = request
        .headers
        .iter()
        .find(|h| h.name == "authorization")
        .and_then(|h| h.value.strip_prefix(b"Bearer "))
        .ok_or(Error::Unauthorized)?;
    let installed = core
        .extension_get("inkcre/memos")
        .await
        .map_err(|_| Error::Database)?
        .ok_or(Error::Unauthorized)?;
    let config: Config =
        serde_json::from_value(installed.config).map_err(|_| Error::Unauthorized)?;
    let token = config.personal_access_token.ok_or(Error::Unauthorized)?;
    if !token.starts_with("memos_pat_")
        || token.len() != 42
        || !token.as_bytes()[10..].iter().all(u8::is_ascii_alphanumeric)
        || presented.len() != token.len()
    {
        return Err(Error::Unauthorized);
    }
    let mismatch = presented
        .iter()
        .zip(token.as_bytes())
        .fold(0u8, |a, (x, y)| a | (x ^ y));
    if mismatch != 0 {
        return Err(Error::Unauthorized);
    }
    Ok(())
}
fn parse<T: serde::de::DeserializeOwned>(request: &HttpRequest) -> Result<T> {
    serde_json::from_slice(&request.body.0)
        .map_err(|_| Error::BadRequest("Request body must be a valid Memos JSON object"))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PageToken {
    version: u32,
    generation: String,
    state: String,
    filter: String,
    cursor: MemoCursor,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommentToken {
    version: u32,
    generation: String,
    parent_id: i32,
    after_block_id: i32,
}
fn decode<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T> {
    let raw = URL_SAFE_NO_PAD
        .decode(raw)
        .or_else(|_| URL_SAFE.decode(raw))
        .map_err(|_| Error::BadRequest("Invalid page token"))?;
    serde_json::from_slice(&raw).map_err(|_| Error::BadRequest("Invalid page token"))
}
fn encode(value: &impl Serialize) -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(value)?))
}
fn allowed(query: &HashMap<String, String>, names: &[&str]) -> Result<()> {
    if query.keys().any(|key| !names.contains(&key.as_str())) {
        Err(Error::BadRequest("Unsupported query parameter"))
    } else {
        Ok(())
    }
}

async fn list(core: &Core, query: &HashMap<String, String>) -> Result<HttpResponse> {
    allowed(query, &["pageSize", "pageToken", "state", "filter"])?;
    let limit = wire::limit(query, 50)?;
    let state = query.get("state").map_or("NORMAL", String::as_str);
    if !matches!(state, "NORMAL" | "ARCHIVED") {
        return Err(Error::BadRequest("state must be NORMAL or ARCHIVED"));
    }
    let filter = query.get("filter").map_or("", String::as_str);
    if filter != wire::CREATOR {
        return Err(Error::BadRequest(
            "filter must select the deployment creator",
        ));
    }
    let after = match query.get("pageToken").filter(|v| !v.is_empty()) {
        None => None,
        Some(raw) => {
            let token: PageToken = decode(raw)?;
            if token.version != 1
                || token.generation != "memos-0.29.1"
                || token.state != state
                || token.filter != filter
                || token.cursor.block_id <= 0
            {
                return Err(Error::BadRequest(
                    "Page token does not belong to this memo query",
                ));
            }
            Some(token.cursor)
        }
    };
    let mut tx = core.pool().begin().await?;
    let blocks = graph::all_memos(&mut tx).await?;
    let ids: Vec<_> = blocks.iter().map(|b| b.id).collect();
    let parents: Vec<Option<i32>> = sqlx::query_scalar(
        "SELECT from_ FROM inkcre.relations WHERE from_=ANY($1) AND content='parent'",
    )
    .bind(ids)
    .fetch_all(&mut *tx)
    .await?;
    let parents: HashSet<_> = parents.into_iter().flatten().collect();
    let mut roots = Vec::new();
    for root in blocks {
        if parents.contains(&root.id) {
            continue;
        }
        let canonical: CanonicalMemo = serde_json::from_str(&root.content)?;
        if canonical.archived != (state == "ARCHIVED") {
            continue;
        }
        if after
            .as_ref()
            .is_some_and(|c| (canonical.created_at, root.id) >= (c.created_at, c.block_id))
        {
            continue;
        }
        roots.push((canonical.created_at, root.id));
    }
    roots.sort_unstable_by(|a, b| b.cmp(a));
    let more = roots.len() > limit;
    roots.truncate(limit);
    let mut memos = Vec::new();
    for (_, id) in &roots {
        memos.push(wire::memo(&graph::solve(&mut tx, *id, core).await?));
    }
    let next = if more {
        let (created_at, block_id) = *roots
            .last()
            .ok_or(Error::Integrity("Empty memo pagination"))?;
        encode(&PageToken {
            version: 1,
            generation: "memos-0.29.1".into(),
            state: state.into(),
            filter: filter.into(),
            cursor: MemoCursor {
                created_at,
                block_id,
            },
        })?
    } else {
        String::new()
    };
    Ok(response(200, json!({"memos":memos,"nextPageToken":next})))
}
async fn comments(
    core: &Core,
    parent: i32,
    query: &HashMap<String, String>,
) -> Result<HttpResponse> {
    allowed(query, &["pageSize", "pageToken", "orderBy"])?;
    if query.get("orderBy").is_some_and(|v| !v.is_empty()) {
        return Err(Error::BadRequest("Custom comment orderBy is not supported"));
    }
    let limit = wire::limit(query, 10)?;
    let after = match query.get("pageToken").filter(|v| !v.is_empty()) {
        None => None,
        Some(raw) => {
            let token: CommentToken = decode(raw)?;
            if token.version != 1
                || token.generation != "memos-0.29.1-comments"
                || token.parent_id != parent
                || token.after_block_id <= 0
            {
                return Err(Error::BadRequest(
                    "Comment page token does not belong to this parent",
                ));
            }
            Some(token.after_block_id)
        }
    };
    let mut tx = core.pool().begin().await?;
    graph::block(&mut tx, parent, MEMO, false).await?;
    let ids:Vec<i32>=sqlx::query_scalar("SELECT DISTINCT b.id FROM inkcre.blocks b JOIN inkcre.relations r ON r.from_=b.id WHERE r.to_=$1 AND r.content='parent' AND b.resolver=$2 ORDER BY b.id DESC").bind(parent).bind(MEMO).fetch_all(&mut *tx).await?;
    let total = ids.len();
    let mut selected: Vec<_> = ids
        .into_iter()
        .filter(|id| after.is_none_or(|after| *id < after))
        .collect();
    let more = selected.len() > limit;
    selected.truncate(limit);
    let mut memos = Vec::new();
    for id in &selected {
        memos.push(wire::memo(&graph::solve(&mut tx, *id, core).await?));
    }
    let next = if more {
        encode(&CommentToken {
            version: 1,
            generation: "memos-0.29.1-comments".into(),
            parent_id: parent,
            after_block_id: *selected
                .last()
                .ok_or(Error::Integrity("Empty comment page"))?,
        })?
    } else {
        String::new()
    };
    Ok(response(
        200,
        json!({"memos":memos,"nextPageToken":next,"totalSize":total}),
    ))
}

pub async fn handle(core: &Core, peer: Uuid, request: HttpRequest) -> Result<HttpResponse> {
    if request.method == "GET" && request.path == "/memos/connection" {
        let config: Value = sqlx::query_scalar("SELECT config FROM inkcre.peers WHERE id=$1")
            .bind(peer)
            .fetch_optional(core.pool())
            .await?
            .ok_or(Error::NotFound)?;
        let base = config
            .get("http_public_base_url")
            .and_then(Value::as_str)
            .ok_or(Error::Conflict(
                "Configure this Core's Public HTTP Base URL before connecting Memos.",
            ))?;
        let parsed =
            url::Url::parse(base).map_err(|_| Error::Conflict("Invalid public HTTP base URL"))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || !parsed.has_host()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(Error::Conflict("Invalid public HTTP base URL"));
        }
        return Ok(response(
            200,
            json!({"server_url":format!("{}/memos",base.trim_end_matches('/'))}),
        ));
    }
    if request.method == "GET" && request.path == "/memos/api/v1/instance/profile" {
        return Ok(response(200, json!({"version":"0.29.1"})));
    }
    authenticate(core, &request).await?;
    let query = wire::query(&request.query);
    let path = request.path.strip_prefix("/memos").ok_or(Error::NotFound)?;
    match (request.method.as_str(), path) {
        ("GET", "/api/v1/auth/me") => {
            return Ok(response(
                200,
                json!({"user":{"name":"users/inkcre","role":"ADMIN","username":"inkcre","displayName":"InKCre","state":"NORMAL"}}),
            ));
        }
        ("GET", "/api/v1/users/inkcre/settings/GENERAL") => {
            return Ok(response(
                200,
                json!({"generalSetting":{"memoVisibility":"PRIVATE"}}),
            ));
        }
        ("GET", "/api/v1/memos") => return list(core, &query).await,
        ("POST", "/api/v1/memos") => {
            let input: wire::CreateMemo = parse(&request)?;
            let solved = graph::create(
                core,
                wire::create(&input)?,
                &wire::attachment_ids(input.attachments.as_deref())?,
                None,
            )
            .await?;
            return Ok(response(200, wire::memo(&solved)));
        }
        ("GET", "/api/v1/attachments") => {
            return Ok(response(
                200,
                json!({"attachments":graph::list_attachments(core).await?.iter().map(wire::attachment).collect::<Vec<_>>()}),
            ));
        }
        ("POST", "/api/v1/attachments") => {
            if request.body.0.len() > 45 * 1024 * 1024 {
                return Ok(response(
                    413,
                    json!({"detail":"Attachment request exceeds the 32 MiB decoded limit"}),
                ));
            }
            let input: wire::CreateAttachment = parse(&request)?;
            if input.filename.is_empty()
                || input.filename.chars().count() > 1024
                || matches!(input.filename.as_str(), "." | "..")
                || input.filename.contains(['/', '\\', '\0'])
                || input.media_type.is_empty()
                || input.media_type.chars().count() > 255
                || !input.media_type.contains('/')
                || input.media_type.contains(['\r', '\n'])
            {
                return Err(Error::BadRequest("Invalid attachment metadata"));
            }
            let content = STANDARD
                .decode(&input.content)
                .map_err(|_| Error::BadRequest("Invalid attachment Base64 content"))?;
            if content.len() > 32 * 1024 * 1024 {
                return Ok(response(
                    413,
                    json!({"detail":"Attachment exceeds the 32 MiB decoded limit"}),
                ));
            }
            let memo = input
                .memo
                .as_deref()
                .filter(|value| !value.is_empty())
                .map(|s| wire::resource(s, "memos/"))
                .transpose()?;
            if input.memo.is_some() && memo.is_none() {
                return Err(Error::BadRequest("Invalid memo resource name"));
            }
            let solved =
                graph::create_attachment(core, input.filename, input.media_type, &content, memo)
                    .await?;
            return Ok(response(200, wire::attachment(&solved)));
        }
        _ => (),
    }
    if let Some(suffix) = path.strip_prefix("/api/v1/memos/") {
        if let Some(parent) = suffix.strip_suffix("/comments") {
            let parent = wire::id(parent)?;
            if request.method == "GET" {
                return comments(core, parent, &query).await;
            }
            if request.method == "POST" {
                let input: wire::CreateMemo = parse(&request)?;
                let solved = graph::create(
                    core,
                    wire::create(&input)?,
                    &wire::attachment_ids(input.attachments.as_deref())?,
                    Some(parent),
                )
                .await?;
                return Ok(response(200, wire::memo(&solved)));
            }
        }
        let id = wire::id(suffix)?;
        if request.method == "PATCH" {
            let raw: Value = parse(&request)?;
            let (patch, attachments) =
                wire::update(raw, query.get("updateMask").map(String::as_str))?;
            return Ok(response(
                200,
                wire::memo(&graph::update(core, id, patch, attachments).await?),
            ));
        }
        if request.method == "DELETE" {
            if !query.is_empty() {
                return Err(Error::BadRequest(
                    "Delete memo query parameters are not supported",
                ));
            }
            graph::delete(core, id).await?;
            return Ok(response(200, json!({})));
        }
    }
    if request.method == "DELETE"
        && let Some(id) = path.strip_prefix("/api/v1/attachments/")
    {
        let mut tx = core.pool().begin().await?;
        if !graph::delete_attachment(&mut tx, wire::id(id)?, core).await? {
            return Err(Error::NotFound);
        }
        tx.commit().await?;
        return Ok(response(200, json!({})));
    }
    if request.method == "GET"
        && let Some(resource) = path.strip_prefix("/file/attachments/")
    {
        let (id, filename) = resource.split_once('/').ok_or(Error::NotFound)?;
        let mut connection = core.pool().acquire().await?;
        let solved = graph::solve_attachment(&mut connection, wire::id(id)?, core).await?;
        if solved.canonical.filename != filename {
            return Err(Error::NotFound);
        }
        let child = core
            .block_get(solved.content_block_id)
            .await
            .map_err(|_| Error::Database)?
            .ok_or(Error::NotFound)?;
        let bytes =
            match core
                .knowledge()
                .actual_content(&child)
                .await
                .map_err(|error| match error {
                    inkcre_core::knowledge::Error::Core(inkcre_core::Error::NotFound(_)) => {
                        Error::NotFound
                    }
                    _ => Error::Integrity("Attachment content read failed"),
                })? {
                HydratedContent::Bytes(bytes) => bytes,
                _ => return Err(Error::Integrity("Attachment bytes are not binary")),
            };
        return Ok(HttpResponse {
            status: 200,
            headers: vec![HttpHeader {
                name: "content-type".into(),
                value: solved.canonical.media_type.into_bytes(),
            }],
            body: HttpBody(bytes),
        });
    }
    Err(Error::NotFound)
}
