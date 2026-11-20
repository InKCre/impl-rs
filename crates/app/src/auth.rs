use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde_json::{Value, json};

pub(crate) const TWITTER_ARCHIVE_PURPOSE: &str = "twitter_archive";
const PEER_AUDIENCE: &str = "inkcre-api";
const TWITTER_ARCHIVE_AUDIENCE: &str = "inkcre-twitter-archive";

pub fn verify(token: &str, secret: &str) -> bool {
    claims(token, secret).is_some_and(|claims| claims["session_use"] != TWITTER_ARCHIVE_PURPOSE)
}

pub(crate) fn claims(token: &str, secret: &str) -> Option<Value> {
    claims_for(token, secret, PEER_AUDIENCE)
}

pub(crate) fn twitter_archive_expiry(token: &str, secret: &str) -> Option<u64> {
    let claims =
        claims_for(token, secret, TWITTER_ARCHIVE_AUDIENCE).or_else(|| claims(token, secret))?;
    (claims["session_use"] == TWITTER_ARCHIVE_PURPOSE)
        .then(|| claims["exp"].as_u64())
        .flatten()
}

fn claims_for(token: &str, secret: &str, expected_audience: &str) -> Option<Value> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.required_spec_claims.clear();
    validation.validate_exp = false;
    validation.validate_nbf = false;
    validation.validate_aud = false;
    let claims = jsonwebtoken::decode::<Value>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .ok()?
    .claims;
    let issued_at = claims.get("iat").and_then(Value::as_f64)?;
    let expires_at = claims.get("exp").and_then(Value::as_f64)?;
    let audience = claims.get("aud");
    let valid_audience = audience.and_then(Value::as_str) == Some(expected_audience)
        || audience.and_then(Value::as_array).is_some_and(|items| {
            items.iter().all(Value::is_string)
                && items
                    .iter()
                    .any(|value| value.as_str() == Some(expected_audience))
        });
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    let now = now.as_secs_f64();
    let valid = claims.get("role").and_then(Value::as_str) == Some("authenticated")
        && claims.get("iss").and_then(Value::as_str) == Some("inkcre-peer")
        && valid_audience
        && issued_at.is_finite()
        && expires_at.is_finite()
        && issued_at <= now + 60.0
        && expires_at > now
        && expires_at > issued_at;
    valid.then_some(claims)
}

pub fn issue(secret: &str, lifetime_seconds: u64) -> Result<String, String> {
    issue_token(secret, lifetime_seconds, None).map(|(token, _)| token)
}

pub(crate) fn issue_twitter_archive(
    secret: &str,
    lifetime_seconds: u64,
) -> Result<(String, u64), String> {
    issue_token(secret, lifetime_seconds, Some(TWITTER_ARCHIVE_PURPOSE))
}

fn issue_token(
    secret: &str,
    lifetime_seconds: u64,
    purpose: Option<&str>,
) -> Result<(String, u64), String> {
    crate::config::validate_secret(secret)?;
    if lifetime_seconds == 0 {
        return Err("JWT 有效期必须是正整数".to_owned());
    }
    let issued_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "系统时钟无效")?
        .as_secs();
    let expires_at = issued_at
        .checked_add(lifetime_seconds)
        .ok_or("JWT 有效期超出数值范围")?;
    let audience = if purpose.is_some() {
        TWITTER_ARCHIVE_AUDIENCE
    } else {
        PEER_AUDIENCE
    };
    let mut claims = json!({"role":"authenticated", "iss":"inkcre-peer", "aud":audience, "iat":issued_at,"exp":expires_at});
    if let Some(purpose) = purpose {
        claims["session_use"] = json!(purpose);
    }
    let token = jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|_| "无法签发 JWT".to_owned())?;
    Ok((token, expires_at))
}
