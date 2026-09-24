//! OpenRouter Codex custom-provider and model-discovery primitives.

use futures::StreamExt;
use reqwest::{Client, StatusCode, Url};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt;
use std::time::Duration;
use tokio::process::Command;

pub const OPENROUTER_API_BASE_URL: &str = "https://openrouter.ai/api/v1";
pub const OPENROUTER_PROVIDER_ID: &str = "openrouter";
pub const OPENROUTER_PROVIDER_NAME: &str = "OpenRouter";
pub const OPENROUTER_ENV: &str = "OPEN_ROUTER";
pub const OPENROUTER_DEFAULT_MODEL: &str = "openai/gpt-5.2";
pub const OPENROUTER_MODELS_URL: &str = "https://openrouter.ai/api/v1/models";
pub const OPENROUTER_PICKER_MAX_MODELS: usize = 100;
const CATALOG_MAX_BYTES: usize = 2 * 1024 * 1024;
const CATALOG_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CATALOG_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum OpenRouterError {
    #[error("OpenRouter credential is not configured; set {OPENROUTER_ENV}")]
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

/// Resolves the credential without retaining it in configuration or logs.
pub fn openrouter_credential_from_env() -> Result<String, OpenRouterError> {
    std::env::var(OPENROUTER_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or(OpenRouterError::MissingCredential)
}

#[must_use]
pub fn openrouter_provider_available(codex_cli_available: bool) -> bool {
    codex_cli_available && openrouter_credential_from_env().is_ok()
}

/// Fetches OpenRouter's text, tool-capable catalog in server popularity order.
/// The local projection is capped so a large vendor catalog cannot consume the
/// picker region.
pub async fn discover_openrouter_models() -> Result<Vec<(String, String)>, OpenRouterError> {
    let credential = openrouter_credential_from_env()?;
    OpenRouterCatalogClient::new()?
        .fetch_picker_models(&credential)
        .await
}

struct OpenRouterCatalogClient {
    http: Client,
    models_url: Url,
}

impl OpenRouterCatalogClient {
    fn new() -> Result<Self, OpenRouterError> {
        let http = Client::builder()
            .connect_timeout(CATALOG_CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| OpenRouterError::ClientConfiguration)?;
        let mut models_url =
            Url::parse(OPENROUTER_MODELS_URL).expect("OpenRouter models URL constant must parse");
        models_url
            .query_pairs_mut()
            .append_pair("output_modalities", "text")
            .append_pair("supported_parameters", "tools")
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
