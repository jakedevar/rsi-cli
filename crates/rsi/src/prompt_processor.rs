//! Thin RPC client for prompt compilation.
//!
//! `compile()` and `send()` both delegate to rsid via JSON-RPC on the
//! Unix socket. Post-processing (contract parse, layer validation, think-tag
//! stripping), caching, supersede cancellation, and startup warmup all live in
//! the daemon. This module carries only the TUI-side plumbing needed to issue
//! a `CompilePrompt` / `GenerateText` request and await the result.

use crate::settings::{CustomProviderEntry, PromptProcessorConfig};
use rsi_common::rpc::{
    BusEvent, CompilePromptParams, CompilePromptResponse, GenerateTextParams, GenerateTextResponse,
    RpcRequest, RpcResponse, SubscribeParams,
};
use rsi_common::types::SessionProvider;
use std::path::PathBuf;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use uuid::Uuid;

// Re-export the wire types so callers keep using `crate::prompt_processor::*`.
pub use rsi_common::prompt_compile::{CompileResult, LayerValidation, OutputContract};

// ── AI Assistant system prompts (consumed by `send()`) ─────────────────────

pub const AI_COMMAND_SYSTEM_PROMPT: &str = "\
You are a text transformation assistant. The user will provide source text \
and an instruction. Apply the instruction to the source text and return ONLY \
the modified text. No explanation. No wrapper text. No markdown fences. \
If the instruction is unclear, make your best interpretation and apply it.";

pub const GRAMMAR_SYSTEM_PROMPT: &str = "\
Fix all spelling and grammar errors in the following text. \
Preserve the original meaning, tone, structure, and formatting. \
Do not add, remove, or rephrase content \u{2014} only correct errors. \
If the text has no errors, return it unchanged. \
Return the corrected text only. No explanation. No wrapper text.";

pub const AI_CHAT_SYSTEM_PROMPT: &str = "\
You are a concise text analysis assistant. The user will provide source text \
and ask questions about it. Answer questions directly and briefly. \
Keep responses under 3 sentences unless the question requires more detail.";

// ── Errors ────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum PromptProcessorError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("daemon error: {0}")]
    OllamaError(String),
    #[error("Empty response from model")]
    EmptyResponse,
    #[error("Processor disabled")]
    Disabled,
    /// LLM returned COMPILE_ERROR:AMBIGUOUS_INTENT — surface for clarification.
    #[error("Ambiguous intent: {0}")]
    AmbiguousIntent(String),
}

impl PromptProcessorError {
    pub fn user_facing_message(&self) -> String {
        match self {
            Self::EmptyResponse => "Model returned empty response".to_string(),
            Self::Disabled => "Prompt processor is disabled".to_string(),
            Self::AmbiguousIntent(desc) => format!("Ambiguous: {desc}"),
            Self::Io(e) => format!("IO error talking to daemon: {e}"),
            Self::OllamaError(msg) => {
                if msg.is_empty() {
                    "daemon returned empty error".to_string()
                } else {
                    format!("daemon error: {msg}")
                }
            }
        }
    }
}

// ── Trait ──────────────────────────────────────────────────────────────────

pub trait PromptProcessor: Send + Sync {
    fn name(&self) -> &str;
    fn compile(
        &self,
        input: &str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<CompileResult, PromptProcessorError>>
                + Send
                + '_,
        >,
    >;

    /// Send an arbitrary system/user message pair to the model and return raw text.
    /// Used by the AI assistant (command mode + chat mode) — no compilation post-processing.
    fn send(
        &self,
        system_prompt: &str,
        user_message: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, PromptProcessorError>> + Send + '_>,
    >;
}

// ── RPC processor ──────────────────────────────────────────────────────────

pub struct RpcPromptProcessor {
    socket_path: PathBuf,
    model: String,
    provider: SessionProvider,
    base_url: Option<String>,
    api_key: Option<String>,
    /// Stable per-instance identity. Enables the daemon to supersede an
    /// in-flight compile when the user mashes Ctrl+Y a second time.
    caller_id: Uuid,
}

impl RpcPromptProcessor {
    pub fn new(
        socket_path: PathBuf,
        model: String,
        provider: SessionProvider,
        base_url: Option<String>,
        api_key: Option<String>,
        caller_id: Uuid,
    ) -> Self {
        Self {
            socket_path,
            model,
            provider,
            base_url,
            api_key,
            caller_id,
        }
    }
}

impl PromptProcessor for RpcPromptProcessor {
    fn name(&self) -> &str {
        &self.model
    }

    fn compile(
        &self,
        input: &str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<CompileResult, PromptProcessorError>>
                + Send
                + '_,
        >,
    > {
        let path = self.socket_path.clone();
        let model = self.model.clone();
        let provider = self.provider;
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();
        let caller_id = self.caller_id;
        let input = input.to_string();
        Box::pin(async move {
            compile_via_rpc(
                &path, &model, provider, base_url, api_key, caller_id, &input,
            )
            .await
        })
    }

    fn send(
        &self,
        system_prompt: &str,
        user_message: &str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<String, PromptProcessorError>> + Send + '_>,
    > {
        let path = self.socket_path.clone();
        let model = self.model.clone();
        let provider = self.provider;
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();
        let system = system_prompt.to_string();
        let user = user_message.to_string();
        Box::pin(async move {
            generate_text_via_rpc(&path, &model, provider, base_url, api_key, &system, &user).await
        })
    }
}

// ── Low-level RPC helpers (short-lived connections) ────────────────────────

async fn rpc_call(
    socket_path: &PathBuf,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, PromptProcessorError> {
    let stream = UnixStream::connect(socket_path).await?;
    let (reader, mut writer) = stream.into_split();
    let request = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::Value::Number(1.into())),
        method: method.to_string(),
        params,
        session_token: None,
    };
    let json = serde_json::to_string(&request)
        .map_err(|e| PromptProcessorError::OllamaError(e.to_string()))?;
    writer.write_all(format!("{}\n", json).as_bytes()).await?;

    let mut lines = BufReader::new(reader).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| PromptProcessorError::OllamaError("connection closed".into()))?;
    let resp: RpcResponse = serde_json::from_str(&line)
        .map_err(|e| PromptProcessorError::OllamaError(e.to_string()))?;
    if let Some(err) = resp.error {
        return Err(PromptProcessorError::OllamaError(err.message));
    }
    Ok(resp.result.unwrap_or(serde_json::Value::Null))
}

async fn compile_via_rpc(
    socket_path: &PathBuf,
    model: &str,
    provider: SessionProvider,
    base_url: Option<String>,
    api_key: Option<String>,
    caller_id: Uuid,
    input: &str,
) -> Result<CompileResult, PromptProcessorError> {
    // Open the subscribe connection FIRST so we never miss the completion
    // event for fast/cached requests.
    let sub_stream = UnixStream::connect(socket_path).await?;
    let (sub_reader, mut sub_writer) = sub_stream.into_split();
    let sub_req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::Value::Number(1.into())),
        method: "Subscribe".to_string(),
        params: serde_json::to_value(SubscribeParams {
            event_types: vec![
                "compile_prompt_completed".to_string(),
                "compile_prompt_failed".to_string(),
            ],
            session_id: None,
        })
        .map_err(|e| PromptProcessorError::OllamaError(e.to_string()))?,
        session_token: None,
    };
    let sub_json = serde_json::to_string(&sub_req)
        .map_err(|e| PromptProcessorError::OllamaError(e.to_string()))?;
    sub_writer
        .write_all(format!("{}\n", sub_json).as_bytes())
        .await?;

    let mut sub_lines = BufReader::new(sub_reader).lines();
    // Consume the ack line so subsequent reads yield only BusEvents.
    let _ack = sub_lines.next_line().await?;

    // Fire CompilePrompt on a second, short-lived connection.
    let params = CompilePromptParams {
        input: input.to_string(),
        model: Some(model.to_string()),
        provider: Some(provider),
        base_url,
        api_key,
        caller_id,
    };
    let params_value = serde_json::to_value(&params)
        .map_err(|e| PromptProcessorError::OllamaError(e.to_string()))?;
    let resp_value = rpc_call(socket_path, "CompilePrompt", params_value).await?;
    let resp: CompilePromptResponse = serde_json::from_value(resp_value)
        .map_err(|e| PromptProcessorError::OllamaError(e.to_string()))?;

    if let Some(cached) = resp.cached {
        return Ok(cached);
    }

    // Await the matching completed/failed event on the subscribe stream.
    let request_id = resp.request_id;
    loop {
        let line = match sub_lines.next_line().await? {
            Some(l) => l,
            None => {
                return Err(PromptProcessorError::OllamaError(
                    "subscribe stream closed before compile completion".into(),
                ));
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let event: BusEvent = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };
        match event.event_type.as_str() {
            "compile_prompt_completed" => {
                if event_request_id(&event.data) != Some(request_id) {
                    continue;
                }
                let result_value = event.data.get("result").cloned().unwrap_or_default();
                let result: CompileResult = serde_json::from_value(result_value).map_err(|e| {
                    PromptProcessorError::OllamaError(format!(
                        "malformed compile_prompt_completed: {e}"
                    ))
                })?;
                return Ok(result);
            }
            "compile_prompt_failed" => {
                if event_request_id(&event.data) != Some(request_id) {
                    continue;
                }
                let err = event
                    .data
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown compile failure")
                    .to_string();
                if let Some(desc) = err.strip_prefix("Ambiguous intent: ") {
                    return Err(PromptProcessorError::AmbiguousIntent(desc.to_string()));
                }
                return Err(PromptProcessorError::OllamaError(err));
            }
            _ => continue,
        }
    }
}

fn event_request_id(data: &serde_json::Value) -> Option<Uuid> {
    data.get("request_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
}

async fn generate_text_via_rpc(
    socket_path: &PathBuf,
    model: &str,
    provider: SessionProvider,
    base_url: Option<String>,
    api_key: Option<String>,
    system: &str,
    user: &str,
) -> Result<String, PromptProcessorError> {
    let params = GenerateTextParams {
        system: system.to_string(),
        prompt: user.to_string(),
        model: Some(model.to_string()),
        provider: Some(provider),
        base_url,
        api_key,
    };
    let params_value = serde_json::to_value(&params)
        .map_err(|e| PromptProcessorError::OllamaError(e.to_string()))?;
    let resp_value = rpc_call(socket_path, "GenerateText", params_value).await?;
    let resp: GenerateTextResponse = serde_json::from_value(resp_value)
        .map_err(|e| PromptProcessorError::OllamaError(e.to_string()))?;
    let text = resp.text.trim().to_string();
    if text.is_empty() {
        return Err(PromptProcessorError::EmptyResponse);
    }
    Ok(text)
}

// ── Factory ────────────────────────────────────────────────────────────────

/// Build a processor from config. Returns `None` when disabled.
/// Daemon availability is no longer pre-checked — if the daemon is not
/// reachable, compile/send calls surface their own IO error instead.
pub fn build_processor(config: &PromptProcessorConfig) -> Option<Box<dyn PromptProcessor>> {
    if !config.enabled {
        return None;
    }
    let socket_path = crate::client::DaemonClient::default_socket_path();
    Some(Box::new(RpcPromptProcessor::new(
        socket_path,
        config.model.clone(),
        config.provider,
        config.custom_base_url.clone(),
        config.custom_api_key.clone(),
        Uuid::new_v4(),
    )))
}

/// Return a processor configuration targeted at a model selected for one prompt.
///
/// The new-session modal's model picker is independent from the persistent
/// processor preference. Preserve credentials only when selection identifies a
/// configured custom endpoint; otherwise clear them so they cannot be sent to
/// an unrelated provider.
pub fn config_for_selected_model(
    config: &PromptProcessorConfig,
    model: String,
    provider: SessionProvider,
    custom_provider: Option<&CustomProviderEntry>,
) -> PromptProcessorConfig {
    let mut selected = config.clone();
    selected.model = model;
    selected.provider = provider;
    if let Some(custom_provider) = custom_provider {
        selected.custom_provider_id = Some(custom_provider.id);
        selected.custom_base_url = Some(custom_provider.base_url.clone());
        selected.custom_api_key = Some(custom_provider.api_key.clone());
    } else {
        selected.custom_provider_id = None;
        selected.custom_base_url = None;
        selected.custom_api_key = None;
    }
    selected
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_string_preserved_across_migration() {
        assert_eq!(OutputContract::Complete.to_status_string(), "complete");
        assert_eq!(
            OutputContract::Incomplete {
                criterion: "x".into()
            }
            .to_status_string(),
            "incomplete:x"
        );
        assert_eq!(
            OutputContract::Error {
                kind: "K".into(),
                message: "M".into()
            }
            .to_status_string(),
            "error:K:M"
        );
    }

    #[test]
    fn layer_validation_missing_surfaces_absent_layers() {
        let v = LayerValidation {
            semantic: true,
            syntactic: false,
            deictic: true,
            discourse: false,
            pragmatic: true,
        };
        assert!(!v.all_present());
        assert_eq!(v.missing(), vec!["SYNTACTIC", "DISCOURSE"]);
    }

    #[test]
    fn error_message_surfacing() {
        let e = PromptProcessorError::AmbiguousIntent("unclear".into());
        assert_eq!(e.user_facing_message(), "Ambiguous: unclear");

        let e = PromptProcessorError::Disabled;
        assert_eq!(e.user_facing_message(), "Prompt processor is disabled");
    }

    #[test]
    fn selected_builtin_model_replaces_local_processor_target() {
        let mut configured = PromptProcessorConfig::default();
        configured.custom_base_url = Some("https://example.invalid/v1".into());
        configured.custom_api_key = Some("secret".into());

        let selected = config_for_selected_model(
            &configured,
            "claude-sonnet-5".into(),
            SessionProvider::Claude,
            None,
        );

        assert_eq!(selected.model, "claude-sonnet-5");
        assert_eq!(selected.provider, SessionProvider::Claude);
        assert!(selected.custom_base_url.is_none());
        assert!(selected.custom_api_key.is_none());
    }

    #[test]
    fn selected_custom_model_keeps_its_endpoint_credentials() {
        let configured = PromptProcessorConfig::default();
        let custom = CustomProviderEntry {
            id: Uuid::new_v4(),
            name: "Example API".into(),
            base_url: "https://example.invalid/v1".into(),
            api_key: "secret".into(),
            default_model: "example-model".into(),
        };

        let selected = config_for_selected_model(
            &configured,
            "selected-model".into(),
            SessionProvider::Local,
            Some(&custom),
        );

        assert_eq!(selected.model, "selected-model");
        assert_eq!(selected.custom_provider_id, Some(custom.id));
        assert_eq!(
            selected.custom_base_url.as_deref(),
            Some(custom.base_url.as_str())
        );
        assert_eq!(
            selected.custom_api_key.as_deref(),
            Some(custom.api_key.as_str())
        );
    }
}
