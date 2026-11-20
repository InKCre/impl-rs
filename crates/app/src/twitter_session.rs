//! The host owns the browser session credential for the Twitter archive API.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use inkcre_core::extension::{HttpAuth, HttpDispatch};
use url::Url;

use crate::{
    auth,
    error::{ApiError, ApiResult},
    routes::AppState,
};

pub(super) const EXPIRY_HEADER: &str = "x-inkcre-twitter-session-expiry";
const COOKIE: &str = "inkcre_twitter_archive";
const PREFIX: &str = "/twitter/x-monkey/";

#[derive(Clone, Copy)]
enum Route {
    Issue,
    Account,
    Disconnect,
    Status,
    Archive,
}

impl Route {
    fn classify(method: &str, path: &str) -> Option<Self> {
        match (method, path.strip_prefix(PREFIX)?) {
            ("POST", "session") => Some(Self::Issue),
            ("GET", "account") => Some(Self::Account),
            ("POST", "disconnect") => Some(Self::Disconnect),
            ("GET", "api/v1/session") => Some(Self::Status),
            ("POST", "api/v1/archive/query" | "api/v1/archive/submit") => Some(Self::Archive),
            _ => None,
        }
    }
}

pub(super) struct Session {
    route: Route,
    pub expiry: Option<u64>,
    cookie: Option<HeaderValue>,
}

impl Session {
    pub(super) async fn prepare(
        state: &AppState,
        dispatch: &HttpDispatch,
        method: &str,
        path: &str,
        headers: &HeaderMap,
    ) -> ApiResult<Option<Self>> {
        if dispatch.extension_name() != "inkcre/twitter" {
            return Ok(None);
        }
        let Some(route) = Route::classify(method, path) else {
            return Ok(None);
        };
        let mut session = Self {
            route,
            expiry: None,
            cookie: None,
        };
        let public_base = state
            .core
            .peer_get(state.peer_id)
            .await?
            .and_then(|peer| Url::parse(peer.config["http_public_base_url"].as_str()?).ok())
            .filter(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.has_host()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && !url.path().contains([';', ','])
            });
        let cookie_path = public_base.as_ref().map_or_else(
            || PREFIX.to_owned(),
            |base| format!("{}{PREFIX}", base.path().trim_end_matches('/')),
        );
        let age = state.twitter_session_lifetime;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| unavailable())?
            .as_secs();
        let (provided, token) = cookie_token(headers);
        let verified = token.and_then(|token| auth::twitter_archive_expiry(token, &state.secret));
        match route {
            Route::Issue => {
                let ordinary = header_text(headers, header::AUTHORIZATION)
                    .and_then(|value| value.strip_prefix("Bearer "))
                    .and_then(|token| auth::claims(token, &state.secret))
                    .is_some_and(|claims| claims.get("session_use").is_none());
                let json = header_text(headers, header::CONTENT_TYPE)
                    .and_then(|value| value.split(';').next())
                    .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"));
                if dispatch.auth == HttpAuth::Peer
                    && ordinary
                    && json
                    && public_base
                        .as_ref()
                        .is_some_and(|base| same_origin(headers, base))
                {
                    session.issue(state, &cookie_path, age)?;
                }
            }
            Route::Account => session.expiry = verified,
            Route::Disconnect => session.cookie = Some(cookie("", &cookie_path, 0)?),
            Route::Status | Route::Archive => {
                session.expiry = verified;
                if let Some(expiry) = verified {
                    if public_base.is_some() && expiry.saturating_sub(now) <= age / 2 {
                        session.issue(state, &cookie_path, age)?;
                    }
                } else if provided && matches!(route, Route::Status) {
                    session.cookie = Some(cookie("", &cookie_path, 0)?);
                }
            }
        }
        Ok(Some(session))
    }

    fn issue(&mut self, state: &AppState, path: &str, age: u64) -> ApiResult<()> {
        let (token, expires) =
            auth::issue_twitter_archive(&state.secret, age).map_err(|_| unavailable())?;
        self.expiry = Some(expires);
        self.cookie = Some(cookie(&token, path, age)?);
        Ok(())
    }

    pub(super) fn apply(self, status: StatusCode, headers: &mut HeaderMap) {
        let accepted = if matches!(self.route, Route::Disconnect) {
            status == StatusCode::SEE_OTHER
        } else {
            status.is_success()
        };
        if accepted && let Some(cookie) = self.cookie {
            headers.append(header::SET_COOKIE, cookie);
        }
    }
}

fn header_text(headers: &HeaderMap, name: axum::http::HeaderName) -> Option<&str> {
    headers.get(name)?.to_str().ok()
}

fn same_origin(headers: &HeaderMap, base: &Url) -> bool {
    header_text(headers, header::ORIGIN)
        .and_then(|value| Url::parse(value).ok())
        .is_some_and(|origin| {
            origin.origin() == base.origin()
                && origin.path() == "/"
                && origin.username().is_empty()
                && origin.password().is_none()
                && origin.query().is_none()
                && origin.fragment().is_none()
        })
}

fn cookie_token(headers: &HeaderMap) -> (bool, Option<&str>) {
    let mut tokens = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .filter_map(|(name, value)| (name == COOKIE).then_some(value));
    let token = tokens.next();
    (token.is_some(), token.filter(|_| tokens.next().is_none()))
}

fn cookie(token: &str, path: &str, age: u64) -> ApiResult<HeaderValue> {
    HeaderValue::from_str(&format!(
        "{COOKIE}={token}; Path={path}; Max-Age={age}; HttpOnly; Secure; SameSite=None"
    ))
    .map_err(|_| unavailable())
}

fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "twitter_archive_session_unavailable",
        "Twitter archive session is unavailable",
    )
}
