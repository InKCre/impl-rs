//! Fixed x-monkey DTOs and origin policy; JWT credentials stay in the trusted Host.
use crate::{Result, conflict, graph, invalid, schema::*};
use chrono::{DateTime, Utc};
use inkcre_core::Core;
use inkcre_extension_sdk::{
    HttpAuth, HttpBody, HttpHeader, HttpPathMatch, HttpRequest, HttpResponse, HttpRoute,
};
use serde_json::{Value, json};
use uuid::Uuid;
const ROOT: &str = "/twitter/x-monkey";
const EXPIRY: &str = "x-inkcre-twitter-session-expiry";
pub fn routes() -> Vec<HttpRoute> {
    [
        ("POST", "/twitter/x-monkey/session", HttpAuth::Peer),
        ("GET", "/twitter/x-monkey/account", HttpAuth::Protocol),
        ("POST", "/twitter/x-monkey/disconnect", HttpAuth::Protocol),
        (
            "GET",
            "/twitter/x-monkey/api/v1/session",
            HttpAuth::Protocol,
        ),
        (
            "POST",
            "/twitter/x-monkey/api/v1/archive/query",
            HttpAuth::Protocol,
        ),
        (
            "POST",
            "/twitter/x-monkey/api/v1/archive/submit",
            HttpAuth::Protocol,
        ),
    ]
    .into_iter()
    .map(|(method, path, auth)| HttpRoute {
        method: method.into(),
        path: path.into(),
        path_match: HttpPathMatch::Exact,
        auth,
    })
    .collect()
}
fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|header| header.name.eq_ignore_ascii_case(name))
        .and_then(|header| std::str::from_utf8(&header.value).ok())
}
fn expiry(request: &HttpRequest) -> Option<i64> {
    header(request, EXPIRY)?
        .parse::<i64>()
        .ok()
        .filter(|time| *time > Utc::now().timestamp())
}
fn formatted_expiry(time: i64) -> Option<String> {
    DateTime::<Utc>::from_timestamp(time, 0)
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}
async fn peer(core: &Core, id: Uuid) -> Result<(Option<String>, String)> {
    let peer = core
        .peer_get(id)
        .await
        .map_err(|_| conflict("Core Peer lookup failed"))?
        .ok_or_else(|| conflict("Core Peer missing"))?;
    Ok((
        peer.config["http_public_base_url"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|base| base.trim_end_matches('/').to_owned()),
        peer.name,
    ))
}
fn origin(value: &str) -> Option<String> {
    let parsed = url::Url::parse(value).ok()?;
    if !matches!(
        parsed.scheme(),
        "http" | "https" | "chrome-extension" | "moz-extension"
    ) || !parsed.username().is_empty()
        || parsed.password().is_some()
        || !matches!(parsed.path(), "" | "/")
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    let authority = value.split_once("://")?.1.split('/').next()?;
    if authority.is_empty() {
        return None;
    }
    if matches!(parsed.scheme(), "chrome-extension" | "moz-extension")
        && !authority
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
    {
        return None;
    }
    if parsed.scheme() == "https" && authority.eq_ignore_ascii_case("x.com") {
        Some("https://x.com".into())
    } else {
        Some(format!("{}://{authority}", parsed.scheme()))
    }
}
fn core_origin(base: Option<&str>) -> Option<String> {
    let base = base?;
    let parsed = url::Url::parse(base).ok()?;
    let authority = base.split_once("://")?.1.split('/').next()?;
    Some(format!("{}://{authority}", parsed.scheme()))
}
fn allowed(request: &HttpRequest, base: Option<&str>) -> bool {
    let Some(raw) = header(request, "origin") else {
        return !request
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("origin"));
    };
    let Some(origin) = origin(raw) else {
        return false;
    };
    origin == "https://x.com"
        || origin.starts_with("chrome-extension://")
        || origin.starts_with("moz-extension://")
        || Some(origin) == core_origin(base)
}
fn json_response(status: u16, value: Value) -> Result<HttpResponse> {
    Ok(HttpResponse {
        status,
        headers: vec![
            HttpHeader {
                name: "content-type".into(),
                value: b"application/json".to_vec(),
            },
            HttpHeader {
                name: "cache-control".into(),
                value: b"no-store".to_vec(),
            },
            HttpHeader {
                name: "x-content-type-options".into(),
                value: b"nosniff".to_vec(),
            },
        ],
        body: HttpBody(
            serde_json::to_vec(&value).map_err(|_| invalid("Archive response encoding failed"))?,
        ),
    })
}
fn error(status: u16, message: &str) -> Result<HttpResponse> {
    json_response(status, json!({"error":message}))
}
fn session(authenticated: bool, name: &str, base: Option<&str>, expiry: Option<i64>) -> Value {
    json!({"authenticated":authenticated,"registered":authenticated,"username":if authenticated{Some(name)}else{None},"expires_at":expiry.and_then(formatted_expiry),"account_url":base.map(|base|format!("{base}/account"))})
}
fn content_type(request: &HttpRequest) -> &str {
    header(request, "content-type")
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
}
async fn submit(core: &Core, value: Value) -> Result<Value> {
    validate("ArchiveBatch", &value)?;
    let batch = Batch::parse(value)?;
    let result = graph::write(core, &batch, false).await?;
    let mut response = json!({});
    let (mut total, mut accepted) = (0, 0);
    for kind in KINDS {
        let mut items = Vec::new();
        for record in batch.items(kind) {
            let id = record["id"]
                .as_str()
                .ok_or_else(|| invalid("Archive native ID missing"))?;
            let changed = result.changed.contains(&(kind.into(), id.into()));
            total += 1;
            accepted += usize::from(changed);
            items.push(json!({"id":id,"status":if changed{"accepted"}else{"unchanged"}}));
        }
        response[kind] = json!(items);
    }
    response["summary"] = json!({"total":total,"accepted":accepted,"unchanged":total-accepted,"partial":0,"failed":0});
    Ok(response)
}
async fn query(core: &Core, value: Value) -> Result<Value> {
    validate("ArchiveQuery", &value)?;
    let mut result = graph::read(core, &value).await?;
    for kind in KINDS {
        if let Some(items) = result[kind].as_array_mut() {
            for item in items {
                if item["status"] != "found" {
                    continue;
                }
                match wire(kind, &item["data"]) {
                    Ok(data) => item["data"] = data,
                    Err(_) => {
                        if let Some(item) = item.as_object_mut() {
                            item.remove("data");
                            item.insert("status".into(), json!("failed"));
                            item.insert(
                                "error".into(),
                                json!("Stored record cannot use this archive format"),
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(result)
}
fn html(body: &str, status: u16, nonce: Option<&str>) -> HttpResponse {
    let script = nonce
        .map(|nonce| format!("script-src 'nonce-{nonce}'; "))
        .unwrap_or_default();
    HttpResponse{status,headers:vec![HttpHeader{name:"content-type".into(),value:b"text/html; charset=utf-8".to_vec()},HttpHeader{name:"cache-control".into(),value:b"no-store".to_vec()},HttpHeader{name:"content-security-policy".into(),value:format!("default-src 'none'; {script}connect-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'").into_bytes()},HttpHeader{name:"referrer-policy".into(),value:b"no-referrer".to_vec()},HttpHeader{name:"x-content-type-options".into(),value:b"nosniff".to_vec()}],body:HttpBody(format!("<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>x-monkey 连接</title></head><body><main><h1>InKCre x-monkey 连接</h1>{body}</main></body></html>").into_bytes())}
}
fn account(request: &HttpRequest, base: Option<&str>, name: &str) -> HttpResponse {
    let expires = expiry(request);
    let nonce = Uuid::new_v4().simple().to_string();
    let safe_name = html_escape::encode_text(name);
    let state = if expires.is_some() {
        format!(
            "<p id=\"connection-status\" role=\"status\">Connected</p><p id=\"connection-peer\">InKCre Peer: {safe_name}</p>"
        )
    } else {
        "<p id=\"connection-status\" role=\"status\">尚未连接。</p><p id=\"connection-peer\" hidden></p>".into()
    };
    let expiration = if let Some(expires) = expires.and_then(formatted_expiry) {
        format!(
            "<p id=\"connection-expiry\">会话到期时间：<time id=\"session-expires-at\">{expires}</time></p>"
        )
    } else {
        "<p id=\"connection-expiry\" hidden>会话到期时间：<time id=\"session-expires-at\"></time></p>".into()
    };
    let disconnect=base.map(|base|format!("<form method=\"post\" action=\"{}\"><button type=\"submit\">断开此浏览器</button></form>",html_escape::encode_double_quoted_attribute(&format!("{base}/disconnect")))).unwrap_or_default();
    let script = include_str!("account-script.html")
        .replace("__NONCE__", &nonce)
        .replace("__JWT_ROLE__", "authenticated")
        .replace("__JWT_ISSUER__", "inkcre-peer")
        .replace("__JWT_AUDIENCE__", "inkcre-api")
        .replace("__JWT_LIFETIME__", "60");
    html(
        &format!(
            "{state}{expiration}<h2>连接 x-monkey</h2><label for=\"jwt-secret\">JWT_SECRET</label><input id=\"jwt-secret\" type=\"password\" autocomplete=\"off\" spellcheck=\"false\"><button id=\"connect\" type=\"button\">Connect</button><p>密钥只在此页面内存中用于签发短时 Peer JWT，不会保存。</p>{disconnect}<p>断开只清除此浏览器中的会话，不会删除已保存的信息。</p>{script}"
        ),
        200,
        Some(&nonce),
    )
}
pub async fn http(core: &Core, peer_id: Uuid, request: HttpRequest) -> Result<HttpResponse> {
    let (base, name) = peer(core, peer_id).await?;
    let protocol = base.as_ref().map(|base| format!("{base}{ROOT}"));
    let protocol = protocol.as_deref();
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/twitter/x-monkey/account") => Ok(account(&request, protocol, &name)),
        ("POST", "/twitter/x-monkey/session") => {
            if header(&request, "origin").and_then(origin) != core_origin(base.as_deref()) {
                return error(403, "same-origin request required");
            }
            if !content_type(&request).eq_ignore_ascii_case("application/json") {
                return error(415, "application/json required");
            }
            let Some(protocol) = protocol else {
                return error(503, "public Core URL is not configured");
            };
            let Some(expires) = expiry(&request) else {
                return error(401, "ordinary Peer credential required");
            };
            json_response(200, session(true, &name, Some(protocol), Some(expires)))
        }
        ("POST", "/twitter/x-monkey/disconnect") => {
            if header(&request, "origin").and_then(origin) != core_origin(base.as_deref())
                || header(&request, "sec-fetch-mode") != Some("navigate")
                || header(&request, "sec-fetch-dest") != Some("document")
            {
                return Ok(html("<p>断开请求来源无效。</p>", 403, None));
            }
            let Some(protocol) = protocol else {
                return Ok(html("<p>Core 尚未配置公共 HTTPS 地址。</p>", 503, None));
            };
            let mut response = html("", 303, None);
            response.headers.push(HttpHeader {
                name: "location".into(),
                value: format!("{protocol}/account").into_bytes(),
            });
            Ok(response)
        }
        ("GET", "/twitter/x-monkey/api/v1/session") => {
            if header(&request, "sec-fetch-mode") == Some("no-cors")
                || !allowed(&request, base.as_deref())
            {
                return error(403, "request source is not allowed");
            }
            let expires = expiry(&request).filter(|_| protocol.is_some());
            json_response(200, session(expires.is_some(), &name, protocol, expires))
        }
        (
            "POST",
            path @ ("/twitter/x-monkey/api/v1/archive/query"
            | "/twitter/x-monkey/api/v1/archive/submit"),
        ) => {
            if !allowed(&request, base.as_deref()) {
                return error(403, "archive session or request headers are invalid");
            }
            if !content_type(&request).eq_ignore_ascii_case("application/json") {
                return error(415, "archive session or request headers are invalid");
            }
            if header(&request, "x-tweet-db-client") != Some("x-monkey") {
                return error(400, "archive session or request headers are invalid");
            }
            if expiry(&request).is_none() {
                return error(401, "archive session or request headers are invalid");
            }
            let value = serde_json::from_slice(&request.body.0)
                .map_err(|_| invalid("Invalid archive JSON"));
            let result = match value {
                Ok(value) => {
                    if path.ends_with("/query") {
                        query(core, value).await
                    } else {
                        submit(core, value).await
                    }
                }
                Err(error) => Err(error),
            };
            match result {
                Ok(value) => json_response(200, value),
                Err(error) if error.code == "invalid_input" => {
                    json_response(422, json!({"detail":error.message}))
                }
                Err(error) => Err(error),
            }
        }
        _ => error(404, "archive route not found"),
    }
}
