use crate::bus::EventBus;
use crate::error::{DaemonError, Result};
use crate::model_control::registry::RuntimeExecutionRoute;
use crate::model_control::{
    AdmissionDecision, AdmissionPermit, CliExecutionCapability, ModelAdmissionRequest,
    ModelExecutionCapability, admit_invocation, classify_error_class, completion_with_wall_time,
    settle_result,
};
use crate::process_control::{
    CaptureError, CaptureLimits, CapturedOutput, capture_bounded_with_spawn,
};
use crate::session::harness::provider::{ApiProvider, resolve_provider};
use crate::session::harness::providers::openai_api::OpenAiApiProvider;
use crate::session::harness::types::{ChatMessage, ChatRequest, ProviderQuirks};
use crate::store::Store;
use rsi_common::model_control::ModelUsageConfidence;
use rsi_common::types::SessionProvider;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct MemoryLlmTarget {
    pub provider: SessionProvider,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

impl std::fmt::Debug for MemoryLlmTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MemoryLlmTarget")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LlmExecutionPath {
    ClaudeCli,
    CodexCli,
    AgyCli,
    OpenAiCompatibleApi,
    OllamaLocal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApiRouteKind {
    Local,
    Remote,
}

#[derive(Debug, Clone)]
struct ResolvedMemoryLlmTarget {
    target: MemoryLlmTarget,
    execution_path: LlmExecutionPath,
    api_route: Option<ApiRouteKind>,
    local_env_override: bool,
}

fn classify_api_route(base_url: &str) -> Result<ApiRouteKind> {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        return Err(DaemonError::InvalidParam(
            "OpenAI-compatible base_url cannot be empty".to_string(),
        ));
    }

    let parsed = reqwest::Url::parse(trimmed).map_err(|e| {
        DaemonError::InvalidParam(format!(
            "Invalid OpenAI-compatible base_url '{trimmed}': {e}"
        ))
    })?;

    match parsed.scheme() {
        "http" | "https" => {}
        scheme => {
            return Err(DaemonError::InvalidParam(format!(
                "Unsupported OpenAI-compatible base_url scheme '{scheme}'"
            )));
        }
    }

    let host = parsed.host_str().ok_or_else(|| {
        DaemonError::InvalidParam(format!(
            "OpenAI-compatible base_url '{trimmed}' must include a host"
        ))
    })?;

    if host.eq_ignore_ascii_case("localhost") {
        return Ok(ApiRouteKind::Local);
    }

    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(if ip.is_loopback() {
            ApiRouteKind::Local
        } else {
            ApiRouteKind::Remote
        });
    }

    Ok(ApiRouteKind::Remote)
}

fn infer_execution_path(target: &MemoryLlmTarget) -> Result<LlmExecutionPath> {
    if let Some(base_url) = target
        .base_url
        .as_deref()
        .filter(|url| !url.trim().is_empty())
    {
        let _ = classify_api_route(base_url)?;
        return Ok(LlmExecutionPath::OpenAiCompatibleApi);
    }

    // Codex-backed sessions authenticate through the logged-in Codex CLI, not
    // an OPENAI_API_KEY. Check the provider before model-family inference so a
    // `gpt-*` selection does not silently become an unauthenticated HTTP call.
    if matches!(
        target.provider,
        SessionProvider::Codex
            | SessionProvider::CodexAppServer
            | SessionProvider::Pioneer
            | SessionProvider::OpenRouter
            | SessionProvider::Bedrock
    ) {
        return Ok(LlmExecutionPath::CodexCli);
    }

    let model = target.model.to_ascii_lowercase();
    if target.provider == SessionProvider::Harness && model.starts_with("claude-") {
        return Ok(LlmExecutionPath::OpenAiCompatibleApi);
    }
    if model.starts_with("claude-") {
        return Ok(LlmExecutionPath::ClaudeCli);
    }
    if model.starts_with("gemini-") {
        return Ok(LlmExecutionPath::AgyCli);
    }
    if model.starts_with("gpt-")
        || model.starts_with("o1-")
        || model.starts_with("o3-")
        || model.starts_with("o4-")
        || crate::session::harness::compatible_table::lookup(&model).is_some()
    {
        return Ok(LlmExecutionPath::OpenAiCompatibleApi);
    }

    Ok(match target.provider {
        SessionProvider::Claude => LlmExecutionPath::ClaudeCli,
        SessionProvider::Antigravity => LlmExecutionPath::AgyCli,
        SessionProvider::Codex
        | SessionProvider::Pioneer
        | SessionProvider::OpenRouter
        | SessionProvider::Bedrock
        | SessionProvider::CodexAppServer
        | SessionProvider::Harness => LlmExecutionPath::OpenAiCompatibleApi,
        _ => LlmExecutionPath::OllamaLocal,
    })
}

fn resolve_target(target: &MemoryLlmTarget) -> Result<ResolvedMemoryLlmTarget> {
    let execution_path = infer_execution_path(target)?;
    let mut target = target.clone();
    let mut local_env_override = false;
    if target.provider == SessionProvider::Pioneer {
        let credential = crate::pioneer::pioneer_credential_from_env()
            .map_err(|error| DaemonError::Process(error.to_string()))?;
        target.base_url = Some(crate::pioneer::PIONEER_API_BASE_URL.to_string());
        target.api_key = Some(credential.value().to_string());
    }
    if execution_path == LlmExecutionPath::OllamaLocal {
        let configured_base_url = std::env::var("LOCAL_LLM_BASE_URL")
            .ok()
            .filter(|url| !url.trim().is_empty());
        local_env_override = configured_base_url.is_some();
        target.base_url =
            Some(configured_base_url.unwrap_or_else(|| "http://localhost:11434/v1".to_string()));
        target.api_key = target
            .api_key
            .or_else(|| std::env::var("LOCAL_LLM_API_KEY").ok());
    }
    let api_route = match execution_path {
        LlmExecutionPath::OpenAiCompatibleApi | LlmExecutionPath::OllamaLocal => target
            .base_url
            .as_deref()
            .map(classify_api_route)
            .transpose()?,
        LlmExecutionPath::ClaudeCli | LlmExecutionPath::CodexCli | LlmExecutionPath::AgyCli => None,
    };
    Ok(ResolvedMemoryLlmTarget {
        target,
        execution_path,
        api_route,
        local_env_override,
    })
}

pub(crate) fn uses_native_ollama(target: &MemoryLlmTarget) -> Result<bool> {
    let resolved = resolve_target(target)?;
    Ok(resolved.execution_path == LlmExecutionPath::OllamaLocal && !resolved.local_env_override)
}

fn resolved_backend_label(resolved: &ResolvedMemoryLlmTarget) -> &'static str {
    match resolved.execution_path {
        LlmExecutionPath::ClaudeCli => "claude_cli",
        LlmExecutionPath::CodexCli => "codex_cli",
        LlmExecutionPath::AgyCli => "agy_cli",
        LlmExecutionPath::OllamaLocal if !resolved.local_env_override => "ollama",
        LlmExecutionPath::OpenAiCompatibleApi | LlmExecutionPath::OllamaLocal => {
            if resolved.api_route == Some(ApiRouteKind::Local) {
                "openai_compatible_local"
            } else {
                "openai_compatible_api"
            }
        }
    }
}

fn resolved_provider_label(resolved: &ResolvedMemoryLlmTarget) -> String {
    if resolved.target.provider == SessionProvider::Pioneer {
        return "Pioneer".to_string();
    }
    match resolved.execution_path {
        LlmExecutionPath::ClaudeCli => "Claude".to_string(),
        LlmExecutionPath::CodexCli => match resolved.target.provider {
            SessionProvider::Pioneer => "Pioneer".to_string(),
            SessionProvider::OpenRouter => "OpenRouter".to_string(),
            SessionProvider::Bedrock => "Bedrock".to_string(),
            SessionProvider::CodexAppServer => "CodexAppServer".to_string(),
            _ => "Codex".to_string(),
        },
        LlmExecutionPath::AgyCli => "Antigravity".to_string(),
        LlmExecutionPath::OpenAiCompatibleApi | LlmExecutionPath::OllamaLocal => {
            if resolved.api_route == Some(ApiRouteKind::Local) {
                "Local".to_string()
            } else {
                "Remote".to_string()
            }
        }
    }
}

pub(crate) fn backend_label(target: &MemoryLlmTarget) -> Result<&'static str> {
    let resolved = resolve_target(target)?;
    Ok(resolved_backend_label(&resolved))
}

pub(crate) fn provider_label(target: &MemoryLlmTarget) -> Result<String> {
    let resolved = resolve_target(target)?;
    Ok(resolved_provider_label(&resolved))
}

pub async fn admit_and_generate_text(
    store: &Arc<Mutex<Store>>,
    event_bus: &Arc<EventBus>,
    mut request: ModelAdmissionRequest,
    target: &MemoryLlmTarget,
    prompt: &str,
    max_tokens: u32,
    purpose: &str,
) -> Result<String> {
    let resolved = resolve_target(target)?;
    let provider = resolved_provider_label(&resolved);
    let backend = resolved_backend_label(&resolved);
    request.provider = Some(provider);
    request.model = Some(resolved.target.model.clone());
    request.backend = Some(backend.to_string());
    let dedup_key = request.dedup_key.clone();

    let permit = match admit_invocation(store, request, event_bus).await? {
        AdmissionDecision::Admitted(permit) => permit,
        AdmissionDecision::Duplicate { invocation_id } => {
            let duplicate_state = if let Some(key) = dedup_key.as_deref() {
                let guard = store.lock().await;
                crate::model_control::lookup_existing_invocation_by_dedup(&guard, key)?
            } else {
                None
            };
            let detail = duplicate_state
                .map(|existing| {
                    format!(
                        "duplicate model invocation {purpose} already {} ({})",
                        existing.status, existing.invocation_id
                    )
                })
                .unwrap_or_else(|| {
                    format!("duplicate model invocation suppressed for {purpose} ({invocation_id})")
                });
            return Err(DaemonError::PolicyDenied(format!("{detail}")));
        }
    };
    let started_at = Instant::now();
    let result = generate_text_cancellable_resolved(
        &permit,
        &resolved,
        prompt,
        max_tokens,
        purpose,
        &CancellationToken::new(),
    )
    .await;
    let completion = match &result {
        Ok(_) => completion_with_wall_time(started_at, None, ModelUsageConfidence::Partial),
        Err(error) => completion_with_wall_time(
            started_at,
            Some(classify_error_class(error)),
            ModelUsageConfidence::Partial,
        ),
    };
    settle_result(store, &permit, completion, result, purpose, event_bus).await
}

pub async fn generate_text(
    _permit: &AdmissionPermit,
    target: &MemoryLlmTarget,
    prompt: &str,
    max_tokens: u32,
    purpose: &str,
) -> Result<String> {
    generate_text_cancellable(
        _permit,
        target,
        prompt,
        max_tokens,
        purpose,
        &CancellationToken::new(),
    )
    .await
}

/// Executes a permitted model call while honoring local cancellation.
///
/// HTTP transports can only abandon the local request; provider-side remote
/// cancellation is not universally available. CLI transports are stronger:
/// cancellation kills and reaps the owned child before this function returns.
pub async fn generate_text_cancellable(
    _permit: &AdmissionPermit,
    target: &MemoryLlmTarget,
    prompt: &str,
    max_tokens: u32,
    purpose: &str,
    cancel: &CancellationToken,
) -> Result<String> {
    let resolved = resolve_target(target)?;
    generate_text_cancellable_resolved(_permit, &resolved, prompt, max_tokens, purpose, cancel)
        .await
}

async fn generate_text_cancellable_resolved(
    _permit: &AdmissionPermit,
    resolved: &ResolvedMemoryLlmTarget,
    prompt: &str,
    max_tokens: u32,
    purpose: &str,
    cancel: &CancellationToken,
) -> Result<String> {
    if cancel.is_cancelled() {
        return Err(DaemonError::ChannelClosed);
    }
    let target = &resolved.target;
    match resolved.execution_path {
        LlmExecutionPath::ClaudeCli => {
            generate_claude_cli(_permit, target, prompt, purpose, cancel).await
        }
        LlmExecutionPath::CodexCli => {
            generate_codex_cli(_permit, target, prompt, purpose, cancel).await
        }
        LlmExecutionPath::AgyCli => {
            generate_agy_cli(_permit, target, prompt, purpose, cancel).await
        }
        LlmExecutionPath::OpenAiCompatibleApi => {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(DaemonError::ChannelClosed),
                result = generate_harness_api(_permit, target, prompt, max_tokens) => result,
            }
        }
        LlmExecutionPath::OllamaLocal => {
            let execution =
                _permit.claim_model_execution(RuntimeExecutionRoute::SessionHarnessOpenAiHttp)?;
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(DaemonError::ChannelClosed),
                result = generate_local_or_custom_api(target, prompt, max_tokens, execution) => result,
            }
        }
    }
}

async fn generate_harness_api(
    permit: &AdmissionPermit,
    target: &MemoryLlmTarget,
    prompt: &str,
    max_tokens: u32,
) -> Result<String> {
    let provider = resolve_provider(
        &target.model,
        target.base_url.as_deref(),
        target.api_key.as_deref(),
    )?;
    let execution_route = if provider.name() == "anthropic" {
        RuntimeExecutionRoute::SessionHarnessAnthropicHttp
    } else {
        RuntimeExecutionRoute::SessionHarnessOpenAiHttp
    };
    let execution = permit.claim_model_execution(execution_route)?;
    generate_api(provider, &target.model, prompt, max_tokens, execution).await
}

async fn generate_local_or_custom_api(
    target: &MemoryLlmTarget,
    prompt: &str,
    max_tokens: u32,
    execution: ModelExecutionCapability,
) -> Result<String> {
    let base_url = target
        .base_url
        .clone()
        .or_else(|| std::env::var("LOCAL_LLM_BASE_URL").ok())
        .unwrap_or_else(|| "http://localhost:11434/v1".to_string());
    let api_key = target
        .api_key
        .clone()
        .or_else(|| std::env::var("LOCAL_LLM_API_KEY").ok());
    let provider = OpenAiApiProvider::with_config(base_url, api_key, ProviderQuirks::default())?;
    generate_api(
        Box::new(provider),
        &target.model,
        prompt,
        max_tokens,
        execution,
    )
    .await
}

async fn generate_api(
    provider: Box<dyn ApiProvider>,
    model: &str,
    prompt: &str,
    max_tokens: u32,
    execution: ModelExecutionCapability,
) -> Result<String> {
    let response = provider
        .chat(
            &ChatRequest {
                messages: vec![ChatMessage::user(prompt)],
                model: model.to_string(),
                temperature: Some(0.3),
                max_tokens: Some(max_tokens),
                tools: Vec::new(),
                stream: false,
                reasoning_effort: None,
            },
            execution,
        )
        .await?;
    let text = response.content.trim().to_string();
    if text.is_empty() {
        return Err(DaemonError::Store(format!(
            "{} returned empty response",
            provider.name()
        )));
    }
    Ok(text)
}

async fn generate_claude_cli(
    permit: &AdmissionPermit,
    target: &MemoryLlmTarget,
    prompt: &str,
    purpose: &str,
    cancel: &CancellationToken,
) -> Result<String> {
    let execution = permit.claim_cli_execution(RuntimeExecutionRoute::MemoryCli)?;
    let mut command = tokio::process::Command::new("claude");
    command.args([
        "-p",
        prompt,
        "--model",
        &target.model,
        "--output-format",
        "text",
        "--no-session-persistence",
        "--permission-mode",
        "bypassPermissions",
    ]);
    let output = run_cancellable_cli(command, execution, purpose, cancel).await?;

    process_cli_output(output, purpose, "Claude", &target.model)
}

async fn generate_codex_cli(
    permit: &AdmissionPermit,
    target: &MemoryLlmTarget,
    prompt: &str,
    purpose: &str,
    cancel: &CancellationToken,
) -> Result<String> {
    let execution = permit.claim_cli_execution(RuntimeExecutionRoute::MemoryCli)?;
    let mut command = tokio::process::Command::new("codex");
    command.args([
        "exec",
        "--skip-git-repo-check",
        "--sandbox",
        "read-only",
        "--ephemeral",
        "--color",
        "never",
    ]);

    let model = if target.provider == SessionProvider::Pioneer {
        let credential = crate::pioneer::pioneer_credential_from_env()
            .map_err(|error| DaemonError::Process(error.to_string()))?;
        command.env(credential.source().env_name(), credential.value());
        crate::pioneer::PioneerCodexConfigOverrides::new(
            credential.source(),
            crate::pioneer::existing_pioneer_codex_catalog_path().as_deref(),
        )
        .map_err(|error| DaemonError::Process(error.to_string()))?
        .append_to(&mut command);
        crate::pioneer::pioneer_launch_model(Some(&target.model))
    } else {
        if target.provider == SessionProvider::Bedrock {
            let credential = crate::bedrock::credential().map_err(DaemonError::Process)?;
            command.env(crate::bedrock::BEDROCK_ENV, credential);
            let region = crate::bedrock::region().map_err(DaemonError::Process)?;
            let binary = which::which("codex").unwrap_or_else(|_| "codex".into());
            crate::bedrock::CodexOverrides::for_launch(&region, &binary, Some(&target.model))
                .append_to(&mut command);
        } else if target.provider == SessionProvider::OpenRouter {
            let credential = crate::openrouter::openrouter_credential_from_env()
                .map_err(|error| DaemonError::Process(error.to_string()))?;
            crate::openrouter::OpenRouterCodexConfigOverrides::new(&credential)
                .append_to(&mut command);
        }
        &target.model
    };
    command.args(["--model", model, prompt]);

    let output = run_cancellable_cli(command, execution, purpose, cancel).await?;
    process_cli_output(output, purpose, "Codex", model)
}

async fn generate_agy_cli(
    permit: &AdmissionPermit,
    target: &MemoryLlmTarget,
    prompt: &str,
    purpose: &str,
    cancel: &CancellationToken,
) -> Result<String> {
    let execution = permit.claim_cli_execution(RuntimeExecutionRoute::MemoryCli)?;
    let binary_path = which::which("agy")
        .or_else(|_| which::which("antigravity"))
        .or_else(|_| which::which("antigravity-cli"))
        .unwrap_or_else(|_| std::path::PathBuf::from("agy"));

    let mut cmd = tokio::process::Command::new(&binary_path);
    cmd.arg("-p").arg(prompt).args(["--model", &target.model]);
    cmd.arg("--dangerously-skip-permissions");

    let output = run_cancellable_cli(cmd, execution, purpose, cancel).await?;

    process_cli_output(output, purpose, "Antigravity", &target.model)
}

async fn run_cancellable_cli(
    command: tokio::process::Command,
    execution: CliExecutionCapability,
    purpose: &str,
    cancel: &CancellationToken,
) -> Result<CapturedOutput> {
    run_cancellable_cli_with_limits(
        command,
        execution,
        purpose,
        cancel,
        CaptureLimits::memory_cli(),
    )
    .await
}

async fn run_cancellable_cli_with_limits(
    command: tokio::process::Command,
    execution: CliExecutionCapability,
    purpose: &str,
    cancel: &CancellationToken,
    limits: CaptureLimits,
) -> Result<CapturedOutput> {
    // The non-clone capability is consumed at the immediate process boundary;
    // the separate admission permit stays with the caller for settlement after
    // this function has killed, drained, and reaped the owned child.
    capture_bounded_with_spawn(command, limits, cancel, move |command| {
        execution
            .bind_command(RuntimeExecutionRoute::MemoryCli, command)
            .spawn()
            .map_err(|error| CaptureError::Spawn(error.to_string()))
    })
    .await
    .map_err(|error| match error {
        CaptureError::Cancelled => DaemonError::ChannelClosed,
        error if cancel.is_cancelled() => DaemonError::CancellationCleanup(format!(
            "{purpose} cleanup failed after cancellation: {error}"
        )),
        error => DaemonError::Store(format!("{purpose} failed: {error}")),
    })
}

fn process_cli_output(
    output: CapturedOutput,
    purpose: &str,
    provider: &str,
    model: &str,
) -> Result<String> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            format!("exit status {}", output.status)
        } else {
            format!("exit status {}; stderr: {}", output.status, stderr)
        };
        return Err(DaemonError::Store(format!(
            "{purpose} fallback failed using {provider} model '{model}': {detail}"
        )));
    }

    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        return Err(DaemonError::Store(format!(
            "{purpose} fallback returned empty response using {provider} model '{model}'"
        )));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::EventBus;
    use crate::model_control::{
        ModelControlRuntime, complete_invocation, request_invocation_cancellation,
    };
    use rsi_common::model_control::{
        InvocationOwner, ModelControlMode, ModelInvocationPurpose, ModelInvocationStatus,
    };
    use std::path::Path;
    use tempfile::TempDir;
    use tokio::net::TcpListener;
    use tokio::time::{Duration, timeout};
    use uuid::Uuid;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn target(provider: SessionProvider, model: &str) -> MemoryLlmTarget {
        MemoryLlmTarget {
            provider,
            model: model.to_string(),
            base_url: None,
            api_key: None,
        }
    }

    #[test]
    fn memory_llm_target_debug_redacts_api_credentials() {
        let mut target = target(SessionProvider::Pioneer, "claude-sonnet-5");
        target.api_key = Some("fixture-pioneer-secret".to_string());

        let debug = format!("{target:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("fixture-pioneer-secret"));
    }

    #[test]
    fn infer_execution_path_prefers_explicit_base_url() {
        let mut target = target(SessionProvider::Local, "claude-sonnet-5");
        target.base_url = Some("https://example.com/v1".to_string());
        assert_eq!(
            infer_execution_path(&target).expect("route"),
            LlmExecutionPath::OpenAiCompatibleApi
        );
    }

    #[test]
    fn infer_execution_path_uses_codex_cli_for_codex_backed_models() {
        for provider in [
            SessionProvider::Codex,
            SessionProvider::CodexAppServer,
            SessionProvider::Pioneer,
        ] {
            assert_eq!(
                infer_execution_path(&target(provider, "gpt-6-astra"))
                    .expect("Codex-backed provider route"),
                LlmExecutionPath::CodexCli
            );
        }
    }

    #[test]
    fn infer_execution_path_prefers_model_family_over_local_provider() {
        assert_eq!(
            infer_execution_path(&target(SessionProvider::Local, "claude-sonnet-5"))
                .expect("route"),
            LlmExecutionPath::ClaudeCli
        );
        assert_eq!(
            infer_execution_path(&target(SessionProvider::Local, "gemini-3.5-pro-high"))
                .expect("route"),
            LlmExecutionPath::AgyCli
        );
        assert_eq!(
            infer_execution_path(&target(SessionProvider::Local, "gpt-5.5")).expect("route"),
            LlmExecutionPath::OpenAiCompatibleApi
        );
        assert_eq!(
            infer_execution_path(&target(SessionProvider::Local, "mercury-2")).expect("route"),
            LlmExecutionPath::OpenAiCompatibleApi
        );
    }

    #[test]
    fn infer_execution_path_falls_back_to_provider_when_model_is_ambiguous() {
        assert_eq!(
            infer_execution_path(&target(SessionProvider::Claude, "custom-reasoner"))
                .expect("route"),
            LlmExecutionPath::ClaudeCli
        );
        assert_eq!(
            infer_execution_path(&target(SessionProvider::Harness, "custom-reasoner"))
                .expect("route"),
            LlmExecutionPath::OpenAiCompatibleApi
        );
        assert_eq!(
            infer_execution_path(&target(SessionProvider::Local, "qwen3:14b")).expect("route"),
            LlmExecutionPath::OllamaLocal
        );
    }

    #[test]
    fn invalid_base_url_is_rejected() {
        let mut target = target(SessionProvider::Harness, "gpt-5.4");
        target.base_url = Some("not a url".to_string());

        let error = provider_label(&target).expect_err("invalid url must fail closed");
        assert!(
            error
                .to_string()
                .contains("Invalid OpenAI-compatible base_url")
        );
    }

    #[test]
    fn loopback_base_url_uses_openai_compatible_transport() {
        let mut target = target(SessionProvider::Harness, "gpt-5.4");
        target.base_url = Some("http://127.0.0.1:8000/v1".to_string());

        assert_eq!(provider_label(&target).expect("provider"), "Local");
        assert!(!uses_native_ollama(&target).expect("route"));
        assert_eq!(
            backend_label(&target).expect("backend"),
            "openai_compatible_local"
        );
    }

    fn http_request(
        purpose: ModelInvocationPurpose,
        target: &MemoryLlmTarget,
        dedup_key: &str,
    ) -> ModelAdmissionRequest {
        let provider = provider_label(target).expect("resolved provider");
        let backend = backend_label(target).expect("resolved backend");
        ModelAdmissionRequest {
            purpose,
            provider: Some(provider.clone()),
            model: Some(target.model.clone()),
            backend: Some(backend.to_string()),
            effort: None,
            trigger: "memory_llm_http_test".to_string(),
            owner: InvocationOwner::default(),
            dedup_key: Some(dedup_key.to_string()),
            request_fingerprint: Some(format!("test:{dedup_key}")),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(crate::model_control::explicit_expected_usage(
                purpose,
                Some(&provider),
                Some(backend),
                Some(&target.model),
            )),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        }
    }

    async fn admitted_http_permit(
        store: &Arc<Mutex<Store>>,
        bus: &Arc<EventBus>,
        target: &MemoryLlmTarget,
        dedup_key: &str,
    ) -> AdmissionPermit {
        match admit_invocation(
            store,
            http_request(ModelInvocationPurpose::PromptCompile, target, dedup_key),
            bus,
        )
        .await
        .expect("memory HTTP admission")
        {
            AdmissionDecision::Admitted(permit) => permit,
            AdmissionDecision::Duplicate { invocation_id } => {
                panic!("unexpected duplicate admission: {invocation_id}")
            }
        }
    }

    #[tokio::test]
    async fn memory_openai_loopback_reaches_session_harness_route_exactly_once() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{
                    "message": {"content": "openai once"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let target = MemoryLlmTarget {
            provider: SessionProvider::Harness,
            model: "gpt-test".to_string(),
            base_url: Some(format!("{}/v1", server.uri())),
            api_key: None,
        };
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(EventBus::new(8));
        let permit = admitted_http_permit(&store, &bus, &target, "openai-loopback").await;

        let response = generate_text(&permit, &target, "hello", 32, "OpenAI loopback")
            .await
            .expect("successful OpenAI loopback");

        assert_eq!(response, "openai once");
        server.verify().await;
    }

    #[tokio::test]
    async fn memory_anthropic_loopback_reaches_session_harness_route_exactly_once() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "anthropic once"}],
                "usage": {"input_tokens": 1, "output_tokens": 2},
                "stop_reason": "end_turn"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let target = MemoryLlmTarget {
            provider: SessionProvider::Harness,
            model: "claude-test".to_string(),
            base_url: None,
            api_key: Some("test-key".to_string()),
        };
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(EventBus::new(8));

        let response =
            temp_env::async_with_vars([("ANTHROPIC_API_BASE_URL", Some(server.uri()))], async {
                let permit =
                    admitted_http_permit(&store, &bus, &target, "anthropic-loopback").await;
                generate_text(&permit, &target, "hello", 32, "Anthropic loopback").await
            })
            .await
            .expect("successful Anthropic loopback");

        assert_eq!(response, "anthropic once");
        server.verify().await;
    }

    async fn assert_remote_memory_target_denied(target: &MemoryLlmTarget) {
        for mode in [ModelControlMode::DenyPaid, ModelControlMode::LocalOnly] {
            let store = Store::open_in_memory().expect("store");
            store.set_model_control_mode(mode).expect("control mode");
            let store = Arc::new(Mutex::new(store));
            let bus = Arc::new(EventBus::new(8));
            let request = http_request(
                ModelInvocationPurpose::PromptCompile,
                target,
                &format!("remote-{mode:?}-{}", Uuid::new_v4()),
            );
            let mut request = request;
            request.provider = Some("Local".to_string());
            request.backend = Some("ollama".to_string());
            let error =
                admit_and_generate_text(&store, &bus, request, target, "no network", 16, "denial")
                    .await
                    .expect_err("remote target must be denied before transport");
            assert!(matches!(error, DaemonError::PolicyDenied(_)), "{error}");
        }
    }

    #[tokio::test]
    async fn explicit_remote_local_provider_target_is_denied_by_factory_admission() {
        let target = MemoryLlmTarget {
            provider: SessionProvider::Local,
            model: "qwen-test".to_string(),
            base_url: Some("https://memory-llm.remote.invalid/v1".to_string()),
            api_key: None,
        };
        assert_eq!(provider_label(&target).expect("provider"), "Remote");
        assert_eq!(
            backend_label(&target).expect("backend"),
            "openai_compatible_api"
        );
        assert_remote_memory_target_denied(&target).await;
    }

    #[tokio::test]
    async fn environment_remote_local_provider_target_is_denied_by_factory_admission() {
        let target = target(SessionProvider::Local, "qwen-test");
        temp_env::async_with_vars(
            [(
                "LOCAL_LLM_BASE_URL",
                Some("https://memory-llm.remote.invalid/v1"),
            )],
            async {
                assert_eq!(provider_label(&target).expect("provider"), "Remote");
                assert_eq!(
                    backend_label(&target).expect("backend"),
                    "openai_compatible_api"
                );
                assert!(
                    !uses_native_ollama(&target).expect("resolved transport"),
                    "LOCAL_LLM_BASE_URL must select the admitted compatible transport"
                );
                assert_remote_memory_target_denied(&target).await;
            },
        )
        .await;
    }

    #[tokio::test]
    async fn pre_cancelled_direct_http_never_claims_or_connects_and_settles_truthfully() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local HTTP listener");
        let address = listener.local_addr().expect("listener address");
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(EventBus::new(8));
        let session_id = Uuid::new_v4();
        let request = ModelAdmissionRequest {
            purpose: ModelInvocationPurpose::SessionLaunchFresh,
            provider: Some("Local".to_string()),
            model: Some("gpt-test".to_string()),
            backend: Some("openai_compatible_api".to_string()),
            effort: None,
            trigger: "pre_cancelled_direct_http".to_string(),
            owner: InvocationOwner {
                session_id: Some(session_id),
                ..Default::default()
            },
            dedup_key: Some(format!("pre-cancel-http:{session_id}")),
            request_fingerprint: Some("sha256:pre-cancel-http".to_string()),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(crate::model_control::explicit_expected_usage(
                ModelInvocationPurpose::SessionLaunchFresh,
                Some("Local"),
                Some("openai_compatible_api"),
                Some("gpt-test"),
            )),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        };
        let permit = match admit_invocation(&store, request, &bus)
            .await
            .expect("admission")
        {
            AdmissionDecision::Admitted(permit) => permit,
            AdmissionDecision::Duplicate { invocation_id } => {
                panic!("unexpected duplicate admission: {invocation_id}")
            }
        };
        let invocation_id = permit.invocation_id();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let target = MemoryLlmTarget {
            provider: SessionProvider::Harness,
            model: "gpt-test".to_string(),
            base_url: Some(format!("http://{address}/v1")),
            api_key: None,
        };

        let result =
            generate_text_cancellable(&permit, &target, "hello", 32, "pre-cancel-http", &cancel)
                .await;
        assert!(matches!(result, Err(DaemonError::ChannelClosed)));
        let _unused_execution = permit
            .claim_model_execution(RuntimeExecutionRoute::SessionHarnessOpenAiHttp)
            .expect("pre-cancel must fail before claiming execution authority");
        assert!(
            timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "pre-cancelled direct HTTP must open zero connections"
        );

        complete_invocation(
            &store,
            &permit,
            crate::model_control::InvocationCompletion {
                error_class: Some("cancelled".to_string()),
                confidence: Some(ModelUsageConfidence::Unavailable),
                ..Default::default()
            },
            &bus,
        )
        .await
        .expect("settle pre-cancelled HTTP");
        let guard = store.lock().await;
        let row: (String, Option<String>) = guard
            .conn
            .query_row(
                "SELECT status, error_class FROM model_invocations WHERE id = ?1",
                rusqlite::params![invocation_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("invocation row");
        assert_eq!(row.0, "failed");
        assert_eq!(row.1.as_deref(), Some("cancelled"));
    }

    #[cfg(unix)]
    fn cli_request(purpose: ModelInvocationPurpose, dedup_key: &str) -> ModelAdmissionRequest {
        ModelAdmissionRequest {
            purpose,
            provider: Some("Claude".to_string()),
            model: Some("claude-opus-4-1".to_string()),
            backend: Some("claude_cli".to_string()),
            effort: None,
            trigger: "memory_llm_test".to_string(),
            owner: InvocationOwner {
                session_id: Some(Uuid::new_v4()),
                ..Default::default()
            },
            dedup_key: Some(dedup_key.to_string()),
            request_fingerprint: Some(format!("test:{dedup_key}")),
            parent_invocation_id: None,
            retry_of_invocation_id: None,
            expected_usage: Some(crate::model_control::explicit_expected_usage(
                purpose,
                Some("Claude"),
                Some("claude_cli"),
                Some("claude-opus-4-1"),
            )),
            baseline_input_tokens: 0,
            baseline_output_tokens: 0,
            baseline_cache_creation_tokens: 0,
            baseline_cache_read_tokens: 0,
            baseline_reasoning_tokens: 0,
            baseline_embedding_input_count: 0,
            baseline_wall_time_ms: 0,
        }
    }

    #[cfg(unix)]
    fn oversized_stub_command(marker: &Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg("printf spawned > \"$1\"; dd if=/dev/zero bs=65536 count=16 2>/dev/null | tr '\\000' o; dd if=/dev/zero bs=65536 count=16 2>/dev/null | tr '\\000' e >&2; while :; do sleep 1; done")
            .arg("model-control-stub")
            .arg(marker);
        command
    }

    #[cfg(unix)]
    fn cancellable_stub_command(marker: &Path) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg("printf spawned > \"$1\"; dd if=/dev/zero bs=65536 count=1 2>/dev/null | tr '\\000' o; dd if=/dev/zero bs=65536 count=1 2>/dev/null | tr '\\000' e >&2; while :; do sleep 1; done")
            .arg("model-control-stub")
            .arg(marker);
        command
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn memory_cli_child_receives_exact_durable_invocation_stamp() {
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(EventBus::new(8));
        let permit = match admit_invocation(
            &store,
            cli_request(ModelInvocationPurpose::PromptCompile, "memory-cli-stamp"),
            &bus,
        )
        .await
        .expect("admission")
        {
            AdmissionDecision::Admitted(permit) => permit,
            AdmissionDecision::Duplicate { invocation_id } => {
                panic!("unexpected duplicate admission: {invocation_id}")
            }
        };
        let invocation_id = permit.invocation_id();
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg(format!(
            "printf %s \"${}\"",
            rsi_common::identity::ENV_MODEL_INVOCATION_ID
        ));

        let output = run_cancellable_cli(
            command,
            permit
                .claim_cli_execution(RuntimeExecutionRoute::MemoryCli)
                .expect("MemoryCli capability"),
            "memory-cli-stamp",
            &CancellationToken::new(),
        )
        .await
        .expect("run stamped MemoryCli child");

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            invocation_id.to_string()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn denied_cli_admission_never_spawns_the_local_stub() {
        let temp = TempDir::new().expect("tempdir");
        let marker = temp.path().join("spawned");
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(EventBus::new(8));

        let result = async {
            let permit = match admit_invocation(
                &store,
                cli_request(ModelInvocationPurpose::DreamConsolidation, "denied-cli"),
                &bus,
            )
            .await?
            {
                AdmissionDecision::Admitted(permit) => permit,
                AdmissionDecision::Duplicate { invocation_id } => {
                    return Err(DaemonError::PolicyDenied(format!(
                        "unexpected duplicate admission: {invocation_id}"
                    )));
                }
            };
            run_cancellable_cli(
                oversized_stub_command(&marker),
                permit.claim_cli_execution(RuntimeExecutionRoute::MemoryCli)?,
                "denied-cli",
                &CancellationToken::new(),
            )
            .await
        }
        .await;

        assert!(matches!(result, Err(DaemonError::PolicyDenied(_))));
        assert!(
            !marker.exists(),
            "denied admission must not create the local stub process"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pre_cancelled_cli_never_spawns_the_local_stub() {
        let temp = TempDir::new().expect("tempdir");
        let marker = temp.path().join("spawned");
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(EventBus::new(8));
        let permit = match admit_invocation(
            &store,
            cli_request(ModelInvocationPurpose::PromptCompile, "pre-cancel-cli"),
            &bus,
        )
        .await
        .expect("admission")
        {
            AdmissionDecision::Admitted(permit) => permit,
            AdmissionDecision::Duplicate { invocation_id } => {
                panic!("unexpected duplicate admission: {invocation_id}")
            }
        };
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = run_cancellable_cli(
            oversized_stub_command(&marker),
            permit
                .claim_cli_execution(RuntimeExecutionRoute::MemoryCli)
                .expect("first execution capability"),
            "pre-cancel-cli",
            &cancel,
        )
        .await;

        assert!(matches!(result, Err(DaemonError::ChannelClosed)));
        assert!(
            !marker.exists(),
            "an already-cancelled invocation must fail before spawning a CLI child"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_drains_reaps_and_settles_cli_stub_once() {
        let temp = TempDir::new().expect("tempdir");
        let marker = temp.path().join("spawned");
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(EventBus::new(8));
        let runtime = ModelControlRuntime::default_normal();
        let permit = match admit_invocation(
            &store,
            cli_request(ModelInvocationPurpose::PromptCompile, "cancel-cli"),
            &bus,
        )
        .await
        .expect("foreground admission")
        {
            AdmissionDecision::Admitted(permit) => permit,
            AdmissionDecision::Duplicate { invocation_id } => {
                panic!("unexpected duplicate admission: {invocation_id}")
            }
        };
        let invocation_id = permit.invocation_id();
        let cancel = CancellationToken::new();
        let started_at = Instant::now();
        let run = run_cancellable_cli(
            cancellable_stub_command(&marker),
            permit
                .claim_cli_execution(RuntimeExecutionRoute::MemoryCli)
                .expect("first execution capability"),
            "cancel-cli",
            &cancel,
        );
        tokio::pin!(run);
        let mut cancellation_requested = false;

        let result = timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    result = &mut run => break result,
                    _ = tokio::time::sleep(Duration::from_millis(10)), if !cancellation_requested => {
                        if marker.exists() {
                            request_invocation_cancellation(
                                &store,
                                &runtime,
                                &bus,
                                invocation_id,
                                "test cancellation",
                                "local_stub",
                            )
                            .await?;
                            cancel.cancel();
                            cancel.cancel();
                            cancellation_requested = true;
                        }
                    }
                }
            }
        })
        .await
        .expect("oversized pipes must not deadlock");

        assert!(
            cancellation_requested,
            "stub never reached its running state"
        );
        assert!(
            marker.exists(),
            "admitted CLI call must spawn the local stub"
        );
        assert!(matches!(result, Err(DaemonError::ChannelClosed)));
        assert!(
            permit
                .claim_cli_execution(RuntimeExecutionRoute::MemoryCli)
                .is_err(),
            "one admission must not authorize a second CLI child"
        );

        let completion = completion_with_wall_time(
            started_at,
            Some("cancelled".to_string()),
            ModelUsageConfidence::Unavailable,
        );
        complete_invocation(&store, &permit, completion.clone(), &bus)
            .await
            .expect("settle after child reap");
        complete_invocation(&store, &permit, completion, &bus)
            .await
            .expect("repeat settlement is idempotent");

        let guard = store.lock().await;
        let record = guard
            .load_model_invocation_record(invocation_id)
            .expect("load invocation")
            .expect("invocation exists");
        assert_eq!(record.status, ModelInvocationStatus::Cancelled);
        assert!(
            guard
                .build_model_control_status(8)
                .expect("control status")
                .active_invocations
                .is_empty(),
            "terminal settlement must release active counters exactly once"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn memory_cli_stderr_overflow_is_bounded_reaped_and_truthfully_settled() {
        let temp = TempDir::new().expect("tempdir");
        let marker = temp.path().join("spawned");
        let store = Arc::new(Mutex::new(Store::open_in_memory().expect("store")));
        let bus = Arc::new(EventBus::new(8));
        let permit = match admit_invocation(
            &store,
            cli_request(ModelInvocationPurpose::PromptCompile, "overflow-cli"),
            &bus,
        )
        .await
        .expect("foreground admission")
        {
            AdmissionDecision::Admitted(permit) => permit,
            AdmissionDecision::Duplicate { invocation_id } => {
                panic!("unexpected duplicate admission: {invocation_id}")
            }
        };
        let invocation_id = permit.invocation_id();
        let started_at = Instant::now();

        let error = timeout(
            Duration::from_secs(5),
            run_cancellable_cli(
                oversized_stub_command(&marker),
                permit
                    .claim_cli_execution(RuntimeExecutionRoute::MemoryCli)
                    .expect("first execution capability"),
                "overflow-cli",
                &CancellationToken::new(),
            ),
        )
        .await
        .expect("overflow cleanup must remain bounded")
        .expect_err("memory CLI must reject stderr beyond its byte budget");

        assert!(
            marker.exists(),
            "admitted CLI call must spawn the local stub"
        );
        assert!(
            error.to_string().contains("process stderr exceeded"),
            "typed overflow must reach the caller: {error}"
        );
        assert!(
            permit
                .claim_cli_execution(RuntimeExecutionRoute::MemoryCli)
                .is_err(),
            "overflow must not restore consumed execution authority"
        );

        let completion = completion_with_wall_time(
            started_at,
            Some(classify_error_class(&error)),
            ModelUsageConfidence::Unavailable,
        );
        complete_invocation(&store, &permit, completion, &bus)
            .await
            .expect("settle overflow after child reap");

        let guard = store.lock().await;
        let record = guard
            .load_model_invocation_record(invocation_id)
            .expect("load invocation")
            .expect("invocation exists");
        assert_eq!(record.status, ModelInvocationStatus::Failed);
        assert!(
            guard
                .build_model_control_status(8)
                .expect("control status")
                .active_invocations
                .is_empty(),
            "overflow settlement must release active counters"
        );
    }
}
