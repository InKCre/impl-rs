use axum::{
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::Response,
};
use inkcre_core::extension::{
    self, HttpAuth, HttpBody, HttpHeader, HttpRequest, MAX_HTTP_BODY_BYTES,
};

use crate::{
    error::{ApiError, ApiResult},
    routes::{AppState, extension_error, peer_authentication_error},
    twitter_session::{EXPIRY_HEADER, Session},
};

pub const RESERVED_MOUNTS: &[&str] = &[
    "agent-tools",
    "agents",
    "ai",
    "api",
    "blocks",
    "config-schemas",
    "configs",
    "cron",
    "crons",
    "embedding-profiles",
    "entities",
    "extension-management",
    "extensions",
    "graph",
    "health",
    "job-types",
    "jobs",
    "lexical-retrieval",
    "livez",
    "observability",
    "organization",
    "peers",
    "readyz",
    "relations",
    "resolvers",
    "retrieval",
    "self",
    "semantic-retrieval",
    "sink-types",
    "sinks",
    "source-types",
    "sources",
    "storage-blobs",
    "storage-types",
    "storages",
    "telemetry",
];

pub async fn route(State(state): State<AppState>, request: Request) -> ApiResult<Response> {
    let (parts, body) = request.into_parts();
    let path = percent_encoding::percent_decode_str(parts.uri.path())
        .decode_utf8()
        .map_err(|_| ApiError::not_found("route not found"))?
        .into_owned();
    let method = parts.method.as_str();
    let dispatch = state
        .host
        .admit_http(method, &path)
        .await
        .ok_or_else(|| ApiError::not_found("route not found"))?;
    if dispatch.auth == HttpAuth::Peer
        && let Some(error) = peer_authentication_error(&parts.headers, &state.secret)
    {
        return Ok(error);
    }
    let _permit = state
        .extension_http
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "extension_http_capacity",
                "Extension HTTP capacity reached",
            )
        })?;
    let session = Session::prepare(&state, &dispatch, method, &path, &parts.headers).await?;
    let mut headers: Vec<_> = parts
        .headers
        .iter()
        .filter(|(name, _)| {
            !unsafe_header(name, &parts.headers)
                && (dispatch.auth == HttpAuth::Protocol || *name != header::AUTHORIZATION)
                && (session.is_none()
                    || (*name != header::COOKIE && *name != header::AUTHORIZATION))
        })
        .map(|(name, value)| HttpHeader {
            name: name.as_str().into(),
            value: value.as_bytes().to_vec(),
        })
        .collect();
    if let Some(expiry) = session.as_ref().and_then(|session| session.expiry) {
        headers.push(HttpHeader {
            name: EXPIRY_HEADER.into(),
            value: expiry.to_string().into_bytes(),
        });
    }
    let body = to_bytes(body, MAX_HTTP_BODY_BYTES).await.map_err(|_| {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "extension_body_too_large",
            "Extension HTTP body exceeds the transport limit",
        )
    })?;
    let request = HttpRequest {
        method: method.into(),
        path,
        query: parts.uri.query().unwrap_or_default().into(),
        headers,
        body: HttpBody(body.to_vec()),
    };
    request
        .validate()
        .map_err(|_| ApiError::invalid("invalid Extension HTTP request"))?;
    let response = state
        .host
        .dispatch_http(dispatch, request)
        .await
        .map_err(http_error)?;
    let status = StatusCode::from_u16(response.status).map_err(|_| invalid_response())?;
    let mut headers = HeaderMap::new();
    for header in response.headers {
        let name =
            HeaderName::from_bytes(header.name.as_bytes()).map_err(|_| invalid_response())?;
        let value = HeaderValue::from_bytes(&header.value).map_err(|_| invalid_response())?;
        headers.append(name, value);
    }
    let mut allowed = HeaderMap::new();
    for (name, value) in &headers {
        if !unsafe_header(name, &headers) && (session.is_none() || *name != header::SET_COOKIE) {
            allowed.append(name.clone(), value.clone());
        }
    }
    if let Some(session) = session {
        session.apply(status, &mut allowed);
    }
    let mut outgoing = Response::new(Body::from(response.body.0));
    *outgoing.status_mut() = status;
    *outgoing.headers_mut() = allowed;
    Ok(outgoing)
}

fn unsafe_header(name: &HeaderName, headers: &HeaderMap) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "content-length"
            | "host"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | EXPIRY_HEADER
    ) || headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|field| name.as_str().eq_ignore_ascii_case(field.trim()))
}

fn invalid_response() -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "invalid_extension_response",
        "Extension returned an invalid HTTP response",
    )
}

fn http_error(error: extension::Error) -> ApiError {
    let extension::Error::Remote(code) = &error else {
        return extension_error(error);
    };
    match code.as_str() {
        "not_found" => ApiError::not_found("Extension resource not found"),
        "invalid_input" => ApiError::invalid("invalid Extension input"),
        "materialization_unavailable" => ApiError::new(
            StatusCode::CONFLICT,
            "materialization_unavailable",
            "Extension could not materialize the requested content",
        ),
        _ => extension_error(error),
    }
}
