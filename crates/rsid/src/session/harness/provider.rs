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

/// Explicit route facts for provider-hosted web tools.
///
/// Unknown routes have no capability by default. Model compatibility is still
/// determined by the provider; this struct only records an explicit route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebCapabilities {
    pub api_surface: &'static str,
    pub endpoint: String,
    pub model: String,
    pub tools: Vec<HostedWebTool>,
    pub filter_supported: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedWebTool {
    pub name: &'static str,
    pub wire_type: String,
}

impl WebCapabilities {
    pub fn none() -> Self {
        Self {
            api_surface: "none",
            endpoint: String::new(),
            model: String::new(),
            tools: Vec::new(),
            filter_supported: false,
        }
    }

    pub fn anthropic_messages(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_surface: "anthropic_messages",
            endpoint: endpoint.into(),
            model: model.into(),
            tools: vec![
                HostedWebTool {
                    name: "web_search",
                    wire_type: "web_search_20250305".to_string(),
                },
                HostedWebTool {
                    name: "web_fetch",
                    wire_type: "web_fetch_20250910".to_string(),
                },
            ],
            filter_supported: true,
        }
    }

    pub fn openai_responses(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            api_surface: "openai_responses",
            endpoint: endpoint.into(),
            model: model.into(),
            tools: vec![HostedWebTool {
                name: "web_search",
                wire_type: "web_search".to_string(),
            }],
            filter_supported: true,
        }
    }

    pub fn specs(&self) -> Vec<HarnessToolSpec> {
        self.tools
            .iter()
            .map(|tool| HarnessToolSpec {
                name: tool.name.to_string(),
                description: format!(
                    "Provider-hosted {name} on {surface}",
                    name = tool.name,
                    surface = self.api_surface
                ),
                parameters_json: "{}".to_string(),
                freeform: None,
                kind: match (self.api_surface, tool.name) {
                    ("anthropic_messages", "web_search") => {
                        HarnessToolSpecKind::AnthropicWebSearch {
                            wire_type: tool.wire_type.clone(),
                        }
                    }
                    ("anthropic_messages", "web_fetch") => HarnessToolSpecKind::AnthropicWebFetch {
                        wire_type: tool.wire_type.clone(),
                    },
                    _ => HarnessToolSpecKind::ResponsesWebSearch,
                },
            })
            .collect()
    }
}

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

    /// The route's default per-call output budget, reasoning included
    /// (#2253). The Harness loop lifts its generic per-call cap to this when
    /// it is larger. `None` keeps the generic cap.
    fn default_max_output_tokens(&self) -> Option<u32> {
        None
    }

    /// Whether the route takes `tool_choice: "none"` next to `tools` while
    /// the history holds `tool_calls` (#2253). When true the wrap-up and
    /// final-answer turns keep `tools`; otherwise the loop clears them.
    fn accepts_tool_choice_none(&self) -> bool {
        false
    }

    /// Whether the resolved model accepts image inputs on this provider route.
    fn supports_image_input(&self, _model: &str) -> bool {
        false
    }

    /// Explicit hosted-web route data. Defaults to no hosted tools.
    fn web_capabilities(&self, _model: &str) -> WebCapabilities {
        WebCapabilities::none()
    }

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
/// 1. Explicit `openai_base_url` in config: a native Anthropic Messages
///    endpoint (path `/v1/messages`, #595) -> AnthropicProvider at its API
///    root; any other URL -> OpenAI-compatible provider.
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
            super::api_key::ApiCredential::None,
            ProviderQuirks {
                native_tools: true,
                auth_style: super::types::AuthStyle::None,
                ..Default::default()
            },
        )?));
    }

    if let Some(url) = base_url {
        if let Some(api_root) = anthropic_messages_api_root(url)? {
            return Ok(Box::new(anthropic_messages_endpoint_provider(
                &api_root, api_key,
            )?));
        }
        return Ok(Box::new(OpenAiApiProvider::with_config(
            url.to_string(),
            super::api_key::ApiCredential::explicit(api_key),
            ProviderQuirks {
                native_tools: true,
                ..Default::default()
            },
        )?));
    }

    match crate::bedrock::bedrock_vendor(model) {
        Some(crate::bedrock::BedrockVendor::Anthropic) => {
            let region = crate::bedrock::region().map_err(DaemonError::Process)?;
            return Ok(Box::new(AnthropicProvider::bedrock(
                region,
                bedrock_credential()?,
            )?));
        }
        // Same Responses backend the Bedrock provider's Harness route uses.
        Some(crate::bedrock::BedrockVendor::OpenAi) => {
            return Ok(Box::new(
                super::providers::openai_responses::OpenAiResponsesProvider::bedrock()?,
            ));
        }
        None => {}
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
        // Resolved per request, so a vault rotation reaches a live session.
        let credential = super::api_key::ApiCredential::for_slot(
            api_key,
            crate::vault::slots::slot_for_compatible_entry(entry.name),
        );
        return Ok(Box::new(OpenAiApiProvider::with_config(
            entry.base_url.to_string(),
            credential,
            entry.quirks.clone(),
        )?));
    }

    Err(DaemonError::InvalidParam(format!(
        "unknown OpenAI-compatible route for model '{model}'; explicit base_url required"
    )))
}

/// The native Anthropic Messages path. The Messages provider appends it to the
/// API root it is given.
const ANTHROPIC_MESSAGES_PATH: &str = "/v1/messages";

/// #595: the API root of a configured endpoint that is a native Anthropic
/// Messages URL (`<root>/v1/messages`, one trailing `/` allowed), or `None`.
/// Native URLs with query, fragment or inline credentials are refused before
/// launch: the Messages client appends a path to the root and cannot preserve
/// those components. Never echo the URL, which may contain a secret.
///
/// Classified by the URL path alone, never by model name: an OpenAI-compatible
/// base (including Anthropic's own compatibility root `…/v1`) never ends in
/// `/v1/messages`, because RSI appends `/chat/completions` to it, so every
/// existing OpenAI-compatible route keeps its protocol.
pub(crate) fn anthropic_messages_api_root(base_url: &str) -> Result<Option<String>> {
    let Ok(url) = reqwest::Url::parse(base_url.trim()) else {
        return Ok(None);
    };
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Ok(None);
    }
    let path = url.path().strip_suffix('/').unwrap_or(url.path());
    if !path.ends_with(ANTHROPIC_MESSAGES_PATH) {
        return Ok(None);
    }
    if url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(DaemonError::InvalidParam(
            "anthropic_endpoint_components_unsupported: native Anthropic Messages URLs \
             cannot contain a query, fragment or inline credentials; configure a URL \
             ending in /v1/messages without these components in Settings > API Providers, \
             and set the key in the provider's API key field"
                .into(),
        ));
    }
    // Components have been checked independently. Strip the path suffix from
    // the parsed URL so prefixes, ports and normalized paths remain intact.
    Ok(Some(
        url.as_str()
            .strip_suffix('/')
            .unwrap_or(url.as_str())
            .strip_suffix(ANTHROPIC_MESSAGES_PATH)
            .expect("the parsed path ends in the Messages suffix")
            .to_owned(),
    ))
}

/// #595: the provider a launch with an explicit endpoint runs on. A Local
/// launch (the OpenAI-compatible Chat Completions client) cannot speak a
/// native Anthropic Messages endpoint, so it runs on Harness, whose
/// AnthropicProvider does; every other launch keeps its provider.
pub(crate) fn provider_for_explicit_endpoint(
    requested: Option<rsi_common::types::SessionProvider>,
    base_url: Option<&str>,
) -> Result<Option<rsi_common::types::SessionProvider>> {
    use rsi_common::types::SessionProvider;
    if matches!(
        requested,
        Some(SessionProvider::Local | SessionProvider::Harness)
    ) {
        if let Some(url) = base_url {
            if anthropic_messages_api_root(url)?.is_some() {
                return Ok(Some(SessionProvider::Harness));
            }
        }
    }
    Ok(requested)
}

/// A configured native Anthropic Messages endpoint with its own key. Only the
/// endpoint's own key is used (no Anthropic vault-slot fallback), so the
/// operator's global Anthropic key is never sent to a custom host.
fn anthropic_messages_endpoint_provider(
    api_root: &str,
    api_key: Option<&str>,
) -> Result<super::providers::anthropic::AnthropicProvider> {
    let credential = super::api_key::ApiCredential::explicit(api_key);
    if credential.current().is_none() {
        return Err(DaemonError::InvalidParam(format!(
            "anthropic_endpoint_key_missing: the Anthropic Messages endpoint \
             {api_root}{ANTHROPIC_MESSAGES_PATH} has no API key; set the key on this \
             custom provider (Settings > API Providers)"
        )));
    }
    super::providers::anthropic::AnthropicProvider::with_credential(
        api_root.to_string(),
        credential,
    )
}

/// The Harness Bedrock credential. A vault or env-compat key resolves per
/// request so a rotation reaches the live session; a generator token is minted
/// once per launch (the generator is a subprocess, too slow per request).
fn bedrock_credential() -> Result<super::api_key::ApiCredential> {
    use super::api_key::ApiCredential;
    let resolved =
        crate::bedrock::credential_from(&crate::vault::global()).map_err(DaemonError::Process)?;
    Ok(
        if resolved.source == crate::vault::CredentialSource::Generator {
            ApiCredential::Fixed(resolved.secret)
        } else {
            ApiCredential::for_slot_only(crate::vault::Slot::Bedrock)
        },
    )
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn resolves_mercury_to_openai_compatible_provider() {
        let provider = resolve_provider("mercury-2", None, Some("test-key")).unwrap();
        assert_eq!(provider.name(), "openai");
        assert!(provider.supports_native_tools());
    }

    /// #595: only a native Messages URL is classified native; every
    /// OpenAI-compatible base, Anthropic's own compatibility root included,
    /// keeps its protocol whatever the model is called.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn classifies_native_anthropic_messages_endpoints_by_path() {
        for (url, root) in [
            (
                "https://api.anthropic.com/v1/messages",
                "https://api.anthropic.com",
            ),
            (
                "https://api.anthropic.com/v1/messages/",
                "https://api.anthropic.com",
            ),
            (
                " https://proxy.example.invalid/anthropic/v1/messages ",
                "https://proxy.example.invalid/anthropic",
            ),
            ("http://127.0.0.1:4010/v1/messages", "http://127.0.0.1:4010"),
        ] {
            assert_eq!(
                anthropic_messages_api_root(url).unwrap().as_deref(),
                Some(root),
                "{url}"
            );
        }
        for url in [
            "https://api.anthropic.com/v1",
            "https://api.anthropic.com/v1/",
            "https://api.anthropic.com",
            "https://api.example.invalid/openai/v1",
            "https://api.example.invalid/v1/messages/chat/completions",
            "https://api.example.invalid/v1/messagesx",
            "https://api.example.invalid/v1?route=/v1/messages",
            "https://api.example.invalid/v1#route/v1/messages",
            "https://api.example.invalid/v1/messagesx?route=/v1/messages/",
            "https://api.example.invalid/v1/messages//",
            "https:///v1/messages",
            "/v1/messages",
        ] {
            assert_eq!(anthropic_messages_api_root(url).unwrap(), None, "{url}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn only_a_local_launch_of_a_native_messages_endpoint_moves_to_harness() {
        use rsi_common::types::SessionProvider;
        let native = Some("https://api.anthropic.com/v1/messages");
        let compatible = Some("https://api.anthropic.com/v1");
        assert_eq!(
            provider_for_explicit_endpoint(Some(SessionProvider::Local), native).unwrap(),
            Some(SessionProvider::Harness)
        );
        for (requested, url) in [
            (Some(SessionProvider::Local), compatible),
            (Some(SessionProvider::Local), None),
            (Some(SessionProvider::Harness), native),
            (Some(SessionProvider::Codex), native),
            (Some(SessionProvider::Claude), native),
            (None, native),
        ] {
            assert_eq!(
                provider_for_explicit_endpoint(requested, url).unwrap(),
                requested
            );
        }
    }

    /// #595: an explicit native Messages URL runs on the Anthropic provider
    /// with the endpoint's own key; Anthropic's OpenAI-compatible root stays
    /// OpenAI-compatible for a claude model; a native endpoint without a key
    /// is refused with the fix, not sent unauthenticated.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
    #[test]
    fn explicit_endpoints_pick_the_protocol_their_url_names() {
        let native = resolve_provider(
            "claude-opus-5-5",
            Some("https://api.anthropic.com/v1/messages"),
            Some("sk-ant-api03-inert-595"),
        )
        .unwrap();
        assert_eq!(native.name(), "anthropic");
        let compatible = resolve_provider(
            "claude-opus-5-5",
            Some("https://api.anthropic.com/v1"),
            Some("sk-ant-api03-inert-595"),
        )
        .unwrap();
        assert_eq!(compatible.name(), "openai");

        for key in [None, Some("")] {
            let error = match resolve_provider(
                "claude-opus-5-5",
                Some("https://api.anthropic.com/v1/messages"),
                key,
            ) {
                Ok(_) => panic!("a keyless native endpoint must be refused"),
                Err(error) => error.to_string(),
            };
            assert!(
                error.contains("anthropic_endpoint_key_missing")
                    && error.contains("https://api.anthropic.com/v1/messages")
                    && error.contains("Settings > API Providers"),
                "{error}"
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
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
