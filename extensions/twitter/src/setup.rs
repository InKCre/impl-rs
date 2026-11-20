//! Whole-extension OAuth setup. Authorization transitions are owned by one database row.
use crate::{Result, conflict, db, invalid, official, provider, schema::EXTENSION};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use inkcre_core::Core;
use inkcre_extension_sdk::{
    HttpAuth, HttpBody, HttpHeader, HttpIngress, HttpPathMatch, HttpRequest, HttpResponse,
    HttpRoute, PeerHttpInbound, RpcError,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;
const SCOPES: &str = "tweet.read users.read bookmark.read offline.access";
const ROUTES: [(&str, &str, &str); 5] = [
    ("GET", "/twitter/setup", "inkcre.twitter.setup.status.v1"),
    (
        "PUT",
        "/twitter/setup/oauth-app",
        "inkcre.twitter.oauth-app.configure.v1",
    ),
    (
        "POST",
        "/twitter/setup/oauth-transactions",
        "inkcre.twitter.oauth.begin.v1",
    ),
    (
        "POST",
        "/twitter/setup/oauth-transaction",
        "inkcre.twitter.oauth.transaction.read.v1",
    ),
    (
        "DELETE",
        "/twitter/setup/account",
        "inkcre.twitter.oauth.disconnect.v1",
    ),
];
pub fn ingress() -> HttpIngress {
    let mut routes: Vec<_> = ROUTES
        .iter()
        .map(|(method, path, _)| HttpRoute {
            method: (*method).into(),
            path: (*path).into(),
            path_match: HttpPathMatch::Exact,
            auth: HttpAuth::Peer,
        })
        .collect();
    routes.push(HttpRoute {
        method: "GET".into(),
        path: "/twitter/auth/callback".into(),
        path_match: HttpPathMatch::Exact,
        auth: HttpAuth::Public,
    });
    routes.extend(crate::archive::routes());
    HttpIngress {
        routes,
        peer_inbounds: ROUTES
            .iter()
            .map(|(method, path, capability)| PeerHttpInbound {
                capability: (*capability).into(),
                method: (*method).into(),
                path: (*path).into(),
            })
            .collect(),
    }
}
fn now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}
fn time(value: &Value) -> Option<DateTime<Utc>> {
    value
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
}
fn pending(transaction: &Value) -> bool {
    matches!(
        transaction["status"].as_str(),
        Some("pending" | "exchanging")
    )
}
fn terminal(transaction: &mut Value, status: &str, message: Option<&str>) {
    transaction["status"] = json!(status);
    transaction["provider_state"] = Value::Null;
    transaction["pkce_verifier"] = Value::Null;
    transaction["closed_at"] = json!(now());
    transaction["error"] = message.map_or(Value::Null, |message| json!(message));
}
fn transactions(state: &mut Value) -> Result<&mut serde_json::Map<String, Value>> {
    if state.get("oauth_transactions").is_none() {
        state["oauth_transactions"] = json!({})
    }
    state["oauth_transactions"]
        .as_object_mut()
        .ok_or_else(|| conflict("Invalid Twitter OAuth state"))
}
async fn mutate<T>(
    core: &Core,
    update: impl FnOnce(&mut Value, &mut Value) -> Result<T>,
) -> Result<(Value, Value, T)> {
    let mut tx = core.pool().begin().await.map_err(db)?;
    let (mut config, mut state): (Value, Value) =
        sqlx::query_as("SELECT config,state FROM inkcre.extensions WHERE name=$1 FOR UPDATE")
            .bind(EXTENSION)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db)?
            .ok_or_else(|| conflict("Twitter extension missing"))?;
    if !config.is_object() || !state.is_object() {
        return Err(conflict("Invalid Twitter extension state"));
    }
    let result = update(&mut config, &mut state)?;
    sqlx::query("UPDATE inkcre.extensions SET config=$2,state=$3 WHERE name=$1")
        .bind(EXTENSION)
        .bind(&config)
        .bind(&state)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok((config, state, result))
}
fn invalidate(config: &Value, state: &mut Value, reason: &str) -> Result<()> {
    let fingerprint = official::fingerprint(config);
    if state
        .get("account")
        .is_some_and(|a| !a.is_null() && a["app_fingerprint"] != fingerprint)
    {
        state["account"] = Value::Null;
    }
    for transaction in transactions(state)?.values_mut() {
        if pending(transaction) && transaction["app_fingerprint"] != fingerprint {
            terminal(transaction, "expired", Some(reason));
        }
    }
    Ok(())
}
pub async fn reconcile(core: &Core) -> Result<()> {
    mutate(core, |config, state| {
        invalidate(config, state, "OAuth App changed")
    })
    .await?;
    Ok(())
}
async fn base(core: &Core, peer: Uuid) -> Result<String> {
    let peer = core
        .peer_get(peer)
        .await
        .map_err(|_| conflict("Core Peer public HTTP URL unavailable"))?
        .ok_or_else(|| conflict("Core Peer missing"))?;
    peer.config["http_public_base_url"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.trim_end_matches('/').to_owned())
        .ok_or_else(|| conflict("Core Peer public HTTP URL is not configured"))
}
async fn status(core: &Core, peer: Uuid) -> Result<Value> {
    let (config, state, _) = mutate(core, |config, state| {
        invalidate(config, state, "OAuth App changed")
    })
    .await?;
    let base = base(core, peer).await?;
    let account = &state["account"];
    let connected = config["client_id"].as_str().is_some_and(|s| !s.is_empty())
        && config["client_secret"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
        && account.is_object()
        && account["reconnect_required"] != true
        && account["app_fingerprint"] == official::fingerprint(&config);
    Ok(
        json!({"callback_url":format!("{base}/twitter/auth/callback"),"x_monkey_base_url":format!("{base}/twitter/x-monkey"),"connected":connected,"user_id":if connected{account["user_id"].clone()}else{Value::Null},"handle":if connected{account["handle"].clone()}else{Value::Null},"scopes":if connected{account.get("scopes").cloned().unwrap_or_else(||json!([]))}else{json!([])},"reconnect_required":account["reconnect_required"]==true}),
    )
}
#[derive(Deserialize)]
struct AppRequest {
    client_id: String,
    client_secret: String,
    #[serde(default)]
    confirm_account_reset: bool,
}
async fn save_app(core: &Core, peer: Uuid, body: &[u8]) -> Result<Value> {
    let request: AppRequest =
        serde_json::from_slice(body).map_err(|_| invalid("Invalid Twitter OAuth App request"))?;
    if request.client_id.is_empty()
        || request.client_id.chars().count() > 256
        || request.client_secret.is_empty()
        || request.client_secret.chars().count() > 1024
    {
        return Err(invalid("Invalid Twitter OAuth App credentials"));
    }
    mutate(core, |config, state| {
        let mut next = config.clone();
        next["backend"] = json!("official");
        next["client_id"] = json!(request.client_id.trim());
        next["client_secret"] = json!(request.client_secret);
        let changed = official::fingerprint(&next) != official::fingerprint(config);
        let live = state.get("account").is_some_and(|a| !a.is_null())
            || transactions(state)?.values().any(pending);
        if changed && live && !request.confirm_account_reset {
            return Err(conflict(
                "Replacing the OAuth App requires account-reset confirmation",
            ));
        }
        if changed {
            invalidate(&next, state, "OAuth App changed")?;
        }
        *config = next;
        Ok(())
    })
    .await?;
    status(core, peer).await
}
fn secret() -> String {
    let mut bytes = Vec::new();
    for _ in 0..3 {
        bytes.extend_from_slice(Uuid::new_v4().as_bytes())
    }
    URL_SAFE_NO_PAD.encode(bytes)
}
fn view(id: &str, transaction: &Value, url: Option<String>) -> Value {
    json!({"id":id,"status":transaction["status"],"authorize_url":url,"expires_at":transaction["expires_at"],"error":transaction.get("error").cloned().unwrap_or(Value::Null)})
}
async fn begin(core: &Core, peer: Uuid) -> Result<Value> {
    let redirect = format!("{}/twitter/auth/callback", base(core, peer).await?);
    let id = Uuid::new_v4().to_string();
    let state = secret();
    let verifier = secret();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let created = Utc::now();
    let(config,_,transaction)=mutate(core,|config,state_model|{
  if config["backend"].as_str().unwrap_or("official")!="official"||config["client_id"].as_str().is_none_or(str::is_empty)||config["client_secret"].as_str().is_none_or(str::is_empty){return Err(conflict("Configure the Twitter OAuth App first"))}
  let transactions=transactions(state_model)?;
  transactions.retain(|_,t|time(&t["closed_at"]).is_none_or(|closed|closed+chrono::Duration::minutes(10)>created));for t in transactions.values_mut(){if pending(t){terminal(t,"expired",Some("Superseded by a newer setup"))}}
  let transaction=json!({"status":"pending","provider_state":state,"pkce_verifier":verifier,"app_fingerprint":official::fingerprint(config),"redirect_uri":redirect,"created_at":created.to_rfc3339_opts(chrono::SecondsFormat::Micros,true),"expires_at":(created+chrono::Duration::minutes(10)).to_rfc3339_opts(chrono::SecondsFormat::Micros,true),"closed_at":null,"error":null});transactions.insert(id.clone(),transaction.clone());
  if transactions.len()>8{let mut ordered:Vec<_>=transactions.iter().map(|(key,t)|(key.clone(),t["created_at"].as_str().unwrap_or_default().to_owned())).collect();ordered.sort_by(|a,b|b.1.cmp(&a.1));for(key,_)in ordered.into_iter().skip(8){transactions.remove(&key);}}
  Ok(transaction)
 }).await?;
    let authorize = std::env::var("INKCRE_TWITTER_AUTHORIZE_URL")
        .unwrap_or_else(|_| "https://x.com/i/oauth2/authorize".into());
    let mut url = url::Url::parse(&authorize)
        .map_err(|_| invalid("Invalid Twitter authorization endpoint"))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair(
            "client_id",
            config["client_id"].as_str().unwrap_or_default(),
        )
        .append_pair("redirect_uri", &redirect)
        .append_pair("scope", SCOPES)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(view(&id, &transaction, Some(url.into())))
}
#[derive(Deserialize)]
struct TransactionRequest {
    transaction_id: String,
}
async fn transaction(core: &Core, body: &[u8]) -> Result<Value> {
    let request: TransactionRequest =
        serde_json::from_slice(body).map_err(|_| invalid("Invalid OAuth transaction request"))?;
    let (_, _, transaction) = mutate(core, |_, state| {
        let transaction = transactions(state)?
            .get_mut(&request.transaction_id)
            .ok_or_else(|| RpcError::new("not_found", "OAuth transaction not found"))?;
        if pending(transaction)
            && time(&transaction["expires_at"]).is_some_and(|expires| expires <= Utc::now())
        {
            terminal(transaction, "expired", Some("OAuth transaction expired"))
        }
        Ok(transaction.clone())
    })
    .await?;
    Ok(view(&request.transaction_id, &transaction, None))
}
async fn disconnect(core: &Core, peer: Uuid) -> Result<Value> {
    mutate(core, |_, state| {
        state["account"] = Value::Null;
        for t in transactions(state)?.values_mut() {
            if pending(t) {
                terminal(t, "expired", Some("Account disconnected"))
            }
        }
        Ok(())
    })
    .await?;
    status(core, peer).await
}
fn html(status: u16, title: &str, message: &str) -> HttpResponse {
    let title = html_escape::encode_text(title);
    let message = html_escape::encode_text(message);
    HttpResponse{status,headers:vec![HttpHeader{name:"content-type".into(),value:b"text/html; charset=utf-8".to_vec()}],body:HttpBody(format!("<!doctype html><html><head><meta charset='utf-8'><title>{title}</title></head><body><main><h1>{title}</h1><p>{message}</p><p>You can close this window.</p></main></body></html>").into_bytes())}
}
async fn exchange(config: &Value, transaction: &Value, code: &str) -> Result<Value> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| provider("Twitter OAuth client unavailable"))?;
    let base = std::env::var("INKCRE_TWITTER_API_BASE_URL")
        .unwrap_or_else(|_| "https://api.x.com/2/".into());
    let base = base.trim_end_matches('/');
    let response = client
        .post(format!("{base}/oauth2/token"))
        .basic_auth(
            config["client_id"].as_str().unwrap_or_default(),
            config["client_secret"].as_str(),
        )
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            (
                "redirect_uri",
                transaction["redirect_uri"].as_str().unwrap_or_default(),
            ),
            (
                "code_verifier",
                transaction["pkce_verifier"].as_str().unwrap_or_default(),
            ),
        ])
        .send()
        .await
        .map_err(|_| provider("Twitter token exchange request failed"))?;
    if !response.status().is_success() {
        return Err(provider("Twitter rejected the token exchange"));
    }
    let mut token = official::read(response).await?;
    let access = token["access_token"]
        .as_str()
        .ok_or_else(|| provider("Twitter access token missing"))?
        .to_owned();
    if let Some(seconds) = token["expires_in"].as_i64() {
        token["expires_at"] = json!(Utc::now().timestamp() + seconds);
    }
    let response = client
        .get(format!("{base}/users/me"))
        .bearer_auth(access)
        .send()
        .await
        .map_err(|_| provider("Twitter current-user lookup failed"))?;
    if !response.status().is_success() {
        return Err(provider(
            if response.status() == reqwest::StatusCode::PAYMENT_REQUIRED {
                "Twitter current-user lookup requires X API credits or project access (HTTP 402)"
            } else {
                "Twitter current-user lookup failed"
            },
        ));
    }
    let user = official::read(response).await?;
    let user_id = user["data"]["id"]
        .as_str()
        .ok_or_else(|| provider("Twitter current-user identity missing"))?;
    let handle = user["data"]["username"]
        .as_str()
        .ok_or_else(|| provider("Twitter current-user handle missing"))?;
    let scopes = token["scope"]
        .as_str()
        .unwrap_or(SCOPES)
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    Ok(
        json!({"token":token,"user_id":user_id,"handle":handle,"scopes":scopes,"app_fingerprint":transaction["app_fingerprint"],"authorization_id":Uuid::new_v4().to_string(),"connected_at":now(),"reconnect_required":false}),
    )
}
async fn finish(
    core: &Core,
    id: &str,
    claimed: &Value,
    account: Option<Value>,
    error: Option<&str>,
) -> Result<()> {
    mutate(core, |config, state| {
        let current = transactions(state)?
            .get_mut(id)
            .ok_or_else(|| conflict("OAuth transaction superseded"))?;
        if current["status"] != "exchanging"
            || current["provider_state"] != claimed["provider_state"]
            || current["app_fingerprint"] != claimed["app_fingerprint"]
            || (account.is_some() && official::fingerprint(config) != claimed["app_fingerprint"])
        {
            return Err(conflict("OAuth transaction superseded"));
        }
        if let Some(account) = account {
            terminal(current, "succeeded", None);
            state["account"] = account;
        } else {
            terminal(
                current,
                "failed",
                Some(error.unwrap_or("Twitter authorization failed")),
            )
        }
        Ok(())
    })
    .await?;
    Ok(())
}
async fn callback(core: &Core, query: &str) -> HttpResponse {
    let query: std::collections::BTreeMap<_, _> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    let Some(provider_state) = query.get("state").filter(|s| !s.is_empty()) else {
        return html(400, "Twitter setup failed", "Missing OAuth state.");
    };
    let claim = mutate(core, |_, state| {
        let (id, transaction) = transactions(state)?
            .iter_mut()
            .find(|(_, t)| t["provider_state"] == *provider_state)
            .ok_or_else(|| conflict("OAuth transaction is unknown"))?;
        if transaction["status"] != "pending"
            || time(&transaction["expires_at"]).is_none_or(|expires| expires <= Utc::now())
        {
            return Err(conflict("OAuth transaction is no longer active"));
        }
        let claimed = transaction.clone();
        transaction["status"] = json!("exchanging");
        Ok((id.clone(), claimed))
    })
    .await;
    let Ok((config, _, (id, transaction))) = claim else {
        return html(
            400,
            "Twitter setup failed",
            "OAuth transaction is unknown or inactive.",
        );
    };
    if let Some(error) = query.get("error").filter(|s| !s.is_empty()) {
        let message = if error == "access_denied" {
            "Twitter authorization was declined"
        } else {
            "Twitter returned an authorization error"
        };
        let _ = finish(core, &id, &transaction, None, Some(message)).await;
        return html(400, "Twitter setup declined", message);
    }
    let Some(code) = query.get("code").filter(|s| !s.is_empty()) else {
        let _ = finish(
            core,
            &id,
            &transaction,
            None,
            Some("Missing authorization code"),
        )
        .await;
        return html(400, "Twitter setup failed", "Missing authorization code");
    };
    if official::fingerprint(&config) != transaction["app_fingerprint"] {
        let _ = finish(
            core,
            &id,
            &transaction,
            None,
            Some("OAuth App changed during authorization"),
        )
        .await;
        return html(
            400,
            "Twitter setup failed",
            "OAuth App changed during authorization",
        );
    }
    match exchange(&config, &transaction, code).await {
        Ok(account) => {
            if finish(core, &id, &transaction, Some(account), None)
                .await
                .is_ok()
            {
                html(
                    200,
                    "Twitter connected",
                    "Authorization completed successfully.",
                )
            } else {
                let _ = finish(
                    core,
                    &id,
                    &transaction,
                    None,
                    Some("OAuth transaction was superseded"),
                )
                .await;
                html(
                    400,
                    "Twitter setup failed",
                    "OAuth transaction was superseded.",
                )
            }
        }
        Err(error) => {
            let _ = finish(core, &id, &transaction, None, Some(&error.message)).await;
            html(502, "Twitter is unavailable", &error.message)
        }
    }
}
pub async fn http(core: &Core, peer: Uuid, request: HttpRequest) -> Result<HttpResponse> {
    if request.path.starts_with("/twitter/x-monkey/") {
        return crate::archive::http(core, peer, request).await;
    }
    if request.method == "GET" && request.path == "/twitter/auth/callback" {
        return Ok(callback(core, &request.query).await);
    }
    let result = match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/twitter/setup") => status(core, peer).await,
        ("PUT", "/twitter/setup/oauth-app") => save_app(core, peer, &request.body.0).await,
        ("POST", "/twitter/setup/oauth-transactions") => begin(core, peer).await,
        ("POST", "/twitter/setup/oauth-transaction") => transaction(core, &request.body.0).await,
        ("DELETE", "/twitter/setup/account") => disconnect(core, peer).await,
        _ => Err(RpcError::new("not_found", "Twitter route not found")),
    };
    let (status, value) = match result {
        Ok(value) => (200, value),
        Err(error) => (
            match error.code.as_str() {
                "invalid_input" => 422,
                "not_found" => 404,
                "provider_error" => 502,
                _ => 409,
            },
            json!({"detail":error.message}),
        ),
    };
    Ok(HttpResponse {
        status,
        headers: vec![HttpHeader {
            name: "content-type".into(),
            value: b"application/json".to_vec(),
        }],
        body: HttpBody(
            serde_json::to_vec(&value).map_err(|_| invalid("Twitter response encoding failed"))?,
        ),
    })
}
