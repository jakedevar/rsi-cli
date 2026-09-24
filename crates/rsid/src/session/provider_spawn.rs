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
            | (SessionProvider::Codex, ProviderProcess::Codex(_))
            | (SessionProvider::Pioneer, ProviderProcess::Codex(_))
            | (SessionProvider::OpenRouter, ProviderProcess::Codex(_))
            | (SessionProvider::Bedrock, ProviderProcess::Codex(_))
            | (SessionProvider::Local, ProviderProcess::Local(_))
            | (
                SessionProvider::Antigravity,
                ProviderProcess::Antigravity(_)
            )
            | (SessionProvider::Harness, ProviderProcess::Harness(_))
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
    launcher: &L,
    admission_permit: &AdmissionPermit,
    _guard: &SpawnGuard,
) -> Result<(ProviderProcess, mpsc::Receiver<StreamEvent>)> {
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
        SessionProvider::Bedrock | SessionProvider::OpenRouter => {
            let (p, rx) = launcher.launch_codex(
                config,
                admission_permit.claim_cli_execution(RuntimeExecutionRoute::CodexCli)?,
            )?;
            Ok((ProviderProcess::Codex(p), rx))
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

/// Parametrizes the genuine per-site launch differences behind a closed set of
/// two impls: [`CachedLauncher`] (the `&self` sites, reusing the manager's
/// cached provider clients) and [`FreshLauncher`] (the rotation sites, building
/// fresh clients inline). Each leaf launches one provider and returns the same
/// `(Process, event_rx)` pair the primitive wraps.
pub(super) trait ProviderLauncher {
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
}

impl ProviderLauncher for CachedLauncher<'_> {
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
        )
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
    pub memory_handle: Option<crate::memory::worker::MemoryHandle>,
    pub initial_admission_permit: AdmissionPermit,
    /// Exact active budget already selected for this Session incarnation.
    pub resolved_context_budget: rsi_common::ResolvedContextBudget,
    /// Caller session id bound into the native `rsi_control` tools — the rotated
    /// session's own id (`session_id` for handoff-write, `child_id` for a
    /// rotation child). Bound server-side; the agent cannot forge it.
    pub bound_session_id: Uuid,
}

impl FreshLauncher {
    fn harness_client(
        codegraph_handle: Option<&crate::codegraph::IndexHandle>,
    ) -> crate::session::harness::HarnessClient {
        let mut client = crate::session::harness::HarnessClient::new();
        if let Some(handle) = codegraph_handle {
            client.set_codegraph_handle(handle.clone());
        }
        client
    }
}

impl ProviderLauncher for FreshLauncher {
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
        AgyClient::new()?.launch(config, execution)
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
        Self::harness_client(self.harness.codegraph_handle.as_ref()).launch(
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
        )
    }
}

#[cfg(test)]
mod codegraph_rotation_tests {
    use super::*;
    use crate::session::harness::tools::HarnessToolRegistry;

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
        let client = FreshLauncher::harness_client(Some(&handle));
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
