use axum::{
    extract::Request,
    http::{HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use inkcre_core::peer::EXECUTION_HEADER;

pub async fn apply(request: Request, next: Next) -> Response {
    let Some(origin) = request.headers().get(header::ORIGIN).cloned() else {
        return next.run(request).await;
    };

    // Protocol OPTIONS (e.g. CalDAV capability discovery) still goes to its handler.
    let preflight = request.method() == Method::OPTIONS
        && request
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
    let mut response = if preflight {
        preflight_response(&request)
    } else {
        let mut response = next.run(request).await;
        response.headers_mut().insert(
            header::ACCESS_CONTROL_EXPOSE_HEADERS,
            HeaderValue::from_static(EXECUTION_HEADER),
        );
        response
    };

    let headers = response.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
        HeaderValue::from_static("true"),
    );
    headers.append(header::VARY, HeaderValue::from_static("Origin"));
    response
}

fn preflight_response(request: &Request) -> Response {
    let headers = request.headers();
    let method_allowed = headers
        .get(header::ACCESS_CONTROL_REQUEST_METHOD)
        .is_some_and(|method| {
            matches!(
                method.as_bytes(),
                b"DELETE" | b"GET" | b"HEAD" | b"OPTIONS" | b"PATCH" | b"POST" | b"PUT"
            )
        });
    let private_network = headers.contains_key("access-control-request-private-network");
    let mut response = match (method_allowed, private_network) {
        (true, false) => (StatusCode::OK, "OK"),
        (false, false) => (StatusCode::BAD_REQUEST, "Disallowed CORS method"),
        (true, true) => (StatusCode::BAD_REQUEST, "Disallowed CORS private-network"),
        (false, true) => (
            StatusCode::BAD_REQUEST,
            "Disallowed CORS method, private-network",
        ),
    }
    .into_response();
    let response_headers = response.headers_mut();
    response_headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("DELETE, GET, HEAD, OPTIONS, PATCH, POST, PUT"),
    );
    response_headers.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static("600"),
    );
    if let Some(requested) = headers.get(header::ACCESS_CONTROL_REQUEST_HEADERS) {
        response_headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, requested.clone());
    }
    response
}
