//! Shared Peer HTTP v1 discovery and one-shot delegation.
use crate::{Capability, Core, Error, Peer, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
use uuid::Uuid;

pub const HTTP_PROTOCOL: &str = "core.peer.protocol.http.v1";
pub const EXECUTION_HEADER: &str = "inkcre-peer-execution";

#[derive(Debug, thiserror::Error)]
pub enum DelegationError {
    #[error("no eligible Peer completed the capability")]
    Unavailable,
    #[error("Peer dispatch outcome is unknown")]
    OutcomeUnknown,
    #[error("Peer HTTP protocol is invalid")]
    Protocol,
    #[error(transparent)]
    Core(#[from] Error),
}

/// Caller-local transport credentials. Never serialize into advertisements.
#[derive(Clone)]
pub struct PeerHttpOptions {
    pub jwt_secret: String,
    pub lifetime_seconds: u64,
    pub timeout: Duration,
}
impl PeerHttpOptions {
    pub fn new(jwt_secret: String) -> Self {
        Self {
            jwt_secret,
            lifetime_seconds: 3600,
            timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpInbound {
    pub method: String,
    pub url: String,
}
impl HttpInbound {
    pub fn validate(&self) -> Result<()> {
        reqwest::Method::from_bytes(self.method.to_uppercase().as_bytes())
            .map_err(|_| Error::InvalidInput("invalid Peer HTTP method".into()))?;
        validate_url(&self.url, false)?;
        Ok(())
    }
}

/// Construct an advertisement only after the actual business handler is installed.
pub fn http_advertisement(
    capability: &str,
    method: &str,
    path: &str,
    public_base: &str,
) -> Result<Capability> {
    validate_url(public_base, true)?;
    if capability.is_empty() || !path.starts_with('/') || path.contains(['?', '#']) {
        return Err(Error::InvalidInput("invalid Peer capability route".into()));
    }
    let inbound = HttpInbound {
        method: method.to_uppercase(),
        url: format!("{}{}", public_base.trim_end_matches('/'), path),
    };
    inbound.validate()?;
    serde_json::from_value(
        json!({"id":capability,"inbound":{"protocol":HTTP_PROTOCOL,"parameters":inbound}}),
    )
    .map_err(|_| Error::InvalidInput("invalid Peer advertisement".into()))
}
fn validate_url(value: &str, base: bool) -> Result<()> {
    let url = reqwest::Url::parse(value)
        .map_err(|_| Error::InvalidInput("invalid Peer HTTP URL".into()))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || (base && url.query().is_some())
    {
        return Err(Error::InvalidInput("invalid Peer HTTP URL".into()));
    }
    Ok(())
}

fn normalize_map(
    value: Option<&Value>,
) -> std::result::Result<BTreeMap<String, Vec<String>>, DelegationError> {
    let mut output = BTreeMap::new();
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(output);
    };
    for (name, values) in value.as_object().ok_or(DelegationError::Protocol)? {
        let key = name.to_lowercase();
        if key.is_empty() || output.contains_key(&key) {
            return Err(DelegationError::Protocol);
        }
        let strings = values
            .as_array()
            .ok_or(DelegationError::Protocol)?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or(DelegationError::Protocol)
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        output.insert(key, strings);
    }
    Ok(output)
}

impl Core {
    /// Excludes self and expired leases using the database clock.
    pub async fn peer_candidates(&self, self_id: Uuid, target: Option<Uuid>) -> Result<Vec<Peer>> {
        Ok(sqlx::query_as(&format!("SELECT {} FROM inkcre.peers WHERE id<>$1 AND lease_expires_at>statement_timestamp() AND ($2::uuid IS NULL OR id=$2)", super::PEER_COLUMNS))
            .bind(self_id).bind(target).fetch_all(self.pool()).await?)
    }

    /// Failover is permitted only when the transport proves non-execution.
    pub async fn peer_delegate(
        &self,
        self_id: Uuid,
        capability: &str,
        payload: Value,
        target: Option<Uuid>,
        options: &PeerHttpOptions,
    ) -> std::result::Result<Value, DelegationError> {
        if options.jwt_secret.len() < 32
            || options.lifetime_seconds == 0
            || options.timeout.is_zero()
        {
            return Err(DelegationError::Protocol);
        }
        let object = payload.as_object().ok_or(DelegationError::Protocol)?;
        if object
            .keys()
            .any(|k| !matches!(k.as_str(), "query" | "headers" | "body"))
        {
            return Err(DelegationError::Protocol);
        }
        let query = normalize_map(object.get("query"))?;
        let headers = normalize_map(object.get("headers"))?;
        let reserved = [
            "authorization",
            "connection",
            "content-length",
            "host",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
        ];
        if headers.keys().any(|key| reserved.contains(&key.as_str())) {
            return Err(DelegationError::Protocol);
        }
        let mut peers = self.peer_candidates(self_id, target).await?;
        if target.is_none() {
            peers.sort_by_key(|_| Uuid::new_v4());
        }
        let now = chrono::Utc::now().timestamp();
        let expiration = now
            .checked_add(
                i64::try_from(options.lifetime_seconds).map_err(|_| DelegationError::Protocol)?,
            )
            .ok_or(DelegationError::Protocol)?;
        let token = jsonwebtoken::encode(&jsonwebtoken::Header::default(), &json!({"role":"authenticated","iss":"inkcre-peer","aud":"inkcre-api","iat":now,"exp":expiration}), &jsonwebtoken::EncodingKey::from_secret(options.jwt_secret.as_bytes())).map_err(|_| DelegationError::Protocol)?;
        let client = reqwest::Client::builder()
            .timeout(options.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| DelegationError::Protocol)?;
        for peer in peers {
            let Ok(capabilities) =
                serde_json::from_value::<Vec<Capability>>(peer.capabilities.clone())
            else {
                continue;
            };
            let mut seen = BTreeSet::new();
            if capabilities.iter().any(|c| {
                c.id.is_empty()
                    || c.inbound.protocol.is_empty()
                    || !c.inbound.parameters.is_object()
                    || !seen.insert(&c.id)
            }) {
                continue;
            }
            let Some(capability) = capabilities
                .iter()
                .find(|c| c.id == capability && c.inbound.protocol == HTTP_PROTOCOL)
            else {
                continue;
            };
            let Ok(inbound) =
                serde_json::from_value::<HttpInbound>(capability.inbound.parameters.clone())
            else {
                continue;
            };
            if inbound.validate().is_err() {
                continue;
            }
            let method = reqwest::Method::from_bytes(inbound.method.to_uppercase().as_bytes())
                .map_err(|_| DelegationError::Protocol)?;
            let query: Vec<_> = query
                .iter()
                .flat_map(|(key, values)| values.iter().map(move |value| (key, value)))
                .collect();
            let mut request = client
                .request(method, &inbound.url)
                .query(&query)
                .bearer_auth(&token);
            for (name, values) in &headers {
                let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| DelegationError::Protocol)?;
                for value in values {
                    request = request.header(
                        &name,
                        reqwest::header::HeaderValue::from_str(value)
                            .map_err(|_| DelegationError::Protocol)?,
                    );
                }
            }
            if let Some(body) = object.get("body") {
                request = request.json(body)
            }
            let mut response = match request.send().await {
                Ok(response) => response,
                Err(error) if error.is_connect() => {
                    if target.is_some() {
                        break;
                    } else {
                        continue;
                    }
                }
                Err(error) if error.is_builder() => return Err(DelegationError::Protocol),
                Err(_) => return Err(DelegationError::OutcomeUnknown),
            };
            if response
                .headers()
                .get(EXECUTION_HEADER)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("not-executed"))
            {
                if target.is_some() { break } else { continue }
            }
            let status = response.status().as_u16();
            let mut response_headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (name, value) in response.headers() {
                if name != EXECUTION_HEADER {
                    response_headers.entry(name.to_string()).or_default().push(
                        value
                            .to_str()
                            .map_err(|_| DelegationError::Protocol)?
                            .to_owned(),
                    );
                }
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| DelegationError::OutcomeUnknown)?
            {
                if bytes.len().saturating_add(chunk.len()) > 8 * 1024 * 1024 {
                    return Err(DelegationError::Protocol);
                }
                bytes.extend_from_slice(&chunk);
            }
            let mut result = json!({"status":status,"headers":response_headers});
            if !bytes.is_empty() {
                result["body"] =
                    serde_json::from_slice(&bytes).map_err(|_| DelegationError::Protocol)?;
            }
            return Ok(result);
        }
        Err(DelegationError::Unavailable)
    }
}

#[derive(Clone, Debug)]
pub struct HttpCapability {
    pub id: String,
    pub method: String,
    pub path: String,
}

impl Core {
    /// Publish from the owner-controlled config and actual installed inbound routes.
    pub async fn peer_publish_http(&self, id: Uuid, routes: &[HttpCapability]) -> Result<()> {
        let mut tx = self.pool().begin().await?;
        let config: Value =
            sqlx::query_scalar("SELECT config FROM inkcre.peers WHERE id=$1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(Error::NotFound("peer"))?;
        let object = config
            .as_object()
            .ok_or_else(|| Error::InvalidInput("Peer config must be an object".into()))?;
        if object.keys().any(|key| {
            !matches!(
                key.as_str(),
                "http_public_base_url" | "extension_registry_url"
            )
        }) {
            return Err(Error::InvalidInput("unknown Peer config property".into()));
        }
        let mut capabilities = Vec::new();
        if let Some(value) = object
            .get("http_public_base_url")
            .filter(|value| !value.is_null())
        {
            let base = value
                .as_str()
                .ok_or_else(|| Error::InvalidInput("invalid public HTTP base URL".into()))?;
            validate_url(base, true)?;
            for route in routes {
                capabilities.push(http_advertisement(
                    &route.id,
                    &route.method,
                    &route.path,
                    base,
                )?);
            }
        }
        capabilities.sort_by(|a, b| a.id.cmp(&b.id));
        if capabilities.windows(2).any(|pair| pair[0].id == pair[1].id) {
            return Err(Error::Conflict("duplicate Peer inbound capability".into()));
        }
        sqlx::query("UPDATE inkcre.peers SET capabilities=$2 WHERE id=$1 AND capabilities IS DISTINCT FROM $2")
            .bind(id).bind(json!(capabilities)).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
}
