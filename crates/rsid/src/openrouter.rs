//! OpenRouter Codex custom-provider and model-discovery primitives.

pub use crate::store_support::provider_defaults::OPENROUTER_DEFAULT_MODEL;
use futures::StreamExt;
use reqwest::{Client, StatusCode, Url};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::time::Duration;
use tokio::process::Command;

pub const OPENROUTER_API_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const OPENROUTER_PROVIDER_ID: &str = "openrouter";
pub const OPENROUTER_PROVIDER_NAME: &str = "OpenRouter";
pub const OPENROUTER_ENV: &str = "OPEN_ROUTER";
pub const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";
pub const OPENROUTER_PICKER_MAX_MODELS: usize = 100;

/// Model id sent on the OpenRouter wire. Explicit `openrouter/` is a routing
/// prefix; every non-empty `vendor/model` pair remains on OpenRouter.
pub fn harness_model_id(model: &str) -> Option<&str> {
    let normalized = model.strip_prefix("openrouter/").unwrap_or(model);
    let (vendor, name) = normalized.split_once('/')?;
    (!vendor.is_empty() && !name.is_empty()).then_some(normalized)
}
static CATALOG_TOOL_SUPPORT: std::sync::OnceLock<std::sync::RwLock<HashMap<String, bool>>> =
    std::sync::OnceLock::new();
#[cfg(test)]
static TEST_CATALOG_TOOL_SUPPORT: std::sync::OnceLock<std::sync::RwLock<HashMap<String, bool>>> =
    std::sync::OnceLock::new();

/// `None` means discovery has not observed this model; unknown models may try
/// the Harness route and receive their normal upstream error.
pub fn catalog_tool_support(model: &str) -> Option<bool> {
    #[cfg(test)]
    if let Some(supported) = TEST_CATALOG_TOOL_SUPPORT
        .get()
        .and_then(|cache| cache.read().ok()?.get(model).copied())
    {
        return Some(supported);
    }
    CATALOG_TOOL_SUPPORT.get()?.read().ok()?.get(model).copied()
}

#[cfg(test)]
pub(crate) fn record_test_catalog_tool_support(model: &str, supported: bool) {
    TEST_CATALOG_TOOL_SUPPORT
        .get_or_init(|| std::sync::RwLock::new(HashMap::new()))
        .write()
        .unwrap()
        .insert(model.to_owned(), supported);
}
const CATALOG_MAX_BYTES: usize = 8 * 1024 * 1024;
const CATALOG_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CATALOG_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum OpenRouterError {
    #[error(
        "OpenRouter credential is not configured; set it in the RSI key vault (SetProviderCredential) or export {OPENROUTER_ENV}"
    )]
    MissingCredential,
    #[error("OpenRouter catalog HTTP client could not be configured")]
    ClientConfiguration,
    #[error("OpenRouter catalog request timed out")]
    RequestTimeout,
    #[error("OpenRouter catalog request could not connect")]
    RequestConnect,
    #[error("OpenRouter catalog request failed")]
    RequestFailed,
    #[error("OpenRouter catalog request returned HTTP {0}")]
    HttpStatus(StatusCode),
    #[error("OpenRouter catalog exceeded the {CATALOG_MAX_BYTES}-byte limit")]
    CatalogTooLarge,
    #[error("OpenRouter catalog response is malformed")]
    MalformedCatalog,
    #[error("OpenRouter catalog has no usable text and tool-capable models")]
    EmptyCatalog,
}

/// Resolves the credential through the key vault (vault entry, then — unless
/// cleared — `OPEN_ROUTER` / `OPENROUTER_API_KEY` under `vault.env_compat`)
/// without retaining it in configuration or logs.
///
/// # Errors
///
/// Returns [`OpenRouterError::MissingCredential`] when the slot does not resolve.
pub fn openrouter_credential() -> Result<crate::vault::Resolved, OpenRouterError> {
    openrouter_credential_from(&crate::vault::global())
}

///
/// # Errors
///
/// Returns [`OpenRouterError::MissingCredential`] when the slot does not resolve.
pub fn openrouter_credential_from(
    vault: &crate::vault::VaultHandle,
) -> Result<crate::vault::Resolved, OpenRouterError> {
    vault
        .resolve(crate::vault::Slot::Openrouter)
        .ok()
        .flatten()
        .ok_or(OpenRouterError::MissingCredential)
}

#[must_use]
pub fn openrouter_provider_available(codex_cli_available: bool) -> bool {
    openrouter_provider_available_with(&crate::vault::global(), codex_cli_available)
}

/// Availability = credential resolvable AND (route = harness OR Codex present).
#[must_use]
pub fn openrouter_provider_available_with(
    vault: &crate::vault::VaultHandle,
    codex_cli_available: bool,
) -> bool {
    openrouter_provider_available_for_route(vault, codex_cli_available, false)
}

pub fn openrouter_provider_available_for_route(
    vault: &crate::vault::VaultHandle,
    codex_cli_available: bool,
    harness_route: bool,
) -> bool {
    crate::vault::provider_available(
        vault,
        crate::vault::Slot::Openrouter,
        codex_cli_available || harness_route,
    )
}

/// Fetches OpenRouter's text, tool-capable catalog in server popularity order.
/// The local projection is capped so a large vendor catalog cannot consume the
/// picker region.
pub async fn discover_openrouter_models() -> Result<Vec<(String, String)>, OpenRouterError> {
    discover_openrouter_models_with(&crate::vault::global(), OPENROUTER_MODELS_URL).await
}

async fn discover_openrouter_models_with(
    vault: &crate::vault::VaultHandle,
    models_url: &str,
) -> Result<Vec<(String, String)>, OpenRouterError> {
    let credential = openrouter_credential_from(vault)?;
    OpenRouterCatalogClient::new(models_url)?
        .fetch_picker_models(credential.secret.expose())
        .await
}

struct OpenRouterCatalogClient {
    http: Client,
    models_url: Url,
}

impl OpenRouterCatalogClient {
    fn new(models_url: &str) -> Result<Self, OpenRouterError> {
        let http = Client::builder()
            .connect_timeout(CATALOG_CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| OpenRouterError::ClientConfiguration)?;
        let mut models_url =
            Url::parse(models_url).map_err(|_| OpenRouterError::ClientConfiguration)?;
        models_url
            .query_pairs_mut()
            .append_pair("output_modalities", "text")
            .append_pair("sort", "most-popular");
        Ok(Self { http, models_url })
    }

    async fn fetch_picker_models(
        &self,
        credential: &str,
    ) -> Result<Vec<(String, String)>, OpenRouterError> {
        let response = self
            .http
            .get(self.models_url.clone())
            .timeout(CATALOG_REQUEST_TIMEOUT)
            .bearer_auth(credential)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(classify_request_error)?;
        if !response.status().is_success() {
            return Err(OpenRouterError::HttpStatus(response.status()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > CATALOG_MAX_BYTES as u64)
        {
            return Err(OpenRouterError::CatalogTooLarge);
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(classify_request_error)?;
            if bytes.len().saturating_add(chunk.len()) > CATALOG_MAX_BYTES {
                return Err(OpenRouterError::CatalogTooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        parse_picker_models(&bytes)
    }
}

fn classify_request_error(error: reqwest::Error) -> OpenRouterError {
    if error.is_timeout() {
        OpenRouterError::RequestTimeout
    } else if error.is_connect() {
        OpenRouterError::RequestConnect
    } else {
        OpenRouterError::RequestFailed
    }
}

fn parse_catalog_tool_support(data: &[Value]) -> HashMap<String, bool> {
    data.iter()
        .filter_map(|entry| {
            let object = entry.as_object()?;
            let id = object.get("id")?.as_str()?.trim();
            let parameters = object.get("supported_parameters")?.as_array()?;
            (!id.is_empty()).then(|| {
                (
                    id.to_owned(),
                    parameters
                        .iter()
                        .any(|parameter| parameter.as_str() == Some("tools")),
                )
            })
        })
        .collect()
}

fn parse_picker_models(bytes: &[u8]) -> Result<Vec<(String, String)>, OpenRouterError> {
    if bytes.len() > CATALOG_MAX_BYTES {
        return Err(OpenRouterError::CatalogTooLarge);
    }
    let document =
        serde_json::from_slice::<Value>(bytes).map_err(|_| OpenRouterError::MalformedCatalog)?;
    let data = document
        .get("data")
        .and_then(Value::as_array)
        .ok_or(OpenRouterError::MalformedCatalog)?;

    let metadata = parse_catalog_tool_support(data);
    if let Ok(mut cached) = CATALOG_TOOL_SUPPORT
        .get_or_init(|| std::sync::RwLock::new(HashMap::new()))
        .write()
    {
        *cached = metadata;
    }

    let mut seen = HashSet::new();
    let models = data
        .iter()
        .filter_map(|entry| {
            let object = entry.as_object()?;
            let id = object.get("id")?.as_str()?.trim();
            let name = object.get("name")?.as_str()?.trim();
            let supports_tools = object
                .get("supported_parameters")?
                .as_array()?
                .iter()
                .any(|parameter| parameter.as_str() == Some("tools"));
            (supports_tools && !id.is_empty() && !name.is_empty() && seen.insert(id.to_owned()))
                .then(|| (id.to_owned(), name.to_owned()))
        })
        .take(OPENROUTER_PICKER_MAX_MODELS)
        .collect::<Vec<_>>();
    (!models.is_empty())
        .then_some(models)
        .ok_or(OpenRouterError::EmptyCatalog)
}

/// Codex custom-provider overrides. The credential is passed in environment,
/// never in argv: Codex reads it through `env_key`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenRouterCodexConfigOverrides {
    values: Vec<String>,
}

impl OpenRouterCodexConfigOverrides {
    #[must_use]
    pub fn new(_credential: &str) -> Self {
        Self {
            values: vec![
                config_string("model_provider", OPENROUTER_PROVIDER_ID),
                config_string("model_providers.openrouter.name", OPENROUTER_PROVIDER_NAME),
                config_string(
                    "model_providers.openrouter.base_url",
                    OPENROUTER_API_BASE_URL,
                ),
                config_string("model_providers.openrouter.env_key", OPENROUTER_ENV),
                config_string("model_providers.openrouter.wire_api", "responses"),
                "model_providers.openrouter.supports_websockets=false".to_string(),
                // Codex otherwise advertises hosted web search to every custom
                // provider. OpenRouter can reject that server tool and fail
                // the whole turn before the CLI exposes a tool result.
                config_string("web_search", "disabled"),
            ],
        }
    }

    pub fn append_to(&self, command: &mut Command) {
        for value in &self.values {
            command.args(["-c", value]);
        }
    }

    #[must_use]
    pub fn values(&self) -> &[String] {
        &self.values
    }
}

fn config_string(key: &str, value: &str) -> String {
    format!("{key}={value:?}")
}

impl fmt::Display for OpenRouterCodexConfigOverrides {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OpenRouter Codex custom-provider overrides")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn harness_route_keeps_all_vendor_models_on_openrouter_and_strips_explicit_prefix() {
        for model in [
            "z-ai/glm-5.3-flashx",
            "qwen/qwen3.8-flash",
            "minimax/minimax-m3",
            "deepseek/deepseek-v4.1-flash",
            "qwen/qwen3-coder-next",
        ] {
            assert_eq!(harness_model_id(model), Some(model));
            assert_eq!(
                harness_model_id(&format!("openrouter/{model}")),
                Some(model)
            );
        }
        assert_eq!(harness_model_id("qwen3:14b"), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn catalog_metadata_distinguishes_no_tool_support_from_unknown_models() {
        let entries = vec![
            serde_json::json!({"id":"vendor/tool-model", "supported_parameters":["tools"]}),
            serde_json::json!({"id":"vendor/no-tool-model", "supported_parameters":["temperature"]}),
        ];
        let metadata = parse_catalog_tool_support(&entries);
        assert_eq!(metadata.get("vendor/tool-model"), Some(&true));
        assert_eq!(metadata.get("vendor/no-tool-model"), Some(&false));
        assert_eq!(metadata.get("vendor/unknown"), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn config_uses_operator_env_without_exposing_credential() {
        let overrides = OpenRouterCodexConfigOverrides::new("secret");
        assert!(
            overrides
                .values()
                .iter()
                .any(|value| value == "model_providers.openrouter.env_key=\"OPEN_ROUTER\"")
        );
        assert!(
            !overrides
                .values()
                .iter()
                .any(|value| value.contains("secret"))
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn codex_overrides_disable_hosted_web_search_by_default() {
        let overrides = OpenRouterCodexConfigOverrides::new("unused");
        let mut command = Command::new("codex");
        overrides.append_to(&mut command);
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<String>>();
        assert!(
            args.windows(2)
                .any(|pair| { pair[0] == "-c" && pair[1] == "web_search=\"disabled\"" })
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn picker_projection_filters_invalid_entries_deduplicates_and_caps() {
        let entries = (0..OPENROUTER_PICKER_MAX_MODELS + 2)
            .map(|index| {
                serde_json::json!({
                    "id": format!("vendor/model-{index}"),
                    "name": format!("Model {index}"),
                    "supported_parameters": ["tools"],
                })
            })
            .chain(std::iter::once(serde_json::json!({
                "id": "vendor/model-0",
                "name": "Duplicate",
                "supported_parameters": ["tools"],
            })))
            .chain(std::iter::once(serde_json::json!({
                "id": "vendor/no-tools",
                "name": "No tools",
                "supported_parameters": ["temperature"],
            })))
            .collect::<Vec<_>>();
        let bytes = serde_json::to_vec(&serde_json::json!({ "data": entries })).unwrap();

        let models = parse_picker_models(&bytes).unwrap();

        assert_eq!(models.len(), OPENROUTER_PICKER_MAX_MODELS);
        assert_eq!(
            models[0],
            ("vendor/model-0".to_string(), "Model 0".to_string())
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[tokio::test]
    async fn vault_only_openrouter_is_available_and_discovery_sends_vault_key() {
        use wiremock::matchers::{header, method, path};
        let root = tempfile::tempdir().unwrap();
        let vault = crate::vault::VaultHandleBuilder::new(std::sync::Arc::new(
            crate::vault::VaultSettings::default(),
        ))
        .dir(root.path().join("vault"))
        .env(|_| None)
        .open()
        .unwrap();
        assert!(!openrouter_provider_available_with(&vault, true));
        vault
            .set(
                crate::vault::Slot::Openrouter,
                "sk-test-vault-only-openrouter",
            )
            .unwrap();
        assert!(openrouter_provider_available_with(&vault, true));
        // The default route still requires Codex; the Harness route does not.
        assert!(!openrouter_provider_available_with(&vault, false));
        assert!(openrouter_provider_available_for_route(&vault, false, true));

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("GET"))
            .and(path("/api/v1/models"))
            .and(header(
                "authorization",
                "Bearer sk-test-vault-only-openrouter",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"data": [{
                    "id": "vendor/model", "name": "Model",
                    "supported_parameters": ["tools"],
                }]}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let models =
            discover_openrouter_models_with(&vault, &format!("{}/api/v1/models", server.uri()))
                .await
                .unwrap();
        assert_eq!(
            models,
            vec![("vendor/model".to_string(), "Model".to_string())]
        );
    }

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn cleared_openrouter_is_unavailable_even_with_env_key() {
        let root = tempfile::tempdir().unwrap();
        let vault = crate::vault::VaultHandleBuilder::new(std::sync::Arc::new(
            crate::vault::VaultSettings::default(),
        ))
        .dir(root.path().join("vault"))
        .env(|name| (name == OPENROUTER_ENV).then(|| "sk-test-env-openrouter".to_string()))
        .open()
        .unwrap();
        assert!(openrouter_provider_available_with(&vault, true));
        vault.clear(crate::vault::Slot::Openrouter).unwrap();
        assert!(!openrouter_provider_available_with(&vault, true));
        assert!(matches!(
            openrouter_credential_from(&vault),
            Err(OpenRouterError::MissingCredential)
        ));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
    #[test]
    fn picker_projection_rejects_malformed_or_empty_catalogs() {
        assert!(matches!(
            parse_picker_models(br#"{"data":[]}"#),
            Err(OpenRouterError::EmptyCatalog)
        ));
        assert!(matches!(
            parse_picker_models(br#"{"models":[]}"#),
            Err(OpenRouterError::MalformedCatalog)
        ));
    }
}
