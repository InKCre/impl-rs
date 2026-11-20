//! Acquisition from one deployment-selected Registry origin.

use std::{path::PathBuf, time::Duration};

use semver::{Version, VersionReq};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use url::Url;
use uuid::Uuid;

use super::*;
use crate::deployment_config::{self, ConfigRegistry, SchemaDescriptor};

pub const REGISTRY_CONFIG_KEY: &str = "extension.registry";
pub const REGISTRY_CONFIG_SCHEMA: &str = "extension.registry.config.v1";
const DEFAULT_ORIGIN: &str = "https://registry.inkcre.dev";
const TARGET: &str = env!("INKCRE_BUILD_TARGET");
pub(super) const ORIGIN_MARKER: &str = "registry-origin";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryConfig {
    extension_registry_url: Option<String>,
}

pub fn register_registry_config(registry: &mut ConfigRegistry) -> deployment_config::Result<()> {
    registry.register(
        SchemaDescriptor {
            id: REGISTRY_CONFIG_SCHEMA.into(),
            keys: vec![REGISTRY_CONFIG_KEY.into()],
            description: "Default Extension Registry origin.".into(),
            input_schema: Some(json!({"type":"object","additionalProperties":false,"properties":{
                "extension_registry_url":{"type":["string","null"],"default":null}
            }})),
        },
        |value| {
            let config: RegistryConfig = serde_json::from_value(value)
                .map_err(|_| deployment_config::Error::Invalid("invalid Registry configuration"))?;
            let origin = config.extension_registry_url.as_deref().map(str::trim)
                .filter(|value| !value.is_empty()).map(normalize_origin).transpose()
                .map_err(|_| deployment_config::Error::Invalid("invalid Registry origin"))?;
            Ok(json!({"extension_registry_url":origin.map(|url| url.origin().ascii_serialization())}))
        },
    )
}

fn normalize_origin(value: &str) -> Result<Url> {
    let url = Url::parse(value.trim())
        .map_err(|_| Error::Invalid("Registry address must be an HTTP(S) origin".into()))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Invalid(
            "Registry address must be an HTTP(S) origin".into(),
        ));
    }
    Ok(url)
}

#[derive(Deserialize)]
struct Release {
    name: String,
    version: Version,
    state: String,
    distribution: String,
    rust_stdio: RustRelease,
}

#[derive(Deserialize)]
struct RustRelease {
    host_sdk: String,
    host_sdk_version: VersionReq,
    protocol_version: u32,
    artifacts: Vec<Artifact>,
}

#[derive(Deserialize)]
struct Artifact {
    target: String,
    executable_url: String,
    sha256: String,
    size: u64,
}

struct DownloadDirectory(PathBuf);

impl Drop for DownloadDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl ExtensionHost {
    async fn registry_origin(&self) -> Result<Url> {
        let peer = self
            .core
            .peer_get(self.peer_id)
            .await?
            .ok_or(crate::Error::NotFound("peer"))?;
        if let Some(value) = peer
            .config
            .get("extension_registry_url")
            .filter(|v| !v.is_null())
        {
            return normalize_origin(
                value
                    .as_str()
                    .ok_or_else(|| Error::Invalid("invalid Peer Registry origin".into()))?,
            );
        }
        let row: Option<(String, Value)> =
            sqlx::query_as("SELECT schema,value FROM inkcre.configs WHERE key=$1")
                .bind(REGISTRY_CONFIG_KEY)
                .fetch_optional(self.core.pool())
                .await
                .map_err(crate::Error::from)?;
        if let Some((schema, value)) = row {
            if schema != REGISTRY_CONFIG_SCHEMA {
                return Err(Error::Invalid("unrecognized Registry config schema".into()));
            }
            let config: RegistryConfig = serde_json::from_value(value)?;
            if let Some(origin) = config
                .extension_registry_url
                .filter(|url| !url.trim().is_empty())
            {
                return normalize_origin(&origin);
            }
        }
        normalize_origin(DEFAULT_ORIGIN)
    }

    async fn resolve_registry(
        &self,
        name: &str,
        version: &str,
        allow_yanked: bool,
    ) -> Result<(reqwest::Client, Manifest, Url, u64)> {
        self.installed_dir(name, version)?;
        let (namespace, extension) = name.split_once('/').ok_or(Error::NotFound)?;
        let parsed_version = Version::parse(version)
            .map_err(|_| Error::Invalid("invalid extension version".into()))?;
        let origin = self.registry_origin().await?;
        let release_url = origin
            .join(&format!(
                "v2/extensions/{namespace}/{extension}/distributions/rust_stdio/releases/{version}"
            ))
            .map_err(|_| Error::Invalid("invalid Registry release coordinate".into()))?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::Runtime("unable to create Registry client".into()))?;
        let mut response = registry_get(&client, release_url, Duration::from_secs(15)).await?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(registry_error)? {
            if chunk.len() as u64 > MAX_MANIFEST_BYTES.saturating_sub(bytes.len() as u64) {
                return Err(Error::Invalid(
                    "Registry release descriptor exceeds size limit".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let release: Release = serde_json::from_slice(&bytes)?;
        if release.name != name
            || release.version != parsed_version
            || release.distribution != DISTRIBUTION
            || !(release.state == "published" || allow_yanked && release.state == "yanked")
        {
            return Err(Error::Conflict(
                "Registry did not return the published exact coordinate".into(),
            ));
        }
        let release = release.rust_stdio;
        let mut matching = release
            .artifacts
            .into_iter()
            .filter(|entry| entry.target == TARGET);
        let artifact = matching.next().ok_or(Error::NotFound)?;
        if matching.next().is_some() || artifact.size == 0 || artifact.size > MAX_EXECUTABLE_BYTES {
            return Err(Error::Invalid(
                "Registry artifact target or size is invalid".into(),
            ));
        }
        let manifest = Manifest {
            schema_version: 1,
            name: name.into(),
            version: parsed_version,
            distribution: DISTRIBUTION.into(),
            host_sdk: release.host_sdk,
            host_sdk_version: release.host_sdk_version,
            protocol_version: release.protocol_version,
            executable: EXECUTABLE_FILENAME.into(),
            sha256: artifact.sha256,
        };
        manifest.validate()?;
        let binary_url = origin
            .join(&artifact.executable_url)
            .map_err(|_| Error::Invalid("invalid Registry artifact URL".into()))?;
        if binary_url.origin() != origin.origin()
            || !binary_url.username().is_empty()
            || binary_url.password().is_some()
            || binary_url.fragment().is_some()
        {
            return Err(Error::Invalid(
                "Registry artifact must use the configured origin".into(),
            ));
        }
        Ok((client, manifest, binary_url, artifact.size))
    }

    pub(super) async fn verify_registry_admission(&self, manifest: &Manifest) -> Result<()> {
        let (_, expected, _, _) = self
            .resolve_registry(&manifest.name, &manifest.version.to_string(), true)
            .await?;
        if expected != *manifest {
            return Err(Error::Conflict(
                "installed artifact differs from exact Registry Release".into(),
            ));
        }
        Ok(())
    }

    pub async fn install_registry(&self, name: &str, version: &str) -> Result<ExtensionStatus> {
        let (client, manifest, binary_url, expected_size) =
            self.resolve_registry(name, version, false).await?;
        tokio::fs::create_dir_all(&self.manifest_dir).await?;
        let directory = self
            .manifest_dir
            .join(format!(".download-{}", Uuid::new_v4()));
        tokio::fs::create_dir(&directory).await?;
        let directory = DownloadDirectory(directory);
        let mut binary = registry_get(&client, binary_url, Duration::from_secs(120)).await?;
        if binary
            .content_length()
            .is_some_and(|size| size != expected_size)
        {
            return Err(Error::Invalid(
                "Registry artifact length differs from descriptor".into(),
            ));
        }
        let binary_path = directory.0.join(EXECUTABLE_FILENAME);
        let mut file = tokio::fs::File::create_new(&binary_path).await?;
        let mut digest = Sha256::new();
        let mut size = 0u64;
        while let Some(chunk) = binary.chunk().await.map_err(registry_error)? {
            size += chunk.len() as u64;
            if size > expected_size {
                return Err(Error::Invalid(
                    "Registry artifact exceeded declared size".into(),
                ));
            }
            digest.update(&chunk);
            file.write_all(&chunk).await?;
        }
        if size != expected_size || format!("{:x}", digest.finalize()) != manifest.sha256 {
            return Err(Error::Invalid(
                "Registry artifact length or digest mismatch".into(),
            ));
        }
        file.sync_all().await?;
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o700))
                .await?;
        }
        let path = directory.0.join(MANIFEST_FILENAME);
        tokio::fs::write(&path, serde_json::to_vec(&manifest)?).await?;
        self.install_from(path, name, version, true).await
    }
}

fn registry_error(_: reqwest::Error) -> Error {
    Error::Runtime("Registry request failed".into())
}

async fn registry_get(
    client: &reqwest::Client,
    url: Url,
    timeout: Duration,
) -> Result<reqwest::Response> {
    let response = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .map_err(registry_error)?;
    match response.status() {
        reqwest::StatusCode::OK => Ok(response),
        reqwest::StatusCode::NOT_FOUND => Err(Error::NotFound),
        reqwest::StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS => {
            Err(Error::Conflict("Registry Release is blocked".into()))
        }
        _ => Err(Error::Runtime("Registry response rejected".into())),
    }
}
