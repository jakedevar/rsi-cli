//! Pioneer model-catalog and Codex custom-provider primitives.
//!
//! Phase 1 defined the credential, catalog, cache, and Codex-override
//! primitives here. Phase 2 composes those primitives into first-class model
//! discovery, health, and Codex launch paths without duplicating them.

use std::collections::HashSet;
use std::fmt;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use reqwest::{Client, StatusCode, Url};
use serde::Serialize;
use serde_json::Value;
use tempfile::{Builder as TempFileBuilder, NamedTempFile};
use thiserror::Error;
use tokio::process::Command;

pub const PIONEER_API_BASE_URL: &str = "https://api.pioneer.ai/v1";
pub const PIONEER_MODELS_URL: &str = "https://api.pioneer.ai/v1/models";
pub const PIONEER_PROVIDER_ID: &str = "pioneer";
pub const PIONEER_PROVIDER_NAME: &str = "Pioneer";
pub const PIONEER_DEFAULT_MODEL: &str = "claude-sonnet-5";
pub const PIONEER_RETIRED_AUTO_MODEL: &str = "pioneer/auto";
pub const PIONEER_PRIMARY_ENV: &str = "PIONEER_AI_INFERENCE";
pub const PIONEER_FALLBACK_ENV: &str = "PIONEER_API_KEY";
pub const PIONEER_CATALOG_CACHE_SUBDIR: &str = "model-catalogs";
pub const PIONEER_CATALOG_CACHE_FILENAME: &str = "pioneer.json";
pub const MAX_PIONEER_CATALOG_BYTES: usize = 1024 * 1024;
pub const MAX_PIONEER_MODEL_SLUG_BYTES: usize = 256;
pub const PIONEER_CATALOG_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const PIONEER_CATALOG_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[must_use]
pub fn pioneer_launch_model(model: Option<&str>) -> &str {
    match model {
        None | Some(PIONEER_RETIRED_AUTO_MODEL) => PIONEER_DEFAULT_MODEL,
        Some(model) => model,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PioneerCredentialSource {
    AiInference,
    ApiKey,
}

impl PioneerCredentialSource {
    #[must_use]
    pub const fn env_name(self) -> &'static str {
        match self {
            Self::AiInference => PIONEER_PRIMARY_ENV,
            Self::ApiKey => PIONEER_FALLBACK_ENV,
        }
    }
}

#[derive(Clone)]
pub struct PioneerCredential {
    source: PioneerCredentialSource,
    value: String,
}

impl PioneerCredential {
    #[must_use]
    pub const fn source(&self) -> PioneerCredentialSource {
        self.source
    }

    pub(crate) fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Debug for PioneerCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PioneerCredential")
            .field("source", &self.source)
            .field("value", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum PioneerError {
    #[error(
        "Pioneer credential is not configured; set {PIONEER_PRIMARY_ENV} or {PIONEER_FALLBACK_ENV}"
    )]
    MissingCredential,
    #[error("Pioneer catalog HTTP client could not be configured")]
    ClientConfiguration,
    #[error("Pioneer models URL is invalid")]
    InvalidModelsUrl,
    #[error("Pioneer catalog request timed out")]
    RequestTimeout,
    #[error("Pioneer catalog request could not connect")]
    RequestConnect,
    #[error("Pioneer catalog request failed")]
    RequestFailed,
    #[error("Pioneer catalog request returned HTTP {0}")]
    HttpStatus(StatusCode),
    #[error("Pioneer catalog exceeded the {limit}-byte limit")]
    CatalogTooLarge { limit: usize },
    #[error("Pioneer catalog response is malformed JSON at line {line}, column {column}")]
    MalformedJson { line: usize, column: usize },
    #[error("Pioneer catalog response must be a JSON object with a top-level models array")]
    MissingModelsEnvelope,
    #[error("Pioneer catalog response contained an empty top-level models array")]
    EmptyCatalog,
    #[error("RSI data directory must be absolute")]
    InvalidDataDirectory,
    #[error("Pioneer Codex model catalog path must be absolute, normalized UTF-8")]
    InvalidCodexCatalogPath,
    #[error("Pioneer catalog cache operation failed ({0:?})")]
    CacheIo(std::io::ErrorKind),
}

/// Resolves the Pioneer credential using the documented environment precedence.
///
/// # Errors
///
/// Returns [`PioneerError::MissingCredential`] when neither environment variable
/// contains a non-blank credential.
pub fn pioneer_credential_from_env() -> Result<PioneerCredential, PioneerError> {
    resolve_credential_with(|name| std::env::var(name).ok())
}

#[must_use]
pub fn pioneer_provider_available(codex_cli_available: bool) -> bool {
    pioneer_provider_available_with(codex_cli_available, |name| std::env::var(name).ok())
}

fn pioneer_provider_available_with(
    codex_cli_available: bool,
    read: impl FnMut(&str) -> Option<String>,
) -> bool {
    codex_cli_available && resolve_credential_with(read).is_ok()
}

/// Fetches the current Pioneer account catalog, atomically refreshes the RSI
/// cache, and returns the current picker entries.
///
/// # Errors
///
/// Returns a secret-safe credential, transport, catalog, or cache error.
pub async fn discover_pioneer_models() -> Result<Vec<(String, String)>, PioneerError> {
    let credential = pioneer_credential_from_env()?;
    let client = PioneerCatalogClient::new()?;
    discover_pioneer_models_with(&client, &credential, &rsi_common::identity::data_dir()).await
}

async fn discover_pioneer_models_with(
    client: &PioneerCatalogClient,
    credential: &PioneerCredential,
    data_dir: &Path,
) -> Result<Vec<(String, String)>, PioneerError> {
    let catalog = client.fetch(credential).await?;
    catalog.write_cache_atomic_under(data_dir)?;
    Ok(catalog
        .picker_models()
        .into_iter()
        .map(|model| (model.slug, model.display_name))
        .collect())
}

fn resolve_credential_with(
    mut read: impl FnMut(&str) -> Option<String>,
) -> Result<PioneerCredential, PioneerError> {
    for source in [
        PioneerCredentialSource::AiInference,
        PioneerCredentialSource::ApiKey,
    ] {
        if let Some(value) = read(source.env_name()) {
            let value = value.trim();
            if !value.is_empty() {
                return Ok(PioneerCredential {
                    source,
                    value: value.to_string(),
                });
            }
        }
    }
    Err(PioneerError::MissingCredential)
}

#[derive(Clone)]
pub struct PioneerCatalogClient {
    http: Client,
    models_url: Url,
    request_timeout: Duration,
}

impl PioneerCatalogClient {
    /// Builds the production Pioneer catalog client with bounded timeouts and no redirects.
    ///
    /// # Errors
    ///
    /// Returns [`PioneerError::ClientConfiguration`] if the HTTP client cannot be built.
    pub fn new() -> Result<Self, PioneerError> {
        let http = Client::builder()
            .connect_timeout(PIONEER_CATALOG_CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| PioneerError::ClientConfiguration)?;
        let models_url = Url::parse(PIONEER_MODELS_URL)
            .unwrap_or_else(|_| unreachable!("Pioneer models URL is a valid constant"));
        Ok(Self {
            http,
            models_url,
            request_timeout: PIONEER_CATALOG_REQUEST_TIMEOUT,
        })
    }

    #[cfg(test)]
    fn with_endpoint(
        http: Client,
        models_url: &str,
        request_timeout: Duration,
    ) -> Result<Self, PioneerError> {
        let models_url = Url::parse(models_url).map_err(|_| PioneerError::InvalidModelsUrl)?;
        Ok(Self {
            http,
            models_url,
            request_timeout,
        })
    }

    /// Fetches and validates the authenticated Pioneer model catalog.
    ///
    /// # Errors
    ///
    /// Returns a secret-safe [`PioneerError`] for transport, HTTP, size, or catalog
    /// validation failures.
    pub async fn fetch(
        &self,
        credential: &PioneerCredential,
    ) -> Result<PioneerCatalog, PioneerError> {
        let response = self
            .http
            .get(self.models_url.clone())
            .timeout(self.request_timeout)
            .bearer_auth(credential.value())
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|error| classify_request_error(&error))?;

        if !response.status().is_success() {
            return Err(PioneerError::HttpStatus(response.status()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_PIONEER_CATALOG_BYTES as u64)
        {
            return Err(PioneerError::CatalogTooLarge {
                limit: MAX_PIONEER_CATALOG_BYTES,
            });
        }

        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| classify_request_error(&error))?;
            if bytes.len().saturating_add(chunk.len()) > MAX_PIONEER_CATALOG_BYTES {
                return Err(PioneerError::CatalogTooLarge {
                    limit: MAX_PIONEER_CATALOG_BYTES,
                });
            }
            bytes.extend_from_slice(&chunk);
        }

        PioneerCatalog::parse(&bytes)
    }
}

fn classify_request_error(error: &reqwest::Error) -> PioneerError {
    if error.is_timeout() {
        PioneerError::RequestTimeout
    } else if error.is_connect() {
        PioneerError::RequestConnect
    } else {
        PioneerError::RequestFailed
    }
}

#[derive(Clone, Debug)]
pub struct PioneerCatalog {
    models: Vec<Value>,
}

#[derive(Serialize)]
struct PioneerCatalogEnvelope<'a> {
    models: &'a [Value],
}

impl PioneerCatalog {
    /// Parses an exact top-level Pioneer `models` envelope within the size bound.
    ///
    /// # Errors
    ///
    /// Returns a catalog size, JSON syntax, envelope, or empty-catalog error when
    /// the response is not usable.
    pub fn parse(bytes: &[u8]) -> Result<Self, PioneerError> {
        if bytes.len() > MAX_PIONEER_CATALOG_BYTES {
            return Err(PioneerError::CatalogTooLarge {
                limit: MAX_PIONEER_CATALOG_BYTES,
            });
        }

        let document: Value =
            serde_json::from_slice(bytes).map_err(|error| PioneerError::MalformedJson {
                line: error.line(),
                column: error.column(),
            })?;
        let models = document
            .as_object()
            .and_then(|object| object.get("models"))
            .and_then(Value::as_array)
            .cloned()
            .ok_or(PioneerError::MissingModelsEnvelope)?;
        if models.is_empty() {
            return Err(PioneerError::EmptyCatalog);
        }

        Ok(Self { models })
    }

    #[must_use]
    pub fn raw_models(&self) -> &[Value] {
        &self.models
    }

    #[must_use]
    pub fn picker_models(&self) -> Vec<PioneerModel> {
        let mut candidates = self
            .models
            .iter()
            .enumerate()
            .filter_map(|(source_index, raw)| {
                let object = raw.as_object()?;
                let slug = valid_model_slug(object.get("slug")?)?;
                let visible = match object.get("visibility").and_then(Value::as_str) {
                    None | Some("list") => true,
                    Some(_) => false,
                };
                if !visible
                    || object.get("supported_in_api").and_then(Value::as_bool) == Some(false)
                {
                    return None;
                }

                let display_name = object
                    .get("display_name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(slug)
                    .to_string();
                Some((
                    object
                        .get("priority")
                        .and_then(Value::as_i64)
                        .unwrap_or(i64::MAX),
                    source_index,
                    PioneerModel {
                        slug: slug.to_string(),
                        display_name,
                    },
                ))
            })
            .collect::<Vec<_>>();

        candidates.sort_by_key(|(priority, source_index, _)| (*priority, *source_index));
        let mut seen = HashSet::new();
        candidates
            .into_iter()
            .filter_map(|(_, _, model)| seen.insert(model.slug.clone()).then_some(model))
            .collect()
    }

    /// Atomically writes the exact raw catalog envelope under the RSI data directory.
    ///
    /// # Errors
    ///
    /// Returns a size, path, serialization, or filesystem error if the bounded
    /// atomic replacement cannot complete.
    pub fn write_cache_atomic(&self) -> Result<PathBuf, PioneerError> {
        self.write_cache_atomic_under(&rsi_common::identity::data_dir())
    }

    fn write_cache_atomic_under(&self, data_dir: &Path) -> Result<PathBuf, PioneerError> {
        self.write_cache_atomic_under_with(data_dir, |temporary, path| {
            temporary
                .persist(path)
                .map(|_| ())
                .map_err(|error| error.error)
        })
    }

    fn write_cache_atomic_under_with(
        &self,
        data_dir: &Path,
        replace: impl FnOnce(NamedTempFile, &Path) -> std::io::Result<()>,
    ) -> Result<PathBuf, PioneerError> {
        if !data_dir.is_absolute() {
            return Err(PioneerError::InvalidDataDirectory);
        }

        let path = pioneer_catalog_cache_path_under(data_dir);
        let encoded = encode_cache(&self.models)?;
        let parent = path
            .parent()
            .unwrap_or_else(|| unreachable!("fixed catalog path has a parent"));
        std::fs::create_dir_all(parent).map_err(|error| cache_io(error.kind()))?;
        let mut temporary = TempFileBuilder::new()
            .prefix(".pioneer-catalog-")
            .tempfile_in(parent)
            .map_err(|error| cache_io(error.kind()))?;
        temporary
            .write_all(&encoded)
            .map_err(|error| cache_io(error.kind()))?;
        temporary
            .as_file_mut()
            .sync_all()
            .map_err(|error| cache_io(error.kind()))?;
        replace(temporary, &path).map_err(|error| cache_io(error.kind()))?;
        Ok(path)
    }
}

fn encode_cache(models: &[Value]) -> Result<Vec<u8>, PioneerError> {
    let mut encoded = serde_json::to_vec(&PioneerCatalogEnvelope { models })
        .map_err(|_| PioneerError::CacheIo(std::io::ErrorKind::InvalidData))?;
    encoded.push(b'\n');
    if encoded.len() > MAX_PIONEER_CATALOG_BYTES {
        return Err(PioneerError::CatalogTooLarge {
            limit: MAX_PIONEER_CATALOG_BYTES,
        });
    }
    Ok(encoded)
}

fn valid_model_slug(value: &Value) -> Option<&str> {
    let slug = value.as_str()?;
    if slug.is_empty()
        || slug.len() > MAX_PIONEER_MODEL_SLUG_BYTES
        || slug.trim() != slug
        || !slug.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/' | b':' | b'+')
        })
    {
        return None;
    }
    Some(slug)
}

const fn cache_io(kind: std::io::ErrorKind) -> PioneerError {
    PioneerError::CacheIo(kind)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PioneerModel {
    pub slug: String,
    pub display_name: String,
}

#[must_use]
pub fn pioneer_catalog_cache_path() -> PathBuf {
    pioneer_catalog_cache_path_under(&rsi_common::identity::data_dir())
}

/// Returns the fixed RSI Pioneer catalog path only when it is a regular,
/// bounded, parseable cache envelope suitable for Codex.
#[must_use]
pub fn existing_pioneer_codex_catalog_path() -> Option<PathBuf> {
    existing_pioneer_codex_catalog_path_under(&rsi_common::identity::data_dir())
}

/// Returns whether `model` is an API-usable entry in the current cached Pioneer
/// catalog. `None` means no valid cache is available, so callers can preserve
/// first-use and transient-cache behavior without doing network I/O.
#[must_use]
pub fn cached_pioneer_model_is_available(model: &str) -> Option<bool> {
    cached_pioneer_model_is_available_under(&rsi_common::identity::data_dir(), model)
}

fn cached_pioneer_model_is_available_under(data_dir: &Path, model: &str) -> Option<bool> {
    let path = existing_pioneer_codex_catalog_path_under(data_dir)?;
    let bytes = std::fs::read(path).ok()?;
    let catalog = PioneerCatalog::parse(&bytes).ok()?;
    Some(
        catalog
            .picker_models()
            .iter()
            .any(|candidate| candidate.slug == model),
    )
}

fn existing_pioneer_codex_catalog_path_under(data_dir: &Path) -> Option<PathBuf> {
    if !data_dir.is_absolute() {
        return None;
    }
    let path = pioneer_catalog_cache_path_under(data_dir);
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_PIONEER_CATALOG_BYTES as u64
    {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    PioneerCatalog::parse(&bytes).ok()?;
    Some(path)
}

fn pioneer_catalog_cache_path_under(data_dir: &Path) -> PathBuf {
    data_dir
        .join(PIONEER_CATALOG_CACHE_SUBDIR)
        .join(PIONEER_CATALOG_CACHE_FILENAME)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PioneerCodexConfigOverrides {
    values: Vec<String>,
}

impl PioneerCodexConfigOverrides {
    /// Creates deterministic Pioneer custom-provider overrides for Codex.
    ///
    /// # Errors
    ///
    /// Returns [`PioneerError::InvalidCodexCatalogPath`] when the optional catalog
    /// path is not absolute, normalized UTF-8.
    pub fn new(
        credential_source: PioneerCredentialSource,
        catalog_path: Option<&Path>,
    ) -> Result<Self, PioneerError> {
        let mut values = vec![
            config_string("model_provider", PIONEER_PROVIDER_ID),
            config_string("model_providers.pioneer.name", PIONEER_PROVIDER_NAME),
            config_string("model_providers.pioneer.base_url", PIONEER_API_BASE_URL),
            config_string(
                "model_providers.pioneer.env_key",
                credential_source.env_name(),
            ),
            config_string("model_providers.pioneer.wire_api", "responses"),
            "model_providers.pioneer.supports_websockets=false".to_string(),
        ];

        if let Some(path) = catalog_path {
            values.push(config_string(
                "model_catalog_json",
                validated_absolute_path(path)?,
            ));
        }

        Ok(Self { values })
    }

    #[must_use]
    pub fn values(&self) -> &[String] {
        &self.values
    }

    pub fn append_to(&self, command: &mut Command) {
        for value in &self.values {
            command.args(["-c", value]);
        }
    }
}

fn validated_absolute_path(path: &Path) -> Result<&str, PioneerError> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(PioneerError::InvalidCodexCatalogPath);
    }
    path.to_str().ok_or(PioneerError::InvalidCodexCatalogPath)
}

fn config_string(key: &str, value: &str) -> String {
    let Ok(encoded) = serde_json::to_string(value) else {
        unreachable!("serializing a string cannot fail");
    };
    format!("{key}={encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const CATALOG_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/pioneer_models.json");
    const MALFORMED_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/pioneer_malformed.json");
    const TEST_CREDENTIAL: &str = "fixture-credential-never-log";

    fn test_credential(source: PioneerCredentialSource) -> PioneerCredential {
        PioneerCredential {
            source,
            value: TEST_CREDENTIAL.to_string(),
        }
    }

    fn fixture_document() -> Value {
        serde_json::from_slice(CATALOG_FIXTURE).unwrap()
    }

    #[test]
    fn pioneer_catalog_projects_only_valid_top_level_models_deterministically() {
        let catalog = PioneerCatalog::parse(CATALOG_FIXTURE).unwrap();
        let models = catalog.picker_models();
        let slugs = models
            .iter()
            .map(|model| model.slug.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            slugs,
            [
                "vendor/duplicate",
                "vendor/high-priority",
                "vendor/no-display-name",
                "vendor/low-priority",
                "pioneer/auto",
            ]
        );
        assert_eq!(models[0].display_name, "Preferred Duplicate");
        assert_eq!(models[1].display_name, "vendor/high-priority");
        assert_eq!(models[2].display_name, "vendor/no-display-name");
        assert!(!slugs.contains(&"data-only/model"));
        assert!(!slugs.contains(&"vendor/hidden"));
        assert!(!slugs.contains(&"vendor/not-in-api"));
        assert_eq!(
            models
                .iter()
                .filter(|model| model.slug == PIONEER_RETIRED_AUTO_MODEL)
                .count(),
            1
        );
        assert_eq!(models[4].display_name, "Pioneer Auto Router");
    }

    #[test]
    fn pioneer_launch_model_defaults_and_migrates_only_retired_router() {
        assert_eq!(pioneer_launch_model(None), PIONEER_DEFAULT_MODEL);
        assert_eq!(
            pioneer_launch_model(Some(PIONEER_RETIRED_AUTO_MODEL)),
            PIONEER_DEFAULT_MODEL
        );
        assert_eq!(
            pioneer_launch_model(Some("vendor/direct-model")),
            "vendor/direct-model"
        );
    }

    #[test]
    fn pioneer_catalog_does_not_synthesize_models_absent_from_live_catalog() {
        let catalog = PioneerCatalog::parse(
            serde_json::to_string(&json!({
                "models": [{"slug": "vendor/model", "visibility": "list", "priority": 1}]
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();

        assert_eq!(
            catalog.picker_models(),
            [PioneerModel {
                slug: "vendor/model".to_string(),
                display_name: "vendor/model".to_string(),
            }]
        );
    }

    #[test]
    fn cached_pioneer_model_availability_uses_the_filtered_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = PioneerCatalog::parse(CATALOG_FIXTURE).unwrap();
        catalog.write_cache_atomic_under(directory.path()).unwrap();

        assert_eq!(
            cached_pioneer_model_is_available_under(directory.path(), "vendor/high-priority"),
            Some(true)
        );
        assert_eq!(
            cached_pioneer_model_is_available_under(directory.path(), "vendor/hidden"),
            Some(false)
        );
        assert_eq!(
            cached_pioneer_model_is_available_under(directory.path(), "missing/model"),
            Some(false)
        );

        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            cached_pioneer_model_is_available_under(empty.path(), "vendor/high-priority"),
            None
        );
    }

    #[test]
    fn pioneer_catalog_rejects_malformed_missing_non_array_empty_and_oversized() {
        assert!(matches!(
            PioneerCatalog::parse(MALFORMED_FIXTURE),
            Err(PioneerError::MalformedJson { .. })
        ));
        for invalid in [br#"{"data": []}"#.as_slice(), br#"{"models": {}}"#] {
            assert!(matches!(
                PioneerCatalog::parse(invalid),
                Err(PioneerError::MissingModelsEnvelope)
            ));
        }
        assert!(matches!(
            PioneerCatalog::parse(br#"{"models": []}"#),
            Err(PioneerError::EmptyCatalog)
        ));
        assert!(matches!(
            PioneerCatalog::parse(&vec![b'x'; MAX_PIONEER_CATALOG_BYTES + 1]),
            Err(PioneerError::CatalogTooLarge { .. })
        ));
    }

    #[test]
    fn pioneer_credentials_prefer_primary_trim_and_fall_back_safely() {
        let preferred = resolve_credential_with(|name| match name {
            PIONEER_PRIMARY_ENV => Some("  preferred  ".to_string()),
            PIONEER_FALLBACK_ENV => Some("fallback".to_string()),
            _ => None,
        })
        .unwrap();
        assert_eq!(preferred.source(), PioneerCredentialSource::AiInference);
        assert_eq!(preferred.value(), "preferred");

        let fallback = resolve_credential_with(|name| match name {
            PIONEER_PRIMARY_ENV => Some("   ".to_string()),
            PIONEER_FALLBACK_ENV => Some(" fallback ".to_string()),
            _ => None,
        })
        .unwrap();
        assert_eq!(fallback.source(), PioneerCredentialSource::ApiKey);
        assert_eq!(fallback.value(), "fallback");

        assert!(matches!(
            resolve_credential_with(|_| None),
            Err(PioneerError::MissingCredential)
        ));
        assert!(!format!("{preferred:?}").contains("preferred"));
    }

    #[test]
    fn pioneer_availability_requires_codex_boundary_and_supported_credential() {
        assert!(!pioneer_provider_available_with(false, |_| Some(
            "key".to_string()
        )));
        assert!(!pioneer_provider_available_with(true, |_| None));
        assert!(!pioneer_provider_available_with(true, |_| Some(
            "   ".to_string()
        )));
        assert!(pioneer_provider_available_with(true, |name| {
            (name == PIONEER_FALLBACK_ENV).then(|| "fallback".to_string())
        }));
    }

    #[tokio::test]
    async fn pioneer_client_uses_exact_get_auth_and_top_level_models() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", format!("Bearer {TEST_CREDENTIAL}")))
            .and(header("accept", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(CATALOG_FIXTURE))
            .expect(1)
            .mount(&server)
            .await;
        let client = PioneerCatalogClient::with_endpoint(
            Client::new(),
            &format!("{}/v1/models", server.uri()),
            Duration::from_secs(1),
        )
        .unwrap();

        let catalog = client
            .fetch(&test_credential(PioneerCredentialSource::AiInference))
            .await
            .unwrap();
        assert_eq!(
            catalog.raw_models(),
            fixture_document()["models"].as_array().unwrap()
        );
    }

    #[tokio::test]
    async fn pioneer_client_timeout_is_bounded() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(100))
                    .set_body_bytes(CATALOG_FIXTURE),
            )
            .mount(&server)
            .await;
        let client = PioneerCatalogClient::with_endpoint(
            Client::new(),
            &format!("{}/v1/models", server.uri()),
            Duration::from_millis(10),
        )
        .unwrap();

        assert!(matches!(
            client
                .fetch(&test_credential(PioneerCredentialSource::ApiKey))
                .await,
            Err(PioneerError::RequestTimeout)
        ));
    }

    #[tokio::test]
    async fn pioneer_discovery_returns_current_projection_and_refreshes_exact_cache() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", format!("Bearer {TEST_CREDENTIAL}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(CATALOG_FIXTURE))
            .expect(1)
            .mount(&server)
            .await;
        let client = PioneerCatalogClient::with_endpoint(
            Client::new(),
            &format!("{}/v1/models", server.uri()),
            Duration::from_secs(1),
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();

        let models = discover_pioneer_models_with(
            &client,
            &test_credential(PioneerCredentialSource::AiInference),
            directory.path(),
        )
        .await
        .unwrap();

        assert_eq!(models[0].0, "vendor/duplicate");
        assert_eq!(models.len(), 5);
        let cached: Value = serde_json::from_slice(
            &std::fs::read(pioneer_catalog_cache_path_under(directory.path())).unwrap(),
        )
        .unwrap();
        assert_eq!(
            cached.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["models"]
        );
        assert_eq!(cached["models"], fixture_document()["models"]);
    }

    #[tokio::test]
    async fn pioneer_discovery_failure_preserves_previous_cache() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(503).set_body_string(TEST_CREDENTIAL))
            .expect(1)
            .mount(&server)
            .await;
        let client = PioneerCatalogClient::with_endpoint(
            Client::new(),
            &format!("{}/v1/models", server.uri()),
            Duration::from_secs(1),
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = pioneer_catalog_cache_path_under(directory.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"previous-cache\n").unwrap();

        let error = discover_pioneer_models_with(
            &client,
            &test_credential(PioneerCredentialSource::ApiKey),
            directory.path(),
        )
        .await
        .unwrap_err();

        assert!(matches!(
            error,
            PioneerError::HttpStatus(StatusCode::SERVICE_UNAVAILABLE)
        ));
        assert!(!format!("{error:?} {error}").contains(TEST_CREDENTIAL));
        assert_eq!(std::fs::read(&path).unwrap(), b"previous-cache\n");
    }

    #[tokio::test]
    async fn pioneer_client_status_and_oversized_errors_are_secret_safe() {
        let status_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string(format!("body must not be echoed: {TEST_CREDENTIAL}")),
            )
            .mount(&status_server)
            .await;
        let status_client = PioneerCatalogClient::with_endpoint(
            Client::new(),
            &format!("{}/v1/models", status_server.uri()),
            Duration::from_secs(1),
        )
        .unwrap();
        let error = status_client
            .fetch(&test_credential(PioneerCredentialSource::ApiKey))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            PioneerError::HttpStatus(StatusCode::UNAUTHORIZED)
        ));
        assert!(!format!("{error:?} {error}").contains(TEST_CREDENTIAL));

        let large_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![
                b'x';
                MAX_PIONEER_CATALOG_BYTES
                    + 1
            ]))
            .mount(&large_server)
            .await;
        let large_client = PioneerCatalogClient::with_endpoint(
            Client::new(),
            &format!("{}/v1/models", large_server.uri()),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(matches!(
            large_client
                .fetch(&test_credential(PioneerCredentialSource::ApiKey))
                .await,
            Err(PioneerError::CatalogTooLarge { .. })
        ));
    }

    #[test]
    fn pioneer_cache_is_exact_bounded_rsi_envelope_and_replaces_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let path = pioneer_catalog_cache_path_under(directory.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"old cache").unwrap();
        let catalog = PioneerCatalog::parse(CATALOG_FIXTURE).unwrap();

        assert_eq!(
            catalog.write_cache_atomic_under(directory.path()).unwrap(),
            path
        );
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.len() <= MAX_PIONEER_CATALOG_BYTES);
        let cached: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            cached.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["models"]
        );
        assert_eq!(cached["models"], fixture_document()["models"]);
        assert_eq!(cached["models"][0]["future_metadata"]["preserved"], true);
    }

    #[test]
    fn pioneer_cache_failed_replacement_preserves_old_file_and_removes_temp() {
        let directory = tempfile::tempdir().unwrap();
        let path = pioneer_catalog_cache_path_under(directory.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"previous-valid-cache\n").unwrap();
        let catalog = PioneerCatalog::parse(CATALOG_FIXTURE).unwrap();
        let mut temporary_path = PathBuf::new();

        let error = catalog
            .write_cache_atomic_under_with(directory.path(), |temporary, _| {
                temporary_path = temporary.path().to_path_buf();
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            })
            .unwrap_err();

        assert!(matches!(
            error,
            PioneerError::CacheIo(std::io::ErrorKind::PermissionDenied)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"previous-valid-cache\n");
        assert!(!temporary_path.exists());
    }

    #[test]
    fn pioneer_cache_rejects_relative_root_and_oversized_serialization() {
        let catalog = PioneerCatalog::parse(CATALOG_FIXTURE).unwrap();
        assert!(matches!(
            catalog.write_cache_atomic_under(Path::new("relative")),
            Err(PioneerError::InvalidDataDirectory)
        ));

        let oversized = PioneerCatalog {
            models: vec![json!({
                "slug": "vendor/huge",
                "padding": "x".repeat(MAX_PIONEER_CATALOG_BYTES)
            })],
        };
        assert!(matches!(
            oversized.write_cache_atomic_under(tempfile::tempdir().unwrap().path()),
            Err(PioneerError::CatalogTooLarge { .. })
        ));
    }

    #[test]
    fn pioneer_codex_cache_path_requires_regular_bounded_parseable_envelope() {
        let directory = tempfile::tempdir().unwrap();
        let path = pioneer_catalog_cache_path_under(directory.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        assert_eq!(
            existing_pioneer_codex_catalog_path_under(directory.path()),
            None
        );

        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(
            existing_pioneer_codex_catalog_path_under(directory.path()),
            None
        );

        PioneerCatalog::parse(CATALOG_FIXTURE)
            .unwrap()
            .write_cache_atomic_under(directory.path())
            .unwrap();
        assert_eq!(
            existing_pioneer_codex_catalog_path_under(directory.path()),
            Some(path.clone())
        );

        std::fs::write(&path, vec![b'x'; MAX_PIONEER_CATALOG_BYTES + 1]).unwrap();
        assert_eq!(
            existing_pioneer_codex_catalog_path_under(directory.path()),
            None
        );
    }

    #[test]
    fn pioneer_codex_overrides_are_deterministic_optional_and_secret_safe() {
        let directory = tempfile::tempdir().unwrap();
        let catalog_path = directory.path().join("model catalogs/pioneer.json");
        let overrides = PioneerCodexConfigOverrides::new(
            PioneerCredentialSource::AiInference,
            Some(&catalog_path),
        )
        .unwrap();

        assert_eq!(
            overrides.values(),
            [
                "model_provider=\"pioneer\"",
                "model_providers.pioneer.name=\"Pioneer\"",
                "model_providers.pioneer.base_url=\"https://api.pioneer.ai/v1\"",
                "model_providers.pioneer.env_key=\"PIONEER_AI_INFERENCE\"",
                "model_providers.pioneer.wire_api=\"responses\"",
                "model_providers.pioneer.supports_websockets=false",
                &format!(
                    "model_catalog_json={}",
                    serde_json::to_string(catalog_path.to_str().unwrap()).unwrap()
                ),
            ]
        );
        assert!(!format!("{overrides:?}").contains(TEST_CREDENTIAL));
        assert!(
            !format!(
                "{:?}",
                test_credential(PioneerCredentialSource::AiInference)
            )
            .contains(TEST_CREDENTIAL)
        );

        let without_catalog =
            PioneerCodexConfigOverrides::new(PioneerCredentialSource::ApiKey, None).unwrap();
        assert_eq!(without_catalog.values().len(), 6);
        assert_eq!(
            without_catalog.values()[3],
            "model_providers.pioneer.env_key=\"PIONEER_API_KEY\""
        );
        assert!(
            !without_catalog
                .values()
                .iter()
                .any(|value| value.starts_with("model_catalog_json="))
        );
    }

    #[test]
    fn pioneer_codex_overrides_append_exact_scoped_config_arguments() {
        let overrides =
            PioneerCodexConfigOverrides::new(PioneerCredentialSource::ApiKey, None).unwrap();
        let mut command = Command::new("codex");
        overrides.append_to(&mut command);
        let args = command
            .as_std()
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(args.len(), overrides.values().len() * 2);
        for (index, value) in overrides.values().iter().enumerate() {
            assert_eq!(args[index * 2], "-c");
            assert_eq!(args[index * 2 + 1], *value);
        }
        assert!(!args.join(" ").contains(TEST_CREDENTIAL));
    }

    #[test]
    fn pioneer_codex_overrides_reject_relative_and_parent_paths() {
        for path in [
            Path::new("relative/pioneer.json"),
            Path::new("/tmp/model-catalogs/../secret.json"),
        ] {
            assert!(matches!(
                PioneerCodexConfigOverrides::new(PioneerCredentialSource::ApiKey, Some(path),),
                Err(PioneerError::InvalidCodexCatalogPath)
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn pioneer_codex_overrides_reject_non_utf8_absolute_path() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let path = PathBuf::from("/").join(OsString::from_vec(vec![0xff]));
        assert!(matches!(
            PioneerCodexConfigOverrides::new(PioneerCredentialSource::ApiKey, Some(&path),),
            Err(PioneerError::InvalidCodexCatalogPath)
        ));
    }
}
