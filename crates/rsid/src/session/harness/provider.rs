//! ApiProvider trait -- the interface between the agent loop and LLM backends.
//!
//! Each concrete implementation translates between harness types and
//! provider-specific wire format.

// Provider layer is built ahead of resolve_provider() wiring (Phase 6+).
#![allow(dead_code)]

use super::types::*;
use crate::error::DaemonError;
use crate::model_control::ModelExecutionCapability;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub type Result<T> = std::result::Result<T, DaemonError>;

/// Trait for making direct API calls to LLM providers.
///
/// Separate from `ProviderSession` (which is the monitor-loop interface).
/// The agent loop calls `ApiProvider` methods and emits `StreamEvent`s.
#[async_trait::async_trait]
pub(crate) trait ApiProvider: Send + Sync {
    /// Blocking (non-streaming) chat completion.
    async fn chat(
        &self,
        request: &ChatRequest,
        execution: ModelExecutionCapability,
    ) -> Result<ChatResponse>;

    /// Streaming chat completion.
    ///
    /// Sends chunks through `chunk_tx` for live display, accumulates the
    /// final response and returns it. If the provider doesn't support streaming,
    /// calls `chat()` and wraps the result as a single chunk.
    async fn stream_chat(
        &self,
        request: &ChatRequest,
        chunk_tx: mpsc::Sender<StreamChunk>,
        cancel: &CancellationToken,
        execution: ModelExecutionCapability,
    ) -> Result<ChatResponse>;

    /// Whether this provider supports native tool calling (structured JSON tool_calls).
    fn supports_native_tools(&self) -> bool;

    /// Provider name for logging and display.
    fn name(&self) -> &str;

    /// Whether streaming is supported.
    fn supports_streaming(&self) -> bool {
        true
    }
}

/// Resolve which ApiProvider backend to use for a given model and config.
///
/// Routing order (first match wins):
/// 1. Explicit `openai_base_url` in config -> OpenAI-compatible provider.
/// 2. Model prefix "claude-" -> AnthropicProvider.
/// 3. Model prefix "gpt-", "o1-", "o3-", "o4-" -> OpenAiApiProvider.
/// 4. Lookup in COMPATIBLE_TABLE -> OpenAI-compatible provider with preset URL.
/// 5. Unknown routing without an explicit local route is denied.
pub(crate) fn resolve_provider(
    model: &str,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> Result<Box<dyn ApiProvider>> {
    use super::providers::{anthropic::AnthropicProvider, openai_api::OpenAiApiProvider};

    #[cfg(test)]
    if let Some(url) = test_openai_compatible_routes()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(model)
        .cloned()
    {
        return Ok(Box::new(OpenAiApiProvider::with_config(
            url,
            None,
            ProviderQuirks {
                native_tools: true,
                auth_style: super::types::AuthStyle::None,
                ..Default::default()
            },
        )?));
    }

    if let Some(url) = base_url {
        return Ok(Box::new(OpenAiApiProvider::with_config(
            url.to_string(),
            api_key.map(String::from),
            ProviderQuirks {
                native_tools: true,
                ..Default::default()
            },
        )?));
    }

    if model.starts_with("claude-") {
        return Ok(Box::new(AnthropicProvider::new(api_key)?));
    }

    if model.starts_with("gpt-")
        || model.starts_with("o1-")
        || model.starts_with("o3-")
        || model.starts_with("o4-")
    {
        return Ok(Box::new(OpenAiApiProvider::new(api_key)?));
    }

    if let Some(entry) = super::compatible_table::lookup(model) {
        let key = super::api_key::resolve_api_key(api_key, entry.env_vars);
        return Ok(Box::new(OpenAiApiProvider::with_config(
            entry.base_url.to_string(),
            key,
            entry.quirks.clone(),
        )?));
    }

    Err(DaemonError::InvalidParam(format!(
        "unknown OpenAI-compatible route for model '{model}'; explicit base_url required"
    )))
}

#[cfg(test)]
fn test_openai_compatible_routes()
-> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    static ROUTES: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, String>>,
    > = std::sync::OnceLock::new();
    ROUTES.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Bind one unique test model to a loopback OpenAI-compatible endpoint. This
/// keeps production Harness launch and provider-confirmation code intact while
/// ensuring daemon fixtures never discover credentials or contact a paid API.
#[cfg(test)]
pub(crate) fn install_test_openai_compatible_route(model: &str, base_url: &str) {
    test_openai_compatible_routes()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(model.to_string(), base_url.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_mercury_to_openai_compatible_provider() {
        let provider = resolve_provider("mercury-2", None, Some("test-key")).unwrap();
        assert_eq!(provider.name(), "openai");
        assert!(provider.supports_native_tools());
    }

    #[test]
    fn rejects_unknown_model_without_explicit_route() {
        let err = match resolve_provider("mystery-model", None, None) {
            Ok(_) => panic!("must deny"),
            Err(err) => err,
        };
        assert!(
            err.to_string()
                .contains("unknown OpenAI-compatible route for model 'mystery-model'")
        );
    }
}
