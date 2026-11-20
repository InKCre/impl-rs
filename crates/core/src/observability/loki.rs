use super::*;
use reqwest::{
    Client,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde::Deserialize;
use std::collections::BTreeMap;

pub(super) struct Loki {
    url: String,
    deployment: Uuid,
    client: SyncMutex<Option<Client>>,
    stop: watch::Sender<bool>,
}
pub(super) fn validate_url(value: &str) -> Result<()> {
    let url = url::Url::parse(value).map_err(|_| Error::Unavailable)?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Unavailable);
    }
    Ok(())
}
pub(super) fn headers(values: &BTreeMap<String, String>) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (key, value) in values {
        let name = HeaderName::from_bytes(key.as_bytes()).map_err(|_| Error::Unavailable)?;
        if matches!(
            name.as_str(),
            "host" | "content-length" | "connection" | "transfer-encoding"
        ) {
            return Err(Error::Unavailable);
        }
        if headers
            .insert(
                name,
                HeaderValue::from_str(value).map_err(|_| Error::Unavailable)?,
            )
            .is_some()
        {
            return Err(Error::Unavailable);
        }
    }
    Ok(headers)
}
impl Loki {
    pub(super) fn new(
        url: String,
        deployment: Uuid,
        private_headers: &BTreeMap<String, String>,
    ) -> Result<Self> {
        validate_url(&url)?;
        let client = Client::builder()
            .default_headers(headers(private_headers)?)
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::Unavailable)?;
        Ok(Self {
            url: url.trim_end_matches('/').into(),
            deployment,
            client: SyncMutex::new(Some(client)),
            stop: watch::channel(false).0,
        })
    }
    pub(super) async fn write(&self, records: Vec<Record>) -> Result<()> {
        self.until_closed(self.write_inner(records)).await
    }
    pub(super) async fn query(&self, query: Query) -> Result<Page> {
        self.until_closed(self.query_inner(query)).await
    }
    pub(super) fn close(&self) {
        self.stop.send_replace(true);
        self.client.lock().unwrap_or_else(|p| p.into_inner()).take();
    }
    fn client(&self) -> Result<Client> {
        self.client
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or(Error::Unavailable)
    }
    async fn until_closed<T>(
        &self,
        operation: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        let mut stopped = self.stop.subscribe();
        if *stopped.borrow() {
            return Err(Error::Unavailable);
        }
        tokio::select! {
            biased;
            _=stopped.changed()=>Err(Error::Unavailable),
            result=operation=>result,
        }
    }
    async fn write_inner(&self, records: Vec<Record>) -> Result<()> {
        let mut values = Vec::with_capacity(records.len());
        for record in records {
            let job = record
                .attributes
                .get("inkcre.job.id")
                .or_else(|| record.attributes.get("job_id"))
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string())
                })
                .unwrap_or_default();
            let timestamp = record
                .timestamp
                .timestamp_nanos_opt()
                .map(|value| value.to_string())
                .ok_or(Error::Invalid)?;
            let projected = json!({
                "kind":record.kind,
                "severity":record.severity_number,
                "trace":record.trace_id.as_deref().unwrap_or(""),
                "peer":record.attributes.get("inkcre.peer.id").cloned().unwrap_or(json!("")),
                "job":job,
                "operation":record.attributes.get("inkcre.operation").cloned().unwrap_or(json!("")),
                "body":record.body,
                "record":record,
            });
            values.push(json!([timestamp, projected.to_string()]));
        }
        let payload = json!({"streams":[{
            "stream":{"service_name":"inkcre","deployment_id":self.deployment.to_string()},
            "values":values,
        }]});
        let response = self
            .client()?
            .post(format!("{}/loki/api/v1/push", self.url))
            .json(&payload)
            .send()
            .await
            .map_err(|_| Error::Unavailable)?;
        if !matches!(response.status().as_u16(), 200 | 204) {
            return Err(Error::Unavailable);
        }
        Ok(())
    }
    async fn query_inner(&self, query: Query) -> Result<Page> {
        let mut selector = format!(
            "{{service_name=\"inkcre\",deployment_id={}}} | json",
            json!(self.deployment.to_string())
        );
        for (key, value) in [
            ("kind", query.kind.as_deref()),
            ("trace", query.trace_id.as_deref()),
            (
                "peer",
                query.peer_id.as_ref().map(Uuid::to_string).as_deref(),
            ),
            ("operation", query.operation.as_deref()),
        ] {
            if let Some(value) = value {
                selector.push_str(&format!(" | {key} = {}", json!(value)));
            }
        }
        if let Some(id) = query.job_id {
            selector.push_str(&format!(
                " | job = {} or trace = {}",
                json!(id.to_string()),
                json!(format!("job.{id}"))
            ));
        }
        if let Some(severity) = query.severity_min {
            selector.push_str(&format!(" | severity >= {severity}"));
        }
        if let Some(text) = query.text.as_ref().filter(|text| !text.is_empty()) {
            let mut escaped = String::new();
            for character in text.chars() {
                if ".^$*+?()[]{}\\|-".contains(character) {
                    escaped.push('\\')
                }
                escaped.push(character)
            }
            selector.push_str(&format!(
                " | body =~ {}",
                json!(format!("(?is).*{escaped}.*"))
            ));
        }
        let start = query.since.timestamp_nanos_opt().ok_or(Error::Invalid)?;
        let end = query
            .until
            .timestamp_nanos_opt()
            .and_then(|n| n.checked_sub(1))
            .ok_or(Error::Invalid)?;
        let response = self
            .client()?
            .get(format!("{}/loki/api/v1/query_range", self.url))
            .query(&[
                ("query", selector),
                ("start", start.to_string()),
                ("end", end.to_string()),
                ("direction", "backward".into()),
                ("limit", (query.limit + 1).to_string()),
            ])
            .send()
            .await
            .map_err(|_| Error::Unavailable)?;
        if response.status().as_u16() != 200 {
            return Err(Error::Unavailable);
        }
        let bytes = bounded_response(response, 8 * 1024 * 1024).await?;
        let parsed: LokiResponse =
            serde_json::from_slice(&bytes).map_err(|_| Error::Unavailable)?;
        if parsed.status != "success" || parsed.data.result_type != "streams" {
            return Err(Error::Unavailable);
        }
        let mut records = Vec::new();
        for stream in parsed.data.result {
            for (_, line) in stream.values {
                let value: Value = serde_json::from_str(&line).map_err(|_| Error::Unavailable)?;
                let record: Record =
                    serde_json::from_value(value.get("record").cloned().ok_or(Error::Unavailable)?)
                        .map_err(|_| Error::Unavailable)?;
                record.validate().map_err(|_| Error::Unavailable)?;
                records.push(record);
            }
        }
        records.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then_with(|| b.id.cmp(&a.id)));
        let truncated = records.len() as i64 > query.limit;
        records.truncate(query.limit as usize);
        Ok(Page {
            records,
            truncated,
            since: query.since,
            until: query.until,
            backend: "loki".into(),
        })
    }
}
#[derive(Deserialize)]
struct LokiResponse {
    status: String,
    data: LokiData,
}
#[derive(Deserialize)]
struct LokiData {
    #[serde(rename = "resultType")]
    result_type: String,
    result: Vec<LokiStream>,
}
#[derive(Deserialize)]
struct LokiStream {
    values: Vec<(String, String)>,
}
pub(super) async fn bounded_response(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Unavailable)? {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(Error::Unavailable);
        }
        bytes.extend(chunk);
    }
    Ok(bytes)
}
