//! ApiProvider trait -- the interface between the agent loop and LLM backends.
//!
//! Each concrete implementation translates between harness types and provider-specific wire format.

use super::types::*;
use crate::error::DaemonError;
use crate::model_control::ModelExecutionCapability;
use tokio::sync::mpsc;

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
/// 1. Explicit `openai_base_url` in config -> CompatibleProvider
/// 2. Model prefix "claude-" -> AnthropicProvider
/// 3. Model prefix "gpt-", "o1-", "o3-", "o4-" -> OpenAiProvider
/// 4. Lookup in COMPATIBLE_TABLE -> CompatibleProvider with preset URL
/// 5. Known local model prefix -> CompatibleProvider at localhost:11434 (Ollama)
///
/// Unknown model strings fail closed instead of silently routing to localhost.
pub(crate) fn resolve_provider(
    model: &str,
    base_url: Option<&str>,
    api_key: Option<&str>,
) -> Result<Box<dyn ApiProvider>> {
    use super::providers::{anthropic::AnthropicProvider, openai_api::OpenAiApiProvider};

    // 1. Explicit base URL -> OpenAI-compatible provider
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

    // 2. Anthropic models
    if model.starts_with("claude-") {
        return Ok(Box::new(AnthropicProvider::new(api_key)?));
    }

    // 3. OpenAI models
    if model.starts_with("gpt-")
        || model.starts_with("o1-")
        || model.starts_with("o3-")
        || model.starts_with("o4-")
    {
        return Ok(Box::new(OpenAiApiProvider::new(api_key)?));
    }

    // 4. Compatible table lookup
    if let Some(entry) = super::compatible_table::lookup(model) {
        let key = super::api_key::resolve_api_key(api_key, entry.env_vars);
        return Ok(Box::new(OpenAiApiProvider::with_config(
            entry.base_url.to_string(),
            key,
            entry.quirks.clone(),
        )?));
    }

    // 5. Explicitly local model families only.
    if matches!(
        model.split(':').next().unwrap_or(model),
        prefix if prefix.starts_with("qwen")
            || prefix.starts_with("llama")
            || prefix.starts_with("gemma")
            || prefix.starts_with("mistral")
    ) {
        return Ok(Box::new(OpenAiApiProvider::with_config(
            "http://localhost:11434/v1".into(),
            None,
            ProviderQuirks {
                native_tools: false,
                ..Default::default()
            },
        )?));
    }

    Err(DaemonError::InvalidParam(format!(
        "unknown harness model routing for '{model}'"
    )))
}
