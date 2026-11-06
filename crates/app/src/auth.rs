use std::time::{SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde_json::{Value, json};

pub fn verify(token: &str, secret: &str) -> bool {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.required_spec_claims.clear();
    validation.validate_exp = false;
    validation.validate_nbf = false;
    validation.validate_aud = false;
    let Ok(decoded) = jsonwebtoken::decode::<Value>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    ) else {
        return false;
    };
    let claims = decoded.claims;
    let Some(issued_at) = claims.get("iat").and_then(Value::as_f64) else {
        return false;
    };
    let Some(expires_at) = claims.get("exp").and_then(Value::as_f64) else {
        return false;
    };
    let audience = claims.get("aud");
    let valid_audience = audience == Some(&Value::String("inkcre-api".to_owned()))
        || audience.and_then(Value::as_array).is_some_and(|items| {
            items.iter().all(Value::is_string)
                && items
                    .iter()
                    .any(|value| value.as_str() == Some("inkcre-api"))
        });
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return false;
    };
    let now = now.as_secs_f64();
    claims.get("role").and_then(Value::as_str) == Some("authenticated")
        && claims.get("iss").and_then(Value::as_str) == Some("inkcre-peer")
        && valid_audience
        && issued_at.is_finite()
        && expires_at.is_finite()
        && issued_at <= now + 60.0
        && expires_at > now
        && expires_at > issued_at
}

pub fn issue(secret: &str, lifetime_seconds: u64) -> Result<String, String> {
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
    jsonwebtoken::encode(
        &Header::new(Algorithm::HS256),
        &json!({"role":"authenticated", "iss":"inkcre-peer", "aud":"inkcre-api", "iat":issued_at,"exp":expires_at}),
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|_| "无法签发 JWT".to_owned())
}
