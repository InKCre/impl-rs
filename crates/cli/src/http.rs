use crate::{connection::Connection, schema};
use reqwest::{Client as HttpClient, Method, Url};
use serde_json::{Value, json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
pub struct Failure {
    pub code: i32,
    pub http_status: Option<u16>,
    pub detail: Value,
}
impl Failure {
    pub fn as_value(&self) -> Value {
        let mut result = json!({"detail":self.detail});
        if let Some(status) = self.http_status {
            result["http_status"] = json!(status);
        }
        result
    }
    pub fn usage(message: &str) -> Self {
        Self {
            http_status: None,
            code: 2,
            detail: json!(message),
        }
    }
}

#[derive(Default)]
pub struct InputError {
    pub strip: &'static [&'static str],
    pub selectors: &'static [(&'static str, &'static str)],
}

impl InputError {
    pub fn selectors(selectors: &'static [(&'static str, &'static str)]) -> Self {
        Self {
            selectors,
            ..Self::default()
        }
    }

    fn project(&self, detail: &mut Value) {
        if let Some(items) = detail.as_array_mut() {
            for item in items {
                let Some(loc) = item.get_mut("loc").and_then(Value::as_array_mut) else {
                    continue;
                };
                if loc.first().is_none_or(|value| value != "body") {
                    continue;
                }
                loc.remove(0);
                if let Some(prefix) = loc.get(..self.strip.len())
                    && prefix
                        .iter()
                        .zip(self.strip)
                        .all(|(value, field)| value == field)
                {
                    loc.drain(..self.strip.len());
                }
                if let Some(field) = loc.first_mut()
                    && let Some((_, option)) = self.selectors.iter().find(|(name, _)| field == name)
                {
                    *field = json!(option);
                }
            }
        }
    }
}

pub struct Client {
    http: HttpClient,
    base: String,
    secret: String,
}
impl Client {
    pub fn new(connection: &Connection) -> Result<Self, Failure> {
        let url = Url::parse(&connection.base_url).map_err(|_| Failure::usage("无效连接URL"))?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.has_host()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.query().is_some()
            || connection.jwt_secret.is_empty()
        {
            return Err(Failure::usage(
                "需要不含凭据的 HTTP(S) 地址及非空 jwt_secret",
            ));
        }
        let http = HttpClient::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Failure::usage("无法创建HTTP客户端"))?;
        Ok(Self {
            http,
            base: connection.base_url.trim_end_matches('/').into(),
            secret: connection.jwt_secret.clone(),
        })
    }
    pub async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        query: &[(&str, Option<String>)],
    ) -> Result<Value, Failure> {
        self.send_projected(method, path, body, query, InputError::default())
            .await
    }

    pub async fn send_projected(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        query: &[(&str, Option<String>)],
        input: InputError,
    ) -> Result<Value, Failure> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Failure::usage("系统时间无效"))?
            .as_secs();
        let token=jsonwebtoken::encode(&jsonwebtoken::Header::default(),&json!({"role":"authenticated","iss":"inkcre-peer","aud":"inkcre-api","iat":now.saturating_sub(5),"exp":now+895}),&jsonwebtoken::EncodingKey::from_secret(self.secret.as_bytes())).map_err(|_|Failure::usage("无法签发token"))?;
        let query: Vec<_> = query
            .iter()
            .filter_map(|(k, v)| v.as_ref().map(|v| (*k, v)))
            .collect();
        let mut request = self
            .http
            .request(
                Method::from_bytes(method.as_bytes())
                    .map_err(|_| Failure::usage("无效HTTP方法"))?,
                format!("{}{path}", self.base),
            )
            .bearer_auth(token)
            .query(&query);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let mut response = request.send().await.map_err(|_| Failure {
            http_status: None,
            code: 1,
            detail: json!("HTTP请求失败，操作结果可能未知"),
        })?;
        let status = response.status();
        if status.as_u16() == 204 {
            return Ok(Value::Null);
        }
        let mime = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Failure {
            http_status: None,
            code: 1,
            detail: json!("HTTP响应读取失败"),
        })? {
            if chunk.len()
                > 32_usize
                    .saturating_mul(1024 * 1024)
                    .saturating_sub(bytes.len())
            {
                return Err(Failure {
                    http_status: None,
                    code: 1,
                    detail: json!("HTTP响应超过32MiB"),
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        if !status.is_success() {
            let payload = serde_json::from_slice::<Value>(&bytes)
                .unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes)));
            let mut detail = payload.get("detail").cloned().unwrap_or(payload);
            if status.as_u16() == 422 {
                input.project(&mut detail);
            }
            return Err(Failure {
                code: 1,
                http_status: Some(status.as_u16()),
                detail,
            });
        }
        if mime.starts_with("application/octet-stream") {
            return Ok(crate::output::binary(bytes));
        }
        if mime.starts_with("multipart/related") {
            return multipart(&mime, &bytes);
        }
        serde_json::from_slice(&bytes).map_err(|_| Failure {
            http_status: None,
            code: 1,
            detail: json!("无法解码响应JSON，二进制hydrated内容请使用HTTP内容接口"),
        })
    }
}

fn multipart(mime: &str, bytes: &[u8]) -> Result<Value, Failure> {
    let invalid = || Failure {
        http_status: None,
        code: 1,
        detail: json!("无效 multipart 内容响应"),
    };
    let boundary = mime
        .split(';')
        .find_map(|part| part.trim().strip_prefix("boundary="))
        .ok_or_else(invalid)?
        .trim_matches('"');
    if boundary.is_empty() {
        return Err(invalid());
    }
    let marker = format!("--{boundary}").into_bytes();
    let positions: Vec<_> = bytes
        .windows(marker.len())
        .enumerate()
        .filter_map(|(i, window)| (window == marker).then_some(i))
        .collect();
    let mut metadata = None;
    let mut blobs = std::collections::BTreeMap::new();
    for pair in positions.windows(2) {
        let part = &bytes[pair[0] + marker.len()..pair[1]];
        let part = part.strip_prefix(b"\r\n").ok_or_else(invalid)?;
        let split = part
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .ok_or_else(invalid)?;
        let headers = std::str::from_utf8(&part[..split]).map_err(|_| invalid())?;
        let data = part[split + 4..]
            .strip_suffix(b"\r\n")
            .ok_or_else(invalid)?;
        if metadata.is_none() {
            metadata = Some(serde_json::from_slice::<Value>(data).map_err(|_| invalid())?);
        } else {
            let id = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find_map(|(key, value)| {
                    key.eq_ignore_ascii_case("content-id")
                        .then_some(value.trim().trim_matches(['<', '>']))
                })
                .ok_or_else(invalid)?;
            if blobs.insert(id.to_owned(), data).is_some() {
                return Err(invalid());
            }
        }
    }
    let mut metadata = metadata.ok_or_else(invalid)?;
    let mapping = metadata["parts"].as_object().ok_or_else(invalid)?.clone();
    let mut value = metadata.get_mut("value").ok_or_else(invalid)?.take();
    for (pointer, id) in mapping {
        let id = id.as_str().ok_or_else(invalid)?.trim_start_matches("cid:");
        let data = blobs.get(id).ok_or_else(invalid)?;
        let destination = value.pointer_mut(&pointer).ok_or_else(invalid)?;
        *destination = crate::output::binary(data.to_vec());
    }
    Ok(value)
}

impl Client {
    pub async fn schema(&self, method: &str, path: &str) -> Result<Value, Failure> {
        self.schema_without(method, path, &[]).await
    }

    pub async fn schema_without(
        &self,
        method: &str,
        path: &str,
        omit: &[&str],
    ) -> Result<Value, Failure> {
        let document = self.send("GET", "/openapi.json", None, &[]).await?;
        let paths = document["paths"]
            .as_object()
            .ok_or_else(|| Failure::usage("服务端没有发布OpenAPI路径"))?;
        let actual: Vec<_> = path.split('/').collect();
        let entry = paths
            .iter()
            .filter(|(candidate, _)| {
                let segments: Vec<_> = candidate.split('/').collect();
                segments.len() == actual.len()
                    && segments
                        .iter()
                        .zip(&actual)
                        .all(|(a, b)| a == b || (a.starts_with('{') && a.ends_with('}')))
            })
            .max_by_key(|(candidate, _)| {
                candidate.split('/').filter(|s| !s.starts_with('{')).count()
            })
            .map(|(_, value)| value)
            .ok_or_else(|| Failure::usage("该路径未发布输入schema"))?;
        let mut schema=entry[method.to_ascii_lowercase()]["requestBody"]["content"]["application/json"]["schema"].clone();
        if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
            if !reference.starts_with("#/components/schemas/") {
                return Err(Failure::usage("不支持外部schema引用"));
            }
            schema = document
                .pointer(&reference[1..])
                .cloned()
                .ok_or_else(|| Failure::usage("schema引用不存在"))?;
        }
        if !schema.is_object() {
            return Err(Failure::usage("该操作没有JSON输入schema"));
        }
        schema::omit(&mut schema, omit);
        schema::include_definitions(&mut schema, &document)?;
        Ok(schema)
    }
}

impl Client {
    pub async fn ready(&self) -> Result<Value, Failure> {
        let response = self
            .http
            .get(format!("{}/readyz", self.base))
            .send()
            .await
            .map_err(|_| Failure {
                http_status: None,
                code: 1,
                detail: json!("就绪请求失败"),
            })?;
        let response = if response.status().as_u16() == 404 {
            self.http
                .get(format!("{}/health/ready", self.base))
                .send()
                .await
                .map_err(|_| Failure {
                    http_status: None,
                    code: 1,
                    detail: json!("就绪请求失败"),
                })?
        } else {
            response
        };
        let status = response.status();
        Ok(json!({"status":status.as_u16(),"ready":status.is_success()}))
    }
}
