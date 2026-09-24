//! Session management for the flywheel daemon.
//!
//! `SessionManager` orchestrates AI CLI subprocess lifecycles. Domain logic
//! is split across submodules:
//! - `launch`       — session creation and restoration
//! - `monitor`      — async event stream consumption
//! - `rotation`     — context window rotation and child spawning
//! - `lifecycle`    — continue, interrupt, archive, delete, pin
//! - `queries`      — read-only data access for RPC handlers
//! - `projects`     — project CRUD and index management
//! - `workflows`    — workflow query operations
//! - `persistence`  — background SQLite write worker
//! - `title`        — AI-generated session title helpers
//! - `types`        — supporting enums and structs

pub(crate) mod agent_message_arbiter;
pub(crate) mod agent_message_delivery;
pub(crate) mod agent_message_dispatcher;
pub(crate) mod agent_message_reconciler;
pub mod agent_verbs;
mod archive_cleanup;
mod cards;
pub mod chain_driver;
mod cohort_settlement;
mod context_pipeline;
mod delegated_operator;
mod esp_games;
mod graph_executions;
pub(crate) mod graph_runner;
pub(crate) mod harness;
pub(crate) mod harness_hash;
pub mod hierarchy;
mod hierarchy_ops;
mod index_status;
mod labels;
mod launch;
pub(crate) mod lifecycle;
pub(crate) mod manager_actions;
mod manager_coordinator;
pub(crate) mod manager_ledger;
pub(crate) mod manager_reviews;
mod manager_succession;
mod monitor;
mod outcome;
mod pending_approvals;
mod persistence;
pub mod preamble;
mod projects;
mod provider_spawn;
mod queries;
pub(crate) mod question;
mod reaper;
#[cfg(test)]
pub(crate) use reaper::fail_runtime_orphan_reap_for_test;
pub(crate) mod recursive_bridge;
pub(crate) mod retry_policy;
mod rotation;
mod rotation_coordinator;
pub mod spawn_coordinator;
pub mod spawn_directive;
mod spawn_single_flight;
pub(crate) use spawn_single_flight::RotationPublicationGuards;
mod summarizer;
pub(crate) mod tag_ops;
pub(crate) mod title;
pub(crate) mod topology_bridge;
pub(crate) mod topology_ops;
pub mod types;
pub(crate) mod until_evaluator;
mod workflows;

#[cfg(test)]
mod tests;

/// #670 R2 S2: `AgentArchiveChild` store and session-layer tests.
#[cfg(test)]
mod agent_archive_child_tests;

/// C-P2-23 Group A: Issue 21 Phase 2 production-path evidence.
#[cfg(test)]
mod issue21_phase2_tests;

pub use launch::{SandboxBuildCacheReclaimReport, SandboxFilesystemStats};
pub(crate) use rotation_coordinator::RotationState;
pub(crate) use types::PersistenceHandle;

/// A5 orphan-reaper (Change 2). Re-exported `#[doc(hidden)]` from the otherwise
/// private `reaper` submodule solely so the hermetic `orphan_reap_injection`
/// integration test (an external crate) can drive it; it is not part of the
/// daemon's public API. The in-process call site is `continue_session`.
pub use reaper::reap_orphans_for_session;
#[doc(hidden)]
pub use reaper::reap_startup_process_ownership_checked;

/// P2-06c: the reconciliation worker's tick cadence, for `main.rs`'s loop.
pub use agent_message_reconciler::AGENT_MESSAGE_RECONCILE_INTERVAL_SECS;

use crate::agy::AgyClient;
use crate::bus::EventBus;
use crate::claude::ClaudeClient;
use crate::codex::CodexClient;
use crate::config::RuntimeConfig;
use crate::error::{DaemonError, Result};
use crate::monitor as token_monitor;
use crate::openai::OpenAiClient;
use crate::project_cache::ProjectIndex;
use crate::provider_capabilities::CatalogRefreshReason;
use crate::sandbox::SandboxAllocator;
use crate::session::harness::HarnessClient;
use crate::store::Store;
use crate::tool_registry::{ToolRegistry, register_builtin_tools};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::RwLock;
use types::{CompletedSession, TrackedSession};
use uuid::Uuid;

/// In-memory A6 authority registry with bounded lookup in both directions.
/// Production establishment keeps exactly one current token per session.
#[derive(Debug, Default)]
pub(crate) struct AgentTokenRegistry {
    by_token: HashMap<String, Uuid>,
    by_session: HashMap<Uuid, String>,
}

impl AgentTokenRegistry {
    pub(crate) fn get(&self, token: &str) -> Option<&Uuid> {
        self.by_token.get(token)
    }

    pub(crate) fn token_for_session(&self, session_id: Uuid) -> Option<&str> {
        self.by_session.get(&session_id).map(String::as_str)
    }

    pub(crate) fn insert(&mut self, token: String, session_id: Uuid) -> Option<Uuid> {
        if let Some(previous_token) = self.by_session.insert(session_id, token.clone()) {
            self.by_token.remove(&previous_token);
        }
        let previous_session = self.by_token.get(&token).copied();
        if let Some(previous_session) = previous_session
            && previous_session != session_id
            && self.by_session.get(&previous_session) == Some(&token)
        {
            self.by_session.remove(&previous_session);
        }
        self.by_token.insert(token, session_id);
        previous_session
    }

    pub(crate) fn revoke_session(&mut self, session_id: Uuid) {
        if let Some(token) = self.by_session.remove(&session_id) {
            self.by_token.remove(&token);
        }
    }

    /// Revoke only the authority minted for one observed launch incarnation.
    /// A later remint for the same logical Session must survive stale cleanup.
    pub(crate) fn revoke_session_if_token(&mut self, session_id: Uuid, expected: &str) -> bool {
        if self.token_for_session(session_id) != Some(expected) {
            return false;
        }
        self.revoke_session(session_id);
        true
    }

    #[cfg(test)]
    pub(crate) fn values(&self) -> impl Iterator<Item = &Uuid> {
        self.by_token.values()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.by_token.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.by_token.is_empty()
    }
}

#[derive(Debug)]
pub(super) enum ControllerCandidateCommitError {
    Cancelled,
    Confirmation(crate::idea_control::IdeaControlError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SameIdControllerGrantOutcome {
    Installed,
    NotAssigned,
    EstablishmentInvalid,
}

impl std::fmt::Display for ControllerCandidateCommitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => formatter.write_str("controller candidate cancelled"),
            Self::Confirmation(error) => error.fmt(formatter),
        }
    }
}

/// Drive a retry receiver with **bounded concurrency** (A9.1 / D7).
///
/// Each received session id is dispatched to `handler` on its own task, with at
/// most `concurrency` handlers in flight at once. A single slow or hung handler
/// occupies one permit but never blocks the others — the fix for the F-004
/// liveness gap where the previously-serial retry loop let one stuck
/// `launch_retry` stall every other queued retry. This is safe because
/// `launch_retry` single-flights per session id (A9), so concurrent dispatch of
/// distinct sessions cannot double-spawn. Returns when the channel closes (all
/// senders dropped) or the internal semaphore is closed.
pub async fn run_bounded_retry_dispatch<F, Fut>(
    mut rx: tokio::sync::mpsc::Receiver<Uuid>,
    concurrency: usize,
    handler: F,
) where
    F: Fn(Uuid) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let semaphore = Arc::new(tokio::sync::Semaphore::new(concurrency));
    while let Some(session_id) = rx.recv().await {
        // Acquiring here bounds in-flight launches and applies backpressure to
        // the retry channel when every permit is in use.
        let permit = match Arc::clone(&semaphore).acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => break, // semaphore closed — daemon shutting down
        };
        let task = handler(session_id);
        tokio::spawn(async move {
            let _permit = permit; // held until this launch completes
            task.await;
        });
    }
}

pub struct SessionManager {
    pub(super) active: Arc<RwLock<HashMap<Uuid, TrackedSession>>>,
    pub(super) completed: Arc<RwLock<HashMap<Uuid, CompletedSession>>>,
    pub(super) event_bus: Arc<EventBus>,
    pub(super) claude_client: Option<ClaudeClient>,
    pub(super) codex_client: Option<CodexClient>,
    pub(super) local_client: Option<OpenAiClient>,
    pub(super) agy_client: Option<AgyClient>,
    pub(super) harness_client: HarnessClient,
    /// Shared S3 index registration used by construction-bound native reads.
    pub(super) codegraph_handle: Option<crate::codegraph::IndexHandle>,
    pub(super) store: Arc<tokio::sync::Mutex<Store>>,
    /// Bounded, same-boot settlement for custody effect permits.  It shares
    /// this manager's Store handle so custody never creates a parallel mutex
    /// regime around SQLite.
    pub(crate) custody_settlements: crate::sandbox::custody::CustodySettlementService,
    pub(crate) custody_settlement_worker: crate::sandbox::custody::CustodySettlementWorker,
    pub(super) persistence: PersistenceHandle,
    pub(super) model_call_settlements:
        crate::model_control::call_control::ModelCallSettlementWorker,
    pub(super) project_index: Arc<RwLock<ProjectIndex>>,
    pub(super) context_rotation_enabled: bool,
    pub(super) socket_path: std::path::PathBuf,
    /// Shared BPE tokenizer for daemon-side token counting across all sessions.
    pub(super) token_counter: Arc<token_monitor::TokenCounter>,
    /// Optional memory system handle for lifecycle hooks (sync on completion/archive).
    pub(super) memory_handle: Option<crate::memory::worker::MemoryHandle>,
    /// Channel for requesting session retries from the monitor task.
    /// The daemon's main loop receives from this and calls launch_retry().
    pub(crate) retry_tx: tokio::sync::mpsc::Sender<Uuid>,
    retry_rx: Option<tokio::sync::mpsc::Receiver<Uuid>>,
    /// In-memory cache for graph workflow generation artifacts.
    graph_cache: Arc<tokio::sync::Mutex<rsi_graph::cache::GenerationCache>>,
    /// Active and recently completed workflow executions.
    workflow_executions: Arc<std::sync::Mutex<graph_executions::WorkflowExecutionRegistry>>,
    /// One live driver per durable topology execution (#634).
    pub(crate) topology_drivers: Arc<crate::topology::executor::DriverRegistry>,
    /// Optional background task queue handle for enqueuing deferred work.
    pub(crate) queue_handle: Option<crate::queue::worker::QueueHandle>,
    /// In-memory cache of parsed RSI.md configs per project.
    workflow_config_cache: Arc<RwLock<crate::project_workflow::ProjectWorkflowCache>>,
    /// Pre-canonicalized workspace root boundaries. Empty = permissive (no containment check).
    /// Populated from RSI_WORKSPACE_ROOTS at daemon startup.
    workspace_roots: Vec<std::path::PathBuf>,
    /// Tool registry for app-server sessions. Tools are advertised to the provider
    /// via `dynamicTools` in `thread/start` and dispatched in the monitor loop.
    ///
    /// Note: as of project-scoped agent memory (plan §D2), each launch in
    /// `launch.rs` rebuilds a per-session registry so `rsi_memory_search`
    /// can bind to the session's project_id. This field is kept around as
    /// the global default for paths that have no session context (currently
    /// none; allowed to be unused).
    #[allow(dead_code)]
    pub(super) tool_registry: Arc<ToolRegistry>,
    /// Live runtime config for daemon-tunable values (e.g. retry_max_backoff_ms).
    /// Shared with RpcServer; reads are lock-free via atomics.
    pub(super) runtime_config: Arc<RuntimeConfig>,
    /// Per-session sandbox allocator. Consulted by launch_session when
    /// `LaunchConfig.sandbox` is `Some` and by Phase 3 cleanup hooks.
    pub(crate) sandbox_allocator: Arc<SandboxAllocator>,
    /// Daemon-global spawn coordinator. Consulted by `monitor_session` when
    /// it sees a `<docregblock>/spawn_child …</docregblock>` directive in
    /// assistant text. Holds per-Epic token buckets across sessions.
    pub(crate) spawn_coordinator: Arc<spawn_coordinator::SpawnCoordinator>,
    /// The daemon's single AppServer control plane (C-P2-15).
    ///
    /// Exactly one plane exists per daemon, because exactly one
    /// `SessionManager` is constructed per daemon (`main.rs`). It is built
    /// here rather than passed in from `main.rs` for the same reason
    /// `spawn_coordinator`, `sandbox_allocator` and `command_registry` are:
    /// they are daemon-global control machinery whose lifetime is the
    /// manager's, and threading them through the constructor would churn ~20
    /// call sites without changing the one-per-daemon invariant that matters.
    /// Every `CodexAppServerSession` shares this instance, so the frozen
    /// quarantine limits are enforced daemon-wide and never per session.
    pub(crate) app_server_control: Arc<crate::app_server_control::AppServerControlPlane>,
    /// Receiver side of the spawn-request channel. Taken once by main.rs to
    /// spawn the handler that consumes `SpawnRequest`s and calls
    /// `launch_session`. Mirrors the `retry_rx` pattern.
    spawn_rx: Option<tokio::sync::mpsc::Receiver<spawn_coordinator::SpawnRequest>>,
    /// Receiver for durable master-successor reconciliation hints. Taken once
    /// by the daemon main loop; reservation rows remain the source of truth.
    successor_rx: Option<tokio::sync::mpsc::Receiver<spawn_coordinator::SuccessorDispatchRequest>>,
    /// Process-local continuation for bounded startup/periodic successor scans.
    /// Durable reservation rows remain authoritative; reaching the ledger end
    /// resets this cursor so later updates and skipped saturated rows are seen.
    successor_reconcile_cursor:
        tokio::sync::Mutex<Option<crate::store::successor_reservations::AgentSuccessorCursor>>,
    manager_coordinator_cursor: tokio::sync::Mutex<Option<Uuid>>,
    /// #669: wrapping keyset cursor over policy-less V1 manager seats.
    manager_seat_v1_cursor: tokio::sync::Mutex<Option<Uuid>>,
    /// Successful durable baton commits whose exact Epic projection has not
    /// yet been acknowledged in the runtime maps. This is process-local by
    /// design: a daemon restart rebuilds both maps from the durable Store,
    /// while a live daemon retries these ids through the bounded successor
    /// reconciler before revoking the predecessor's credential.
    successor_projection_pending: std::sync::Mutex<HashSet<Uuid>>,
    /// Failed relaunches of each `Uncertain` master-successor reservation in
    /// this process. With the durable age deadline it bounds recovery so one
    /// candidate can never retry forever while its Epic stays locked (#620).
    successor_uncertain_attempts: std::sync::Mutex<HashMap<Uuid, u32>>,
    /// Command frontmatter registry (RSI-010). Loaded at daemon startup
    /// from `.claude/commands/*.md`. Used by RPC launch handlers to stamp
    /// `Session.capability_class` from the leading `/<command>` in a query.
    pub(crate) command_registry: Arc<crate::command_frontmatter::CommandRegistry>,
    /// In-memory `session_token -> session_id` map for the P0 attribution
    /// gate. Minted/re-minted at every process-establishment site (fresh
    /// launch, continue, rotation handoff-writer, rotation child) via the
    /// single `remint_agent_token` helper, looked up by
    /// `handle_request_inner` to resolve `RpcRequest.session_token` into a
    /// caller session id, and revoked on supersession (a re-mint replaces the
    /// session's prior tokens; a rotated-away parent's tokens are revoked at
    /// its archival saga point). A terminal-status revocation sweep is a
    /// separate follow-up (A6 plan §9). Intentionally re-minted (not
    /// persisted) across daemon restart per the design doc's "lean re-mint"
    /// residual-unknown resolution — no migration.
    pub(crate) agent_tokens: Arc<RwLock<AgentTokenRegistry>>,
    /// One immutable identity for this daemon process. ProgramRun claims and
    /// leases persist this witness so restart/ABA recovery can fence old work.
    pub(crate) program_run_boot_id: Uuid,
    /// Restart evidence is fixed for this daemon incarnation. A health read
    /// can return it even while another task holds the Store mutex.
    pub(crate) latest_daemon_restart: Option<rsi_common::rpc::DaemonRestartRecordV1>,
    /// Monotonic per-process incarnation counter for active provider
    /// establishments. Used to ignore stale finalizers from older processes.
    pub(super) spawn_epoch: Arc<AtomicU64>,
    /// P2-04: the daemon-global registry of outstanding agent-message
    /// arbitration grants.
    ///
    /// Daemon-global rather than per-monitor because the *dispatcher* is the
    /// other reader: `plan_dispatch_tick`'s `roots_with_outstanding_grant`
    /// parameter has no correct argument unless this set is visible outside the
    /// monitor that holds the grant.
    pub(crate) agent_message_arbiter: Arc<agent_message_arbiter::AgentMessageArbiter>,
}

/// Schedule a best-effort memory sync after project metadata changes.
///
/// This is used when a session is reassigned to a different project or when
/// a project deletion clears session project assignments. The actual sync
/// logic lives in the memory worker; this helper only kicks it off.
pub(super) fn schedule_memory_sync(
    memory_handle: Option<crate::memory::worker::MemoryHandle>,
    reason: String,
) {
    if let Some(handle) = memory_handle {
        tokio::spawn(async move {
            if let Err(e) = handle.sync_now(false, &reason).await {
                tracing::warn!(
                    reason = %reason,
                    error = %e,
                    "Failed to trigger memory sync after project metadata change"
                );
            }
        });
    }
}

impl SessionManager {
    pub async fn refresh_codex_catalog_at_startup(&self) -> Result<()> {
        let client = self
            .codex_client
            .as_ref()
            .ok_or(DaemonError::CodexBinaryNotFound)?;
        client
            .refresh_catalog(
                crate::provider_capabilities::provider_capabilities(),
                CatalogRefreshReason::Startup,
            )
            .await?;
        Ok(())
    }

    pub(crate) async fn discover_codex_models(
        &self,
        reason: CatalogRefreshReason,
    ) -> Result<Vec<(String, String)>> {
        if let Some(client) = self.codex_client.as_ref() {
            return Ok(client.discover_models_with_reason(reason).await);
        }
        let client = CodexClient::new(Arc::clone(&self.runtime_config))?;
        Ok(client.discover_models_with_reason(reason).await)
    }

    /// Version-change refresh seam for launch/resume integration. A matching
    /// validated CLI version reuses the exact version+digest cache entry.
    pub async fn refresh_codex_catalog_if_version_changed(&self) -> Result<()> {
        let client = self
            .codex_client
            .as_ref()
            .ok_or(DaemonError::CodexBinaryNotFound)?;
        client
            .refresh_catalog(
                crate::provider_capabilities::provider_capabilities(),
                CatalogRefreshReason::VersionChange,
            )
            .await?;
        Ok(())
    }

    /// The manager's sole custody runtime capability. Recursive execution
    /// paths receive this opaque value; they do not reconstruct custody from
    /// session paths or open a second Store/settlement regime.
    pub(super) fn custody_execution_runtime(
        &self,
    ) -> crate::sandbox::custody::CustodyExecutionRuntime {
        crate::sandbox::custody::CustodyExecutionRuntime::new(
            Arc::clone(&self.store),
            self.custody_settlements.clone(),
            self.sandbox_allocator.base_dir().to_path_buf(),
            self.program_run_boot_id,
        )
    }

    pub fn new(
        event_bus: Arc<EventBus>,
        store: Store,
        context_rotation_enabled: bool,
        socket_path: std::path::PathBuf,
        memory_handle: Option<crate::memory::worker::MemoryHandle>,
        workspace_roots: Vec<std::path::PathBuf>,
        runtime_config: Arc<RuntimeConfig>,
        sandbox_base: std::path::PathBuf,
    ) -> Result<Self> {
        let claude_client = ClaudeClient::new(Arc::clone(&runtime_config)).ok();
        let codex_client = CodexClient::new(Arc::clone(&runtime_config)).ok();
        let local_client = OpenAiClient::new_local().ok();
        let agy_client = AgyClient::new().ok();
        let harness_client = HarnessClient::new();

        // Build project index from existing projects in the store.
        let projects = store.load_projects().unwrap_or_default();
        let latest_daemon_restart = store.latest_daemon_restart_record()?;
        let project_index = Arc::new(RwLock::new(ProjectIndex::new(projects.clone())));

        // Build workflow config cache from existing projects' FLYWHEEL.md files.
        let mut wf_cache = crate::project_workflow::ProjectWorkflowCache::new();
        for project in &projects {
            if let Some(ref path) = project.path {
                crate::project_workflow::load_project_workflow(project.id, path, &mut wf_cache);
            }
        }
        let workflow_config_cache = Arc::new(RwLock::new(wf_cache));

        let program_run_boot_id = Uuid::new_v4();
        store.set_program_run_boot_id(program_run_boot_id)?;
        // The agent-message delivery witness is seeded HERE, beside its sibling,
        // for the same reason (Issue 21 P2-05, H21-P2-R4-002). The `Store`
        // constructors also seed a value, but a constructor seed is per-`Store`
        // *instance*, and the daemon opens more than one `Store` over the same
        // database file in a single incarnation (`main.rs:153` and the memory
        // system's own at `main.rs:1221`). This call is what makes the identity
        // a property of the daemon's delivery path rather than of whichever
        // handle happened to write the row — which is the whole basis on which
        // a boot-id MISMATCH may later be trusted as durable proof that a
        // crashed attempt produced no external effect.
        store.set_delivery_boot_id(Uuid::new_v4())?;
        let store = Arc::new(tokio::sync::Mutex::new(store));
        let (custody_settlements, custody_settlement_worker) =
            crate::sandbox::custody::CustodySettlementService::new(Arc::clone(&store))?;
        let persistence = PersistenceHandle::new(Arc::clone(&store));
        let model_call_settlements =
            crate::model_control::call_control::ModelCallSettlementWorker::new(
                Arc::clone(&store),
                Arc::clone(&event_bus),
            )?;

        let (retry_tx, retry_rx) = tokio::sync::mpsc::channel(16);

        // Daemon-global spawn channel. Bounded so a runaway directive can't
        // exhaust memory; the per-Epic token bucket is the primary throttle.
        let (spawn_tx, spawn_rx) = tokio::sync::mpsc::channel(64);
        let spawn_coordinator = Arc::new(spawn_coordinator::SpawnCoordinator::new(spawn_tx));
        let (successor_tx, successor_rx) = tokio::sync::mpsc::channel(64);
        spawn_coordinator
            .install_successor_sender(successor_tx)
            .map_err(|error| DaemonError::Store(error.to_string()))?;
        let app_server_control = Arc::new(crate::app_server_control::AppServerControlPlane::new());

        // Construct sandbox allocator and ensure its base dir exists. A
        // failure here is logged but non-fatal: sandboxed launches will
        // surface the error at allocation time.
        let sandbox_allocator = Arc::new(SandboxAllocator::new(sandbox_base.clone()));
        if let Err(e) = sandbox_allocator.ensure_base() {
            tracing::warn!(
                sandbox_base = %sandbox_base.display(),
                error = %e,
                "Failed to ensure sandbox base dir at startup; sandboxed launches may fail"
            );
        }

        // Build the global tool registry. The memory_search tool is
        // intentionally registered *without* a project scope here — this
        // global registry is the fallback for paths that have no session
        // context. The per-launch flow in `launch.rs` rebuilds the registry
        // with the session's project_id so app-server provider sessions
        // (CodexAppServer) get strict project scoping for their
        // `rsi_memory_search` dispatch. See plan §D2.
        let mut registry = ToolRegistry::new();
        // Global default registry has no session context, so native rsi_control
        // tools (which require a bound caller session id) are not registered
        // here; the per-launch flow in `launch.rs` rebuilds a session-scoped
        // registry that includes them.
        register_builtin_tools(&mut registry, memory_handle.clone(), None, None, None);
        let tool_registry = Arc::new(registry);

        // Load command-frontmatter registry from `.claude/commands/*.md`
        // (RSI-010). Per-file parse errors are logged and skipped — a
        // malformed file must never block daemon startup. Zero validation on
        // empty registry; empty simply means no class resolution at launch.
        let commands_root = crate::command_frontmatter::default_commands_root();
        let command_registry = Arc::new(crate::command_frontmatter::CommandRegistry::load(
            &commands_root,
        ));
        tracing::info!(
            commands_root = %commands_root.display(),
            entry_count = command_registry.len(),
            "CommandRegistry loaded"
        );

        // Spawn the FLYWHEEL.md polling watcher.
        {
            let wf_cache = Arc::clone(&workflow_config_cache);
            let store_for_watcher = Arc::clone(&store);
            let bus_for_watcher = Arc::clone(&event_bus);
            let projects_provider: crate::project_workflow::ProjectsProvider =
                Arc::new(move || {
                    let store = Arc::clone(&store_for_watcher);
                    Box::pin(async move {
                        let store = store.lock().await;
                        store
                            .load_projects()
                            .unwrap_or_default()
                            .into_iter()
                            .filter_map(|p| p.path.map(|path| (p.id, path)))
                            .collect()
                    })
                });
            crate::project_workflow::spawn_watcher(wf_cache, projects_provider, bus_for_watcher);
        }

        Ok(Self {
            active: Arc::new(RwLock::new(HashMap::new())),
            completed: Arc::new(RwLock::new(HashMap::new())),
            event_bus,
            claude_client,
            codex_client,
            local_client,
            agy_client,
            harness_client,
            codegraph_handle: None,
            store,
            custody_settlements,
            custody_settlement_worker,
            persistence,
            model_call_settlements,
            project_index,
            context_rotation_enabled,
            socket_path,
            token_counter: Arc::new(token_monitor::TokenCounter::new()),
            memory_handle,
            retry_tx,
            retry_rx: Some(retry_rx),
            graph_cache: Arc::new(tokio::sync::Mutex::new(
                rsi_graph::cache::GenerationCache::new(),
            )),
            workflow_executions: Arc::new(std::sync::Mutex::new(
                graph_executions::WorkflowExecutionRegistry::default(),
            )),
            topology_drivers: Arc::default(),
            queue_handle: None,
            workflow_config_cache,
            workspace_roots,
            tool_registry,
            runtime_config,
            sandbox_allocator,
            spawn_coordinator,
            app_server_control,
            spawn_rx: Some(spawn_rx),
            successor_rx: Some(successor_rx),
            successor_reconcile_cursor: tokio::sync::Mutex::new(None),
            manager_coordinator_cursor: tokio::sync::Mutex::new(None),
            manager_seat_v1_cursor: tokio::sync::Mutex::new(None),
            successor_projection_pending: std::sync::Mutex::new(HashSet::new()),
            successor_uncertain_attempts: std::sync::Mutex::new(HashMap::new()),
            command_registry,
            agent_tokens: Arc::new(RwLock::new(AgentTokenRegistry::default())),
            program_run_boot_id,
            latest_daemon_restart,
            spawn_epoch: Arc::new(AtomicU64::new(1)),
            agent_message_arbiter: Arc::new(agent_message_arbiter::AgentMessageArbiter::new()),
        })
    }

    pub(super) fn next_spawn_generation(&self) -> u64 {
        Self::next_spawn_generation_from(&self.spawn_epoch)
    }

    pub(super) fn next_spawn_generation_from(spawn_epoch: &Arc<AtomicU64>) -> u64 {
        spawn_epoch.fetch_add(1, Ordering::Relaxed)
    }

    /// Accessor for the command-frontmatter registry (RSI-010). Exposed so
    /// the RPC layer can resolve a `/<command>` query into a declared
    /// `CapabilityClass` before building `LaunchConfig`.
    pub fn command_registry(&self) -> Arc<crate::command_frontmatter::CommandRegistry> {
        Arc::clone(&self.command_registry)
    }

    /// Access the sandbox allocator (Phase 3 cleanup hooks, restore-time
    /// orphan sweep).
    pub fn sandbox_allocator(&self) -> &SandboxAllocator {
        &self.sandbox_allocator
    }

    /// Access the backing store (for direct queries like model segments).
    pub fn store(&self) -> &Arc<tokio::sync::Mutex<Store>> {
        &self.store
    }

    /// Install the live Codegraph handle before this manager is published.
    pub fn set_codegraph_handle(&mut self, handle: crate::codegraph::IndexHandle) {
        self.harness_client.set_codegraph_handle(handle.clone());
        self.codegraph_handle = Some(handle);
    }

    /// Run one bounded, awaited Closure terminal-output recovery pass.
    pub async fn reconcile_closure_outputs_after_restart(
        &self,
    ) -> Result<rsi_common::closure_kernel::ClosureOutputRecoveryResultV1> {
        crate::closure_kernel::ingress::reconcile_after_restart(
            Arc::clone(&self.store),
            rsi_common::closure_kernel::ClosureOutputRecoveryBudgetV1::default(),
            None,
            None,
        )
        .await
    }

    /// Retry bounded Closure sweeps every five seconds. The cursor is retained
    /// only for an unfinished high-water sweep; a completed sweep starts over
    /// so newly terminal or newly rotated sources are observed.
    pub fn run_closure_output_reconciliation_loop(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut cursor = None;
            let mut high_water = None;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(
                    rsi_common::closure_kernel::CLOSURE_OUTPUT_RETRY_DELAY_SECONDS_V1,
                ))
                .await;
                match crate::closure_kernel::ingress::reconcile_after_restart(
                    Arc::clone(&self.store),
                    rsi_common::closure_kernel::ClosureOutputRecoveryBudgetV1::default(),
                    cursor.clone(),
                    high_water.clone(),
                )
                .await
                {
                    Ok(report) => {
                        cursor = report.next_cursor;
                        high_water = report.high_water;
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "Closure output reconciliation tick deferred");
                    }
                }
            }
        });
    }

    /// P2-06c: run ONE bounded agent-message reconciliation pass.
    ///
    /// This is the daemon's only entry into crash recovery and expiry for
    /// agent mail, and it deliberately **cannot fail**: it returns a report,
    /// not a `Result`, so its caller — a task spawned at daemon start — has no
    /// `?` to write and a bad row can never become a boot failure. Every
    /// per-row and per-page error is counted into the report and logged.
    ///
    /// The store guard is taken here and released when this returns. Nothing
    /// suspends while it is held: the pass is a plain synchronous `fn`, which is
    /// what keeps a provider effect structurally unreachable from inside a
    /// SQLite transaction. This body is deliberately four lines, and
    /// `the_store_guard_is_never_held_across_a_suspension_point` asserts it
    /// keeps exactly one suspension point — the lock acquisition. Do not add a
    /// second one; the guard is process-wide.
    ///
    /// A panic is contained here rather than in the loop below, because the pass
    /// is synchronous and so drops straight into `catch_unwind`. Containing it
    /// inside the guard scope also releases the process-wide guard by an ordinary
    /// drop instead of during an unwind. Without this the detached loop would die
    /// silently on the first panic and reconciliation would never run again for
    /// the process lifetime — see
    /// [`reconcile_agent_messages_pass_catching_panics`].
    ///
    /// [`reconcile_agent_messages_pass_catching_panics`]:
    ///     agent_message_reconciler::reconcile_agent_messages_pass_catching_panics
    pub(crate) async fn reconcile_agent_messages_once(
        &self,
    ) -> agent_message_reconciler::AgentMessageReconciliationReport {
        let store = self.store.lock().await;
        let live_boot_id = store.delivery_boot_id();
        agent_message_reconciler::reconcile_agent_messages_pass_catching_panics(
            &store,
            live_boot_id,
            agent_message_reconciler::ReconciliationPassBudget::default(),
        )
    }

    /// P2-06c: the detached periodic reconciliation loop, one tick at a time.
    ///
    /// Split out of `main.rs` so the loop body — including the decision that a
    /// failing pass is logged and retried rather than propagated — lives in the
    /// crate that `cargo test -p rsid --lib` builds.
    ///
    /// The first tick fires immediately: crash recovery runs at start, not one
    /// interval later.
    pub async fn run_agent_message_reconciliation_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(
            AGENT_MESSAGE_RECONCILE_INTERVAL_SECS,
        ));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let report = self.reconcile_agent_messages_once().await;
            if !report.did_work() {
                continue;
            }
            tracing::info!(
                target: "agent_coordination",
                requeued = report.requeued,
                stranded_uncertain = report.stranded_uncertain,
                expired = report.expired,
                errors = report.errors,
                fully_reconciled = report.fully_reconciled(),
                stopped_by_time_budget = report.stopped_by_time_budget,
                stopped_by_page_budget = report.stopped_by_page_budget,
                "agent-message reconciliation pass completed"
            );
        }
    }

    /// Pre-canonicalized workspace root boundaries. Empty slice means no containment check.
    pub fn workspace_roots(&self) -> &[std::path::PathBuf] {
        &self.workspace_roots
    }

    /// Access the in-memory graph generation cache.
    pub fn graph_cache(&self) -> &Arc<tokio::sync::Mutex<rsi_graph::cache::GenerationCache>> {
        &self.graph_cache
    }

    /// Access workflow execution tracking state.
    pub(crate) fn workflow_executions(
        &self,
    ) -> &Arc<std::sync::Mutex<graph_executions::WorkflowExecutionRegistry>> {
        &self.workflow_executions
    }

    /// Access the event bus (for push notification subscriptions).
    pub fn event_bus(&self) -> &Arc<EventBus> {
        &self.event_bus
    }

    pub(crate) fn guard_admitted_model_call(
        &self,
        permit: crate::model_control::AdmissionPermit,
        route: crate::model_control::registry::RuntimeExecutionRoute,
    ) -> Result<crate::model_control::call_control::AdmittedModelCall> {
        crate::model_control::call_control::AdmittedModelCall::real(
            permit,
            route,
            self.model_call_settlements.handle()?,
        )
    }

    #[cfg(test)]
    pub(crate) async fn drain_model_call_settlements(&self) -> Result<()> {
        self.model_call_settlements.drain().await
    }

    /// Take the retry receiver (called once by the daemon's main loop).
    pub fn take_retry_rx(&mut self) -> Option<tokio::sync::mpsc::Receiver<Uuid>> {
        self.retry_rx.take()
    }

    /// Take the spawn-request receiver. Called once by the daemon's main loop
    /// to spawn the handler that consumes `SpawnRequest`s emitted by the
    /// `SpawnCoordinator` when an Epic-lead session emits a `/spawn_child`
    /// directive.
    pub fn take_spawn_rx(
        &mut self,
    ) -> Option<tokio::sync::mpsc::Receiver<spawn_coordinator::SpawnRequest>> {
        self.spawn_rx.take()
    }

    pub fn take_successor_rx(
        &mut self,
    ) -> Option<tokio::sync::mpsc::Receiver<spawn_coordinator::SuccessorDispatchRequest>> {
        self.successor_rx.take()
    }

    /// Clone the daemon-global spawn coordinator. Used by tests and any
    /// future call site that needs to dispatch directives directly.
    pub fn spawn_coordinator(&self) -> Arc<spawn_coordinator::SpawnCoordinator> {
        Arc::clone(&self.spawn_coordinator)
    }

    /// Read-only access to active sessions for the stall detector.
    pub fn active(&self) -> Arc<RwLock<HashMap<Uuid, TrackedSession>>> {
        Arc::clone(&self.active)
    }

    /// Clone the retry sender for use by background tasks (stall detector, reconciliation).
    pub fn retry_sender(&self) -> tokio::sync::mpsc::Sender<Uuid> {
        self.retry_tx.clone()
    }

    /// Access the background queue handle, if the queue is enabled.
    pub fn queue_handle(&self) -> Option<&crate::queue::worker::QueueHandle> {
        self.queue_handle.as_ref()
    }

    /// Set the background queue handle (called once by daemon startup after spawning the worker).
    pub fn set_queue_handle(&mut self, handle: crate::queue::worker::QueueHandle) {
        self.queue_handle = Some(handle);
    }

    /// Access the per-project FLYWHEEL.md config cache.
    pub fn workflow_config_cache(
        &self,
    ) -> &Arc<RwLock<crate::project_workflow::ProjectWorkflowCache>> {
        &self.workflow_config_cache
    }

    /// Resolve a `RpcRequest.session_token` to the session id it was minted
    /// for. Returns `None` for an unknown/stale token (e.g. daemon restarted
    /// since the token was minted — agents re-read env on their next call).
    pub async fn resolve_agent_token(&self, token: &str) -> Option<Uuid> {
        self.agent_tokens.read().await.get(token).copied()
    }

    /// Bind `ProgramRun` controller authority from the live incarnation, D03
    /// semantic grant, and A6 token registries in the global lock order.
    pub(crate) async fn bind_program_run_controller_authority(
        &self,
        session_id: Uuid,
    ) -> std::result::Result<
        crate::program_run_control::BoundProgramRunControllerAuthority,
        crate::program_run_control::ProgramRunControlError,
    > {
        let active = self.active.read().await;
        if !active.contains_key(&session_id) {
            return Err(crate::program_run_control::ProgramRunControlError::Forbidden);
        }
        let store = self.store.lock().await;
        let (grant, grant_incarnation) = store
            .controller_grant_witness_v1(session_id)
            .ok_or(crate::program_run_control::ProgramRunControlError::Forbidden)?;
        let tokens = self.agent_tokens.read().await;
        let a6_token = tokens
            .token_for_session(session_id)
            .map(str::to_owned)
            .ok_or(crate::program_run_control::ProgramRunControlError::Forbidden)?;
        let handle = crate::program_run_control::ProgramRunControlHandle::new_with_live_witnesses(
            Arc::clone(&self.store),
            Arc::new(crate::program_run_control::SystemProgramRunClock),
            Arc::clone(&self.active),
            Arc::clone(&self.agent_tokens),
            self.program_run_boot_id,
        );
        let bound = handle.bind_controller(&grant, grant_incarnation, a6_token);
        drop(tokens);
        drop(store);
        drop(active);
        bound
    }

    pub(crate) async fn is_program_run_controller_live(&self, session_id: Uuid) -> bool {
        self.bind_program_run_controller_authority(session_id)
            .await
            .is_ok()
    }

    /// Bind dispatcher authority without accepting controller identity from a
    /// wire request. The caller supplies only durable action facts reloaded by
    /// the daemon.
    pub(crate) async fn bind_program_run_scheduler_authority(
        &self,
        session_id: Uuid,
        boot_id: Uuid,
    ) -> std::result::Result<
        crate::program_run_control::BoundProgramRunSchedulerAuthority,
        crate::program_run_control::ProgramRunControlError,
    > {
        if boot_id != self.program_run_boot_id {
            return Err(crate::program_run_control::ProgramRunControlError::Forbidden);
        }
        let active = self.active.read().await;
        if !active.contains_key(&session_id) {
            return Err(crate::program_run_control::ProgramRunControlError::Forbidden);
        }
        let store = self.store.lock().await;
        let (grant, grant_incarnation) = store
            .controller_grant_witness_v1(session_id)
            .ok_or(crate::program_run_control::ProgramRunControlError::Forbidden)?;
        let tokens = self.agent_tokens.read().await;
        let a6_token = tokens
            .token_for_session(session_id)
            .map(str::to_owned)
            .ok_or(crate::program_run_control::ProgramRunControlError::Forbidden)?;
        let handle = crate::program_run_control::ProgramRunControlHandle::new_with_live_witnesses(
            Arc::clone(&self.store),
            Arc::new(crate::program_run_control::SystemProgramRunClock),
            Arc::clone(&self.active),
            Arc::clone(&self.agent_tokens),
            self.program_run_boot_id,
        );
        let bound = handle.bind_scheduler(&grant, boot_id, grant_incarnation, a6_token);
        drop(tokens);
        drop(store);
        drop(active);
        bound
    }

    pub(crate) async fn program_run_dispatch_visits(
        &self,
        boot_id: Uuid,
        kinds: &[rsi_common::program_runs::ProgramRunActionKindV1],
    ) -> std::result::Result<
        crate::store::program_runs::ProgramRunDispatchBatchV1,
        crate::program_run_control::ProgramRunControlError,
    > {
        if boot_id != self.program_run_boot_id {
            return Err(crate::program_run_control::ProgramRunControlError::Forbidden);
        }
        self.store
            .lock()
            .await
            .select_program_run_dispatch_visits_v1(
                boot_id,
                kinds,
                chrono::Utc::now(),
                crate::store::program_runs::PROGRAM_RUN_DISPATCH_VISIT_LIMIT,
            )
            .map_err(Into::into)
    }

    pub(crate) async fn advance_program_run_dispatch_cursor(
        &self,
        batch: &crate::store::program_runs::ProgramRunDispatchBatchV1,
        visits: &[crate::store::program_runs::ProgramRunDispatchVisitV1],
    ) -> std::result::Result<bool, crate::program_run_control::ProgramRunControlError> {
        self.store
            .lock()
            .await
            .advance_program_run_dispatch_cursor_v1(batch, visits, chrono::Utc::now())
            .map_err(Into::into)
    }

    pub(crate) fn program_run_control_handle(
        &self,
    ) -> crate::program_run_control::ProgramRunControlHandle {
        crate::program_run_control::ProgramRunControlHandle::new_with_live_witnesses(
            Arc::clone(&self.store),
            Arc::new(crate::program_run_control::SystemProgramRunClock),
            Arc::clone(&self.active),
            Arc::clone(&self.agent_tokens),
            self.program_run_boot_id,
        )
    }

    /// Start the bounded wake dispatcher with a fresh daemon-boot identity.
    ///
    /// # Errors
    ///
    /// Returns a wire-safe message when dispatcher construction is unavailable.
    pub fn start_program_run_dispatcher(
        self: &Arc<Self>,
    ) -> std::result::Result<
        (
            tokio_util::sync::CancellationToken,
            tokio::task::JoinHandle<()>,
        ),
        String,
    > {
        let cancellation = tokio_util::sync::CancellationToken::new();
        let publisher = Arc::new(crate::program_run_dispatch::ScheduledJobWakeAdapter::new(
            Arc::clone(&self.store),
        ));
        let dispatcher = crate::program_run_dispatch::ProgramRunDispatcher::new(
            Arc::clone(self),
            publisher,
            self.program_run_boot_id,
        )
        .map_err(|error| error.to_string())?;
        let handle = crate::program_run_dispatch::spawn_program_run_dispatcher(
            dispatcher,
            cancellation.clone(),
        );
        Ok((cancellation, handle))
    }

    /// Start the bounded AppServer control worker (C-P2-15).
    ///
    /// Startup keyset reconciliation runs FIRST and must succeed: it registers
    /// every durable live AppServer attempt before writer admission is enabled,
    /// so a restart can neither reset the global attempt cap nor admit a
    /// duplicate turn. If the reconciling read fails, the worker is not started
    /// and writer admission stays disabled — failing closed.
    ///
    /// # Errors
    ///
    /// Returns a wire-safe message when startup reconciliation cannot read the
    /// durable attempt keyset.
    pub async fn start_app_server_seal_worker(
        self: &Arc<Self>,
    ) -> std::result::Result<
        (
            tokio_util::sync::CancellationToken,
            tokio::task::JoinHandle<()>,
        ),
        String,
    > {
        let worker = crate::app_server_seal_worker::AppServerSealWorker::new(
            Arc::clone(&self.app_server_control),
            Arc::clone(&self.store),
        );
        let outcome = worker
            .reconcile_startup()
            .await
            .map_err(|error| error.to_string())?;
        tracing::info!(
            registered = outcome.registered,
            sealed_for_lost_evidence = outcome.sealed_for_lost_evidence.len(),
            refused_registry_full = outcome.refused_registry_full.len(),
            "AppServer control plane startup reconciliation complete"
        );
        let cancellation = tokio_util::sync::CancellationToken::new();
        let handle = crate::app_server_seal_worker::spawn_app_server_seal_worker(
            worker,
            cancellation.clone(),
        );
        Ok((cancellation, handle))
    }

    /// Register a token -> session_id binding directly. Production minting
    /// flows exclusively through `remint_agent_token` (A6); this remains for
    /// test seeding of known token values only, hence `#[cfg(test)]`.
    #[cfg(test)]
    pub(crate) async fn register_agent_token(&self, token: String, session_id: Uuid) {
        self.agent_tokens.write().await.insert(token, session_id);
    }

    /// Drop every token binding for a session (rotation supersession,
    /// terminal cleanup). Best-effort: an unknown session is a no-op.
    pub(crate) async fn revoke_agent_token_for_session(&self, session_id: Uuid) {
        revoke_agent_tokens_for_session(&self.agent_tokens, session_id).await;
    }

    /// Thin `&self` convenience over [`remint_agent_token`] for call sites
    /// that hold the manager (fresh launch, continue).
    pub(crate) async fn remint_session_token(&self, session_id: Uuid) -> String {
        remint_agent_token(&self.agent_tokens, session_id).await
    }

    /// Reconstruct a same-ID semantic controller grant only after the installed
    /// provider is alive, the Session row is durably active for that provider,
    /// and the exact freshly reminted A6 binding is current. Lock order is
    /// Active -> Store -> A6 read.
    pub(super) async fn reconstruct_live_same_id_controller_grant(
        active: &RwLock<HashMap<Uuid, TrackedSession>>,
        store: &tokio::sync::Mutex<Store>,
        agent_tokens: &RwLock<AgentTokenRegistry>,
        session_id: Uuid,
        project_id: Option<Uuid>,
        established_provider: rsi_common::types::SessionProvider,
        prospective_token: &str,
    ) -> SameIdControllerGrantOutcome {
        let Some(project_id) = project_id else {
            store.lock().await.remove_controller_grant_v1(session_id);
            return SameIdControllerGrantOutcome::NotAssigned;
        };

        let mut active_guard = active.write().await;
        let provider_live = active_guard.get_mut(&session_id).is_some_and(|tracked| {
            !tracked.interrupt_requested
                && tracked.session.provider == established_provider
                && tracked.process.as_mut().is_some_and(|process| {
                    provider_spawn::installed_provider_confirmation(established_provider, process)
                        .is_some()
                })
        });
        if !provider_live {
            drop(active_guard);
            store.lock().await.remove_controller_grant_v1(session_id);
            return SameIdControllerGrantOutcome::EstablishmentInvalid;
        }

        let store = store.lock().await;
        let grant =
            store.reestablish_idea_controller_v1(session_id, project_id, established_provider);
        let a6 = agent_tokens.read().await;
        if a6.get(prospective_token).copied() != Some(session_id) {
            store.remove_controller_grant_v1(session_id);
            return SameIdControllerGrantOutcome::EstablishmentInvalid;
        }
        let outcome = match grant {
            Ok(Some(grant)) => {
                store.install_controller_grant_v1(grant);
                SameIdControllerGrantOutcome::Installed
            }
            Ok(None) => {
                store.remove_controller_grant_v1(session_id);
                SameIdControllerGrantOutcome::NotAssigned
            }
            Err(_) => {
                store.remove_controller_grant_v1(session_id);
                SameIdControllerGrantOutcome::EstablishmentInvalid
            }
        };
        drop(a6);
        drop(store);
        drop(active_guard);
        outcome
    }

    /// Commit a new-ID controller assignment only while the exact installed
    /// provider incarnation remains live and cannot be interrupted.
    ///
    /// Holding the Active write guard across Store -> A6 -> assignment makes
    /// `interrupt_requested` the cancellation witness shared with the real
    /// InterruptSession/AgentHalt path. An interrupt that acquires Active first
    /// invalidates the captured confirmation; an assignment that acquires it
    /// first commits before the interrupt can report success. No caller may
    /// retire the former controller until this method returns success.
    pub(super) async fn assign_live_controller_candidate(
        active: &RwLock<HashMap<Uuid, TrackedSession>>,
        transfer: &crate::idea_control::IdeaControllerTransferHandle,
        reservation: &rsi_common::types::IdeaControllerReservationV1,
        confirmation: &rsi_common::types::IdeaControllerLaunchConfirmationV1,
        prospective_token: &str,
        agent_tokens: &RwLock<AgentTokenRegistry>,
    ) -> std::result::Result<
        rsi_common::types::IdeaControllerControlResultV1,
        ControllerCandidateCommitError,
    > {
        let mut active_guard = active.write().await;
        let Some(tracked) = active_guard.get_mut(&reservation.candidate_session_id) else {
            return Err(ControllerCandidateCommitError::Confirmation(
                crate::idea_control::IdeaControlError::LaunchNotConfirmed,
            ));
        };
        if tracked.interrupt_requested {
            return Err(ControllerCandidateCommitError::Cancelled);
        }
        let installed_confirmation = tracked.process.as_mut().is_some_and(|process| {
            provider_spawn::matches_live_controller_confirmation(
                confirmation.provider,
                confirmation.confirmation_kind,
                process,
            )
        });
        if tracked.session.id != reservation.candidate_session_id
            || tracked.session.project_id != Some(confirmation.project_id)
            || tracked.session.provider != confirmation.provider
            || !installed_confirmation
        {
            return Err(ControllerCandidateCommitError::Confirmation(
                crate::idea_control::IdeaControlError::LaunchNotConfirmed,
            ));
        }

        #[cfg(test)]
        launch::pause_controller_candidate_test(
            reservation.candidate_session_id,
            launch::ControllerCandidateTestPhase::AssignmentWitnessHeld,
        )
        .await;

        let result = transfer
            .assign_confirmed_guarded(reservation, confirmation, prospective_token, agent_tokens)
            .await
            .map_err(ControllerCandidateCommitError::Confirmation);
        drop(active_guard);
        result
    }
}

/// Mint a per-session authority token (P0 attribution gate). Not a
/// cryptographic boundary against a malicious local process (the threat
/// model in the design doc explicitly disclaims that) — it is a correctness
/// guard against a *confused* agent calling raw daemon verbs. Composed from
/// two v4 UUIDs (256 bits from the OS CSPRNG via `getrandom`) to keep
/// guessability negligible without adding a new dependency.
fn generate_session_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// Revoke-then-mint a session's authority token (A6 re-mint semantics,
/// plan §4.1): atomically drop every token previously registered for
/// `session_id` and register a fresh one, under a single write guard so no
/// reader can observe two live tokens for the same session. This is the
/// ONLY production mint path — fresh launches (`launch_session`) and every
/// non-fresh establishment site (continue G1, rotation handoff-writer G2,
/// rotation child G3) route through it, so a new spawn path cannot "forget"
/// the token by copying a config block. For a fresh session id the retain is
/// a no-op and this degrades to a pure register.
///
/// Returns the token for `LaunchConfig.rsi_session_token` stamping. The
/// token VALUE must never be logged or formatted into diagnostics — log
/// presence booleans and session ids only.
pub(super) async fn remint_agent_token(
    agent_tokens: &RwLock<AgentTokenRegistry>,
    session_id: Uuid,
) -> String {
    let token = generate_session_token();
    let mut map = agent_tokens.write().await;
    map.revoke_session(session_id);
    map.insert(token.clone(), session_id);
    token
}

/// Drop every token registered for `session_id` without minting a
/// replacement. Used where a session is superseded WITHOUT being respawned
/// under the same id — the rotation-archival saga point (A6 D2): the parent
/// is archived only after its child is confirmed launched, and its authority
/// dies with it. The failure/rollback path must NOT call this (a rolled-back
/// parent stays restorable, token intact). Best-effort: unknown session is a
/// no-op.
pub(super) async fn revoke_agent_tokens_for_session(
    agent_tokens: &RwLock<AgentTokenRegistry>,
    session_id: Uuid,
) {
    agent_tokens.write().await.revoke_session(session_id);
}

// ---------------------------------------------------------------------------
// A8 terminal watch — daemon-owned watcher bridge (plan §3.4).
// ---------------------------------------------------------------------------

/// Depth cap for the rotation-lineage chase (`continued_from` successors).
/// Rotation chains are short in practice; the cap only bounds pathological
/// or cyclic data.
pub(crate) const WATCH_LINEAGE_DEPTH_CAP: usize = 8;

/// A8 fire decision for one watched session, derived from its PERSISTED row
/// (never the bus event or the active-map snapshot — the §3.6 ordering
/// guard).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchDecision {
    /// Notify-worthy. `retry_eligible` is `Some` only for `Failed` (D4
    /// annotation); `question_pending` is true only for `WaitingApproval`
    /// (D5).
    Fire {
        retry_eligible: Option<bool>,
        question_pending: bool,
    },
    /// Not notify-worthy yet: the recurring watch row stays armed.
    NotReady,
    /// Watched row missing (deleted from store/purged): disable + surface.
    Abandon,
}

/// Pure predicate (decision-table tested, T-4/T-5):
/// - `Completed` / `Interrupted` / `Archived` → fire.
/// - `Deleted` → fire (a soft-deleted child is permanently non-progressing;
///   suppressing would re-create the sleeps-forever trap the slice fixes).
/// - `Failed` → D4: retry-eligible AND a LIVE in-memory retry timer →
///   `NotReady` (the resurrection flips status non-terminal and the
///   recurring watch keeps waiting); otherwise fire, annotated with
///   eligibility so a post-restart master can decide (no timer survives a
///   daemon restart).
/// - `WaitingApproval` → D5: fire (the master IS the answerer).
/// - `Starting` / `Running` → `NotReady`. Row missing → `Abandon`.
pub(crate) const fn watch_fire_decision(
    status: Option<rsi_common::types::SessionStatus>,
    retry_eligible: bool,
    live_retry_timer: bool,
) -> WatchDecision {
    use rsi_common::types::SessionStatus as S;
    match status {
        None => WatchDecision::Abandon,
        Some(S::Completed | S::Interrupted | S::Archived | S::Deleted) => WatchDecision::Fire {
            retry_eligible: None,
            question_pending: false,
        },
        Some(S::Failed) => {
            if retry_eligible && live_retry_timer {
                WatchDecision::NotReady
            } else {
                WatchDecision::Fire {
                    retry_eligible: Some(retry_eligible),
                    question_pending: false,
                }
            }
        }
        Some(S::WaitingApproval) => WatchDecision::Fire {
            retry_eligible: None,
            question_pending: true,
        },
        // `SessionStatus` is #[non_exhaustive]; treat unknown future states
        // like Starting/Running — stay armed rather than spuriously wake.
        Some(S::Starting | S::Running | _) => WatchDecision::NotReady,
    }
}

/// How long an unconfirmed terminal-watch delivery keeps being retried before
/// it is dropped loudly (issue #12).
///
/// Measured from the watched child's terminal row, not from our own last
/// attempt, so retries cannot slide the deadline forward indefinitely. Paired
/// with [`WATCH_REDELIVERY_BACKOFF`] this bounds the number of provider
/// invocations a single unconsumable notification can bill.
pub(crate) const WATCH_DELIVERY_GIVE_UP_AFTER: chrono::Duration = chrono::Duration::minutes(20);

/// Delay before re-attempting a delivery that produced no provider output.
///
/// Only consumed when the master is idle AND the previous attempt produced
/// nothing — a healthy in-flight turn is held off by `resume_scheduled`'s
/// busy check instead, which maps to `NotReady`.
pub(crate) const WATCH_REDELIVERY_BACKOFF: chrono::Duration = chrono::Duration::minutes(2);

/// Everything about an `OnTerminal` fire EXCEPT the delivery itself. Split
/// from the IO shell so tests can pin predicate/lineage/coalescing/message
/// behavior without spawning provider processes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WatchFirePlan {
    NotReady,
    Abandon(String),
    /// The give-up window elapsed with the delivery never consumed
    /// (issue #12 bound, typed for issue #648).
    AbandonUnconsumed(crate::issue_tracker::poller::UnconsumedDelivery),
    /// A previous delivery to the tip is now proven consumed: the tip produced
    /// provider output after the attempt. Only here is the watch row retired.
    Confirmed,
    Deliver {
        /// Rotation-lineage tip of the wake target.
        tip: Uuid,
        /// Composed `[rsid-watch]` message (one line per coalesced child).
        /// Never contains tokens or other transport credentials.
        message: String,
        /// The fired job plus every fire-ready sibling satisfied by this
        /// single delivery.
        job_ids: Vec<Uuid>,
        /// Version of each selected row before the owner continuation starts.
        observed_job_versions: Vec<(Uuid, chrono::DateTime<chrono::Utc>)>,
    },
}

/// One `[rsid-watch]` message line for a fire-ready watched session.
fn compose_watch_line(
    session: &rsi_common::types::Session,
    retry_eligible: Option<bool>,
    question_pending: bool,
    arm_message: &str,
) -> String {
    let id_str = session.id.to_string();
    let uuid8 = &id_str[..8];
    let title = session.title.as_deref().unwrap_or("");
    let mut annotations: Vec<String> = Vec::new();
    if let Some(eligible) = retry_eligible {
        annotations.push(format!("retry-eligible: {eligible}"));
    }
    if question_pending {
        annotations.push("question-pending: true".to_string());
    }
    let ann = if annotations.is_empty() {
        String::new()
    } else {
        format!(" ({})", annotations.join(", "))
    };
    let mut line = format!(
        "[rsid-watch] {:?} {} \"{}\" → {:?}{}",
        session.session_kind, uuid8, title, session.status, ann
    );
    let note = arm_message.trim();
    if !note.is_empty() {
        line.push_str(" — note: ");
        line.push_str(note);
    }
    line
}

impl SessionManager {
    /// Evaluate the watch predicate for one watched session id against its
    /// persisted row + the in-memory retry-timer marker (D4: eligibility is
    /// DB-visible, pendency is memory-only via `retry_cancel`).
    async fn watch_decision_for(
        &self,
        watched: Uuid,
    ) -> Result<(WatchDecision, Option<rsi_common::types::Session>)> {
        let row = {
            let store = self.store.lock().await;
            store.get_session(watched)?
        };
        let retry_eligible = row.as_ref().is_some_and(|s| {
            let max = s.max_retries.unwrap_or(0);
            max > 0 && s.retry_attempt.unwrap_or(0) < max
        });
        let live_retry_timer = self
            .completed
            .read()
            .await
            .get(&watched)
            .is_some_and(|cs| cs.retry_cancel.is_some() || cs.retry_fired_at.is_some());
        let decision = watch_fire_decision(
            row.as_ref().map(|s| s.status),
            retry_eligible,
            live_retry_timer,
        );
        Ok((decision, row))
    }

    /// Resolve a wake target to its PUBLISHED rotation-lineage tip (RPC-1
    /// C2, depth capped). A reserved, launched-but-unpublished or refused
    /// successor is never the tip. `None` = the origin row no longer exists.
    /// This is the K2 continuation-fence chase; fenced dispatch re-checks it
    /// under the tip's spawn guard.
    pub(crate) async fn resolve_wake_tip(&self, origin: Uuid) -> Result<Option<Uuid>> {
        self.store.lock().await.published_lineage_tip(origin)
    }

    /// Recipient-idle admission for a durable manager notice (issue #627).
    ///
    /// Returns the undelivered subject count when the route SOURCE is not yet
    /// notify-worthy but the route RECIPIENT (already the rotation-lineage tip
    /// resolved by `harness_manager_watch_route`) is idle and this transport
    /// owes it a first wake. Idle means: the row exists, its status is
    /// `Completed` or `Interrupted`, it is absent from the active map, and it
    /// has neither a queued provider turn nor a pending question. `Failed`,
    /// `Archived`, `Deleted`, `WaitingApproval`, `Starting` and `Running` recipients are
    /// never woken by this rule. Delivery itself still passes the stricter
    /// manager-notice resume gate. Kept as one predicate so a later lead
    /// continuation fence can wrap it without touching the planner.
    async fn manager_notice_recipient_idle_wake(
        &self,
        job_id: Uuid,
        recipient: Uuid,
    ) -> Result<Option<i64>> {
        use rsi_common::types::SessionStatus as S;
        if self.active.read().await.contains_key(&recipient) {
            return Ok(None);
        }
        let store = self.store.lock().await;
        let idle = store.get_session(recipient)?.is_some_and(|session| {
            matches!(session.status, S::Completed | S::Interrupted)
                && session.pending_question.is_none()
                && session.queued_turn_count.unwrap_or(0) == 0
        });
        if !idle {
            return Ok(None);
        }
        store.manager_notice_first_wake_pending(job_id)
    }

    /// Plan an `OnTerminal` fire: predicate on the primary watched session,
    /// lineage-chase the wake target, coalesce fire-ready sibling watches
    /// sharing the same tip, compose the delivery message (plan §3.4 1-3).
    async fn plan_terminal_watch_fire(
        &self,
        job: &rsi_common::types::ScheduledJob,
    ) -> Result<WatchFirePlan> {
        use rsi_common::types::WakeMode;

        let WakeMode::OnTerminal(watched) = job.wake_mode else {
            return Err(crate::error::DaemonError::InvalidParam(format!(
                "fire_terminal_watch called on non-watch job {}",
                job.id
            )));
        };
        let Some(master) = job.wake_session_id else {
            return Ok(WatchFirePlan::Abandon(format!(
                "watch job {} has no wake target",
                job.id
            )));
        };

        let (manager_notice, manager_route) = {
            let store = self.store.lock().await;
            let managed = store.is_harness_manager_watch(job.id)?;
            (
                managed,
                if managed {
                    store.harness_manager_watch_route(job.id)?
                } else {
                    None
                },
            )
        };
        if manager_notice && manager_route.is_none() {
            return Ok(WatchFirePlan::Abandon(
                "manager_notice_scope_revoked".into(),
            ));
        }
        let watched = manager_route.map_or(watched, |(source, _)| source);

        // Durable manager notices have their own acknowledgement boundary.
        // A delivered continuation stays armed but quiet until an authorized
        // AgentManagerInbox transaction settles it. Arbitrary provider output
        // is not proof that the exact action/session state was retrieved.
        let durable_manager_notice = if manager_notice {
            match self
                .store
                .lock()
                .await
                .manager_watch_delivery_state(job.id)?
            {
                // A pre-V117 manager watch has no durable subject. Preserve
                // its legacy provider-output confirmation path so upgrade
                // cannot silently retire genuine pending mail.
                None => false,
                Some(false) => return Ok(WatchFirePlan::NotReady),
                Some(true) => true,
            }
        } else {
            false
        };

        // 1. Predicate on the watched lineage tip for ordinary watches (F3
        //    INV-4); manager-routed watches keep the exact watched session.
        //    A durable manager notice whose source is still busy is admitted
        //    when its recipient is idle and owes a first wake (issue #627).
        let subject = if manager_route.is_some() {
            Some(watched)
        } else {
            self.resolve_wake_tip(watched).await?
        };
        let Some(subject) = subject else {
            return Ok(WatchFirePlan::Abandon(format!(
                "watched session {watched} no longer exists"
            )));
        };
        let (decision, row) = self.watch_decision_for(subject).await?;
        let mut recipient_idle_wake = false;
        let (retry_eligible, question_pending) = match decision {
            WatchDecision::NotReady => {
                let Some((_, recipient)) = manager_route.filter(|_| durable_manager_notice) else {
                    return Ok(WatchFirePlan::NotReady);
                };
                if self
                    .manager_notice_recipient_idle_wake(job.id, recipient)
                    .await?
                    .is_none()
                {
                    return Ok(WatchFirePlan::NotReady);
                }
                recipient_idle_wake = true;
                (None, false)
            }
            WatchDecision::Abandon => {
                return Ok(WatchFirePlan::Abandon(format!(
                    "watched session {watched} no longer exists"
                )));
            }
            WatchDecision::Fire {
                retry_eligible,
                question_pending,
            } => (retry_eligible, question_pending),
        };
        let Some(primary_row) = row else {
            // Fire implies a present row by construction.
            return Ok(WatchFirePlan::Abandon(format!(
                "watched session {watched} no longer exists"
            )));
        };

        // 2. Lineage chase: deliver to the live tip, never a superseded row.
        let resolved_tip = if let Some((_, target)) = manager_route {
            Some(target)
        } else {
            self.resolve_wake_tip(master).await?
        };
        let Some(tip) = resolved_tip else {
            return Ok(WatchFirePlan::Abandon(format!(
                "wake target {master} no longer exists"
            )));
        };

        // 2a. Delivery confirmation (issue #12).
        //
        // `last_fired_at` records only that a resume was SPAWNED for this job,
        // which is not delivery: `continue_session` returns as soon as the
        // provider process exists and detaches its monitor, so a turn that dies
        // before emitting anything still looked like a successful delivery and
        // retired the watch. That is the reliable outcome when a fresh
        // `--resume` process inherits an orphaned background shell task from the
        // previous turn, and it permanently destroyed the child-completion
        // notification.
        //
        // So treat a fired-but-unconfirmed row as still owing a delivery, and
        // retire it only on positive evidence that the tip actually ran: a
        // provider-authored event dated after the attempt. `System` rows and the
        // injected prompt itself are excluded by `last_provider_output_at`, so a
        // no-output turn cannot confirm itself.
        //
        // A healthy in-flight turn does not re-deliver: `resume_scheduled`
        // refuses a busy master, which maps to `NotReady` and stays armed.
        if !durable_manager_notice && let Some(last_fired) = job.last_fired_at {
            let produced = {
                let store = self.store.lock().await;
                store.last_provider_output_at(tip)?
            };
            if produced.is_some_and(|at| at > last_fired) {
                return Ok(WatchFirePlan::Confirmed);
            }

            // Bound the re-delivery. Each attempt costs a real provider
            // invocation, so a notification that can never be consumed must fail
            // loudly rather than re-bill forever. The watched child's terminal
            // row is the anchor: it is durable, already loaded, and does not
            // slide forward with our own retries the way `last_fired_at` does.
            let unconsumed_for = chrono::Utc::now().signed_duration_since(primary_row.updated_at);
            if unconsumed_for > WATCH_DELIVERY_GIVE_UP_AFTER {
                // Typed (issue #648): the scheduler settles this exactly like
                // `Abandon`, then records the tip's health fact and, for a
                // managed Epic lead, a manager notice.
                return Ok(WatchFirePlan::AbandonUnconsumed(
                    crate::issue_tracker::poller::UnconsumedDelivery {
                        tip,
                        watched,
                        minutes: unconsumed_for.num_minutes(),
                    },
                ));
            }
        }

        // 3. Coalesce: every enabled sibling watch resolving to the same tip
        //    whose own predicate fires joins this single delivery.
        // Manager transport text lists exact subject identities but not their
        // payloads. Retrieval through the bound tool rechecks scope at the
        // content delivery point.
        let mut lines = if durable_manager_notice {
            vec![job.message.clone()]
        } else {
            let mut line =
                compose_watch_line(&primary_row, retry_eligible, question_pending, &job.message);
            if watched != subject {
                line.push_str(" — lineage: ");
                line.push_str(&watched.to_string());
                line.push('→');
                line.push_str(&subject.to_string());
                if let Some(path) = primary_row.handoff_filepath.as_deref() {
                    line.push_str("; handoff: ");
                    line.push_str(path);
                }
            }
            vec![line]
        };
        let mut job_ids = vec![job.id];
        let mut observed_job_versions = vec![(job.id, job.updated_at)];

        let all_jobs = {
            let store = self.store.lock().await;
            store.list_enabled_terminal_watches()?
        };
        for sibling in &all_jobs {
            if sibling.id == job.id || !sibling.enabled {
                continue;
            }
            let WakeMode::OnTerminal(sib_watched) = sibling.wake_mode else {
                continue;
            };
            let Some(sib_master) = sibling.wake_session_id else {
                continue;
            };
            let (sibling_managed, sibling_route) = {
                let store = self.store.lock().await;
                let managed = store.is_harness_manager_watch(sibling.id)?;
                (
                    managed,
                    if managed {
                        store.harness_manager_watch_route(sibling.id)?
                    } else {
                        None
                    },
                )
            };
            // An ordinary child watch retains its existing delivery policy;
            // it cannot join a manager bundle and bypass the stricter guard.
            if sibling_managed != manager_notice || (sibling_managed && sibling_route.is_none()) {
                continue;
            }
            let sibling_durable_manager_notice = if sibling_managed {
                match self
                    .store
                    .lock()
                    .await
                    .manager_watch_delivery_state(sibling.id)?
                {
                    None => false,
                    Some(false) => continue,
                    Some(true) => true,
                }
            } else {
                false
            };
            if sibling_managed && sibling_durable_manager_notice != durable_manager_notice {
                continue;
            }
            let sib_watched = sibling_route.map_or(sib_watched, |(source, _)| source);
            let resolved_sibling = if let Some((_, target)) = sibling_route {
                Some(target)
            } else {
                self.resolve_wake_tip(sib_master).await?
            };
            let Some(sib_tip) = resolved_sibling else {
                continue;
            };
            if sib_tip != tip {
                continue;
            }
            if !sibling_durable_manager_notice && let Some(delivered_at) = sibling.last_fired_at {
                let produced = self.store.lock().await.last_provider_output_at(sib_tip)?;
                if produced.is_some_and(|at| at > delivered_at) {
                    // This sibling already reached its owner. Its own tick
                    // will retire it; another child's delivery must not
                    // include the consumed notification again.
                    continue;
                }
            }
            let sib_subject = if sibling_managed {
                Some(sib_watched)
            } else {
                self.resolve_wake_tip(sib_watched).await?
            };
            let Some(sib_subject) = sib_subject else {
                continue;
            };
            let (mut sib_decision, sib_row) = self.watch_decision_for(sib_subject).await?;
            if sib_decision == WatchDecision::NotReady
                && sibling_durable_manager_notice
                && self
                    .manager_notice_recipient_idle_wake(sibling.id, sib_tip)
                    .await?
                    .is_some()
            {
                // The shared recipient is idle: this sibling's pending
                // subjects ride the same single wake.
                recipient_idle_wake = true;
                sib_decision = WatchDecision::Fire {
                    retry_eligible: None,
                    question_pending: false,
                };
            }
            if let WatchDecision::Fire {
                retry_eligible: sib_retry,
                question_pending: sib_question,
            } = sib_decision
                && let Some(sib_row) = sib_row
            {
                if durable_manager_notice {
                    lines.push(sibling.message.clone());
                } else {
                    let mut line =
                        compose_watch_line(&sib_row, sib_retry, sib_question, &sibling.message);
                    if sib_watched != sib_subject {
                        line.push_str(" — lineage: ");
                        line.push_str(&sib_watched.to_string());
                        line.push('→');
                        line.push_str(&sib_subject.to_string());
                        if let Some(path) = sib_row.handoff_filepath.as_deref() {
                            line.push_str("; handoff: ");
                            line.push_str(path);
                        }
                    }
                    lines.push(line);
                }
                job_ids.push(sibling.id);
                observed_job_versions.push((sibling.id, sibling.updated_at));
            }
        }

        if recipient_idle_wake {
            let mut pending = 0_i64;
            {
                let store = self.store.lock().await;
                for id in &job_ids {
                    pending += store.manager_notice_undelivered_count(*id)?;
                }
            }
            lines.insert(
                0,
                format!("{pending} durable manager notices pending; read AgentManagerInbox"),
            );
        }

        Ok(WatchFirePlan::Deliver {
            tip,
            message: lines.join("\n"),
            job_ids,
            observed_job_versions,
        })
    }

    /// IO shell for one `OnTerminal` fire attempt: plan, then deliver via the
    /// existing `resume_scheduled` (busy-check + continue, post-A6 token
    /// re-mint included). Delivery rejection (busy master,
    /// transient continue failure) maps to `NotReady` — the recurring row
    /// retries next tick (D3 requeue-until-idle).
    async fn fire_terminal_watch(
        &self,
        job: &rsi_common::types::ScheduledJob,
    ) -> Result<crate::issue_tracker::poller::WatchFireOutcome> {
        use crate::issue_tracker::poller::WatchFireOutcome;

        match self.plan_terminal_watch_fire(job).await? {
            WatchFirePlan::NotReady => Ok(WatchFireOutcome::NotReady),
            WatchFirePlan::Abandon(reason) => Ok(WatchFireOutcome::Abandon { reason }),
            WatchFirePlan::AbandonUnconsumed(delivery) => {
                // #530: a tip behind a retryable custody gate is deferred
                // exactly like a refused delivery, never given up on. Only a
                // tip that is deliverable (or terminally refused, the #648
                // incident) can be retired by the unconsumed-delivery bound.
                if self
                    .store
                    .lock()
                    .await
                    .retryable_custody_gate(delivery.tip)?
                {
                    tracing::debug!(
                        job_id = %job.id,
                        tip = %delivery.tip,
                        "unconsumed-delivery give-up deferred behind a retryable custody gate"
                    );
                    return Ok(WatchFireOutcome::CustodyUnavailable);
                }
                Ok(WatchFireOutcome::AbandonUnconsumed(delivery))
            }
            WatchFirePlan::Confirmed => Ok(WatchFireOutcome::Confirmed),
            WatchFirePlan::Deliver {
                tip,
                message,
                job_ids,
                observed_job_versions,
            } => {
                // Envelope at delivery, not in the plan: plan-level tests pin
                // the raw composed `[rsid-watch]` lines; the wire/store form
                // is daemon-attributed so the master model does not read the
                // notification as the human user speaking.
                let delivery = rsi_common::daemon_message::wrap("terminal-watch", &message);
                let managed = self.store.lock().await.is_harness_manager_watch(job.id)?;
                let resumed = if managed {
                    self.resume_manager_notice(tip, delivery, job_ids.clone())
                        .await
                } else {
                    self.resume_scheduled_for(tip, delivery, Some(job_ids.clone()))
                        .await
                };
                match resumed {
                    Ok(session_id) => {
                        if !managed {
                            let store = self.store.lock().await;
                            for id in &job_ids {
                                store.clear_continuation_retry(*id)?;
                            }
                        }
                        Ok(WatchFireOutcome::Delivered {
                            session_id,
                            delivered_job_ids: job_ids,
                            observed_job_versions,
                        })
                    }
                    // K2: an ordinary child watch records each retryable
                    // refusal (manager-notice watches keep today's
                    // re-evaluation without a backoff record) and is
                    // abandoned, typed, once the bound is spent.
                    Err(e)
                        if !managed
                            && crate::store::manager_actions::fence::continuation_fence_retryable(
                                &e,
                            ) =>
                    {
                        let code = crate::store::manager_actions::fence::continuation_fence_code(&e)
                            .unwrap_or("continuation_fence")
                            .to_string();
                        let recorded = self.store.lock().await.record_continuation_retry(
                            job.id,
                            &code,
                            Some(tip),
                            chrono::Utc::now(),
                            false,
                        )?;
                        match recorded {
                            crate::store::scheduled_jobs::ContinuationRetryOutcome::Backoff {
                                ..
                            } => Ok(WatchFireOutcome::NotReady),
                            crate::store::scheduled_jobs::ContinuationRetryOutcome::Exhausted(
                                retry,
                            ) => {
                                let reason = format!(
                                    "{}: last {} at tip {tip} after {} attempts",
                                    crate::store::manager_actions::fence::CONTINUATION_RETRY_EXHAUSTED,
                                    retry.last_code,
                                    retry.attempts,
                                );
                                self.event_bus.publish(crate::bus::DaemonEvent::SystemMessage {
                                    level: "error".into(),
                                    message: format!("terminal watch {}: {reason}", job.id),
                                });
                                // The Abandon retirement clears the retry
                                // state in its own terminal write.
                                Ok(WatchFireOutcome::Abandon { reason })
                            }
                        }
                    }
                    Err(e) => {
                        if crate::error::is_retryable_custody_wake_error(&e) {
                            return Ok(WatchFireOutcome::CustodyUnavailable);
                        }
                        tracing::debug!(
                            job_id = %job.id,
                            error = %e,
                            "terminal watch delivery deferred (master busy or continue failed); staying armed"
                        );
                        Ok(WatchFireOutcome::NotReady)
                    }
                }
            }
        }
    }
}

impl SessionManager {
    /// Shared scheduled delivery. With `job_ids`, the exact job rows are
    /// revalidated under the target spawn guard before any provider effect.
    async fn resume_scheduled_for(
        &self,
        target: Uuid,
        query: String,
        job_ids: Option<Vec<Uuid>>,
    ) -> Result<Uuid> {
        // A8 §9 Q1 (Jake: INCLUDE): chase rotation lineage so a wake armed
        // against a since-rotated session resumes the live tip, never a
        // superseded row (the duplicate-master incident class). K2: the chase
        // is the published tip (RPC-1 C2) captured with the lead generation;
        // the tip's guard re-checks both, publication and busy, so the
        // busy pre-check moved under the guard.
        let fence = self
            .capture_continuation_fence(
                target,
                crate::store::manager_actions::fence::ContinuationAuthorityV1::Automated,
            )
            .await?;
        let target = fence.tip;

        // Scheduled resume/watch delivery wakes an existing session; it must
        // not reclassify that session as a scheduled-job launch in the TUI's
        // Jobs zone.
        match job_ids {
            Some(job_ids) => {
                self.continue_scheduled_wake(target, query, job_ids, fence)
                    .await?;
            }
            None => self.continue_fenced(target, query, fence).await?,
        }

        Ok(target)
    }
}

/// SessionManager implements the SessionLauncher trait for issue tracker integration.
/// This is a thin wrapper that delegates to the existing launch_session method.
#[async_trait::async_trait]
impl crate::issue_tracker::poller::SessionLauncher for SessionManager {
    async fn launch(&self, config: crate::claude::LaunchConfig) -> Result<Uuid> {
        self.launch_session(config).await
    }

    async fn launch_scheduled_fresh(
        &self,
        config: crate::claude::LaunchConfig,
        initial_rotation_disabled: bool,
    ) -> Result<Uuid> {
        SessionManager::launch_scheduled_fresh(self, config, initial_rotation_disabled).await
    }

    async fn resume_scheduled(&self, target: Uuid, query: String) -> Result<Uuid> {
        self.resume_scheduled_for(target, query, None).await
    }

    async fn resume_scheduled_job(
        &self,
        target: Uuid,
        query: String,
        job_ids: Vec<Uuid>,
    ) -> Result<Uuid> {
        self.resume_scheduled_for(target, query, Some(job_ids))
            .await
    }

    async fn resume_capacity_scheduled(
        &self,
        target: Uuid,
        query: String,
        wake_job_id: Uuid,
        due_slot: chrono::DateTime<chrono::Utc>,
    ) -> Result<Uuid> {
        // K2: fenced like every automated continuation; a refusal leaves the
        // capacity due slot retained (the scheduler settles only on success).
        let fence = self
            .capture_continuation_fence(
                target,
                crate::store::manager_actions::fence::ContinuationAuthorityV1::Automated,
            )
            .await?;
        let target = fence.tip;
        self.continue_capacity_scheduled(target, query, wake_job_id, due_slot, fence)
            .await
    }

    /// A8: terminal-watch fire path (plan §3.4). All policy lives in
    /// `fire_terminal_watch`; the scheduler only maps the returned outcome
    /// onto job rows.
    async fn fire_watch(
        &self,
        job: &rsi_common::types::ScheduledJob,
    ) -> Result<crate::issue_tracker::poller::WatchFireOutcome> {
        self.fire_terminal_watch(job).await
    }

    /// Issue #648: keep a daemon-authored event after the cached transcript.
    async fn transcript_sequence_floor(&self, session: Uuid) -> i32 {
        self.completed
            .read()
            .await
            .get(&session)
            .and_then(|cached| cached.events.last())
            .map_or(0, |event| event.sequence + 1)
    }

    /// Issue #648: mirror the committed health fact into the cached
    /// transcript so a later continuation numbers after it.
    async fn delivery_abandoned_recorded(&self, event: &rsi_common::types::ConversationEvent) {
        if let Some(cached) = self.completed.write().await.get_mut(&event.session_id) {
            cached.events.push(event.clone());
        }
    }
}
