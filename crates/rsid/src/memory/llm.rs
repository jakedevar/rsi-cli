use crate::bus::EventBus;
use crate::error::{DaemonError, Result};
use crate::memory::worker::MemoryWorkContext;
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
        let credential = crate::pioneer::pioneer_credential()
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
    MemoryWorkContext::check_current()?;
    let resolved = resolve_target(target)?;
    let provider = resolved_provider_label(&resolved);
    let backend = resolved_backend_label(&resolved);
    request.provider = Some(provider);
    request.model = Some(resolved.target.model.clone());
    request.backend = Some(backend.to_string());
    let dedup_key = request.dedup_key.clone();

    let permit = match MemoryWorkContext::interruptible(admit_invocation(store, request, event_bus))
        .await?
    {
        AdmissionDecision::Admitted(permit) => permit,
        AdmissionDecision::Duplicate { invocation_id } => {
            let duplicate_state = if let Some(key) = dedup_key.as_deref() {
                MemoryWorkContext::interruptible(async {
                    let guard = store.lock().await;
                    crate::model_control::lookup_existing_invocation_by_dedup(&guard, key)
                })
                .await?
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
    let cancel = CancellationToken::new();
    let execution = generate_text_cancellable_resolved(
        &permit, &resolved, prompt, max_tokens, purpose, &cancel,
    );
    settle_memory_execution(store, event_bus, &permit, execution, &cancel, purpose).await
}

async fn settle_memory_execution<T>(
    store: &Arc<Mutex<Store>>,
    event_bus: &Arc<EventBus>,
    permit: &AdmissionPermit,
    execution: impl std::future::Future<Output = Result<T>>,
    cancel: &CancellationToken,
    purpose: &str,
) -> Result<T> {
    let started_at = Instant::now();
    let result = finish_memory_execution(execution, cancel).await;
    let completion = match &result {
        Ok(_) => completion_with_wall_time(started_at, None, ModelUsageConfidence::Partial),
        Err(error) => completion_with_wall_time(
            started_at,
            Some(classify_error_class(error)),
            ModelUsageConfidence::Partial,
        ),
    };
    settle_result(store, permit, completion, result, purpose, event_bus).await
}

/// Signal cancellation, then await transport cleanup. Dropping this execution
/// future would skip CLI kill/reap; dropping its caller would skip settlement.
async fn finish_memory_execution<T>(
    execution: impl std::future::Future<Output = Result<T>>,
    cancel: &CancellationToken,
) -> Result<T> {
    match MemoryWorkContext::current() {
        Some(context) => {
            tokio::pin!(execution);
            tokio::select! {
                biased;
                _ = context.cancelled() => {
                    cancel.cancel();
                    execution.await
                }
                result = &mut execution => result,
            }
        }
        None => execution.await,
    }
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
    let provider = OpenAiApiProvider::with_config(
        base_url,
        crate::session::harness::api_key::ApiCredential::explicit(api_key.as_deref()),
        ProviderQuirks::default(),
    )?;
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
    let command = claude_memory_command(target, prompt);
    let output = run_cancellable_cli(command, execution, purpose, cancel).await?;

    process_cli_output(output, purpose, "Claude", &target.model)
}

/// The memory LLM's bypass-permissions Claude child; the shared spawn
/// boundary (`run_cancellable_cli_with_limits`) scrubs it before spawn.
fn claude_memory_command(target: &MemoryLlmTarget, prompt: &str) -> tokio::process::Command {
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
    command
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

    let codex_binary = which::which("codex").unwrap_or_else(|_| "codex".into());
    let model = append_codex_route_credential(
        &mut command,
        target,
        &crate::vault::global(),
        &codex_binary,
    )?;
    command.args(["--model", model, prompt]);

    let output = run_cancellable_cli(command, execution, purpose, cancel).await?;
    process_cli_output(output, purpose, "Codex", model)
}

/// Resolve the memory Codex route credential through the vault and inject
/// exactly one var plus the Codex tool-shell exclude (after which the spawn
/// boundary scrubs everything else). Returns the model.
fn append_codex_route_credential<'a>(
    command: &mut tokio::process::Command,
    target: &'a MemoryLlmTarget,
    vault: &crate::vault::VaultHandle,
    codex_binary: &std::path::Path,
) -> Result<&'a str> {
    use crate::vault::Slot;
    use rsi_common::provider_credentials::{
        CliExposureConsumer::MemoryCodexCli, CliExposureReason::Memory,
    };
    if target.provider == SessionProvider::Pioneer {
        let credential = crate::pioneer::pioneer_credential_from(vault)
            .map_err(|error| DaemonError::Process(error.to_string()))?;
        crate::pioneer::PioneerCodexConfigOverrides::new(
            credential.source(),
            crate::pioneer::existing_pioneer_codex_catalog_path().as_deref(),
        )
        .map_err(|error| DaemonError::Process(error.to_string()))?
        .append_to(command);
        vault.inject_cli_credential(
            command,
            Slot::Pioneer,
            credential.source().env_name(),
            &crate::vault::SecretString::new(credential.value().to_owned()),
            MemoryCodexCli,
            Memory,
        );
        return Ok(crate::pioneer::pioneer_launch_model(Some(&target.model)));
    }
    if target.provider == SessionProvider::Bedrock {
        let credential = crate::bedrock::credential_from(vault).map_err(DaemonError::Process)?;
        let region = crate::bedrock::region().map_err(DaemonError::Process)?;
        crate::bedrock::CodexOverrides::for_launch(&region, codex_binary, Some(&target.model))
            .append_to(command);
        vault.inject_cli_credential(
            command,
            Slot::Bedrock,
            crate::bedrock::BEDROCK_ENV,
            &credential.secret,
            MemoryCodexCli,
            Memory,
        );
    } else if target.provider == SessionProvider::OpenRouter {
        let credential = crate::openrouter::openrouter_credential_from(vault)
            .map_err(|error| DaemonError::Process(error.to_string()))?;
        crate::openrouter::OpenRouterCodexConfigOverrides::new(credential.secret.expose())
            .append_to(command);
        // OpenRouter used to rely on an inherited `OPEN_ROUTER`; it is now an
        // explicit single-var injection.
        vault.inject_cli_credential(
            command,
            Slot::Openrouter,
            crate::openrouter::OPENROUTER_ENV,
            &credential.secret,
            MemoryCodexCli,
            Memory,
        );
    }
    Ok(&target.model)
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
    mut command: tokio::process::Command,
    execution: CliExecutionCapability,
    purpose: &str,
    cancel: &CancellationToken,
    limits: CaptureLimits,
) -> Result<CapturedOutput> {
    // #694 K1: memory CLIs (Claude, Codex, AGY) are agent-facing and sit
    // outside the provider stamp chokepoint. Only an explicitly injected
    // route credential survives the scrub.
    crate::vault::scrub_credential_env(&mut command);
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
    use crate::memory::worker::MemoryWorkContext;
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
    #[test]
    fn memory_llm_target_debug_redacts_api_credentials() {
        let mut target = target(SessionProvider::Pioneer, "claude-sonnet-5");
        target.api_key = Some("fixture-pioneer-secret".to_string());

        let debug = format!("{target:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("fixture-pioneer-secret"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
    #[test]
    fn infer_execution_path_prefers_explicit_base_url() {
        let mut target = target(SessionProvider::Local, "claude-sonnet-5");
        target.base_url = Some("https://example.com/v1".to_string());
        assert_eq!(
            infer_execution_path(&target).expect("route"),
            LlmExecutionPath::OpenAiCompatibleApi
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
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

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
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
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
    #[tokio::test(start_paused = true)]
    async fn live_memory_off_llm_waits_for_cleanup_before_settlement() {
        use std::sync::atomic::Ordering;
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let bus = Arc::new(EventBus::new(16));
        let permit = match admit_invocation(
            &store,
            cli_request(
                ModelInvocationPurpose::SessionLaunchFresh,
                "memory-off-fake-cleanup",
            ),
            &bus,
        )
        .await
        .unwrap()
        {
            AdmissionDecision::Admitted(permit) => permit,
            _ => panic!("unique admission"),
        };
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env());
        runtime.memory_enabled.store(true, Ordering::Relaxed);
        let context = MemoryWorkContext::new(Arc::clone(&runtime), CancellationToken::new());
        let cancel = CancellationToken::new();
        let started = tokio::sync::Notify::new();
        let cleaning = tokio::sync::Notify::new();
        let (release, cleanup) = tokio::sync::oneshot::channel();
        let invocation = context.scope(settle_memory_execution(
            &store,
            &bus,
            &permit,
            async {
                started.notify_one();
                cancel.cancelled().await;
                cleaning.notify_one();
                cleanup.await.unwrap();
                Err::<(), _>(DaemonError::ChannelClosed)
            },
            &cancel,
            "fake memory cleanup",
        ));
        tokio::pin!(invocation);
        tokio::select! {
            _ = started.notified() => {},
            result = &mut invocation => panic!("execution must wait: {result:?}"),
        }
        runtime.memory_enabled.store(false, Ordering::Relaxed);
        tokio::select! {
            _ = cleaning.notified() => {},
            result = &mut invocation => panic!("cleanup must be awaited: {result:?}"),
        }
        assert_eq!(
            store
                .lock()
                .await
                .load_model_invocation_record(permit.invocation_id())
                .unwrap()
                .unwrap()
                .status,
            ModelInvocationStatus::Running
        );
        release.send(()).unwrap();
        assert!(matches!(invocation.await, Err(DaemonError::ChannelClosed)));
        let guard = store.lock().await;
        let row = guard
            .load_model_invocation_record(permit.invocation_id())
            .unwrap()
            .unwrap();
        assert_eq!(row.status, ModelInvocationStatus::Failed);
        assert_eq!(row.error_class.as_deref(), Some("cancelled"));
        assert!(
            guard
                .build_model_control_status(8)
                .unwrap()
                .active_invocations
                .is_empty()
        );
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
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
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
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
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
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
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
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
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
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
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

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn memory_vault(slot: crate::vault::Slot, secret: &str) -> crate::vault::VaultHandle {
        let vault = crate::vault::VaultHandleBuilder::new(std::sync::Arc::new(
            crate::vault::VaultSettings::default(),
        ))
        .env(|_| None)
        .open()
        .unwrap();
        vault.set(slot, secret).unwrap();
        vault
            .set(crate::vault::Slot::Anthropic, "sk-test-memory-anthropic")
            .unwrap();
        vault
    }

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn assert_memory_route(
        provider: SessionProvider,
        slot: crate::vault::Slot,
        var: &str,
        secret: &str,
        model: &str,
    ) {
        // Vault-only: no env value exists for any slot.
        let vault = memory_vault(slot, secret);
        let target = MemoryLlmTarget {
            provider,
            model: model.into(),
            base_url: None,
            api_key: None,
        };
        let mut command = tokio::process::Command::new("codex");
        append_codex_route_credential(
            &mut command,
            &target,
            &vault,
            std::path::Path::new("/nonexistent/rsi-test-codex"),
        )
        .unwrap();
        // `run_cancellable_cli_with_limits` scrubs again before spawn.
        crate::vault::scrub_credential_env(&mut command);
        crate::vault::env_scrub::tests::assert_only_injected(&command, Some(var));
        crate::vault::env_scrub::tests::assert_child_env_suppresses_keys(&command, Some(var));
        let injected = command
            .as_std()
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new(var))
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned());
        assert_eq!(injected.as_deref(), Some(secret));
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&crate::vault::env_scrub::codex_shell_exclude_arg(var)));
        assert!(!args.join(" ").contains(secret));
        // The injection is reported as a CLI exposure.
        assert!(vault.metadata(slot).last_cli_exposure_at.is_some());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
    #[test]
    fn memory_openrouter_cli_injects_explicit_open_router_from_vault() {
        assert_memory_route(
            SessionProvider::OpenRouter,
            crate::vault::Slot::Openrouter,
            crate::openrouter::OPENROUTER_ENV,
            "sk-test-memory-openrouter",
            "vendor/model",
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
    #[test]
    fn memory_pioneer_cli_injects_only_pioneer_var_from_vault() {
        assert_memory_route(
            SessionProvider::Pioneer,
            crate::vault::Slot::Pioneer,
            crate::pioneer::PIONEER_PRIMARY_ENV,
            "sk-test-memory-pioneer",
            "vendor/model",
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
    #[test]
    fn memory_bedrock_cli_injects_only_bedrock_token_from_vault() {
        temp_env::with_vars([("AWS_REGION", Some("us-west-1"))], || {
            assert_memory_route(
                SessionProvider::Bedrock,
                crate::vault::Slot::Bedrock,
                crate::bedrock::BEDROCK_ENV,
                "bedrock-api-key-test-memory",
                "global.openai.gpt-5.6-sol",
            );
        });
    }

    #[allow(clippy::unwrap_used, clippy::expect_used)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
    #[test]
    fn memory_cli_spawn_boundary_scrubs_credentials() {
        // Claude and AGY memory CLIs inject nothing; the shared spawn
        // boundary must scrub every inherited credential.
        let source = include_str!("llm.rs");
        let boundary = source
            .find("async fn run_cancellable_cli_with_limits(")
            .expect("memory CLI spawn boundary");
        let body = &source[boundary..boundary + 1200];
        assert!(body.contains("crate::vault::scrub_credential_env(&mut command);"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-memory-01"))]
    #[test]
    fn memory_claude_child_suppresses_daemon_keys_and_keeps_ordinary_env() {
        let target = MemoryLlmTarget {
            provider: SessionProvider::Claude,
            model: "claude-sonnet-5".into(),
            base_url: None,
            api_key: None,
        };
        let mut command = claude_memory_command(&target, "summarize");
        // What `run_cancellable_cli_with_limits` does before spawning.
        crate::vault::scrub_credential_env(&mut command);
        crate::vault::env_scrub::tests::assert_child_env_suppresses_keys(&command, None);
    }
}
