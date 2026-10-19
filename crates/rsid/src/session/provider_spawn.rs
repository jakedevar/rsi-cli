//! The single provider-spawn chokepoint.
//!
//! Every provider subprocess (for a CLI-shaped provider) is created here, in
//! [`spawn_provider_process`], which performs the provider `match` +
//! `client.launch()` exactly once and wraps the result in [`ProviderProcess`].
//! The four spawn entry points — `launch_session`, `continue_session`,
//! `resume_for_handoff_write`, `spawn_rotation_child` — no longer hand-roll a
//! dispatch block; they build a [`ProviderLauncher`] and call this primitive.
//!
//! Single-flight coverage is therefore **structural**: [`spawn_provider_process`]
//! takes a `&SpawnGuard` witness it cannot run without, so a future 5th spawn
//! caller must route through the guarded funnel — an unguarded dispatch block is
//! no longer expressible. The guard's acquire / adopt-on-contended / drop timing
//! stays entirely at the call sites (the primitive only *borrows* the witness),
//! preserving the per-site single-flight semantics and the rotation guard-drop
//! rule byte-for-byte.
//!
//! The genuine per-site differences — cached provider clients (`&self` sites)
//! vs. freshly-constructed clients (rotation sites), the two harness call
//! shapes, and the Local ad-hoc client — live behind the [`ProviderLauncher`]
//! trait's two impls ([`CachedLauncher`], [`FreshLauncher`]). The provider
//! `match` skeleton, `ProviderProcess` wrapping, and CodexAppServer→Codex
//! fallback are shared here exactly once.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, mpsc};
use uuid::Uuid;

use crate::agy::{AgyClient, AgyProcess};
use crate::claude::{ClaudeClient, ClaudeProcess, LaunchConfig, StreamEvent};
use crate::codex::{CodexClient, CodexProcess};
use crate::error::{DaemonError, Result};
use crate::model_control::call_control::{
    ModelCallControl, ModelCallSettlementHandle, StoreBackedModelCallControl,
};
use crate::model_control::registry::RuntimeExecutionRoute;
use crate::model_control::{AdmissionPermit, CliExecutionCapability};
use crate::openai::{OpenAiClient, OpenAiProcess};
use crate::store::Store;
use rsi_common::types::SessionProvider;

use super::harness::types::ChatMessage;
use super::spawn_coordinator::SpawnCoordinator;
use super::spawn_single_flight::SpawnGuard;
use super::types::{CompletedSession, HarnessProcess, ProviderProcess, TrackedSession};

/// Effective provider for synchronous continue/rotation launches. The app
/// server's synchronous compatibility path is a Codex CLI process and must be
/// represented by a fresh Codex Session row, never hidden behind an existing
/// `CodexAppServer` row.
pub(super) const fn effective_sync_provider(provider: SessionProvider) -> SessionProvider {
    if matches!(provider, SessionProvider::CodexAppServer) {
        SessionProvider::Codex
    } else {
        provider
    }
}

/// Provider-neutral installed-handle proof used by D03 candidate
/// confirmation. App-server confirmation is produced separately after its
/// successful initialize/thread-start return and process installation.
pub(super) fn installed_provider_confirmation(
    provider: SessionProvider,
    process: &mut ProviderProcess,
) -> Option<rsi_common::types::ControllerConfirmationKindV1> {
    #[cfg(test)]
    if provider != SessionProvider::CodexAppServer
        && matches!(process, ProviderProcess::Scripted(_))
        && process.is_alive()
    {
        return Some(rsi_common::types::ControllerConfirmationKindV1::InstalledProvider);
    }
    let exact_variant = matches!(
        (provider, &*process),
        (SessionProvider::Claude, ProviderProcess::Claude(_))
            | (
                SessionProvider::Codex
                    | SessionProvider::Pioneer
                    | SessionProvider::OpenRouter
                    | SessionProvider::Bedrock,
                ProviderProcess::Codex(_)
            )
            | (
                SessionProvider::OpenRouter | SessionProvider::Bedrock | SessionProvider::Harness,
                ProviderProcess::Harness(_)
            )
            | (SessionProvider::Local, ProviderProcess::Local(_))
            | (
                SessionProvider::Antigravity,
                ProviderProcess::Antigravity(_)
            )
    );
    if !exact_variant || !process.is_alive() {
        return None;
    }
    Some(rsi_common::types::ControllerConfirmationKindV1::InstalledProvider)
}

/// Match a captured D03 establishment confirmation to the **current**
/// installed process under the Active commit lock. Synchronous providers need
/// a live generic handle. A fresh app-server candidate instead has already
/// completed initialize/thread-start before it is installed and signals its
/// typed confirmation immediately afterwards; that confirmation is valid only
/// for the live app-server process variant, never for a synchronous Codex
/// fallback process.
pub(super) fn matches_live_controller_confirmation(
    provider: SessionProvider,
    confirmation: rsi_common::types::ControllerConfirmationKindV1,
    process: &mut ProviderProcess,
) -> bool {
    use rsi_common::types::ControllerConfirmationKindV1;

    match confirmation {
        ControllerConfirmationKindV1::InstalledProvider => {
            installed_provider_confirmation(provider, process)
                == Some(ControllerConfirmationKindV1::InstalledProvider)
        }
        ControllerConfirmationKindV1::CodexAppServerInitialized => {
            provider == SessionProvider::CodexAppServer
                && matches!(process, ProviderProcess::CodexAppServer(_))
                && process.is_alive()
        }
    }
}

/// The single provider-dispatch chokepoint. Performs the provider `match` +
/// `client.launch()` exactly once and wraps the result in [`ProviderProcess`].
///
/// `_guard` is a **mandatory single-flight witness**: this fn cannot be called
/// without an acquired [`SpawnGuard`], so every provider subprocess is created
/// inside a guarded span by construction. The caller retains ownership of the
/// guard and controls its drop timing (fn-return for the `tokio::spawn`-monitor
/// sites; explicit drop-before-inline-monitor for the two rotation sites). The
/// borrow ends when this fn returns, before the caller's `active.insert` and any
/// `drop(spawn_guard)`, so guard lifetime is unchanged at every call site.
///
/// Returns `(ProviderProcess, event_rx)` for CLI-shaped providers. The
/// `CodexAppServer` *deferred/async* spawn (the background task in `launch.rs`)
/// is intentionally NOT routed here — its shape is `(…, impl ProviderSession)`,
/// incompatible with `(…, rx)`. `CodexAppServer` reaching *this* fn (continue /
/// rotation) falls back to the Codex CLI, matching pre-refactor behavior.
// The Codex and CodexAppServer→Codex-fallback arms share a body but are kept
// distinct so the fallback stays self-documenting (mirrors the removed per-site
// dispatch); do not merge them.
#[allow(clippy::match_same_arms)]
pub(super) fn spawn_provider_process<L: ProviderLauncher>(
    provider: SessionProvider,
    config: &LaunchConfig,
    inner: &L,
    admission_permit: &AdmissionPermit,
    _guard: &SpawnGuard,
) -> Result<(ProviderProcess, mpsc::Receiver<StreamEvent>)> {
    // #692: the operator launch-model allowlist also gates this provider-level
    // chokepoint, which continuations, rotations and successors reach without
    // passing through `launch_session`. Callers preflight earlier, before any
    // admission or custody effect; this is the structural backstop. An empty
    // list is a no-op.
    if let Some(reason) = inner.runtime_config().launch_model_refusal(
        provider,
        super::launch::provider_effective_model(provider, config.model.as_deref()).as_deref(),
    ) {
        return Err(DaemonError::PolicyDenied(reason));
    }
    // #2759: the turn the topology executor opened to obtain a node's handoff
    // carries the handoff schema, on the CLIs that take one (Claude
    // `--json-schema`, Codex `--output-schema`). Every other launch, and every
    // other provider, is unchanged.
    let structured_config;
    let config = if matches!(
        provider,
        SessionProvider::Claude
            | SessionProvider::Codex
            | SessionProvider::Pioneer
            | SessionProvider::CodexAppServer
    ) && config
        .rsi_session_id
        .is_some_and(crate::structured_handoff::take_for_launch)
    {
        structured_config = LaunchConfig {
            structured_handoff: true,
            ..config.clone()
        };
        &structured_config
    } else {
        config
    };
    // #792: a session with a tool policy may only run on the Harness loop.
    let guarded = ToolPolicyGuard { inner, provider };
    let launcher = &guarded;
    // #694 K1: an authoritative invalid/exhausted credential check is a
    // launch-time admission refusal, never a mid-run crash.
    crate::vault::admit_provider_spawn(provider, config)?;
    match provider {
        SessionProvider::Claude => {
            let (p, rx) = launcher.launch_claude(
                config,
                admission_permit.claim_cli_execution(RuntimeExecutionRoute::ClaudeCli)?,
            )?;
            Ok((ProviderProcess::Claude(p), rx))
        }
        SessionProvider::Codex => {
            let (p, rx) = launcher.launch_codex(
                config,
                admission_permit.claim_cli_execution(RuntimeExecutionRoute::CodexCli)?,
            )?;
            Ok((ProviderProcess::Codex(p), rx))
        }
        SessionProvider::Pioneer => {
            let (p, rx) = launcher.launch_codex(
                config,
                admission_permit.claim_cli_execution(RuntimeExecutionRoute::CodexCli)?,
            )?;
            Ok((ProviderProcess::Codex(p), rx))
        }
        SessionProvider::Bedrock => {
            // Codex and the Bedrock Responses Harness speak only the
            // OpenAI-compatible API. Fresh launches move Claude models to
            // Claude Code or the Harness provider before this point.
            if config
                .model
                .as_deref()
                .and_then(crate::bedrock::bedrock_vendor)
                == Some(crate::bedrock::BedrockVendor::Anthropic)
            {
                return Err(DaemonError::InvalidParam(
                    "Bedrock Claude models run through Claude Code or the Harness; relaunch the session instead of resuming it on the Bedrock provider".into(),
                ));
            }
            let model = config
                .model
                .as_deref()
                .unwrap_or(crate::bedrock::BEDROCK_DEFAULT_MODEL);
            if launcher.runtime_config().bedrock_route_for(model)
                == crate::config::OpenRouterRoute::Harness
            {
                let preflight = if model.trim().is_empty() {
                    Err("unsupported_model")
                } else if !launcher.bedrock_region_available() {
                    Err("missing_region")
                } else if !launcher.bedrock_credential_available() {
                    Err("missing_credential")
                } else {
                    Ok(())
                };
                if let Err(cause) = preflight {
                    if !launcher.runtime_config().bedrock_fallback_enabled() {
                        return Err(DaemonError::InvalidParam(format!(
                            "Bedrock harness route preflight: {cause}"
                        )));
                    }
                    let (p, rx) = launcher.launch_codex(
                        config,
                        admission_permit.claim_cli_execution(RuntimeExecutionRoute::CodexCli)?,
                    )?;
                    return Ok((
                        ProviderProcess::Codex(p),
                        prepend_fallback_events(rx, cause),
                    ));
                }
                let (p, rx) = launcher.launch_harness(config)?;
                return Ok((ProviderProcess::Harness(p), rx));
            }
            let (p, rx) = launcher.launch_codex(
                config,
                admission_permit.claim_cli_execution(RuntimeExecutionRoute::CodexCli)?,
            )?;
            Ok((ProviderProcess::Codex(p), rx))
        }
        SessionProvider::OpenRouter => {
            let model = config
                .model
                .as_deref()
                .unwrap_or(crate::openrouter::OPENROUTER_DEFAULT_MODEL);
            let (route, source) = launcher.runtime_config().openrouter_route_resolution(model);
            let selection = |engine, fallback_cause| crate::api_route_selection::RouteSelection {
                engine,
                configured_route: route.as_str(),
                source: source.as_str(),
                fallback_cause,
            };
            // The operator's explicit `codex_cli` route (global or per model) is
            // an opt-in, not a fallback; it is recorded with its source.
            if route == crate::config::OpenRouterRoute::CodexCli {
                let (p, rx) = launcher.launch_codex(
                    config,
                    admission_permit.claim_cli_execution(RuntimeExecutionRoute::CodexCli)?,
                )?;
                note_route_selection(config, selection("codex_cli", None));
                return Ok((ProviderProcess::Codex(p), rx));
            }
            let preflight =
                openrouter_harness_preflight(model, launcher.openrouter_credential_available());
            if let Err(cause) = preflight {
                // #3141: a failed Harness preflight is a loud typed refusal
                // unless the operator opted into the Codex CLI fallback.
                if !launcher.runtime_config().openrouter_fallback_enabled() {
                    return Err(DaemonError::OpenRouterRoutePreflight { cause });
                }
                let (p, rx) = launcher.launch_codex(
                    config,
                    admission_permit.claim_cli_execution(RuntimeExecutionRoute::CodexCli)?,
                )?;
                note_route_selection(config, selection("codex_cli", Some(cause)));
                return Ok((
                    ProviderProcess::Codex(p),
                    prepend_fallback_events(rx, cause),
                ));
            }
            let (p, rx) = launcher.launch_harness(config)?;
            note_route_selection(config, selection("harness", None));
            Ok((ProviderProcess::Harness(p), rx))
        }
        // Local is an in-process OpenAI-compatible HTTP task, not a CLI child.
        // Its per-request admission is intentionally outside this CLI/process
        // slice and cannot consume this execution capability.
        SessionProvider::Local => {
            let (p, rx) = launcher.launch_local(config, admission_permit.clone())?;
            Ok((ProviderProcess::Local(p), rx))
        }
        SessionProvider::Antigravity => {
            let (p, rx) = launcher.launch_agy(
                config,
                admission_permit.claim_cli_execution(RuntimeExecutionRoute::AntigravityCli)?,
            )?;
            Ok((ProviderProcess::Antigravity(p), rx))
        }
        // Continue/rotation route CodexAppServer to the Codex CLI. `launch_session`
        // intercepts CodexAppServer as its deferred path BEFORE calling this fn, so
        // this arm is only reached from continue/rotation — mirroring the old
        // per-site Codex-CLI fallback.
        SessionProvider::CodexAppServer => {
            let (p, rx) = launcher.launch_codex(
                config,
                admission_permit.claim_cli_execution(RuntimeExecutionRoute::CodexCli)?,
            )?;
            Ok((ProviderProcess::Codex(p), rx))
        }
        // Harness is an in-process API loop. Its explicit per-turn controller
        // remains outside this CLI/process slice.
        SessionProvider::Harness => {
            let (p, rx) = launcher.launch_harness(config)?;
            Ok((ProviderProcess::Harness(p), rx))
        }
        // `SessionProvider` is `#[non_exhaustive]`; an unknown provider is
        // rejected rather than silently spawned.
        other => Err(DaemonError::InvalidParam(format!(
            "unsupported provider for spawn: {other:?}"
        ))),
    }
}

/// #3141: the cause of a refused OpenRouter Harness preflight, if `error` is one.
pub(super) fn openrouter_preflight_cause(error: &DaemonError) -> Option<&'static str> {
    match error {
        DaemonError::OpenRouterRoutePreflight { cause } => Some(cause),
        _ => None,
    }
}

/// #3141: end an existing session whose continuation or rotation resume was
/// refused by the OpenRouter Harness preflight. Writes the terminal reason
/// (`openrouter_harness_preflight:<cause>`) and one visible `System` row;
/// the caller has already marked the session `Failed`.
pub(super) async fn record_route_preflight_failure(
    store: &Arc<Mutex<Store>>,
    session_id: Uuid,
    cause: &'static str,
) {
    let code = crate::error::openrouter_preflight_code(cause);
    let explanation = format!(
        "OpenRouter Harness preflight failed ({cause}); the session was not started on the Codex CLI. Set up the Harness route, or opt in to the fallback with api_route.fallback.openrouter."
    );
    if let Err(error) =
        store
            .lock()
            .await
            .replace_session_failure_cause(session_id, &code, &explanation)
    {
        tracing::warn!(%session_id, %error, "route preflight failure cause not recorded");
    }
}

/// #3141: [`record_route_preflight_failure`] for a session held as a completed
/// entry: also marks the entry and the stored row `Failed` with the same code
/// as its stop reason, so the runtime and the store agree on how it ended.
pub(super) async fn fail_completed_session_for_route_preflight(
    persistence: &super::PersistenceHandle,
    store: &Arc<Mutex<Store>>,
    entry: &mut CompletedSession,
    cause: &'static str,
) {
    let code = crate::error::openrouter_preflight_code(cause);
    let session_id = entry.session.id;
    entry.session.status = rsi_common::types::SessionStatus::Failed;
    entry.session.stop_reason = Some(code.clone());
    entry.session.terminal_reason = Some(code.clone());
    entry.session.updated_at = chrono::Utc::now();
    if let Err(error) = persistence
        .update_terminal_status(session_id, rsi_common::types::SessionStatus::Failed, code)
        .await
    {
        tracing::warn!(%session_id, %error, "route preflight terminal status not persisted");
    }
    // The cause is written only on a row that is already terminal: wait for the
    // queued status write to commit first.
    if let Err(error) = persistence.barrier().await {
        tracing::warn!(%session_id, %error, "route preflight persistence barrier failed");
    }
    record_route_preflight_failure(store, session_id, cause).await;
}

/// #3141: record the route an OpenRouter launch took for its session's monitor.
fn note_route_selection(
    config: &LaunchConfig,
    selection: crate::api_route_selection::RouteSelection,
) {
    if let Some(session_id) = config.rsi_session_id {
        crate::api_route_selection::note_selected(session_id, selection);
    }
}

fn openrouter_harness_preflight(
    model: &str,
    credential_available: bool,
) -> std::result::Result<(), &'static str> {
    let normalized = crate::openrouter::harness_model_id(model).ok_or("unsupported_model")?;
    if !credential_available {
        return Err("missing_credential");
    }
    if crate::openrouter::catalog_tool_support(normalized) == Some(false) {
        return Err("no_tool_support");
    }
    Ok(())
}

fn prepend_fallback_events(
    mut source: mpsc::Receiver<StreamEvent>,
    cause: &'static str,
) -> mpsc::Receiver<StreamEvent> {
    let (tx, rx) = mpsc::channel(256);
    for event in [
        StreamEvent {
            event_type: "route_fallback".into(),
            data: serde_json::json!({"from":"harness","to":"codex_cli","reason":cause}),
        },
        StreamEvent {
            event_type: "credential_cli_exposure".into(),
            data: serde_json::json!({"reason":"fallback"}),
        },
    ] {
        let _ = tx.try_send(event);
    }
    tokio::spawn(async move {
        while let Some(event) = source.recv().await {
            if tx.send(event).await.is_err() {
                break;
            }
        }
    });
    rx
}

/// Parametrizes the genuine per-site launch differences behind a closed set of
/// two impls: [`CachedLauncher`] (the `&self` sites, reusing the manager's
/// cached provider clients) and [`FreshLauncher`] (the rotation sites, building
/// fresh clients inline). Each leaf launches one provider and returns the same
/// `(Process, event_rx)` pair the primitive wraps.
pub(super) trait ProviderLauncher {
    fn runtime_config(&self) -> &crate::config::RuntimeConfig;
    /// #792: the launching session's Harness tool policy, if any. A session
    /// with a policy may only be started on the Harness loop, where it is
    /// enforced; [`ToolPolicyGuard`] refuses every other provider process.
    fn session_tool_policy(&self) -> Option<&rsi_common::harness_tool_policy::HarnessToolPolicy> {
        None
    }
    /// #2755: the refusal of a Codex CLI launch that would run without the
    /// sandbox custody hook because the installed codex-cli does not trust it,
    /// or `None`. The default asks the process-wide canary; a launcher that
    /// owns a private service overrides it.
    fn codex_custody_refusal(&self, config: &LaunchConfig) -> Option<String> {
        codex_custody_refusal_with(
            crate::provider_contract::ProviderContractService::global(),
            self.runtime_config(),
            config,
        )
    }
    fn bedrock_credential_available(&self) -> bool {
        crate::vault::global().resolvable(crate::vault::Slot::Bedrock)
    }
    fn bedrock_region_available(&self) -> bool {
        crate::bedrock::region().is_ok()
    }
    fn openrouter_credential_available(&self) -> bool {
        crate::openrouter::openrouter_credential().is_ok()
    }
    fn launch_claude(
        &self,
        config: &LaunchConfig,
        execution: CliExecutionCapability,
    ) -> Result<(ClaudeProcess, mpsc::Receiver<StreamEvent>)>;
    fn launch_codex(
        &self,
        config: &LaunchConfig,
        execution: CliExecutionCapability,
    ) -> Result<(CodexProcess, mpsc::Receiver<StreamEvent>)>;
    fn launch_local(
        &self,
        config: &LaunchConfig,
        initial_admission_permit: AdmissionPermit,
    ) -> Result<(OpenAiProcess, mpsc::Receiver<StreamEvent>)>;
    fn launch_agy(
        &self,
        config: &LaunchConfig,
        execution: CliExecutionCapability,
    ) -> Result<(AgyProcess, mpsc::Receiver<StreamEvent>)>;
    fn launch_harness(
        &self,
        config: &LaunchConfig,
    ) -> Result<(HarnessProcess, mpsc::Receiver<StreamEvent>)>;
}

/// #792, #2758: wraps a launcher so a session under a tool policy only starts a
/// process that can enforce every field the policy sets. The Harness loop
/// enforces the Harness fields; the Claude and Codex CLIs enforce `cli_access`
/// (the guard hands them a config that carries the resolved policy, so a
/// continue, rotation or retry maps it exactly like a fresh launch); every other
/// route (Local, Antigravity, a Harness -> Codex CLI fallback of a restricted
/// session) is refused before any effect with
/// `tool_policy_unsupported_for_provider:<what>`.
struct ToolPolicyGuard<'a, L> {
    inner: &'a L,
    /// The session's provider, needed to decide whether the daemon-wide
    /// defaults (never stored) put it under a policy.
    provider: SessionProvider,
}

impl<L: ProviderLauncher> ToolPolicyGuard<'_, L> {
    fn under_policy(&self) -> bool {
        self.inner
            .runtime_config()
            .session_is_under_tool_policy(self.provider, self.inner.session_tool_policy())
    }

    fn refusal(refusal: &str) -> DaemonError {
        DaemonError::PolicyDenied(format!(
            "{refusal}: this session's tool policy cannot be enforced on the selected provider \
             route"
        ))
    }

    /// The launch config a Claude or Codex CLI process starts with: the same
    /// config carrying the session's resolved policy, or a refusal when the CLI
    /// cannot express every field of it. A session under no policy keeps
    /// `config` untouched, so its argv is exactly what it was.
    fn cli_config<'c>(
        &self,
        config: &'c LaunchConfig,
    ) -> Result<std::borrow::Cow<'c, LaunchConfig>> {
        if !self.under_policy() {
            return Ok(std::borrow::Cow::Borrowed(config));
        }
        let stored = self.inner.session_tool_policy();
        // The daemon-wide Harness defaults are never stored and no CLI enforces
        // them, so a Harness-loop session they restrict cannot take a CLI route.
        let defaults_restrict =
            rsi_common::harness_tool_policy::provider_runs_harness_loop(self.provider)
                && !self
                    .inner
                    .runtime_config()
                    .harness_tool_policy_defaults()
                    .is_default();
        let unsupported = if defaults_restrict || stored.is_none() {
            Some("daemon_defaults")
        } else {
            stored
                .and_then(rsi_common::harness_tool_policy::HarnessToolPolicy::cli_unsupported_field)
        };
        if let Some(what) = unsupported {
            return Err(Self::refusal(&format!(
                "{}:{}:{what}",
                rsi_common::harness_tool_policy::TOOL_POLICY_UNSUPPORTED_FOR_PROVIDER,
                rsi_common::harness_tool_policy::provider_label(self.provider)
            )));
        }
        let mut resolved = config.clone();
        resolved.tool_policy = stored.cloned();
        Ok(std::borrow::Cow::Owned(resolved))
    }

    /// A route that enforces no part of a policy (Local, Antigravity).
    fn refuse_if_policy(&self) -> Result<()> {
        if self.under_policy() {
            return Err(Self::refusal(&format!(
                "{}:{}",
                rsi_common::harness_tool_policy::TOOL_POLICY_UNSUPPORTED_FOR_PROVIDER,
                rsi_common::harness_tool_policy::provider_label(self.provider)
            )));
        }
        Ok(())
    }
}

/// #2755: refuse a Codex CLI launch that installs the sandbox custody hook
/// (a session launch with an installed `rsi-rpc` sibling; the same condition as
/// `CodexClient::build_cmd_with_custom_provider`) while the contract canary
/// found the installed codex-cli does not trust that hook and the operator
/// setting `codex_refuse_untrusted_custody_hook` is on. A launch with no
/// session identity installs no hook and is never refused.
pub(super) fn codex_custody_refusal_with(
    service: &crate::provider_contract::ProviderContractService,
    runtime_config: &crate::config::RuntimeConfig,
    config: &LaunchConfig,
) -> Option<String> {
    config.rsi_session_id?;
    service.codex_custody_refusal(runtime_config.codex_refuse_untrusted_custody_hook_enabled())
}

impl<L: ProviderLauncher> ProviderLauncher for ToolPolicyGuard<'_, L> {
    fn runtime_config(&self) -> &crate::config::RuntimeConfig {
        self.inner.runtime_config()
    }
    fn codex_custody_refusal(&self, config: &LaunchConfig) -> Option<String> {
        self.inner.codex_custody_refusal(config)
    }
    fn session_tool_policy(&self) -> Option<&rsi_common::harness_tool_policy::HarnessToolPolicy> {
        self.inner.session_tool_policy()
    }
    fn bedrock_credential_available(&self) -> bool {
        self.inner.bedrock_credential_available()
    }
    fn bedrock_region_available(&self) -> bool {
        self.inner.bedrock_region_available()
    }
    fn openrouter_credential_available(&self) -> bool {
        self.inner.openrouter_credential_available()
    }
    fn launch_claude(
        &self,
        config: &LaunchConfig,
        execution: CliExecutionCapability,
    ) -> Result<(ClaudeProcess, mpsc::Receiver<StreamEvent>)> {
        let config = self.cli_config(config)?;
        self.inner.launch_claude(&config, execution)
    }
    fn launch_codex(
        &self,
        config: &LaunchConfig,
        execution: CliExecutionCapability,
    ) -> Result<(CodexProcess, mpsc::Receiver<StreamEvent>)> {
        let config = self.cli_config(config)?;
        if let Some(refusal) = self.inner.codex_custody_refusal(&config) {
            tracing::warn!("Codex launch refused: the custody hook is not trusted (#2755)");
            return Err(DaemonError::PolicyDenied(refusal));
        }
        self.inner.launch_codex(&config, execution)
    }
    fn launch_local(
        &self,
        config: &LaunchConfig,
        initial_admission_permit: AdmissionPermit,
    ) -> Result<(OpenAiProcess, mpsc::Receiver<StreamEvent>)> {
        self.refuse_if_policy()?;
        self.inner.launch_local(config, initial_admission_permit)
    }
    fn launch_agy(
        &self,
        config: &LaunchConfig,
        execution: CliExecutionCapability,
    ) -> Result<(AgyProcess, mpsc::Receiver<StreamEvent>)> {
        self.refuse_if_policy()?;
        self.inner.launch_agy(config, execution)
    }
    fn launch_harness(
        &self,
        config: &LaunchConfig,
    ) -> Result<(HarnessProcess, mpsc::Receiver<StreamEvent>)> {
        // The Harness loop has no read-only CLI sandbox to apply; a policy that
        // asks for one must not start here and run unrestricted.
        if let Some(refusal) = rsi_common::harness_tool_policy::provider_policy_refusal(
            SessionProvider::Harness,
            self.inner.session_tool_policy(),
        ) {
            return Err(Self::refusal(&refusal));
        }
        self.inner.launch_harness(config)
    }
}

/// Family A — the two `&self` sites (`launch_session`, `continue_session`). Uses
/// the [`SessionManager`](super::SessionManager)'s cached provider clients and
/// the full-featured in-process harness client (`session::harness::HarnessClient`,
/// with the tool registry / memory / project scope and the native `rsi_control`
/// tools). Only the harness `conversation_history` (None for a fresh launch,
/// Some for a continue) and the project scope differ per site — carried in
/// [`HarnessLaunchCtx`]; everything else is derived from `config`/the manager.
pub(super) struct CachedLauncher<'a> {
    pub mgr: &'a super::SessionManager,
    pub harness: HarnessLaunchCtx,
}

/// Per-site harness context for [`CachedLauncher`]. The 9-arg harness `launch`
/// differs across the two Family-A sites only by these two inputs.
pub(super) struct HarnessLaunchCtx {
    pub monitor_generation: u64,
    /// `None` for a fresh launch (`launch_session`); `Some(events→messages)` for
    /// a continue (`continue_session`) on the Harness path — precomputed lazily
    /// by the caller so non-harness continues never iterate events.
    pub conversation_history: Option<Vec<ChatMessage>>,
    /// The launching session's project scope for the harness `memory_search`
    /// tool (Site 1: freshly resolved project id; Site 2: completed-session
    /// project id). The agent cannot widen this through tool args.
    pub project_id: Option<Uuid>,
    /// The already-admitted session-lifecycle permit. The harness boundary
    /// consumes this for the first actual backend call and derives child rows
    /// for subsequent tool-loop and compaction calls.
    pub initial_admission_permit: AdmissionPermit,
    /// Producer ownership acquired before launch-side effects. Reusing it at
    /// the Harness boundary prevents shutdown sealing from rejecting a late
    /// handle acquisition after the invocation was admitted.
    pub model_call_settlements: ModelCallSettlementHandle,
    /// Exact active budget already selected for this Session incarnation.
    pub resolved_context_budget: rsi_common::ResolvedContextBudget,
    /// #792: the session's stored or inherited Harness tool policy, resolved
    /// before launch. Daemon defaults are layered on inside the Harness client.
    pub tool_policy: Option<rsi_common::harness_tool_policy::HarnessToolPolicy>,
}

impl ProviderLauncher for CachedLauncher<'_> {
    fn runtime_config(&self) -> &crate::config::RuntimeConfig {
        &self.mgr.runtime_config
    }
    fn codex_custody_refusal(&self, config: &LaunchConfig) -> Option<String> {
        codex_custody_refusal_with(
            &self.mgr.provider_contract,
            &self.mgr.runtime_config,
            config,
        )
    }
    fn launch_claude(
        &self,
        config: &LaunchConfig,
        execution: CliExecutionCapability,
    ) -> Result<(ClaudeProcess, mpsc::Receiver<StreamEvent>)> {
        self.mgr
            .claude_client
            .as_ref()
            .ok_or(DaemonError::ClaudeBinaryNotFound)?
            .launch(config, execution)
    }

    fn launch_codex(
        &self,
        config: &LaunchConfig,
        execution: CliExecutionCapability,
    ) -> Result<(CodexProcess, mpsc::Receiver<StreamEvent>)> {
        self.mgr
            .codex_client
            .as_ref()
            .ok_or(DaemonError::CodexBinaryNotFound)?
            .launch(config, execution)
    }

    fn launch_local(
        &self,
        config: &LaunchConfig,
        initial_admission_permit: AdmissionPermit,
    ) -> Result<(OpenAiProcess, mpsc::Receiver<StreamEvent>)> {
        // Absorbs the Local ad-hoc branch: `launch_session` may carry an inline
        // custom provider (`openai_base_url`), in which case a per-launch client
        // is built; `continue_session` always sets `openai_base_url = None`, so
        // it keeps using the cached client — behavior identical to both sites.
        let model = config
            .model
            .clone()
            .unwrap_or_else(|| "qwen3:14b".to_string());
        let launch_invocation_id = initial_admission_permit.invocation_id();
        let launch = |client: &OpenAiClient| {
            let control: Arc<dyn ModelCallControl> = Arc::new(StoreBackedModelCallControl::new(
                Arc::clone(&self.mgr.store),
                Arc::clone(self.mgr.event_bus()),
                self.harness.model_call_settlements.clone(),
                rsi_common::model_control::InvocationOwner {
                    session_id: config.rsi_session_id,
                    project_id: config.project_id,
                    ..Default::default()
                },
                client.resolved_provider_label(),
                Some(model.clone()),
                "openai_compatible_api",
                config.effort.clone(),
                "session_openai_compatible",
                initial_admission_permit.clone(),
                rsi_common::model_control::ModelInvocationPurpose::SessionOpenAiCompatibleTurn,
                None,
                RuntimeExecutionRoute::LocalOpenAiCompatibleHttp,
            ));
            client.launch(config, control, launch_invocation_id)
        };
        if let Some(ref url) = config.openai_base_url {
            let ad_hoc = OpenAiClient::with_config(url.clone(), config.openai_api_key.clone())?;
            launch(&ad_hoc)
        } else {
            let client = self
                .mgr
                .local_client
                .as_ref()
                .ok_or(DaemonError::OpenAiApiError(
                    "Local model server not available".to_string(),
                ))?;
            launch(client)
        }
    }

    fn launch_agy(
        &self,
        config: &LaunchConfig,
        execution: CliExecutionCapability,
    ) -> Result<(AgyProcess, mpsc::Receiver<StreamEvent>)> {
        self.mgr
            .agy_client
            .as_ref()
            .ok_or(DaemonError::AgyBinaryNotFound)?
            .clone()
            .with_runtime_config(self.mgr.runtime_config.clone())
            .launch(config, execution)
    }

    fn launch_harness(
        &self,
        config: &LaunchConfig,
    ) -> Result<(HarnessProcess, mpsc::Receiver<StreamEvent>)> {
        // `config.working_dir` is the effective (possibly sandbox) cwd both sites
        // already set; harness tools pass it through to shell/git/file tools.
        let harness_working_dir = config
            .working_dir
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new("/tmp"));
        self.mgr.harness_client.launch(
            self.harness.monitor_generation,
            config,
            &self.harness.resolved_context_budget,
            config.system_prompt.clone(),
            harness_working_dir,
            self.harness.conversation_history.clone(),
            self.harness.initial_admission_permit.clone(),
            self.mgr.memory_handle.clone(),
            self.harness.project_id,
            Arc::clone(&self.mgr.store),
            Arc::clone(self.mgr.event_bus()),
            self.harness.model_call_settlements.clone(),
            config.rsi_session_id,
            Some(self.mgr.agent_control()),
            self.harness.tool_policy.clone(),
        )
    }

    fn session_tool_policy(&self) -> Option<&rsi_common::harness_tool_policy::HarnessToolPolicy> {
        self.harness.tool_policy.as_ref()
    }
}

/// Continue-only launch adapter: install custody-bound Codegraph tools before Harness starts.
pub(super) struct ResumedLauncher<'a> {
    pub base: CachedLauncher<'a>,
    pub codegraph_binding: Option<crate::codegraph::NativeCodegraphBinding>,
}

impl ProviderLauncher for ResumedLauncher<'_> {
    fn runtime_config(&self) -> &crate::config::RuntimeConfig {
        self.base.runtime_config()
    }
    fn codex_custody_refusal(&self, config: &LaunchConfig) -> Option<String> {
        self.base.codex_custody_refusal(config)
    }

    fn launch_claude(
        &self,
        config: &LaunchConfig,
        execution: CliExecutionCapability,
    ) -> Result<(ClaudeProcess, mpsc::Receiver<StreamEvent>)> {
        self.base.launch_claude(config, execution)
    }

    fn launch_codex(
        &self,
        config: &LaunchConfig,
        execution: CliExecutionCapability,
    ) -> Result<(CodexProcess, mpsc::Receiver<StreamEvent>)> {
        self.base.launch_codex(config, execution)
    }

    fn launch_local(
        &self,
        config: &LaunchConfig,
        permit: AdmissionPermit,
    ) -> Result<(OpenAiProcess, mpsc::Receiver<StreamEvent>)> {
        self.base.launch_local(config, permit)
    }

    fn launch_agy(
        &self,
        config: &LaunchConfig,
        execution: CliExecutionCapability,
    ) -> Result<(AgyProcess, mpsc::Receiver<StreamEvent>)> {
        self.base.launch_agy(config, execution)
    }

    fn launch_harness(
        &self,
        config: &LaunchConfig,
    ) -> Result<(HarnessProcess, mpsc::Receiver<StreamEvent>)> {
        let working_dir = config
            .working_dir
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new("/tmp"));
        self.base.mgr.harness_client.launch_with_binding(
            self.base.harness.monitor_generation,
            config,
            &self.base.harness.resolved_context_budget,
            config.system_prompt.clone(),
            working_dir,
            self.base.harness.conversation_history.clone(),
            self.base.harness.initial_admission_permit.clone(),
            self.base.mgr.memory_handle.clone(),
            self.base.harness.project_id,
            Arc::clone(&self.base.mgr.store),
            Arc::clone(self.base.mgr.event_bus()),
            self.base.harness.model_call_settlements.clone(),
            config.rsi_session_id,
            Some(self.base.mgr.agent_control()),
            self.base.harness.tool_policy.clone(),
            self.codegraph_binding.clone(),
        )
    }

    fn session_tool_policy(&self) -> Option<&rsi_common::harness_tool_policy::HarnessToolPolicy> {
        self.base.session_tool_policy()
    }
}

/// Family B — the two owned/static rotation sites (`resume_for_handoff_write`,
/// `spawn_rotation_child`). Constructs fresh provider clients inline. The Harness
/// arm rebuilds the SAME full in-process client fresh launches use, with an
/// [`AgentControlHandle`](crate::session::agent_verbs::AgentControlHandle)
/// reconstructed from the daemon-global collaborator `Arc`s — so a rotated
/// Harness session carries an identical tool set (native `rsi_control` +
/// `schedule_wake`). Both rotation sites differ only in the bound caller id.
pub(super) struct FreshLauncher {
    pub runtime_config: Arc<crate::config::RuntimeConfig>,
    pub harness: FreshHarnessCtx,
}

/// Collaborator handles the Family-B Harness arm needs to rebuild its
/// `AgentControlHandle` byte-for-byte. Cheap `Arc`/handle clones; only the
/// Harness arm reads them.
pub(super) struct FreshHarnessCtx {
    pub codegraph_handle: Option<crate::codegraph::IndexHandle>,
    pub custody_runtime: crate::sandbox::custody::CustodyExecutionRuntime,
    pub active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    pub completed: Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
    pub store: Arc<Mutex<Store>>,
    pub event_bus: Arc<crate::bus::EventBus>,
    pub model_call_settlements: ModelCallSettlementHandle,
    pub spawn_coordinator: Arc<SpawnCoordinator>,
    pub agent_message_arbiter: Arc<crate::session::agent_message_arbiter::AgentMessageArbiter>,
    pub process_registry_manager:
        Arc<crate::session::harness::tools::process_registry::HarnessProcessRegistryManager>,
    pub monitor_generation: u64,
    pub memory_handle: Option<crate::memory::worker::MemoryHandle>,
    pub initial_admission_permit: AdmissionPermit,
    /// Exact active budget already selected for this Session incarnation.
    pub resolved_context_budget: rsi_common::ResolvedContextBudget,
    /// Caller session id bound into the native `rsi_control` tools — the rotated
    /// session's own id (`session_id` for handoff-write, `child_id` for a
    /// rotation child). Bound server-side; the agent cannot forge it.
    pub bound_session_id: Uuid,
    /// #792: the rotated session's inherited Harness tool policy.
    pub tool_policy: Option<rsi_common::harness_tool_policy::HarnessToolPolicy>,
}

impl FreshLauncher {
    fn harness_client(
        codegraph_handle: Option<&crate::codegraph::IndexHandle>,
        runtime_config: &Arc<crate::config::RuntimeConfig>,
        agent_message_arbiter: &Arc<crate::session::agent_message_arbiter::AgentMessageArbiter>,
        process_registry_manager: &Arc<
            crate::session::harness::tools::process_registry::HarnessProcessRegistryManager,
        >,
    ) -> crate::session::harness::HarnessClient {
        let mut client = crate::session::harness::HarnessClient::new();
        client.set_runtime_config(Arc::clone(runtime_config));
        client.set_agent_message_arbiter(Arc::clone(agent_message_arbiter));
        client.set_process_registry_manager(Arc::clone(process_registry_manager));
        if let Some(handle) = codegraph_handle {
            client.set_codegraph_handle(handle.clone());
        }
        client
    }
}

impl ProviderLauncher for FreshLauncher {
    fn runtime_config(&self) -> &crate::config::RuntimeConfig {
        &self.runtime_config
    }
    fn launch_claude(
        &self,
        config: &LaunchConfig,
        execution: CliExecutionCapability,
    ) -> Result<(ClaudeProcess, mpsc::Receiver<StreamEvent>)> {
        ClaudeClient::new(Arc::clone(&self.runtime_config))?.launch(config, execution)
    }

    fn launch_codex(
        &self,
        config: &LaunchConfig,
        execution: CliExecutionCapability,
    ) -> Result<(CodexProcess, mpsc::Receiver<StreamEvent>)> {
        CodexClient::new(Arc::clone(&self.runtime_config))?.launch(config, execution)
    }

    fn launch_local(
        &self,
        config: &LaunchConfig,
        initial_admission_permit: AdmissionPermit,
    ) -> Result<(OpenAiProcess, mpsc::Receiver<StreamEvent>)> {
        let client = OpenAiClient::new_local()?;
        let model = config
            .model
            .clone()
            .unwrap_or_else(|| "qwen3:14b".to_string());
        let launch_invocation_id = initial_admission_permit.invocation_id();
        let control: Arc<dyn ModelCallControl> = Arc::new(StoreBackedModelCallControl::new(
            Arc::clone(&self.harness.store),
            Arc::clone(&self.harness.event_bus),
            self.harness.model_call_settlements.clone(),
            rsi_common::model_control::InvocationOwner {
                session_id: config.rsi_session_id,
                project_id: config.project_id,
                ..Default::default()
            },
            client.resolved_provider_label(),
            Some(model),
            "openai_compatible_api",
            config.effort.clone(),
            "session_openai_compatible",
            initial_admission_permit,
            rsi_common::model_control::ModelInvocationPurpose::SessionOpenAiCompatibleTurn,
            None,
            RuntimeExecutionRoute::LocalOpenAiCompatibleHttp,
        ));
        client.launch(config, control, launch_invocation_id)
    }

    fn launch_agy(
        &self,
        config: &LaunchConfig,
        execution: CliExecutionCapability,
    ) -> Result<(AgyProcess, mpsc::Receiver<StreamEvent>)> {
        AgyClient::new()?
            .with_runtime_config(self.runtime_config.clone())
            .launch(config, execution)
    }

    fn launch_harness(
        &self,
        config: &LaunchConfig,
    ) -> Result<(HarnessProcess, mpsc::Receiver<StreamEvent>)> {
        let control = crate::session::agent_verbs::AgentControlHandle::new(
            Arc::clone(&self.harness.active),
            Arc::clone(&self.harness.completed),
            Arc::clone(&self.harness.store),
            Arc::clone(&self.harness.event_bus),
            Arc::clone(&self.harness.spawn_coordinator),
        )
        .with_custody_runtime(self.harness.custody_runtime.clone());
        let harness_working_dir = config
            .working_dir
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new("/tmp"));
        Self::harness_client(
            self.harness.codegraph_handle.as_ref(),
            &self.runtime_config,
            &self.harness.agent_message_arbiter,
            &self.harness.process_registry_manager,
        )
        .launch(
            self.harness.monitor_generation,
            config,
            &self.harness.resolved_context_budget,
            config.system_prompt.clone(),
            harness_working_dir,
            None, // rotation launches fresh (no reconstructed history)
            self.harness.initial_admission_permit.clone(),
            self.harness.memory_handle.clone(),
            config.project_id,
            Arc::clone(&self.harness.store),
            Arc::clone(&self.harness.event_bus),
            self.harness.model_call_settlements.clone(),
            Some(self.harness.bound_session_id),
            Some(control),
            self.harness.tool_policy.clone(),
        )
    }

    fn session_tool_policy(&self) -> Option<&rsi_common::harness_tool_policy::HarnessToolPolicy> {
        self.harness.tool_policy.as_ref()
    }
}

#[cfg(test)]
mod codegraph_rotation_tests {
    use super::*;
    use crate::session::harness::tools::HarnessToolRegistry;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[test]
    fn fresh_harness_client_rebinds_registered_codegraph_scope() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let index = tempfile::tempdir().unwrap();
        let project_id = Uuid::new_v4();
        let workspace =
            crate::codegraph::RegisteredWorkspace::primary(project_id, root.path()).unwrap();
        let (_manager, handle) =
            crate::codegraph::IndexManager::new(index.path().to_path_buf(), vec![workspace])
                .unwrap();
        let store = Arc::new(Mutex::new(Store::open_in_memory().unwrap()));
        let runtime_config =
            crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        let agent_message_arbiter =
            Arc::new(crate::session::agent_message_arbiter::AgentMessageArbiter::new());
        let process_registry_manager = Arc::new(
            crate::session::harness::tools::process_registry::HarnessProcessRegistryManager::new(),
        );
        let client = FreshLauncher::harness_client(
            Some(&handle),
            &runtime_config,
            &agent_message_arbiter,
            &process_registry_manager,
        );
        let mut registered = HarnessToolRegistry::new();
        client.register_codegraph_tools(
            &mut registered,
            Some(project_id),
            Some(Uuid::new_v4()),
            root.path(),
            &store,
        );
        let names: Vec<_> = registered
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        assert_eq!(names, vec!["rsi_codegraph_status"]);

        let mut wrong_root = HarnessToolRegistry::new();
        client.register_codegraph_tools(
            &mut wrong_root,
            Some(project_id),
            Some(Uuid::new_v4()),
            other.path(),
            &store,
        );
        assert!(wrong_root.specs().is_empty());

        let mut wrong_project = HarnessToolRegistry::new();
        client.register_codegraph_tools(
            &mut wrong_project,
            Some(Uuid::new_v4()),
            Some(Uuid::new_v4()),
            root.path(),
            &store,
        );
        assert!(wrong_project.specs().is_empty());
    }
}

#[cfg(test)]
mod route_tests {
    use super::*;
    use rsi_common::types::ControllerConfirmationKindV1;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn route_child() -> std::io::Result<tokio::process::Child> {
        let mut command = tokio::process::Command::new("sleep");
        command.arg("30").kill_on_drop(true);
        // The real CLI has its own process group. Give the double the same
        // containment so force-stop reaps it instead of waiting for sleep.
        #[cfg(unix)]
        command.process_group(0);
        command.spawn()
    }

    struct RouteLauncher {
        runtime: Arc<crate::config::RuntimeConfig>,
        credential_available: bool,
        codex_starts: AtomicUsize,
        harness_starts: AtomicUsize,
        policy: Option<rsi_common::harness_tool_policy::HarnessToolPolicy>,
        /// The `tool_policy` each CLI launch actually received, by route.
        cli_seen: std::sync::Mutex<
            Vec<(
                &'static str,
                Option<rsi_common::harness_tool_policy::HarnessToolPolicy>,
            )>,
        >,
        contract: Arc<crate::provider_contract::ProviderContractService>,
    }

    impl RouteLauncher {
        fn with_policy(mut self) -> Self {
            self.policy = Some(rsi_common::harness_tool_policy::HarnessToolPolicy {
                web_access: Some(rsi_common::harness_tool_policy::WebAccessMode::Disabled),
                ..Default::default()
            });
            self
        }

        fn new(runtime: Arc<crate::config::RuntimeConfig>, credential_available: bool) -> Self {
            Self {
                runtime,
                credential_available,
                codex_starts: AtomicUsize::new(0),
                harness_starts: AtomicUsize::new(0),
                policy: None,
                cli_seen: std::sync::Mutex::new(Vec::new()),
                contract: crate::provider_contract::ProviderContractService::for_manager(),
            }
        }

        fn with_stored(
            mut self,
            policy: rsi_common::harness_tool_policy::HarnessToolPolicy,
        ) -> Self {
            self.policy = Some(policy);
            self
        }
    }

    impl ProviderLauncher for RouteLauncher {
        fn runtime_config(&self) -> &crate::config::RuntimeConfig {
            &self.runtime
        }
        fn session_tool_policy(
            &self,
        ) -> Option<&rsi_common::harness_tool_policy::HarnessToolPolicy> {
            self.policy.as_ref()
        }
        fn codex_custody_refusal(&self, config: &LaunchConfig) -> Option<String> {
            codex_custody_refusal_with(&self.contract, &self.runtime, config)
        }
        fn openrouter_credential_available(&self) -> bool {
            self.credential_available
        }
        fn bedrock_credential_available(&self) -> bool {
            self.credential_available
        }
        fn bedrock_region_available(&self) -> bool {
            true
        }
        fn launch_claude(
            &self,
            config: &LaunchConfig,
            _: CliExecutionCapability,
        ) -> Result<(crate::claude::ClaudeProcess, mpsc::Receiver<StreamEvent>)> {
            self.cli_seen
                .lock()
                .unwrap()
                .push(("claude", config.tool_policy.clone()));
            // No real Claude binary here: the recorded config is the assertion.
            Err(DaemonError::Process("route test: claude reached".into()))
        }
        fn launch_codex(
            &self,
            config: &LaunchConfig,
            _: CliExecutionCapability,
        ) -> Result<(CodexProcess, mpsc::Receiver<StreamEvent>)> {
            self.cli_seen
                .lock()
                .unwrap()
                .push(("codex", config.tool_policy.clone()));
            self.codex_starts.fetch_add(1, Ordering::SeqCst);
            let child = route_child()?;
            let (_, rx) = mpsc::channel(1);
            Ok((CodexProcess::from_child_for_route_test(child), rx))
        }
        fn launch_local(
            &self,
            _: &LaunchConfig,
            _: AdmissionPermit,
        ) -> Result<(OpenAiProcess, mpsc::Receiver<StreamEvent>)> {
            unreachable!()
        }
        fn launch_agy(
            &self,
            _: &LaunchConfig,
            _: CliExecutionCapability,
        ) -> Result<(AgyProcess, mpsc::Receiver<StreamEvent>)> {
            unreachable!()
        }
        fn launch_harness(
            &self,
            _: &LaunchConfig,
        ) -> Result<(HarnessProcess, mpsc::Receiver<StreamEvent>)> {
            self.harness_starts.fetch_add(1, Ordering::SeqCst);
            let (_, rx) = mpsc::channel(1);
            Ok((
                HarnessProcess {
                    task_handle: tokio::spawn(std::future::pending()),
                    cancel: tokio_util::sync::CancellationToken::new(),
                },
                rx,
            ))
        }
    }

    fn route_config(model: String) -> LaunchConfig {
        LaunchConfig {
            completion_gates: None,
            query: "route dispatch".into(),
            title: None,
            agent_role: None,
            epic_spawn_ordinal: None,
            working_dir: None,
            provider: Some(SessionProvider::OpenRouter),
            model: Some(model),
            configured_context_window: None,
            max_turns: None,
            system_prompt: None,
            resume_session_id: None,
            session_kind: None,
            project_id: None,
            rsi_session_id: None,
            rsi_socket: None,
            rsi_session_token: None,
            continued_from: None,
            openai_base_url: None,
            openai_api_key: None,
            conversation_history: None,
            workflow_id: None,
            workflow_id_override: None,
            max_retries: None,
            group_id: None,
            skip_project_model_default: false,
            tool_policy: None,
            model_invocation_purpose:
                rsi_common::model_control::ModelInvocationPurpose::SessionLaunchFresh,
            parent_id: None,
            effort: None,
            issue_identifier: None,
            issue_url: None,
            issue_tracker_id: None,
            scheduled_job_id: None,
            model_invocation_owner: None,
            model_invocation_dedup_key: None,
            model_invocation_request_fingerprint: None,
            sandbox: None,
            cargo_target_dir: None,
            execution_scratch: None,
            is_eval: false,
            skip_context_pipeline: true,
            capability_class: None,
            tags: Vec::new(),
            topology_node_id: None,
            topology_iteration: 0,
            closure_selector: None,
            structured_handoff: false,
        }
    }

    /// Issue #692: the provider-level chokepoint (reached by continuations,
    /// rotations and successors) refuses a model off the operator allowlist
    /// before any provider process is created, and lets an allowed one run.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn provider_spawn_refuses_a_model_off_the_operator_allowlist() {
        let guard = super::super::spawn_single_flight::acquire_spawn_guard(Uuid::new_v4()).await;
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        runtime
            .update_field("api_route.openrouter", &serde_json::json!("harness"))
            .unwrap();
        runtime
            .update_field(
                "launch_model_allowlist",
                &serde_json::json!(["vendor/allowed-model"]),
            )
            .unwrap();
        let launcher = RouteLauncher::new(Arc::clone(&runtime), true);

        for model in [Some("vendor/other-model".to_string()), None] {
            let mut config = route_config(String::new());
            config.model = model;
            let refused = spawn_provider_process(
                SessionProvider::OpenRouter,
                &config,
                &launcher,
                &AdmissionPermit::for_route_dispatch_test(),
                &guard,
            );
            let Err(DaemonError::PolicyDenied(reason)) = refused else {
                panic!("a model off the allowlist must be refused");
            };
            assert!(reason.contains("launch_model_not_allowed"), "{reason}");
            assert!(reason.contains("vendor/allowed-model"), "{reason}");
        }

        // A bracket suffix is part of a non-Claude model string (OpenRouter
        // preserves it on the wire), so it never rides the bare allowlist entry.
        let tagged = route_config("vendor/allowed-model[bogus]".to_string());
        let refused = spawn_provider_process(
            SessionProvider::OpenRouter,
            &tagged,
            &launcher,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        );
        let Err(DaemonError::PolicyDenied(reason)) = refused else {
            panic!("a fabricated variant tag must be refused");
        };
        assert!(reason.contains("launch_model_not_allowed"), "{reason}");

        let config = route_config("vendor/allowed-model".to_string());
        let (mut process, _) = spawn_provider_process(
            SessionProvider::OpenRouter,
            &config,
            &launcher,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        )
        .expect("an allowed model still spawns");
        process.kill().await.unwrap();
    }

    /// #2755: the provider-level chokepoint (fresh launches, continuations and
    /// rotations) refuses a Codex CLI launch with the typed hook reason before
    /// any process exists when the canary found the custody hook untrusted and
    /// the setting is on, and only then: no session identity (no hook installed),
    /// the setting off, an `ok`/`unchecked` result and a non-custody failure all
    /// admit.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn provider_spawn_refuses_a_codex_launch_whose_custody_hook_is_untrusted() {
        use crate::provider_contract::ContractProvider;
        use rsi_common::provider_contract::ProviderContractState as State;
        let guard = super::super::spawn_single_flight::acquire_spawn_guard(Uuid::new_v4()).await;
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        let launcher = RouteLauncher::new(Arc::clone(&runtime), true);
        let mut config = route_config("gpt-6-astra".to_string());
        config.rsi_session_id = Some(Uuid::new_v4());
        let spawn = |provider| {
            spawn_provider_process(
                provider,
                &config,
                &launcher,
                &AdmissionPermit::for_route_dispatch_test(),
                &guard,
            )
        };

        launcher.contract.seed_for_test(
            ContractProvider::Codex,
            State::Degraded,
            Some("codex_hook_hash_mismatch"),
        );
        for provider in [SessionProvider::Codex, SessionProvider::Pioneer] {
            let Err(DaemonError::PolicyDenied(reason)) = spawn(provider) else {
                panic!("{provider:?}: an untrusted custody hook must refuse the launch");
            };
            assert!(
                reason.starts_with("codex_hook_hash_mismatch:"),
                "{provider:?}: {reason}"
            );
        }
        assert_eq!(launcher.codex_starts.load(Ordering::SeqCst), 0);

        // The setting off launches without custody.
        runtime
            .update_field(
                "codex_refuse_untrusted_custody_hook",
                &serde_json::json!(false),
            )
            .unwrap();
        let (mut process, _) = spawn(SessionProvider::Codex).expect("setting off admits");
        process.kill().await.unwrap();
        runtime
            .update_field(
                "codex_refuse_untrusted_custody_hook",
                &serde_json::json!(true),
            )
            .unwrap();

        // No session identity: Codex installs no hook, so there is no custody
        // to lose and nothing to refuse.
        let anonymous = LaunchConfig {
            rsi_session_id: None,
            ..config.clone()
        };
        let (mut process, _) = spawn_provider_process(
            SessionProvider::Codex,
            &anonymous,
            &launcher,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        )
        .expect("no hook installed, nothing to refuse");
        process.kill().await.unwrap();

        // `ok`, `unchecked` and a non-custody degradation all admit.
        for (state, reason) in [
            (State::Ok, None),
            (State::Unchecked, Some("canary_failed")),
            (State::Degraded, Some("codex_handshake_failed")),
        ] {
            launcher
                .contract
                .seed_for_test(ContractProvider::Codex, state, reason);
            let (mut process, _) =
                spawn(SessionProvider::Codex).unwrap_or_else(|e| panic!("{state:?}: {e}"));
            process.kill().await.unwrap();
        }
    }

    /// Issue #1407: the provider-level backstop applies the operator provider
    /// profile: under `aws_only` no provider process starts for a non-Bedrock
    /// launch; `all` lets the same launch run.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn provider_spawn_refuses_a_non_bedrock_launch_under_the_aws_only_profile() {
        let guard = super::super::spawn_single_flight::acquire_spawn_guard(Uuid::new_v4()).await;
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        runtime
            .update_field("api_route.openrouter", &serde_json::json!("harness"))
            .unwrap();
        runtime
            .update_field("provider_profile", &serde_json::json!("aws_only"))
            .unwrap();
        let launcher = RouteLauncher::new(Arc::clone(&runtime), true);
        let config = route_config("vendor/some-model".to_string());
        for provider in [SessionProvider::OpenRouter, SessionProvider::Claude] {
            let refused = spawn_provider_process(
                provider,
                &config,
                &launcher,
                &AdmissionPermit::for_route_dispatch_test(),
                &guard,
            );
            let Err(DaemonError::PolicyDenied(reason)) = refused else {
                panic!("{provider:?}: a non-Bedrock launch must be refused");
            };
            assert!(reason.starts_with("provider_profile_refused"), "{reason}");
        }

        runtime
            .update_field("provider_profile", &serde_json::json!("all"))
            .unwrap();
        let (mut process, _) = spawn_provider_process(
            SessionProvider::OpenRouter,
            &config,
            &launcher,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        )
        .expect("`all` keeps today's behaviour");
        process.kill().await.unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_session_with_a_tool_policy_can_only_start_the_harness_loop() {
        let guard = super::super::spawn_single_flight::acquire_spawn_guard(Uuid::new_v4()).await;
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        runtime
            .update_field("api_route.openrouter", &serde_json::json!("harness"))
            .unwrap();
        // The opt-in fallback is what the policy guard must still refuse.
        runtime
            .update_field("api_route.fallback.openrouter", &serde_json::json!(true))
            .unwrap();
        let config = route_config(format!("vendor/policy-{}", Uuid::new_v4()));

        // The Harness route runs and enforces the policy.
        let harness = RouteLauncher::new(Arc::clone(&runtime), true).with_policy();
        let (mut process, _) = spawn_provider_process(
            SessionProvider::OpenRouter,
            &config,
            &harness,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        )
        .unwrap();
        assert!(matches!(process, ProviderProcess::Harness(_)));
        process.kill().await.unwrap();

        // A Harness -> Codex CLI preflight fallback would escape the policy.
        let fallback = RouteLauncher::new(Arc::clone(&runtime), false).with_policy();
        let refused = spawn_provider_process(
            SessionProvider::OpenRouter,
            &config,
            &fallback,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        );
        let Err(DaemonError::PolicyDenied(reason)) = refused else {
            panic!("fallback must be refused");
        };
        assert!(
            reason.starts_with("tool_policy_unsupported_for_provider:openrouter:web_access"),
            "{reason}"
        );
        assert_eq!(fallback.codex_starts.load(Ordering::SeqCst), 0);

        // A CLI provider is refused outright, before any process effect.
        let codex = RouteLauncher::new(Arc::clone(&runtime), true).with_policy();
        assert!(matches!(
            spawn_provider_process(
                SessionProvider::Codex,
                &config,
                &codex,
                &AdmissionPermit::for_route_dispatch_test(),
                &guard,
            ),
            Err(DaemonError::PolicyDenied(_))
        ));
        assert_eq!(codex.codex_starts.load(Ordering::SeqCst), 0);
    }

    /// #2758: a read-only policy is enforced by the Claude and Codex CLIs, so the
    /// guard lets those launch and hands them the resolved policy (a continue or
    /// rotation carries it on the launcher, not on the config); a policy field a
    /// CLI cannot express, or a route that enforces nothing, is refused typed
    /// before any process effect; the Harness loop refuses the CLI-only field.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_read_only_policy_reaches_the_cli_routes_and_nothing_else() {
        use rsi_common::harness_tool_policy::{CliAccessMode, HarnessToolPolicy, WebAccessMode};
        let guard = super::super::spawn_single_flight::acquire_spawn_guard(Uuid::new_v4()).await;
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        let config = route_config(format!("vendor/read-only-{}", Uuid::new_v4()));
        let read_only = HarnessToolPolicy {
            cli_access: Some(CliAccessMode::ReadOnly),
            ..HarnessToolPolicy::default()
        };
        let permit = AdmissionPermit::for_route_dispatch_test;

        // Codex, Pioneer: allowed; the launch config carries the stored policy.
        for provider in [SessionProvider::Codex, SessionProvider::Pioneer] {
            let launcher =
                RouteLauncher::new(Arc::clone(&runtime), true).with_stored(read_only.clone());
            let (mut process, _) =
                spawn_provider_process(provider, &config, &launcher, &permit(), &guard)
                    .unwrap_or_else(|error| panic!("{provider:?}: {error}"));
            process.kill().await.unwrap();
            assert_eq!(
                launcher.cli_seen.lock().unwrap().as_slice(),
                [("codex", Some(read_only.clone()))],
                "{provider:?}"
            );
        }
        // Claude: allowed (the test launcher stops after recording the config).
        let launcher =
            RouteLauncher::new(Arc::clone(&runtime), true).with_stored(read_only.clone());
        let reached = spawn_provider_process(
            SessionProvider::Claude,
            &config,
            &launcher,
            &permit(),
            &guard,
        );
        assert!(
            matches!(&reached, Err(DaemonError::Process(text)) if text.contains("claude reached")),
            "{:?}",
            reached.err()
        );
        assert_eq!(
            launcher.cli_seen.lock().unwrap().as_slice(),
            [("claude", Some(read_only.clone()))]
        );
        // No policy: the config reaches the launcher untouched.
        let launcher = RouteLauncher::new(Arc::clone(&runtime), true);
        let _ = spawn_provider_process(
            SessionProvider::Claude,
            &config,
            &launcher,
            &permit(),
            &guard,
        );
        assert_eq!(
            launcher.cli_seen.lock().unwrap().as_slice(),
            [("claude", None)]
        );

        // A field a CLI cannot express is refused, never dropped.
        let mixed = HarnessToolPolicy {
            web_access: Some(WebAccessMode::Disabled),
            ..read_only.clone()
        };
        for (provider, what) in [
            (SessionProvider::Claude, "claude:web_access"),
            (SessionProvider::Codex, "codex:web_access"),
        ] {
            let launcher =
                RouteLauncher::new(Arc::clone(&runtime), true).with_stored(mixed.clone());
            let refused = spawn_provider_process(provider, &config, &launcher, &permit(), &guard);
            let Err(DaemonError::PolicyDenied(reason)) = refused else {
                panic!("{provider:?}: an inexpressible field must be refused");
            };
            assert!(
                reason.starts_with(&format!("tool_policy_unsupported_for_provider:{what}")),
                "{reason}"
            );
            assert!(launcher.cli_seen.lock().unwrap().is_empty());
        }

        // The Harness loop has no CLI sandbox: it must not run a read-only policy
        // unrestricted.
        runtime
            .update_field("api_route.openrouter", &serde_json::json!("harness"))
            .unwrap();
        let launcher = RouteLauncher::new(Arc::clone(&runtime), true).with_stored(read_only);
        let refused = spawn_provider_process(
            SessionProvider::OpenRouter,
            &config,
            &launcher,
            &permit(),
            &guard,
        );
        let Err(DaemonError::PolicyDenied(reason)) = refused else {
            panic!("a Harness launch must refuse cli_access");
        };
        assert!(
            reason.starts_with("tool_policy_unsupported_for_provider:harness:cli_access"),
            "{reason}"
        );
        assert_eq!(launcher.harness_starts.load(Ordering::SeqCst), 0);
    }

    /// The daemon-wide defaults are never stored, so a default-only restriction
    /// must bind the route guard too: a Harness-loop session it restricts cannot
    /// fall back to the Codex CLI. With all-default config the fallback stays.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_default_only_restriction_refuses_the_codex_fallback() {
        let guard = super::super::spawn_single_flight::acquire_spawn_guard(Uuid::new_v4()).await;
        let config = route_config(format!("vendor/default-only-{}", Uuid::new_v4()));

        // Restricted by defaults only (no stored policy on the launcher).
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        runtime
            .update_field("api_route.openrouter", &serde_json::json!("harness"))
            .unwrap();
        runtime
            .update_field("api_route.fallback.openrouter", &serde_json::json!(true))
            .unwrap();
        runtime
            .update_field("harness_web_access", &serde_json::json!("disabled"))
            .unwrap();
        let fallback = RouteLauncher::new(Arc::clone(&runtime), false);
        assert!(fallback.session_tool_policy().is_none());
        let refused = spawn_provider_process(
            SessionProvider::OpenRouter,
            &config,
            &fallback,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        );
        let Err(DaemonError::PolicyDenied(reason)) = refused else {
            panic!("a default-restricted session must not fall back to the Codex CLI");
        };
        assert!(
            reason.starts_with("tool_policy_unsupported_for_provider:openrouter:daemon_defaults"),
            "{reason}"
        );
        assert_eq!(fallback.codex_starts.load(Ordering::SeqCst), 0);

        // The Harness route itself still runs under the defaults.
        let harness = RouteLauncher::new(Arc::clone(&runtime), true);
        let (mut process, _) = spawn_provider_process(
            SessionProvider::OpenRouter,
            &config,
            &harness,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        )
        .unwrap();
        assert!(matches!(process, ProviderProcess::Harness(_)));
        process.kill().await.unwrap();

        // A CLI provider is not restricted by Harness defaults.
        let codex = RouteLauncher::new(Arc::clone(&runtime), true);
        let (mut process, _) = spawn_provider_process(
            SessionProvider::Codex,
            &config,
            &codex,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        )
        .unwrap();
        assert!(matches!(process, ProviderProcess::Codex(_)));
        process.kill().await.unwrap();

        // With the operator opt-in and no restriction, the preflight fallback runs.
        let plain = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        plain
            .update_field("api_route.openrouter", &serde_json::json!("harness"))
            .unwrap();
        plain
            .update_field("api_route.fallback.openrouter", &serde_json::json!(true))
            .unwrap();
        let fallback = RouteLauncher::new(plain, false);
        let (mut process, _) = spawn_provider_process(
            SessionProvider::OpenRouter,
            &config,
            &fallback,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        )
        .unwrap();
        assert!(matches!(process, ProviderProcess::Codex(_)));
        process.kill().await.unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn openrouter_spawn_dispatch_selects_harness_codex_and_preflight_fallback() {
        let model = format!("vendor/dispatch-{}", Uuid::new_v4());
        let config = route_config(model);
        let guard = super::super::spawn_single_flight::acquire_spawn_guard(Uuid::new_v4()).await;
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        runtime
            .update_field("api_route.openrouter", &serde_json::json!("harness"))
            .unwrap();
        let harness_launcher = RouteLauncher::new(Arc::clone(&runtime), true);
        let (mut process, _) = spawn_provider_process(
            SessionProvider::OpenRouter,
            &config,
            &harness_launcher,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        )
        .unwrap();
        assert!(matches!(process, ProviderProcess::Harness(_)));
        assert_eq!(harness_launcher.harness_starts.load(Ordering::SeqCst), 1);
        process.kill().await.unwrap();

        // The Codex fallback is an operator opt-in (default off, #3141).
        runtime
            .update_field("api_route.fallback.openrouter", &serde_json::json!(true))
            .unwrap();
        let fallback_launcher = RouteLauncher::new(Arc::clone(&runtime), false);
        let (mut process, mut events) = spawn_provider_process(
            SessionProvider::OpenRouter,
            &config,
            &fallback_launcher,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        )
        .unwrap();
        assert!(matches!(process, ProviderProcess::Codex(_)));
        assert_eq!(events.recv().await.unwrap().event_type, "route_fallback");
        assert_eq!(
            events.recv().await.unwrap().event_type,
            "credential_cli_exposure"
        );
        process.kill().await.unwrap();

        runtime
            .update_field("api_route.fallback.openrouter", &serde_json::json!(false))
            .unwrap();
        let refusing_launcher = RouteLauncher::new(Arc::clone(&runtime), false);
        assert!(matches!(
            spawn_provider_process(
                SessionProvider::OpenRouter,
                &config,
                &refusing_launcher,
                &AdmissionPermit::for_route_dispatch_test(),
                &guard
            ),
            Err(DaemonError::OpenRouterRoutePreflight {
                cause: "missing_credential"
            })
        ));
        assert_eq!(refusing_launcher.codex_starts.load(Ordering::SeqCst), 0);

        crate::openrouter::record_test_catalog_tool_support(
            config.model.as_deref().unwrap(),
            false,
        );
        let no_tools_launcher = RouteLauncher::new(Arc::clone(&runtime), true);
        assert!(matches!(
            spawn_provider_process(
                SessionProvider::OpenRouter,
                &config,
                &no_tools_launcher,
                &AdmissionPermit::for_route_dispatch_test(),
                &guard
            ),
            Err(DaemonError::OpenRouterRoutePreflight {
                cause: "no_tool_support"
            })
        ));
        assert_eq!(no_tools_launcher.codex_starts.load(Ordering::SeqCst), 0);
        runtime
            .update_field("api_route.fallback.openrouter", &serde_json::json!(true))
            .unwrap();
        let no_tools_fallback = RouteLauncher::new(Arc::clone(&runtime), true);
        let (mut process, mut events) = spawn_provider_process(
            SessionProvider::OpenRouter,
            &config,
            &no_tools_fallback,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        )
        .unwrap();
        assert!(matches!(process, ProviderProcess::Codex(_)));
        assert_eq!(
            events.recv().await.unwrap().data["reason"],
            "no_tool_support"
        );
        process.kill().await.unwrap();

        runtime
            .update_field("api_route.openrouter", &serde_json::json!("codex_cli"))
            .unwrap();
        let codex_launcher = RouteLauncher::new(runtime, false);
        let (mut process, _) = spawn_provider_process(
            SessionProvider::OpenRouter,
            &config,
            &codex_launcher,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        )
        .unwrap();
        assert!(matches!(process, ProviderProcess::Codex(_)));
        assert_eq!(codex_launcher.harness_starts.load(Ordering::SeqCst), 0);
        process.kill().await.unwrap();
    }

    /// #3141 (a): with the default config (Harness route, OpenRouter fallback
    /// off) a failed Harness preflight refuses the launch with the typed error
    /// and never starts the Codex CLI, for a fresh launch, a continuation (a
    /// resumed provider thread) and a rotation (a continued-from child). All
    /// four spawn sites, through `CachedLauncher` and `FreshLauncher`, reach
    /// this one chokepoint; no `credential_cli_exposure` event exists either.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn openrouter_preflight_failure_with_fallback_off_refuses_every_launch_shape() {
        let guard = super::super::spawn_single_flight::acquire_spawn_guard(Uuid::new_v4()).await;
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        assert!(!runtime.openrouter_fallback_enabled());

        let mut fresh = route_config(format!("vendor/refuse-{}", Uuid::new_v4()));
        fresh.rsi_session_id = Some(Uuid::new_v4());
        let mut continuation = fresh.clone();
        continuation.rsi_session_id = Some(Uuid::new_v4());
        continuation.resume_session_id = Some("provider-thread".into());
        let mut rotation = fresh.clone();
        rotation.rsi_session_id = Some(Uuid::new_v4());
        rotation.continued_from = Some(Uuid::new_v4());
        let mut unsupported = fresh.clone();
        unsupported.rsi_session_id = Some(Uuid::new_v4());
        unsupported.model = Some("not-a-vendor-model".into());

        for (shape, config, credential, cause) in [
            ("fresh", &fresh, false, "missing_credential"),
            ("continuation", &continuation, false, "missing_credential"),
            ("rotation", &rotation, false, "missing_credential"),
            ("unsupported model", &unsupported, true, "unsupported_model"),
        ] {
            let launcher = RouteLauncher::new(Arc::clone(&runtime), credential);
            let refused = spawn_provider_process(
                SessionProvider::OpenRouter,
                config,
                &launcher,
                &AdmissionPermit::for_route_dispatch_test(),
                &guard,
            );
            let Err(error) = refused else {
                panic!("{shape}: a failed preflight must refuse the launch");
            };
            assert!(
                matches!(&error, DaemonError::OpenRouterRoutePreflight { cause: c } if *c == cause),
                "{shape}: {error}"
            );
            assert!(
                error
                    .to_string()
                    .starts_with(&format!("openrouter_harness_preflight:{cause}:")),
                "{shape}: the message leads with the stable code: {error}"
            );
            assert_eq!(launcher.codex_starts.load(Ordering::SeqCst), 0, "{shape}");
            assert_eq!(launcher.harness_starts.load(Ordering::SeqCst), 0, "{shape}");
            assert_eq!(
                crate::api_route_selection::take_selected(config.rsi_session_id.unwrap()),
                None,
                "{shape}: a refused launch records no engine"
            );
        }
    }

    /// #3141 (b) and the effective-route record: with the operator's opt-in
    /// `api_route.fallback.openrouter` the fallback still works, tagged with its
    /// cause, and every launch notes the engine it took and the source of the
    /// route (default, global setting, model override).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn openrouter_opt_in_fallback_works_and_every_launch_records_its_engine_and_source() {
        let guard = super::super::spawn_single_flight::acquire_spawn_guard(Uuid::new_v4()).await;
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        let mut config = route_config(format!("vendor/record-{}", Uuid::new_v4()));
        let session_id = Uuid::new_v4();
        config.rsi_session_id = Some(session_id);

        // Default route (no operator choice): Harness, source `default`.
        let launcher = RouteLauncher::new(Arc::clone(&runtime), true);
        let (mut process, _) = spawn_provider_process(
            SessionProvider::OpenRouter,
            &config,
            &launcher,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        )
        .unwrap();
        assert!(matches!(process, ProviderProcess::Harness(_)));
        process.kill().await.unwrap();
        assert_eq!(
            crate::api_route_selection::take_selected(session_id),
            Some(crate::api_route_selection::RouteSelection {
                engine: "harness",
                configured_route: "harness",
                source: "default",
                fallback_cause: None,
            })
        );

        // The operator's global setting is recorded as such.
        runtime
            .update_field("api_route.openrouter", &serde_json::json!("harness"))
            .unwrap();
        let (mut process, _) = spawn_provider_process(
            SessionProvider::OpenRouter,
            &config,
            &launcher,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        )
        .unwrap();
        process.kill().await.unwrap();
        assert_eq!(
            crate::api_route_selection::take_selected(session_id).map(|selection| selection.source),
            Some("global_setting")
        );

        // The opt-in fallback: Codex runs, with its cause and events.
        runtime
            .update_field("api_route.fallback.openrouter", &serde_json::json!(true))
            .unwrap();
        let broken = RouteLauncher::new(Arc::clone(&runtime), false);
        let (mut process, mut events) = spawn_provider_process(
            SessionProvider::OpenRouter,
            &config,
            &broken,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        )
        .unwrap();
        assert!(matches!(process, ProviderProcess::Codex(_)));
        assert_eq!(broken.codex_starts.load(Ordering::SeqCst), 1);
        assert_eq!(events.recv().await.unwrap().event_type, "route_fallback");
        assert_eq!(
            events.recv().await.unwrap().event_type,
            "credential_cli_exposure"
        );
        process.kill().await.unwrap();
        assert_eq!(
            crate::api_route_selection::take_selected(session_id),
            Some(crate::api_route_selection::RouteSelection {
                engine: "codex_cli",
                configured_route: "harness",
                source: "global_setting",
                fallback_cause: Some("missing_credential"),
            })
        );

        // An explicit per-model `codex_cli` override is an opt-in, not a
        // fallback: it records its source and no cause.
        runtime
            .update_field(
                &format!("api_route.openrouter.{}", config.model.as_deref().unwrap()),
                &serde_json::json!("codex_cli"),
            )
            .unwrap();
        let codex = RouteLauncher::new(Arc::clone(&runtime), true);
        let (mut process, _) = spawn_provider_process(
            SessionProvider::OpenRouter,
            &config,
            &codex,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        )
        .unwrap();
        assert!(matches!(process, ProviderProcess::Codex(_)));
        process.kill().await.unwrap();
        assert_eq!(
            crate::api_route_selection::take_selected(session_id),
            Some(crate::api_route_selection::RouteSelection {
                engine: "codex_cli",
                configured_route: "codex_cli",
                source: "model_override",
                fallback_cause: None,
            })
        );
    }

    /// #3141 (c): Bedrock is unchanged under the defaults (fallback on), and
    /// neither the OpenRouter opt-in nor its legacy-key write can change it
    /// except through the Bedrock value.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn bedrock_fallback_is_unchanged_under_defaults_and_independent_of_openrouter() {
        let mut config = route_config("global.openai.gpt-5.6-sol".into());
        config.provider = Some(SessionProvider::Bedrock);
        let guard = super::super::spawn_single_flight::acquire_spawn_guard(Uuid::new_v4()).await;
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        runtime
            .update_field("api_route.bedrock", &serde_json::json!("harness"))
            .unwrap();
        assert!(runtime.bedrock_fallback_enabled());
        assert!(!runtime.openrouter_fallback_enabled());

        // Defaults: a failed Bedrock preflight still falls back to Codex.
        let broken = RouteLauncher::new(Arc::clone(&runtime), false);
        let (mut process, mut events) = spawn_provider_process(
            SessionProvider::Bedrock,
            &config,
            &broken,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        )
        .unwrap();
        assert!(matches!(process, ProviderProcess::Codex(_)));
        assert_eq!(events.recv().await.unwrap().event_type, "route_fallback");
        process.kill().await.unwrap();

        // Turning the OpenRouter opt-in on or off does not touch Bedrock.
        runtime
            .update_field("api_route.fallback.openrouter", &serde_json::json!(false))
            .unwrap();
        assert!(runtime.bedrock_fallback_enabled());

        // Only the Bedrock key (or its legacy alias) refuses Bedrock.
        runtime
            .update_field("api_route.fallback", &serde_json::json!(false))
            .unwrap();
        let refusing = RouteLauncher::new(Arc::clone(&runtime), false);
        assert!(matches!(
            spawn_provider_process(SessionProvider::Bedrock, &config, &refusing, &AdmissionPermit::for_route_dispatch_test(), &guard),
            Err(DaemonError::InvalidParam(message)) if message.contains("missing_credential")
        ));
        assert_eq!(refusing.codex_starts.load(Ordering::SeqCst), 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn openrouter_controller_and_rotation_witness_accepts_each_installed_route() {
        let child = route_child().unwrap();
        let mut codex = ProviderProcess::Codex(CodexProcess::from_child_for_route_test(child));
        assert_eq!(
            installed_provider_confirmation(SessionProvider::OpenRouter, &mut codex),
            Some(ControllerConfirmationKindV1::InstalledProvider)
        );
        assert!(matches_live_controller_confirmation(
            SessionProvider::OpenRouter,
            ControllerConfirmationKindV1::InstalledProvider,
            &mut codex
        ));
        codex.kill().await.unwrap();

        let mut harness = ProviderProcess::Harness(HarnessProcess {
            task_handle: tokio::spawn(std::future::pending()),
            cancel: tokio_util::sync::CancellationToken::new(),
        });
        assert_eq!(
            installed_provider_confirmation(SessionProvider::OpenRouter, &mut harness),
            Some(ControllerConfirmationKindV1::InstalledProvider)
        );
        assert!(matches_live_controller_confirmation(
            SessionProvider::OpenRouter,
            ControllerConfirmationKindV1::InstalledProvider,
            &mut harness
        ));
        harness.kill().await.unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn bedrock_dispatch_selects_both_routes_and_preflight_fallback() {
        let mut config = route_config("global.openai.gpt-5.6-sol".into());
        config.provider = Some(SessionProvider::Bedrock);
        let guard = super::super::spawn_single_flight::acquire_spawn_guard(Uuid::new_v4()).await;
        let runtime = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        let codex_launcher = RouteLauncher::new(Arc::clone(&runtime), true);
        let (mut process, _) = spawn_provider_process(
            SessionProvider::Bedrock,
            &config,
            &codex_launcher,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        )
        .unwrap();
        assert!(matches!(process, ProviderProcess::Codex(_)));
        process.kill().await.unwrap();

        runtime
            .update_field("api_route.bedrock", &serde_json::json!("harness"))
            .unwrap();
        let harness_launcher = RouteLauncher::new(Arc::clone(&runtime), true);
        let (mut process, _) = spawn_provider_process(
            SessionProvider::Bedrock,
            &config,
            &harness_launcher,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        )
        .unwrap();
        assert!(matches!(process, ProviderProcess::Harness(_)));
        assert_eq!(harness_launcher.harness_starts.load(Ordering::SeqCst), 1);
        process.kill().await.unwrap();

        let fallback_launcher = RouteLauncher::new(Arc::clone(&runtime), false);
        let (mut process, mut events) = spawn_provider_process(
            SessionProvider::Bedrock,
            &config,
            &fallback_launcher,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        )
        .unwrap();
        assert!(matches!(process, ProviderProcess::Codex(_)));
        assert_eq!(events.recv().await.unwrap().event_type, "route_fallback");
        assert_eq!(
            events.recv().await.unwrap().event_type,
            "credential_cli_exposure"
        );
        process.kill().await.unwrap();

        runtime
            .update_field("api_route.fallback", &serde_json::json!(false))
            .unwrap();
        let refusing = RouteLauncher::new(Arc::clone(&runtime), false);
        assert!(matches!(
            spawn_provider_process(
                SessionProvider::Bedrock, &config, &refusing,
                &AdmissionPermit::for_route_dispatch_test(), &guard,
            ),
            Err(DaemonError::InvalidParam(message)) if message.contains("missing_credential")
        ));

        runtime
            .update_field(
                "api_route.bedrock.global.openai.gpt-5.6-sol",
                &serde_json::json!("codex_cli"),
            )
            .unwrap();
        let override_launcher = RouteLauncher::new(runtime, false);
        let (mut process, _) = spawn_provider_process(
            SessionProvider::Bedrock,
            &config,
            &override_launcher,
            &AdmissionPermit::for_route_dispatch_test(),
            &guard,
        )
        .unwrap();
        assert!(matches!(process, ProviderProcess::Codex(_)));
        process.kill().await.unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn bedrock_controller_witness_accepts_harness_process() {
        let mut harness = ProviderProcess::Harness(HarnessProcess {
            task_handle: tokio::spawn(std::future::pending()),
            cancel: tokio_util::sync::CancellationToken::new(),
        });
        assert_eq!(
            installed_provider_confirmation(SessionProvider::Bedrock, &mut harness),
            Some(ControllerConfirmationKindV1::InstalledProvider)
        );
        assert!(matches_live_controller_confirmation(
            SessionProvider::Bedrock,
            ControllerConfirmationKindV1::InstalledProvider,
            &mut harness,
        ));
        harness.kill().await.unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn openrouter_fallback_emits_route_and_cli_exposure_before_provider_events() {
        let (tx, source) = mpsc::channel(1);
        tx.send(StreamEvent {
            event_type: "system".into(),
            data: serde_json::json!({}),
        })
        .await
        .unwrap();
        drop(tx);
        let mut rx = prepend_fallback_events(source, "no_tool_support");
        let fallback = rx.recv().await.unwrap();
        assert_eq!(fallback.event_type, "route_fallback");
        assert_eq!(fallback.data["reason"], "no_tool_support");
        let exposure = rx.recv().await.unwrap();
        assert_eq!(exposure.event_type, "credential_cli_exposure");
        assert_eq!(exposure.data["reason"], "fallback");
        assert_eq!(rx.recv().await.unwrap().event_type, "system");
    }
}
