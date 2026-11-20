use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Caller-local destinations and credentials. This type is deliberately not serializable.
#[derive(Clone, Default)]
pub struct LocalOptions {
    pub loki_url: Option<String>,
    pub loki_headers: BTreeMap<String, String>,
    pub allow_external_content: bool,
    pub telemetry_enabled: bool,
    pub otlp: BTreeMap<String, OtlpConnection>,
}
#[derive(Clone)]
pub struct OtlpConnection {
    pub endpoint: String,
    pub headers: BTreeMap<String, String>,
    pub timeout_seconds: f64,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Deployment {
    pub deployment_id: Uuid,
    pub otlp_http_endpoints: Option<Endpoints>,
    pub diagnostics_url: Option<String>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoints {
    pub traces: Option<String>,
    pub logs: Option<String>,
    pub metrics: Option<String>,
}
impl Deployment {
    pub fn validate(&self) -> Result<()> {
        if let Some(url) = &self.diagnostics_url {
            super::loki::validate_url(url)?
        }
        if let Some(endpoints) = &self.otlp_http_endpoints {
            for endpoint in [&endpoints.traces, &endpoints.logs, &endpoints.metrics]
                .into_iter()
                .flatten()
            {
                super::loki::validate_url(endpoint)?
            }
        }
        Ok(())
    }
    pub fn endpoint(&self, signal: &str) -> Option<&str> {
        let endpoints = self.otlp_http_endpoints.as_ref()?;
        match signal {
            "traces" => endpoints.traces.as_deref(),
            "logs" => endpoints.logs.as_deref(),
            "metrics" => endpoints.metrics.as_deref(),
            _ => None,
        }
    }
}
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Selection {
    backend: String,
}
impl Default for Selection {
    fn default() -> Self {
        Self {
            backend: "postgresql".into(),
        }
    }
}
#[derive(Clone)]
pub(super) enum Backend {
    Postgresql,
    Loki(Arc<super::loki::Loki>),
}
impl Backend {
    pub(super) fn close(&self) {
        if let Self::Loki(loki) = self {
            loki.close();
        }
    }
    pub(super) async fn write(&self, core: &Core, records: Vec<Record>) -> Result<()> {
        match self {
            Self::Postgresql => super::postgresql::write(core, records).await,
            Self::Loki(loki) => loki.write(records).await,
        }
    }
    pub(super) async fn query(&self, core: &Core, query: Query) -> Result<Page> {
        match self {
            Self::Postgresql => super::postgresql::query(core, query).await,
            Self::Loki(loki) => loki.query(query).await,
        }
    }
}
pub(super) async fn configure(
    core: &Core,
    options: &LocalOptions,
) -> (String, Option<Backend>, Option<Deployment>, Option<String>) {
    let mut selected = "postgresql".to_string();
    let configuration = async {
        let row: Option<(String, Value)> = sqlx::query_as(
            "SELECT schema,value FROM inkcre.configs WHERE key='inkcre.observability.backend'",
        )
        .fetch_optional(core.pool())
        .await
        .map_err(|_| Error::Unavailable)?;
        if let Some((schema, value)) = row {
            if schema != "inkcre.observability.backend.v1" {
                return Err(Error::Unavailable);
            }
            let selection: Selection =
                serde_json::from_value(value).map_err(|_| Error::Unavailable)?;
            if !matches!(selection.backend.as_str(), "postgresql" | "loki") {
                return Err(Error::Unavailable);
            }
            selected = selection.backend;
        }
        let mut deployment = None;
        if selected == "loki" || options.telemetry_enabled {
            let row: Option<(String, Value)> = sqlx::query_as(
                "SELECT schema,value FROM inkcre.configs WHERE key='inkcre.observability'",
            )
            .fetch_optional(core.pool())
            .await
            .map_err(|_| Error::Unavailable)?;
            if let Some((schema, value)) = row {
                if schema != "inkcre.observability.v1" {
                    return Err(Error::Unavailable);
                }
                let parsed: Deployment =
                    serde_json::from_value(value).map_err(|_| Error::Unavailable)?;
                parsed.validate()?;
                deployment = Some(parsed);
            }
        }
        let backend = if selected == "postgresql" {
            Backend::Postgresql
        } else {
            let deployment = deployment.as_ref().ok_or(Error::Unavailable)?;
            Backend::Loki(Arc::new(super::loki::Loki::new(
                options.loki_url.clone().ok_or(Error::Unavailable)?,
                deployment.deployment_id,
                &options.loki_headers,
            )?))
        };
        Ok::<_, Error>((backend, deployment))
    };
    match timeout(Duration::from_secs(5), configuration).await {
        Ok(Ok((backend, deployment))) => (selected, Some(backend), deployment, None),
        _ => (
            selected,
            None,
            None,
            Some("Observability configuration unavailable or incomplete".into()),
        ),
    }
}
