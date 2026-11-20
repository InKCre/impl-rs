//! Explicit local Twikit transport; native archive mapping stays in Rust.
use crate::{Result, conflict, db, invalid, official::Page, provider, schema::*};
use inkcre_core::Core;
use inkcre_extension_sdk::read_frame_with_limit;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

const ENVIRONMENT: &str = "INKCRE_TWITTER_TWIKIT_RUNTIME";
const ADAPTER: &str = include_str!("twikit/adapter.py");
const MAX_FRAME: usize = 16 * 1024 * 1024;
const MAX_REQUEST: usize = 64 * 1024;

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Runtime {
    python_executable: PathBuf,
    cookies_file: PathBuf,
}
#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
struct Login {
    email: String,
    username: String,
    password: String,
    totp_secret: Option<String>,
    api_language: String,
    proxy: Option<String>,
}
impl Login {
    fn from_config(config: &Value) -> Result<Self> {
        if config["backend"] != "twikit" {
            return Err(conflict("Twitter backend is not twikit"));
        }
        let mut login: Self = serde_json::from_value(config.clone())
            .map_err(|_| invalid("Invalid Twikit credentials"))?;
        if login.api_language.is_empty() {
            login.api_language = "en-US".into();
        }
        if [
            &login.email,
            &login.username,
            &login.password,
            &login.api_language,
        ]
        .iter()
        .any(|s| s.len() > 8192)
            || login.totp_secret.as_ref().is_some_and(|s| s.len() > 8192)
            || login.proxy.as_ref().is_some_and(|s| s.len() > 8192)
        {
            return Err(invalid("Twikit configuration exceeds its limit"));
        }
        Ok(login)
    }
    fn fingerprint(&self) -> Result<String> {
        Ok(format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(self).map_err(|_| invalid("Invalid Twikit configuration"))?
            )
        ))
    }
}
fn regular(path: &Path, executable: bool, private: bool) -> Result<std::fs::Metadata> {
    let info =
        std::fs::metadata(path).map_err(|_| conflict("Twikit runtime file is unavailable"))?;
    if !info.is_file() {
        return Err(conflict("Twikit runtime requires regular files"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = info.permissions().mode();
        if mode & 0o022 != 0 || (private && mode & 0o077 != 0) || (executable && mode & 0o111 == 0)
        {
            return Err(conflict("Twikit runtime file permissions are invalid"));
        }
    }
    #[cfg(not(unix))]
    let _ = (executable, private);
    Ok(info)
}
async fn runtime(path: &Path) -> Result<Runtime> {
    if !path.is_absolute() || regular(path, false, false)?.len() > 16 * 1024 {
        return Err(conflict("Twikit runtime configuration is invalid"));
    }
    let mut data = Vec::new();
    tokio::fs::File::open(path)
        .await
        .map_err(|_| conflict("Twikit runtime configuration cannot be read"))?
        .take(16 * 1024 + 1)
        .read_to_end(&mut data)
        .await
        .map_err(|_| conflict("Twikit runtime configuration cannot be read"))?;
    if data.len() > 16 * 1024 {
        return Err(conflict("Twikit runtime configuration exceeds its limit"));
    }
    let runtime: Runtime = serde_json::from_slice(&data)
        .map_err(|_| conflict("Twikit runtime configuration is invalid"))?;
    if !runtime.python_executable.is_absolute()
        || !runtime.cookies_file.is_absolute()
        || runtime.cookies_file.file_name().is_none()
    {
        return Err(conflict("Twikit runtime paths must be absolute"));
    }
    regular(&runtime.python_executable, true, false)?;
    if let Ok(info) = std::fs::symlink_metadata(&runtime.cookies_file) {
        if info.file_type().is_symlink() {
            return Err(conflict("Twikit cookies cannot be a symlink"));
        }
        if regular(&runtime.cookies_file, false, true)?.len() > 4 * 1024 * 1024 {
            return Err(conflict("Twikit cookie file exceeds its limit"));
        }
    }
    Ok(runtime)
}

pub struct Twikit {
    core: Core,
    login: Login,
    fingerprint: String,
    runtime_path: PathBuf,
    runtime: Runtime,
    bridge: Mutex<Option<Bridge>>,
    identity: Mutex<Option<(String, String)>>,
}
impl Twikit {
    pub async fn new(core: &Core, config: &Value) -> Result<Self> {
        let login = Login::from_config(config)?;
        let fingerprint = login.fingerprint()?;
        let runtime_path = std::env::var_os(ENVIRONMENT)
            .map(PathBuf::from)
            .ok_or_else(|| conflict("Twikit runtime is not configured on this Peer"))?;
        let runtime = runtime(&runtime_path).await?;
        let current = core
            .extension_get(EXTENSION)
            .await
            .map_err(db_core)?
            .ok_or_else(|| conflict("Twitter extension is not configured"))?;
        if Login::from_config(&current.config)?.fingerprint()? != fingerprint {
            return Err(conflict("Twikit configuration changed"));
        }
        Ok(Self {
            core: core.clone(),
            login,
            fingerprint,
            runtime_path,
            runtime,
            bridge: Mutex::new(None),
            identity: Mutex::new(None),
        })
    }
    async fn guard(&self, config: &Value) -> Result<()> {
        if Login::from_config(config)?.fingerprint()? != self.fingerprint
            || runtime(&self.runtime_path).await? != self.runtime
        {
            return Err(conflict("Twikit configuration changed during capture"));
        }
        Ok(())
    }
    pub async fn guard_in(&self, connection: &mut sqlx::PgConnection) -> Result<(Value, Value)> {
        let (config, state): (Value, Value) =
            sqlx::query_as("SELECT config,state FROM inkcre.extensions WHERE name=$1 FOR SHARE")
                .bind(EXTENSION)
                .fetch_one(connection)
                .await
                .map_err(db)?;
        self.guard(&config).await?;
        Ok((config, state))
    }
    pub async fn bookmarks(&self, page: Option<&str>, limit: usize) -> Result<Page> {
        if !(1..=100).contains(&limit) || page.is_some_and(|p| p.len() > 16384) {
            return Err(invalid("Invalid Twikit bookmark page request"));
        }
        let current = self
            .core
            .extension_get(EXTENSION)
            .await
            .map_err(db_core)?
            .ok_or_else(|| conflict("Twitter extension is not configured"))?;
        self.guard(&current.config).await?;
        let mut bridge = self.bridge.lock().await;
        if bridge.is_none() {
            *bridge = Some(Bridge::start(&self.runtime, &self.login).await?);
        }
        let response = bridge
            .as_mut()
            .expect("initialized bridge")
            .request("bookmarks", json!({"count":limit,"cursor":page}))
            .await;
        let response = match response {
            Ok(value) => value,
            Err(error) => {
                if let Some(mut child) = bridge.take() {
                    child.abort().await;
                }
                return Err(error);
            }
        };
        let raw: Capture = serde_json::from_value(response)
            .map_err(|_| provider("Twikit returned an invalid capture"))?;
        native("users", &json!({"id":raw.user_id}))?;
        let mut identity = self.identity.lock().await;
        let account = (raw.user_id.clone(), raw.user_handle.clone());
        if identity.as_ref().is_some_and(|known| known != &account) {
            return Err(conflict("Twikit authenticated account changed"));
        }
        *identity = Some(account);
        let primary = raw.primary.clone();
        let next = raw.next.clone().filter(|s| !s.is_empty());
        let batch = convert(raw)?;
        self.guard(
            &self
                .core
                .extension_get(EXTENSION)
                .await
                .map_err(db_core)?
                .ok_or_else(|| conflict("Twitter extension is not configured"))?
                .config,
        )
        .await?;
        Ok(Page {
            batch,
            primary,
            next,
        })
    }
    pub async fn close(&self) -> Result<()> {
        let Some(mut bridge) = self.bridge.lock().await.take() else {
            return Ok(());
        };
        let result = bridge.request("close", json!({})).await;
        match tokio::time::timeout(Duration::from_secs(5), bridge.child.wait()).await {
            Ok(Ok(status)) if status.success() => (),
            _ => {
                bridge.abort().await;
            }
        }
        result.map(|_| ())
    }
}
fn db_core(_: inkcre_core::Error) -> inkcre_extension_sdk::RpcError {
    provider("Twikit shared configuration could not be read")
}
struct Bridge {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next_id: u64,
}
impl Bridge {
    async fn start(runtime: &Runtime, login: &Login) -> Result<Self> {
        let mut child = Command::new(&runtime.python_executable)
            .args(["-I", "-B", "-u", "-c", ADAPTER])
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| conflict("Twikit Python runtime cannot be started"))?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| provider("Twikit stdin unavailable"))?;
        let output = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| provider("Twikit stdout unavailable"))?,
        );
        let mut bridge = Self {
            child,
            input,
            output,
            next_id: 0,
        };
        let initialized: Result<()> = async {
            let hello = bridge
                .request("hello", json!({}))
                .await
                .map_err(|_| conflict("Twikit 2.3.3 runtime handshake failed"))?;
            if hello["ready"] != true || hello["twikit_version"] != "2.3.3" {
                return Err(conflict("Twikit 2.3.3 runtime is required"));
            }
            let ready = bridge
                .request(
                    "initialize",
                    json!({"config":login,"cookies_file":runtime.cookies_file}),
                )
                .await?;
            if ready["ready"] != true || ready["twikit_version"] != "2.3.3" {
                return Err(conflict("Twikit 2.3.3 runtime is required"));
            }
            Ok(())
        }
        .await;
        if let Err(error) = initialized {
            bridge.abort().await;
            return Err(error);
        }
        Ok(bridge)
    }
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let mut request =
            serde_json::to_vec(&json!({"version":1,"id":id,"method":method,"params":params}))
                .map_err(|_| provider("Twikit request cannot be encoded"))?;
        request.push(b'\n');
        if request.len() > MAX_REQUEST {
            return Err(invalid("Twikit request exceeds its limit"));
        }
        let response = tokio::time::timeout(Duration::from_secs(120), async {
            self.input
                .write_all(&request)
                .await
                .map_err(|_| provider("Twikit transport write failed"))?;
            self.input
                .flush()
                .await
                .map_err(|_| provider("Twikit transport write failed"))?;
            let frame = read_frame_with_limit(&mut self.output, MAX_FRAME)
                .await
                .map_err(|_| provider("Twikit transport frame invalid"))?
                .ok_or_else(|| provider("Twikit transport closed"))?;
            serde_json::from_slice::<Value>(&frame)
                .map_err(|_| provider("Twikit transport response invalid"))
        })
        .await
        .map_err(|_| provider("Twikit operation timed out"))??;
        if response["version"] != 1
            || response["id"] != id
            || response.get("result").is_some() == response.get("error").is_some()
        {
            return Err(provider("Twikit transport correlation mismatch"));
        }
        if let Some(error) = response.get("error") {
            return Err(match error["code"].as_str() {
                Some("runtime_unavailable") => {
                    conflict("Twikit 2.3.3 runtime or private cookie file is unavailable")
                }
                Some("authentication_required") => {
                    conflict("Twikit account requires operator authentication")
                }
                Some("challenge_required") => {
                    conflict("Twikit platform challenge requires operator action")
                }
                Some("rate_limited") => provider("Twikit bookmark request was rate limited"),
                _ => provider("Twikit bookmark provider failed"),
            });
        }
        Ok(response["result"].clone())
    }
    async fn abort(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Capture {
    user_id: String,
    user_handle: String,
    primary: Vec<String>,
    next: Option<String>,
    tweets: Vec<Value>,
}
fn convert(capture: Capture) -> Result<Batch> {
    if capture.primary.len() > 1000 || capture.tweets.len() > 4096 {
        return Err(provider("Twikit capture exceeds its limit"));
    }
    let mut raw = BTreeMap::new();
    for tweet in capture.tweets {
        let id = tweet["id"]
            .as_str()
            .ok_or_else(|| provider("Twikit post identity missing"))?
            .to_owned();
        if raw.insert(id, tweet).is_some() {
            return Err(provider("Twikit duplicate captured identity"));
        }
    }
    let mut collection = Collection {
        raw,
        posts: Vec::new(),
        users: Vec::new(),
        media: Vec::new(),
        seen: HashSet::new(),
        observed: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    };
    for id in capture.primary {
        collection.collect(&id, 0)?;
    }
    Batch::parse(
        json!({"users":collection.users,"posts":collection.posts,"media":collection.media}),
    )
}
struct Collection {
    raw: BTreeMap<String, Value>,
    posts: Vec<Value>,
    users: Vec<Value>,
    media: Vec<Value>,
    seen: HashSet<String>,
    observed: String,
}
fn upsert(items: &mut Vec<Value>, item: Value) {
    if let Some(existing) = items.iter_mut().find(|v| v["id"] == item["id"]) {
        *existing = item;
    } else {
        items.push(item);
    }
}
impl Collection {
    fn collect(&mut self, id: &str, depth: usize) -> Result<()> {
        if self.seen.contains(id) {
            return Ok(());
        }
        if depth > 128 {
            return Err(provider("Twikit reference depth exceeds its limit"));
        }
        self.seen.insert(id.into());
        let raw = self
            .raw
            .get(id)
            .cloned()
            .ok_or_else(|| provider("Twikit referenced post snapshot missing"))?;
        let user = &raw["user"];
        let timestamp = raw["created_at"]
            .as_str()
            .ok_or_else(|| provider("Twikit post timestamp missing"))?;
        let timestamp = chrono::DateTime::parse_from_rfc3339(timestamp)
            .map_err(|_| provider("Twikit post timestamp invalid"))?
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let mut post = json!({"id":id,"authorId":user["id"],"createdAt":timestamp});
        let index = self.posts.len();
        self.posts.push(post.clone());
        upsert(
            &mut self.users,
            json!({"id":user["id"],"handle":user["screen_name"],"name":user["name"]}),
        );
        if let Some(repost) = raw["retweeted_id"].as_str() {
            post["repostOf"] = json!(repost);
            self.posts[index] = post;
            self.collect(repost, depth + 1)?;
            return Ok(());
        }
        let text = raw["full_text"]
            .as_str()
            .ok_or_else(|| provider("Twikit full text missing"))?;
        post["text"] = json!({"text":html_escape::decode_html_entities(text)});
        if let Some(reply) = raw["in_reply_to"].as_str().filter(|s| !s.is_empty()) {
            post["replyTo"] = json!({"postId":reply});
        }
        if let Some(conversation) = raw["legacy"]["conversation_id_str"]
            .as_str()
            .filter(|s| *s != id && !s.is_empty())
        {
            post["conversationId"] = json!(conversation);
        }
        if let Some(quote) = raw["quote_id"].as_str() {
            post["quote"] = json!({"postId":quote});
            self.collect(quote, depth + 1)?;
        }
        if let Some(lang) = raw["lang"].as_str().filter(|s| !s.is_empty()) {
            post["lang"] = json!(lang);
        }
        let mut counts = json!({});
        let mut observed = json!({});
        for (source, target) in [
            ("reply_count", "replies"),
            ("retweet_count", "reposts"),
            ("quote_count", "quotes"),
            ("favorite_count", "likes"),
            ("bookmark_count", "bookmarks"),
        ] {
            if let Some(value) = raw["legacy"].get(source).filter(|v| !v.is_null()) {
                counts[target] = value.clone();
                observed[target] = json!(self.observed);
            }
        }
        if counts.as_object().is_some_and(|v| !v.is_empty()) {
            post["counts"] = counts;
            post["countsObservedAt"] = observed;
        }
        let mut attached = Vec::new();
        for item in raw["media"].as_array().into_iter().flatten() {
            let kind = match item["type"].as_str() {
                Some("photo") => "photo",
                Some("video") => "video",
                Some("animated_gif") => "gif",
                _ => continue,
            };
            let Some(media_id) = item["id"].as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            let Some(url) = item["media_url"].as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            let mut media = json!({"id":media_id,"type":kind,"url":url});
            for key in ["width", "height"] {
                if let Some(value) = item["data"]["original_info"]
                    .get(key)
                    .filter(|v| !v.is_null())
                {
                    media[key] = value.clone();
                }
            }
            if let Some(alt) = item["data"].get("ext_alt_text").filter(|v| !v.is_null()) {
                media["alt"] = alt.clone();
            }
            let info = &item["data"]["video_info"];
            if let Some(variants) = info["variants"].as_array().filter(|v| !v.is_empty()) {
                media["video"] = json!({"variants":variants.iter().map(|v|{let mut variant=json!({"url":v["url"],"type":v["content_type"]});if let Some(bitrate)=v.get("bitrate"){variant["bitrate"]=bitrate.clone();}variant}).collect::<Vec<_>>()});
                if let Some(duration) = info.get("duration_millis").filter(|v| !v.is_null()) {
                    media["video"]["durationMs"] = duration.clone();
                }
            }
            upsert(&mut self.media, media);
            attached.push(media_id.to_owned());
        }
        if !attached.is_empty() {
            post["mediaIds"] = json!(attached);
        }
        self.posts[index] = post;
        Ok(())
    }
}
