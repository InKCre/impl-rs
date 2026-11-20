use crate::{Result, conflict, db, invalid, provider, schema::*};
use inkcre_core::Core;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration;

pub fn fingerprint(config: &Value) -> String {
    let material = format!(
        "inkcre-twitter-oauth-app\0{}\0{}",
        config["client_id"].as_str().unwrap_or_default(),
        config["client_secret"].as_str().unwrap_or_default()
    );
    format!("{:x}", Sha256::digest(material.as_bytes()))
}
pub struct Official {
    core: Core,
    authorization: String,
    client: reqwest::Client,
    base: url::Url,
}
pub use crate::capture::Page;
impl Official {
    pub async fn new(core: &Core) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| provider("X client unavailable"))?;
        let base = url::Url::parse(
            &std::env::var("INKCRE_TWITTER_API_BASE_URL")
                .unwrap_or_else(|_| "https://api.x.com/2/".into()),
        )
        .map_err(|_| invalid("Invalid X endpoint"))?;
        if !matches!(base.scheme(), "https" | "http")
            || base.cannot_be_a_base()
            || !base.username().is_empty()
            || base.password().is_some()
        {
            return Err(invalid("Invalid X endpoint"));
        }
        let extension = core
            .extension_get(EXTENSION)
            .await
            .map_err(|_| provider("Twitter state read failed"))?
            .ok_or_else(|| conflict("Twitter extension not configured"))?;
        if extension.config["backend"].as_str().unwrap_or("official") != "official" {
            return Err(conflict(
                "The twikit backend requires an equivalent capture provider",
            ));
        }
        let account = &extension.state["account"];
        admit(&extension.config, account)?;
        let authorization = account["authorization_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| conflict("Twitter authorization missing"))?
            .into();
        Ok(Self {
            core: core.clone(),
            authorization,
            client,
            base,
        })
    }
    pub async fn guard_in(&self, c: &mut sqlx::PgConnection) -> Result<(Value, Value)> {
        let (config, state): (Value, Value) =
            sqlx::query_as("SELECT config,state FROM inkcre.extensions WHERE name=$1 FOR SHARE")
                .bind(EXTENSION)
                .fetch_one(c)
                .await
                .map_err(db)?;
        self.guard(&config, &state)?;
        Ok((config, state))
    }
    fn guard(&self, config: &Value, state: &Value) -> Result<()> {
        admit(config, &state["account"])?;
        if state["account"]["authorization_id"] != self.authorization {
            return Err(conflict("Twitter authorization changed"));
        }
        Ok(())
    }
    async fn current(&self) -> Result<(Value, Value)> {
        let e = self
            .core
            .extension_get(EXTENSION)
            .await
            .map_err(|_| provider("Twitter state read failed"))?
            .ok_or_else(|| conflict("Twitter extension missing"))?;
        self.guard(&e.config, &e.state)?;
        Ok((e.config, e.state))
    }
    async fn reconnect(&self) -> Result<()> {
        sqlx::query("UPDATE inkcre.extensions SET state=jsonb_set(state,'{account,reconnect_required}','true') WHERE name=$1 AND state->'account'->>'authorization_id'=$2").bind(EXTENSION).bind(&self.authorization).execute(self.core.pool()).await.map_err(db)?;
        Ok(())
    }
    fn endpoint(&self, path: &str) -> Result<url::Url> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| invalid("Invalid X endpoint"))?
            .pop_if_empty()
            .extend(path.trim_start_matches('/').split('/'));
        Ok(url)
    }
    async fn token(&self, config: &Value, state: &Value) -> Result<Value> {
        let token = &state["account"]["token"];
        if token["expires_at"]
            .as_f64()
            .is_none_or(|n| n > chrono::Utc::now().timestamp() as f64 + 60.0)
        {
            return Ok(token.clone());
        }
        let Some(refresh) = token["refresh_token"].as_str() else {
            self.reconnect().await?;
            return Err(conflict("Twitter token refresh unavailable"));
        };
        let response = self
            .client
            .post(self.endpoint("oauth2/token")?)
            .basic_auth(
                config["client_id"].as_str().unwrap_or_default(),
                config["client_secret"].as_str(),
            )
            .form(&[("grant_type", "refresh_token"), ("refresh_token", refresh)])
            .send()
            .await
            .map_err(|_| provider("Twitter refresh request failed"))?;
        let status = response.status();
        if status.is_server_error() {
            return Err(provider("Twitter token refresh provider unavailable"));
        }
        if !status.is_success() {
            let body = read(response).await?;
            if status == reqwest::StatusCode::UNAUTHORIZED || body.get("error").is_some() {
                self.reconnect().await?;
                return Err(conflict("Twitter authorization requires reconnection"));
            }
            return Err(provider("Twitter token refresh rejected"));
        }
        let mut next = read(response).await?;
        if !next.is_object() || next["access_token"].as_str().is_none() {
            self.reconnect().await?;
            return Err(conflict("Invalid Twitter token refresh"));
        }
        if next.get("refresh_token").is_none() {
            next["refresh_token"] = token["refresh_token"].clone();
        }
        if let Some(seconds) = next["expires_in"].as_i64() {
            next["expires_at"] = json!(chrono::Utc::now().timestamp() + seconds);
        }
        let mut tx = self.core.pool().begin().await.map_err(db)?;
        let (current_config, mut current): (Value, Value) =
            sqlx::query_as("SELECT config,state FROM inkcre.extensions WHERE name=$1 FOR UPDATE")
                .bind(EXTENSION)
                .fetch_one(&mut *tx)
                .await
                .map_err(db)?;
        self.guard(&current_config, &current)?;
        if current["account"]["token"] != *token {
            return Err(conflict("Twitter token changed during refresh"));
        }
        current["account"]["token"] = next.clone();
        sqlx::query("UPDATE inkcre.extensions SET state=$2 WHERE name=$1")
            .bind(EXTENSION)
            .bind(current)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(next)
    }
    pub async fn bookmarks(&self, page: Option<&str>, limit: usize) -> Result<Page> {
        for retry in 0..=3 {
            let (config, state) = self.current().await?;
            let token = self.token(&config, &state).await?;
            let user = state["account"]["user_id"]
                .as_str()
                .ok_or_else(|| conflict("Twitter account identity missing"))?;
            let mut url = self.endpoint(&format!("users/{user}/bookmarks"))?;
            url.query_pairs_mut().append_pair("max_results",&limit.to_string()).append_pair("tweet.fields","author_id,created_at,attachments,entities,lang,conversation_id,referenced_tweets,public_metrics,possibly_sensitive,note_tweet").append_pair("media.fields","alt_text,media_key,url,type,preview_image_url,width,height,duration_ms,variants").append_pair("user.fields","id,name,username,description,profile_image_url,url,protected,location,public_metrics,created_at").append_pair("expansions","author_id,attachments.media_keys,referenced_tweets.id,referenced_tweets.id.author_id");
            if let Some(page) = page {
                url.query_pairs_mut().append_pair("pagination_token", page);
            }
            let response = self
                .client
                .get(url)
                .bearer_auth(
                    token["access_token"]
                        .as_str()
                        .ok_or_else(|| conflict("Twitter access token missing"))?,
                )
                .send()
                .await
                .map_err(|_| provider("Twitter request failed"))?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED {
                self.reconnect().await?;
                return Err(conflict("Twitter authorization requires reconnection"));
            }
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS && retry < 3 {
                if let Some(reset) = response.headers().get("x-rate-limit-reset") {
                    let reset = reset
                        .to_str()
                        .ok()
                        .and_then(|s| s.parse::<i64>().ok())
                        .ok_or_else(|| provider("Invalid Twitter rate limit reset"))?;
                    let wait = (reset - chrono::Utc::now().timestamp() + 5).max(0) as u64;
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                }
                continue;
            }
            if !response.status().is_success() {
                return Err(provider("Twitter API rejected request"));
            }
            let value = read(response).await?;
            let raw = value
                .get("data")
                .map(Value::as_array)
                .unwrap_or(Some(&Vec::new()))
                .cloned()
                .ok_or_else(|| provider("Invalid Twitter data"))?;
            let primary = raw
                .iter()
                .map(|post| {
                    post["id"]
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| provider("Twitter primary ID missing"))
                })
                .collect::<Result<Vec<_>>>()?;
            let batch = convert(&raw, &value["includes"])?;
            let next = value["meta"]["next_token"].as_str().map(str::to_owned);
            return Ok(Page {
                batch,
                primary,
                next,
            });
        }
        Err(provider("Twitter rate limit exceeded"))
    }
}
fn admit(config: &Value, account: &Value) -> Result<()> {
    if config["backend"].as_str().unwrap_or("official") != "official"
        || !account.is_object()
        || account["reconnect_required"] == true
        || account["app_fingerprint"] != fingerprint(config)
    {
        return Err(conflict("Twitter account is not connected"));
    }
    Ok(())
}
pub async fn read(mut response: reqwest::Response) -> Result<Value> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| provider("Twitter response interrupted"))?
    {
        if chunk.len() > 16 * 1024 * 1024usize - bytes.len() {
            return Err(provider("Twitter response exceeded limit"));
        }
        bytes.extend(chunk)
    }
    serde_json::from_slice(&bytes).map_err(|_| provider("Invalid Twitter response"))
}
fn instant(value: &Value) -> Result<String> {
    let s = value
        .as_str()
        .ok_or_else(|| provider("Twitter timestamp missing"))?;
    chrono::DateTime::parse_from_rfc3339(s)
        .or_else(|_| chrono::DateTime::parse_from_rfc2822(s))
        .map(|t| {
            t.with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
        .map_err(|_| provider("Invalid Twitter provider timestamp"))
}
fn copy(raw: &Value, target: &mut Value, fields: &[(&str, &str)]) {
    for (source, destination) in fields {
        if let Some(value) = raw.get(source).filter(|v| !v.is_null()) {
            target[*destination] = value.clone();
        }
    }
}
fn counts(raw: &Value, target: &mut Value, fields: &[(&str, &str)], observed: &str) {
    let mut values = json!({});
    let mut times = json!({});
    for (source, destination) in fields {
        if let Some(value) = raw.get(source).filter(|v| !v.is_null()) {
            values[*destination] = value.clone();
            times[*destination] = json!(observed);
        }
    }
    if values.as_object().is_some_and(|o| !o.is_empty()) {
        target["counts"] = values;
        target["countsObservedAt"] = times;
    }
}
fn items<'a>(value: &'a Value, key: &str) -> Result<&'a [Value]> {
    match value.get(key) {
        None => Ok(&[]),
        Some(Value::Array(values)) => Ok(values),
        _ => Err(provider("Invalid Twitter provider collection")),
    }
}
fn media_id(raw: &Value) -> Option<String> {
    if let Some(value) = raw.get("id") {
        if let Some(id) = value.as_str() {
            return Some(id.into());
        }
        if let Some(id) = value.as_u64() {
            return Some(id.to_string());
        }
    }
    let (prefix, id) = raw["media_key"].as_str()?.split_once('_')?;
    matches!(prefix, "3" | "7" | "13").then(|| id.to_owned())
}
pub fn convert(raw_posts: &[Value], includes: &Value) -> Result<Batch> {
    let observed = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let mut media = Vec::new();
    let mut media_ids = std::collections::BTreeMap::new();
    for raw in items(includes, "media")? {
        let Some(id) = media_id(raw) else { continue };
        let key = raw["media_key"]
            .as_str()
            .ok_or_else(|| provider("Twitter media key missing"))?;
        media_ids.insert(key, id.clone());
        let kind = match raw["type"].as_str() {
            Some("photo") => "photo",
            Some("video") => "video",
            Some("animated_gif") => "gif",
            _ => continue,
        };
        let url = if kind == "photo" {
            &raw["url"]
        } else {
            &raw["preview_image_url"]
        };
        if url.as_str().is_none_or(|s| s.is_empty()) {
            continue;
        }
        let mut item = json!({"id":id,"type":kind,"url":url});
        copy(
            raw,
            &mut item,
            &[
                ("width", "width"),
                ("height", "height"),
                ("alt_text", "alt"),
            ],
        );
        if let Some(variants) = raw["variants"].as_array().filter(|v| !v.is_empty()) {
            let variants: Vec<_> = variants
                .iter()
                .map(|v| {
                    let mut variant = json!({"url":v["url"],"type":v["content_type"]});
                    copy(v, &mut variant, &[("bit_rate", "bitrate")]);
                    variant
                })
                .collect();
            item["video"] = json!({"variants":variants});
            copy(raw, &mut item["video"], &[("duration_ms", "durationMs")]);
        }
        media.push(item);
    }
    let mut users = Vec::new();
    for raw in items(includes, "users")? {
        let mut user = json!({"id":raw["id"],"handle":raw["username"],"name":raw["name"]});
        copy(
            raw,
            &mut user,
            &[
                ("profile_image_url", "avatarUrl"),
                ("location", "location"),
                ("url", "website"),
                ("protected", "protected"),
            ],
        );
        if let Some(description) = raw["description"].as_str() {
            user["bio"] = json!({"text":html_escape::decode_html_entities(description)});
        }
        if raw.get("created_at").is_some() {
            user["createdAt"] = json!(instant(&raw["created_at"])?);
        }
        counts(
            &raw["public_metrics"],
            &mut user,
            &[
                ("followers_count", "followers"),
                ("following_count", "following"),
                ("tweet_count", "posts"),
            ],
            &observed,
        );
        users.push(user);
    }
    let mut posts = Vec::new();
    for raw in raw_posts.iter().chain(items(includes, "tweets")?) {
        let mut post = json!({"id":raw["id"],"authorId":raw["author_id"],"createdAt":instant(&raw["created_at"])?});
        let refs: std::collections::BTreeMap<_, _> = items(raw, "referenced_tweets")?
            .iter()
            .filter_map(|r| r["type"].as_str().map(|k| (k, &r["id"])))
            .collect();
        if let Some(id) = refs.get("retweeted") {
            post["repostOf"] = (*id).clone();
        } else {
            let text = raw["note_tweet"]["text"]
                .as_str()
                .or(raw["text"].as_str())
                .ok_or_else(|| provider("Twitter post text missing"))?;
            post["text"] = json!({"text":html_escape::decode_html_entities(text)});
            for (key, field) in [("replied_to", "replyTo"), ("quoted", "quote")] {
                if let Some(id) = refs.get(key) {
                    post[field] = json!({"postId":id});
                }
            }
            if raw
                .get("conversation_id")
                .is_some_and(|id| !id.is_null() && id != &raw["id"])
            {
                post["conversationId"] = raw["conversation_id"].clone();
            }
            copy(
                raw,
                &mut post,
                &[("lang", "lang"), ("possibly_sensitive", "sensitive")],
            );
            let attached: Vec<_> = raw["attachments"]["media_keys"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter_map(|key| media_ids.get(key))
                .cloned()
                .collect();
            if !attached.is_empty() {
                post["mediaIds"] = json!(attached)
            }
            counts(
                &raw["public_metrics"],
                &mut post,
                &[
                    ("reply_count", "replies"),
                    ("retweet_count", "reposts"),
                    ("quote_count", "quotes"),
                    ("like_count", "likes"),
                    ("bookmark_count", "bookmarks"),
                    ("impression_count", "views"),
                ],
                &observed,
            );
        }
        posts.push(post);
    }
    Batch::parse(json!({"users":users,"posts":posts,"media":media}))
}
