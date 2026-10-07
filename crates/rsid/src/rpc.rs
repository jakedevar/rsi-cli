use crate::bus::DaemonEvent;
use crate::claude::LaunchConfig;
use crate::config::RuntimeConfig;
use crate::error::{DaemonError, Result, agent_read_scope_denied, issue_workspace_error};
use crate::idea_control::IdeaControlError;
use crate::program_run_control::{BoundProgramRunOperatorAuthority, ProgramRunControlError};
use crate::provider_capabilities::CatalogRefreshReason;
use crate::recursive_dag::live::{
    RecursiveDagLiveApprovalPolicy, RecursiveDagLiveBudgetPlaceholders,
    RecursiveDagLiveOutputCommitResult, RecursiveDagLiveOutputNotReadyReason,
    RecursiveDagLiveSandboxPolicy, RecursiveDagLiveSchedulerRunRequest,
    RecursiveDagLiveSessionManagerBinding, RecursiveDagLiveToolPolicy,
};
use crate::session::SessionManager;
use crate::store::agent_coordination::AgentReadClass;
use chrono::{DateTime, Utc};
use rsi_common::agent_control_schema::{
    AgentCancelWakeParams, AgentControlVerbV1, AgentGetStatusParams, AgentHaltParams,
    AgentListWakesParams, AgentScheduleWakeParams, LIST_WAKES_DEFAULT_LIMIT,
};
use rsi_common::agent_coordination::{
    AgentArchiveChildRequestV1, AgentContinueChildRequestV1, AgentGetProgressParamsV1,
    AgentReserveSuccessorRequestV1, AgentSendMessageRequestV1, AgentSpawnChildRequestV1,
};
use rsi_common::archive_cleanup::{ArchiveSessionParamsV1, GetArchiveCleanupStatusParamsV1};
use rsi_common::cohort_settlement::{
    ApplySourceWorktreeCohortParams, AuditSourceWorktreeCohortParams,
    GetSourceWorktreeSettlementRunParams, ListSourceWorktreeCohortsParams,
};
use rsi_common::harness_manager::{
    AgentManagerInboxRequestV1, AgentManagerNotifyRequestV1, AgentManagerProgressRequestV1,
    AgentManagerReplyRequestV1, AgentManagerSendRequestV1, ConfigureHarnessManagerRequestV1,
    GetHarnessManagerRequestV1,
};
use rsi_common::issue_workspace::{
    ArchiveIssueRequestV1, CreateIssueV2RequestV1, GetIssueInProjectRequestV1,
    IssueDependencyMutationRequestV1, IssueWorkspaceErrorCodeV1, ListIssueDependenciesRequestV1,
    ListIssueEventsV2RequestV1, ListIssuesPageRequestV1, RestoreIssueRequestV1,
    UpdateIssueRequestV1, UpdateIssueStatusV2RequestV1,
};
use rsi_common::model_control::ModelControlMode;
use rsi_common::recursive_dag::{
    EditRecursiveNodeInstructionsParams, EditRecursiveNodeSettingsParams,
    GetRecursiveGraphAsWorkflowParams, GetRecursiveGraphAsWorkflowResponse,
    RECURSIVE_TOPOLOGY_EXECUTION_OWNER_FAKE, RecursiveAttemptId, RecursiveAttemptStatus,
    RecursiveCancellationRequestId, RecursiveCancellationRequestSource,
    RecursiveExecutionArtifactKind, RecursiveExecutionArtifactSummary,
    RecursiveLiveAttemptHeartbeatState, RecursiveLiveAttemptStatus, RecursiveReadPage,
    RecursiveReadbackWarning, RecursiveRecoveryBudget, RecursiveRecoverySource,
    RecursiveSchedulerRunId, RecursiveSchedulerRunSource, RecursiveSchedulerRunStatus,
    RecursiveTaskGraphId, RecursiveTaskId, RunRecursiveLiveSchedulerResponse,
    RunRecursiveTopologyNodeFakeSchedulerResponse,
};
use rsi_common::rpc::{
    AddSessionTagParams, AgentCreateIssueParams, BusEvent, CancelModelInvocationParams,
    CancelProgramRunParams, CommitRecursiveLiveAttemptOutputParams,
    CommitRecursiveLiveAttemptOutputResponse, ContinueRecursiveRecoveryParams,
    ContinueSessionParams, ContinueTopologyRecursiveRecoveryParams, ConversationBatchEntry,
    ConversationBatchResponse, CreateContainerParams, CreateIssueParams, CreateProgramRunParams,
    CreateProgramRunResult, CreateTopologyParams, DeleteTopologyParams, ExecuteTopologyParams,
    GetConversationsSinceParams, GetIdeaParams, GetIndexStatusParams, GetModelControlStatusParams,
    GetProgramRunOperationalStatusParams, GetProgramRunOperationalStatusResult,
    GetProgramRunParams, GetRecursiveExecutionArtifactParams,
    GetRecursiveLiveAttemptArtifactsParams, GetRecursiveLiveAttemptHeartbeatStatusParams,
    GetRecursiveLiveAttemptParams, GetRecursiveLiveInterruptStatusParams,
    GetRecursiveLiveOutputValidationResultParams, GetRecursiveLiveRecoveryStatusParams,
    GetRecursiveRecoveryStatusParams, GetTopologyParams, GetTopologyRecursiveStatusParams,
    INTERNAL_ERROR, INVALID_PARAMS, IssueDepParams, IssueIdParams, LaunchSessionParams,
    LinkIssueToIdeaParams, ListIssueEventsParams, ListModelInvocationsParams,
    ListProgramRunTransitionsParams, ListProgramRunTransitionsResult, ListProgramRunsParams,
    ListProgramRunsResult, ListReadyIssuesParams, ListRecursiveCancellationRequestsParams,
    ListRecursiveExecutionArtifactSummariesParams, ListRecursiveExecutionArtifactsParams,
    ListRecursiveGraphsForTopologyParams, ListRecursiveLifecycleEventsParams,
    ListRecursiveLiveAttemptsParams, ListRecursiveLiveInterruptsParams,
    ListRecursiveLiveOutputValidationResultsParams, ListRecursiveLiveValidationIssuesParams,
    ListRecursiveSchedulerRunEventsParams, ListRecursiveSchedulerRunsParams,
    ListRecursiveTaskAttemptsParams, ListRecursiveTaskGraphsParams, ListRecursiveTasksParams,
    ListSessionChildrenParams, ListStaleRecursiveLiveAttemptHeartbeatsParams, ListTagsParams,
    ListTopologiesParams, METHOD_NOT_FOUND, PreviewRecursiveExecutionArtifactParams,
    QueueSessionModelUpdateParams, ReconcileProgramRunsParams, ReconcileProgramRunsResult,
    RecursiveCancellationRequestIdParams, RecursiveSchedulerRunIdParams,
    RecursiveTaskGraphIdParams, RecursiveTaskIdParams, RemoveSessionTagParams,
    RequestRecursiveGraphCancellationParams, RequestRecursiveSchedulerRunCancellationParams,
    RequestTopologyRecursiveCancellationParams, ResumeBlockedProgramRunParams, RotateSessionParams,
    RpcError, RpcErrorData, RpcRequest, RpcResponse, RunRecursiveFakeSchedulerParams,
    RunRecursiveLiveSchedulerParams, RunRecursiveTopologyNodeFakeSchedulerParams,
    SetEpicLeadParams, SetSessionParentParams, StartChainedWorkflowParams,
    StartChainedWorkflowResponse, SubscribeParams, UpdateIndexStatusParams,
    UpdateIssueStatusParams, UpdateModelControlPolicyParams, UpdateSessionRatingParams,
    UpdateSessionTagsParams, UpdateTopologyParams,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use uuid::Uuid;

mod agent_issues;
mod agent_verbs;
mod codegraph;
mod common;
mod fleet;
mod issues;
mod manager;
mod memory;
mod models;
mod program_runs;
mod projects;
mod recursive_control;
mod recursive_read;
mod remote;
mod satellites;
mod scheduled_jobs;
mod sessions;
mod settings;
mod storage;
mod topology;
use self::agent_issues::*;
pub use self::common::*;
#[cfg(test)]
pub(crate) use self::issues::link_issue_rpc_error;
pub use self::memory::*;
pub use self::models::*;
pub use self::projects::*;
#[cfg(test)]
use self::recursive_control::MAX_RECURSIVE_FAKE_SCHEDULER_RPC_STEPS;
use self::recursive_read::*;
pub use self::sessions::*;
pub use self::settings::*;
pub use self::storage::*;
pub use self::topology::*;

// ─── P0 session-attribution gate ────────────────────────────────────────
//
// Threat model (design doc `2026-06-30-agent-harness-control-via-rpc-cli.md`,
// "Threat model"): the socket is trust-by-ownership, so method-name
// allowlisting is not a security boundary against a malicious local
// process — any process running as the user can already open the socket
// unattributed. This gate is a correctness guard against a *confused*
// agent that follows the documented `rsi-rpc` + `$RSI_SESSION_TOKEN`
// contract: a call that carries a session token is classified as an
// *agent* call and is default-denied except for a closed, explicitly
// enumerated allowlist. A call with no token (TUI/operator/scripts) is
// completely unaffected — full surface, unchanged, exactly as today.
//
// Closed allowlist, NOT prefix matching (`Get*`/`List*` would silently
// admit a future `Get`-named mutation). Every entry here was reviewed and
// confirmed side-effect-free from the calling session's perspective, or is
// an `Agent*` verb that internally enforces its own self/lead scoping.
mod failure_signatures; // #1016: AgentQueryFailureSignatures handler
mod friction; // #1333: ListFrictionRollup and the agent-refusal andon point
mod global_manager; // #872 Slice B: global manager v0
mod manager_issue_worker; // #1100: AgentManagerLaunchIssueWorker handler
mod operator_restart; // #1122: operator quiet-point restart
mod portable; // #1406: operator-only portable install bundle
mod scratch_adopt; // #1147: operator adoption of legacy scratch
mod agent_gate {
    use std::collections::HashSet;
    use std::sync::LazyLock;

    // #1011: the verb lists are derived from the single per-verb declaration
    // in `rsi_common::rpc_verb_registry` (the agent-control descriptor table
    // plus `NON_AGENT_ATTRIBUTED_VERBS`); nothing here spells a method name.
    //
    // `AGENT_VERBS`: `Agent*` verbs — tokened wrappers that internally enforce
    // self/lead scoping via `SessionManager`/`SpawnCoordinator`, or (for
    // `AgentScheduleWake`) bind their WAKE target exclusively to the resolved
    // caller session, never to an agent-supplied id. The optional
    // `watch_session_id` (A8 `mode:"on_terminal"`) names a watched SUBJECT,
    // scoped exactly like `AgentGetStatus` targets (self / direct child /
    // child-of-led-Epic, self-watch rejected) — it cannot steer the wake.
    pub(super) static AGENT_VERBS: LazyLock<&'static [&'static str]> =
        LazyLock::new(rsi_common::rpc_verb_registry::agent_verb_methods);

    // `HOOK_VERBS` (#1049): verbs only the session's own tool-boundary hook
    // calls (`rsi-rpc boundary-mail-hook`). Deliberately NOT in `AGENT_VERBS`:
    // they are not part of the model-facing catalog, native tools or the
    // `rsi-rpc agent` listing. The verb binds strictly to the token-resolved
    // caller.
    pub(super) static HOOK_VERBS: LazyLock<&'static [&'static str]> =
        LazyLock::new(rsi_common::rpc_verb_registry::hook_verb_methods);

    // `READ_VERBS`: enumerated read-only verbs safe for an attributed (agent)
    // caller. Deliberately small — extend by declaring the verb in the
    // registry after review, never by widening to a prefix match.
    pub(super) static READ_VERBS: LazyLock<&'static [&'static str]> =
        LazyLock::new(rsi_common::rpc_verb_registry::read_verb_methods);

    // `UNSCOPED_READ_VERBS`: the only `READ_VERBS` that name no target
    // session. Every other read verb resolves the token caller and admits a
    // target only inside its read scope (#241;
    // `Store::agent_read_scope_admits`), refusing with
    // `agent_read_scope_denied`. Pinned by the #241 coverage test so a new
    // read verb cannot land without either scoping or an explicit entry.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) static UNSCOPED_READ_VERBS: LazyLock<&'static [&'static str]> =
        LazyLock::new(rsi_common::rpc_verb_registry::unscoped_read_verb_methods);

    static ALLOWED: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
        AGENT_VERBS
            .iter()
            .chain(READ_VERBS.iter())
            .chain(HOOK_VERBS.iter())
            .copied()
            .collect()
    });

    /// True if `method` may be called by a session-attributed (tokened)
    /// caller. Unattributed callers bypass this check entirely.
    pub(super) fn is_allowed_for_attributed_caller(method: &str) -> bool {
        ALLOWED.contains(method)
    }
}

/// Result of handling an RPC request. Normal returns a response to send.
/// Subscribe signals the connection should transition to push streaming mode.
enum HandleResult {
    Response(RpcResponse),
    Subscribe {
        response: RpcResponse,
        params: SubscribeParams,
    },
}

// A8.1 F-2: the per-master watch cap moved to `session::agent_verbs` next to
// the single-critical-section arm service that enforces it; re-exported here
// for existing referents.
pub use crate::session::agent_verbs::MAX_TERMINAL_WATCHES_PER_MASTER;

pub struct RpcServer {
    session_manager: Arc<SessionManager>,
    remote_epoch: Uuid,
    remote_signer: Arc<crate::remote_read::RemoteCursorSigner>,
    satellite_incarnation_id: Uuid,
    satellite_snapshots: crate::satellite::SatelliteSessionSnapshots,
    remote: Arc<crate::remote_control::RemoteController>,
    codegraph_handle: Option<crate::codegraph::IndexHandle>,
    memory_manager: Option<Arc<crate::memory::manager::MemoryManager>>,
    dreamer_handle: Option<crate::dreamer::scheduler::DreamerHandle>,
    scheduler_handle: Option<crate::scheduler::SchedulerHandle>,
    issue_tracker_manager: Option<Arc<crate::issue_tracker::manager::IssueTrackerManager>>,
    dialectic_engine: Option<Arc<crate::dialectic::DialecticEngine>>,
    runtime_config: Arc<RuntimeConfig>,
    rsid_scope_settings_path: std::path::PathBuf,
    /// #1036: directory holding the remote-gate spend ledger and the caps
    /// file the daemon mirrors for `scripts/cloud-spend.py`.
    cloud_dir: std::path::PathBuf,
    model_control_runtime: crate::model_control::ModelControlRuntime,
    compile_engine: Arc<crate::prompt_compile::CompileEngine>,
    ollama_http: reqwest::Client,
}

impl RpcServer {
    pub fn new(
        session_manager: Arc<SessionManager>,
        memory_manager: Option<Arc<crate::memory::manager::MemoryManager>>,
        issue_tracker_manager: Option<Arc<crate::issue_tracker::manager::IssueTrackerManager>>,
        // A8: the scheduler handle is a constructor arg (not a post-hoc
        // setter) so the arm-time `trigger_now` plumbing provably exists
        // before the RPC surface is reachable. `None` = scheduler disabled;
        // watch arms still insert, with a warning (plan §5 failure modes).
        scheduler_handle: Option<crate::scheduler::SchedulerHandle>,
        runtime_config: Arc<RuntimeConfig>,
        compile_engine: Arc<crate::prompt_compile::CompileEngine>,
        ollama_http: reqwest::Client,
        model_control_runtime: crate::model_control::ModelControlRuntime,
    ) -> Self {
        let remote_epoch = Uuid::new_v4();
        Self {
            session_manager,
            remote_epoch,
            remote_signer: Arc::new(crate::remote_read::RemoteCursorSigner::new(remote_epoch)),
            satellite_incarnation_id: {
                crate::satellite::note_process_start();
                Uuid::new_v4()
            },
            satellite_snapshots: crate::satellite::SatelliteSessionSnapshots::default(),
            remote: Arc::new(crate::remote_control::RemoteController::new(Arc::new(
                crate::remote_control::HostSystem,
            ))),
            codegraph_handle: None,
            memory_manager,
            dreamer_handle: None,
            scheduler_handle,
            issue_tracker_manager,
            dialectic_engine: None,
            runtime_config,
            rsid_scope_settings_path: rsi_common::identity::data_path(
                "rsid-scope.env",
                "rsid-scope",
            ),
            cloud_dir: crate::cloud_spend::default_cloud_dir(),
            model_control_runtime,
            compile_engine,
            ollama_http,
        }
    }

    /// Replace the host effects behind the operator RSI Remote controls.
    #[cfg(test)]
    pub(crate) fn set_remote_system(
        &mut self,
        system: Arc<dyn crate::remote_control::RemoteSystem>,
    ) {
        self.remote = Arc::new(crate::remote_control::RemoteController::new(system));
    }

    /// Set the dreamer handle for manual dream trigger via RPC.
    pub fn set_dreamer_handle(&mut self, handle: crate::dreamer::scheduler::DreamerHandle) {
        self.dreamer_handle = Some(handle);
    }

    /// Install the live registration/status handle before the RPC server is
    /// published. Operator reads use the same manager snapshot as the indexer.
    pub fn set_codegraph_handle(&mut self, handle: crate::codegraph::IndexHandle) {
        self.codegraph_handle = Some(handle);
    }

    /// Initialize the dialectic engine from daemon config.
    pub fn init_dialectic(&mut self, config: &crate::config::Config) {
        if !config.dialectic_enabled {
            return;
        }
        let memory = self.memory_manager.as_ref().map(|m| m.as_ref().clone());
        let engine = crate::dialectic::DialecticEngine::new(
            memory,
            self.session_manager.clone(),
            config.dialectic_api_url.clone(),
            config.dialectic_api_key.clone(),
            config.dialectic_model.clone(),
            config.dialectic_max_iterations,
        );
        self.dialectic_engine = Some(Arc::new(engine));
    }

    /// Handle a single client connection.
    pub async fn handle_connection(&self, stream: UnixStream) -> Result<()> {
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }

            let mut boundary_operator_ids: Vec<Uuid> = Vec::new();
            let mut boundary_agent_ids: Vec<Uuid> = Vec::new();
            let handle_result = match serde_json::from_str::<RpcRequest>(&line) {
                Ok(request) => {
                    // The six operator-local Remote reads retain their permit
                    // through JSON-RPC serialization and socket settlement.
                    // Tokened calls continue through the agent default-deny gate.
                    if request.session_token.is_none()
                        && crate::remote_read::is_operator_read_method(&request.method)
                    {
                        crate::remote_read::send_operator_read(
                            &self.session_manager,
                            &self.remote_signer,
                            self.remote_epoch,
                            &request,
                            &mut writer,
                        )
                        .await?;
                        continue;
                    }
                    let result = self.handle_request_inner(&request).await;
                    if request.method == rsi_common::boundary_mail_hook::CLAIM_METHOD
                        && let HandleResult::Response(response) = &result
                        && response.error.is_none()
                        && let Some(reply) = response.result.as_ref()
                    {
                        // Operator messages stay open (`effect_possible`) until
                        // the reply carrying them has been written (#1062).
                        boundary_operator_ids =
                            crate::session::boundary_mail::operator_message_ids(reply);
                        // Agent mail waits for the hook's own confirmation
                        // (#1183); a reply that cannot be written settles it
                        // `uncertain` at once.
                        boundary_agent_ids =
                            crate::session::boundary_mail::agent_message_ids(reply);
                    }
                    result
                }
                Err(e) => HandleResult::Response(RpcResponse::error(
                    None,
                    RpcError {
                        code: INVALID_PARAMS,
                        message: format!("Failed to parse request: {}", e),
                        data: None,
                    },
                )),
            };

            match handle_result {
                HandleResult::Response(response) => {
                    let json = serde_json::to_string(&response)?;
                    let written = async {
                        writer.write_all(format!("{}\n", json).as_bytes()).await?;
                        writer.flush().await
                    }
                    .await;
                    if !boundary_operator_ids.is_empty() {
                        if written.is_ok() {
                            self.session_manager
                                .finalize_operator_boundary_delivery(&boundary_operator_ids)
                                .await;
                        } else {
                            self.session_manager
                                .abandon_operator_boundary_delivery(&boundary_operator_ids)
                                .await;
                        }
                    }
                    if !boundary_agent_ids.is_empty() && written.is_err() {
                        self.session_manager
                            .abandon_agent_boundary_delivery(&boundary_agent_ids)
                            .await;
                    }
                    written?;
                }
                HandleResult::Subscribe { response, params } => {
                    // Send the ack response, then transition to streaming mode
                    let json = serde_json::to_string(&response)?;
                    writer.write_all(format!("{}\n", json).as_bytes()).await?;
                    writer.flush().await?;
                    self.run_subscribe_stream(&mut writer, params).await;
                    return Ok(());
                }
            }
        }

        Ok(())
    }

    /// Run the push notification stream after a Subscribe handshake.
    async fn run_subscribe_stream(
        &self,
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        params: SubscribeParams,
    ) {
        let event_bus = self.session_manager.event_bus();
        let mut rx = event_bus.subscribe();

        loop {
            match rx.recv().await {
                Ok(daemon_event) => {
                    // Apply filters from SubscribeParams
                    if !params.event_types.is_empty() {
                        let event_type = event_type_str(&daemon_event);
                        if !params.event_types.iter().any(|t| t == event_type) {
                            continue;
                        }
                    }
                    if let Some(filter_sid) = params.session_id {
                        if !event_matches_session(&daemon_event, filter_sid) {
                            continue;
                        }
                    }

                    let bus_event: BusEvent = (*daemon_event).clone().into();
                    let Ok(json) = serde_json::to_string(&bus_event) else {
                        continue;
                    };
                    if writer
                        .write_all(format!("{}\n", json).as_bytes())
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "Push subscriber lagged, sending reset");
                    let reset = BusEvent {
                        event_type: "subscription_reset".to_string(),
                        timestamp: chrono::Utc::now(),
                        data: serde_json::json!({ "missed": n }),
                    };
                    let Ok(json) = serde_json::to_string(&reset) else {
                        break;
                    };
                    if writer
                        .write_all(format!("{}\n", json).as_bytes())
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }

        event_bus.unsubscribe();
    }

    async fn handle_request_inner(&self, request: &RpcRequest) -> HandleResult {
        // This dispatcher already spans the daemon's complete RPC catalog.
        // Heap-box its generated future so additive operator-only methods do
        // not increase every caller's stack frame (including RPC unit tests
        // that deliberately use the default test-thread stack budget).
        Box::pin(self.handle_request_inner_unboxed(request)).await
    }

    async fn handle_request_inner_unboxed(&self, request: &RpcRequest) -> HandleResult {
        // P0 attribution gate. MUST run before the Subscribe special-case
        // below — a token-carrying Subscribe would otherwise bypass the gate
        // entirely (Subscribe is a real streaming surface, not exempt from
        // attribution). Unattributed calls (no token = TUI/operator) are
        // completely unchanged past this point.
        if request.session_token.is_some()
            && !agent_gate::is_allowed_for_attributed_caller(&request.method)
        {
            return HandleResult::Response(RpcResponse::error(
                request.id.clone(),
                RpcError {
                    code: INVALID_PARAMS,
                    message: format!(
                        "method `{}` is not available to session-attributed callers; use the Agent* verbs instead",
                        request.method
                    ),
                    data: None,
                },
            ));
        }

        // Subscribe is special: transitions the connection to streaming mode
        if request.method == "Subscribe" {
            let params: SubscribeParams = match serde_json::from_value(request.params.clone()) {
                Ok(p) => p,
                Err(e) => {
                    return HandleResult::Response(RpcResponse::error(
                        request.id.clone(),
                        RpcError {
                            code: INVALID_PARAMS,
                            message: format!("Invalid subscribe params: {}", e),
                            data: None,
                        },
                    ));
                }
            };
            return HandleResult::Subscribe {
                response: RpcResponse::success(
                    request.id.clone(),
                    serde_json::json!({ "ok": true }),
                ),
                params,
            };
        }

        let result = match request.method.as_str() {
            "LaunchSession" => self.handle_launch_session(request).await,
            "AgentSpawnChild" => self.handle_agent_spawn_child(request).await,
            "AgentReserveSuccessor" => self.handle_agent_reserve_successor(request).await,
            "AgentGetProgress" => self.handle_agent_get_progress(request).await,
            "AgentSendMessage" => self.handle_agent_send_message(request).await,
            "AgentGetAuthorityCatalog" => self.handle_agent_get_authority_catalog(request).await,
            "ClaimBoundaryMail" => self.handle_claim_boundary_mail(request).await,
            "ConfirmBoundaryMail" => self.handle_confirm_boundary_mail(request).await,
            "AgentGetStatus" => self.handle_agent_get_status(request).await,
            "AgentHalt" => self.handle_agent_halt(request).await,
            "AgentContinueChild" => self.handle_agent_continue_child(request).await,
            "AgentArchiveChild" => self.handle_agent_archive_child(request).await,
            "AgentScheduleWake" => self.handle_agent_schedule_wake(request).await,
            "AgentCancelWake" => self.handle_agent_cancel_wake(request).await,
            "AgentListWakes" => self.handle_agent_list_wakes(request).await,
            "AgentCreateIssue" => self.handle_agent_create_issue(request).await,
            "AgentListIssues" => self.handle_agent_list_issues(request).await,
            "AgentGetIssue" => self.handle_agent_get_issue(request).await,
            "AgentUpdateIssue" => self.handle_agent_update_issue(request).await,
            "AgentUpdateIssueStatus" => self.handle_agent_update_issue_status(request).await,
            "AgentArchiveIssue" => self.handle_agent_archive_issue(request).await,
            "AgentRestoreIssue" => self.handle_agent_restore_issue(request).await,
            "AgentListIssueEvents" => self.handle_agent_list_issue_events(request).await,
            "AgentManagerProgress" => self.handle_agent_manager_progress(request).await,
            "AgentManagerInbox" => self.handle_agent_manager_inbox(request).await,
            "AgentManagerSend" => self.handle_agent_manager_send(request).await,
            "AgentManagerReply" => self.handle_agent_manager_reply(request).await,
            "AgentManagerNotify" => self.handle_agent_manager_notify(request).await,
            "AgentManagerInspect" => self.handle_agent_manager_inspect(request).await,
            "AgentManagerUpdate" => self.handle_agent_manager_update(request).await,
            "AgentSubmitReviewReceipt" => self.handle_agent_submit_review_receipt(request).await,
            "AgentManagerControl" => self.handle_agent_manager_control(request).await,
            "AgentManagerPrepareControl" => {
                self.handle_agent_manager_prepare_control(request).await
            }
            "AgentManagerCommitPreparedControl" => {
                self.handle_agent_manager_commit_prepared_control(request)
                    .await
            }
            "AgentManagerGetAction" => self.handle_agent_manager_get_action(request).await,
            "AgentManagerLaunchIssueWorker" => {
                self.handle_agent_manager_launch_issue_worker(request).await
            }
            "AgentManagerWorkView" => self.handle_agent_manager_work_view(request).await,
            "AgentManagerDelegateNode" => self.handle_agent_manager_delegate_node(request).await,
            "AgentManagerEscalate" => self.handle_agent_manager_escalate(request).await,
            "AgentManagerListEscalations" => {
                self.handle_agent_manager_list_escalations(request).await
            }
            "AgentManagerResolveEscalation" => {
                self.handle_agent_manager_resolve_escalation(request).await
            }
            "AgentTopologyUpsert" => {
                self.handle_agent_topology(request, AgentControlVerbV1::TopologyUpsert)
                    .await
            }
            "AgentTopologyList" => {
                self.handle_agent_topology(request, AgentControlVerbV1::TopologyList)
                    .await
            }
            "AgentTopologyExecute" => {
                self.handle_agent_topology(request, AgentControlVerbV1::TopologyExecute)
                    .await
            }
            "AgentTopologyGetExecution" => {
                self.handle_agent_topology(request, AgentControlVerbV1::TopologyGetExecution)
                    .await
            }
            "AgentTopologyInterrupt" => {
                self.handle_agent_topology(request, AgentControlVerbV1::TopologyInterrupt)
                    .await
            }
            "AgentTopologyResolveAttempt" => {
                self.handle_agent_topology(request, AgentControlVerbV1::TopologyResolveAttempt)
                    .await
            }
            "AgentEnqueueLandingSource" => self.handle_agent_enqueue_landing_source(request).await,
            "AgentReadSessionEvents" => self.handle_agent_read_session_events(request).await,
            "AgentQueryFailureSignatures" => {
                self.handle_agent_query_failure_signatures(request).await
            }
            "AgentGetProviderStatus" => self.handle_agent_get_provider_status(request).await,
            "AgentSubmitJob" => self.handle_agent_submit_job(request).await,
            "AgentGetJob" => self.handle_agent_get_job(request).await,
            "AgentListJobs" => self.handle_agent_list_jobs(request).await,
            "AgentCancelJob" => self.handle_agent_cancel_job(request).await,
            "AgentSendSatelliteMessage" => self.handle_agent_send_satellite_message(request).await,
            "AgentReportToHub" => self.handle_agent_report_to_hub(request).await,
            "AgentGetDaemonInfo" => self.handle_agent_get_daemon_info(request).await,
            "AgentRequestDeploy" => self.handle_agent_request_deploy(request).await,
            "AgentGlobalOverview" => self.handle_agent_global_overview(request).await,
            "AgentManagerOverview" => self.handle_agent_manager_overview(request).await,
            "AgentGlobalSend" => self.handle_agent_global_send(request).await,
            "AgentGlobalAppointManager" => self.handle_agent_global_appoint_manager(request).await,
            "AgentManagerAppointChild" => self.handle_agent_manager_appoint_child(request).await,
            "AgentManagerRevokeChild" => self.handle_agent_manager_revoke_child(request).await,
            "AgentReportToGlobal" => self.handle_agent_report_to_global(request).await,
            "AgentReportUp" => self.handle_agent_report_up(request).await,
            "AgentSendDown" => self.handle_agent_send_down(request).await,

            // Appointment and scope replacement remain operator-only: neither
            // method belongs to AGENT_VERBS or READ_VERBS.
            // #1036: operator-only remote-gate spend view. It must stay out of
            // AGENT_VERBS, READ_VERBS, native tools and the agent CLI catalog.
            "GetCloudSpend" => self.handle_get_cloud_spend(request).await,
            // #872 Slice B: the global manager grant is operator-only.
            "ConfigureGlobalManager" => self.handle_configure_global_manager(request).await,
            "GetGlobalManager" => self.handle_get_global_manager(request).await,
            "GetGlobalManagerWorkspace" => self.handle_get_global_manager_workspace(request).await,
            "GetManagerNodeWorkspace" => self.handle_get_manager_node_workspace(request).await,
            "RevokeGlobalManager" => self.handle_revoke_global_manager(request).await,
            "ListPortfolioNodes" => self.handle_list_portfolio_nodes(request).await,
            "GetPortfolioNode" => self.handle_get_portfolio_node(request).await,
            "ConfigurePortfolioNode" => self.handle_configure_portfolio_node(request).await,
            "RevokePortfolioNode" => self.handle_revoke_portfolio_node(request).await,
            // #1238: the operator escalation queue and top-of-chain notices.
            "ListOperatorEscalations" => self.handle_list_operator_escalations(request).await,
            "RuleOperatorEscalation" => self.handle_rule_operator_escalation(request).await,
            "AcknowledgeOperatorNotice" => self.handle_acknowledge_operator_notice(request).await,
            // #1122: the operator's quiet-point restart; operator-only.
            "RequestOperatorRestart" => self.handle_request_operator_restart(request).await,
            "GetOperatorRestart" => self.handle_get_operator_restart(request).await,
            "CancelOperatorRestart" => self.handle_cancel_operator_restart(request).await,
            "ForceOperatorRestart" => self.handle_force_operator_restart(request).await,
            // #1147: operator-only (not in the verb registry): list and adopt
            // legacy scratch. Adopting records a directory; it deletes nothing.
            "ListLegacyScratch" => self.handle_list_legacy_scratch(request).await,
            // #1333: the operator's friction rollup; operator-only (managers
            // read it through AgentManagerInspect {section:"friction"}).
            "ListFrictionRollup" => self.handle_list_friction_rollup(request).await,
            "AdoptLegacyScratch" => self.handle_adopt_legacy_scratch(request).await,
            // #1406: operator-only (not in the verb registry): clean export of
            // durable state and first-run import on a new machine.
            "ExportPortableBundle" => self.handle_export_portable_bundle(request).await,
            "ImportPortableBundle" => self.handle_import_portable_bundle(request).await,
            "GetHarnessManager" => self.handle_get_harness_manager(request).await,
            "ListHarnessManagerEpics" => self.handle_list_harness_manager_epics(request).await,
            "ListHarnessManagerScope" => self.handle_list_harness_manager_scope(request).await,
            "ConfigureHarnessManager" => self.handle_configure_harness_manager(request).await,
            "ListManagerNodes" => self.handle_list_manager_nodes(request).await,
            "GetManagerNode" => self.handle_get_manager_node(request).await,
            "GetManagerTree" => self.handle_get_manager_tree(request).await,
            "ConfigureManagerNode" => self.handle_configure_manager_node(request).await,
            "RevokeManagerNode" => self.handle_revoke_manager_node(request).await,
            "GetHarnessManagerPolicy" => self.handle_get_harness_manager_policy(request).await,
            "ConfigureHarnessManagerPolicy" => {
                self.handle_configure_harness_manager_policy(request).await
            }
            "GetHarnessManagerState" => self.handle_get_harness_manager_state(request).await,
            "AnswerHarnessManagerDecision" => {
                self.handle_answer_harness_manager_decision(request).await
            }
            // #1415 operator-only: stale decision records are listed and
            // archived (never deleted) by the operator; the TUI decisions
            // board wires both (`X`, #1428).
            "ListStaleManagerDecisions" => self.handle_list_stale_manager_decisions(request).await,
            "ArchiveStaleManagerDecisions" => {
                self.handle_archive_stale_manager_decisions(request).await
            }

            "GetSession" => self.handle_get_session(request).await,
            "ListSessions" => self.handle_list_sessions(request).await,
            // #1096 operator-only RSI Remote controls: keep out of AGENT_VERBS,
            // READ_VERBS, native tools and the agent CLI catalog.
            "RemoteGetStatus" => self.handle_remote_get_status(request).await,
            "RemoteSetConfig" => self.handle_remote_set_config(request).await,
            "GetSatelliteIdentity" => self.handle_get_satellite_identity(request).await,
            "ListSatelliteSessions" => self.handle_list_satellite_sessions(request).await,
            // Hub registry controls are operator-only. They must remain out of
            // AGENT_VERBS, READ_VERBS, native tools and the agent CLI catalog.
            "ListSatellitePeers" => self.handle_list_satellite_peers(request).await,
            "PutSatellitePeer" => self.handle_put_satellite_peer(request).await,
            "PutSatelliteLink" => self.handle_put_satellite_link(request).await,
            // #1017 slice 3. Operator-only, like the rest of this family.
            "PutSatellitePeerScope" => self.handle_put_satellite_peer_scope(request).await,
            "GetSatelliteInboundPolicy" => self.handle_get_satellite_inbound_policy(request).await,
            "PutSatelliteInboundPolicy" => self.handle_put_satellite_inbound_policy(request).await,
            "DeliverHubMessage" => self.handle_deliver_hub_message(request).await,
            "FetchHubReports" => self.handle_fetch_hub_reports(request).await,
            "RequestHubDeploy" => self.handle_request_hub_deploy(request).await,
            "ProbeSatelliteLink" => self.handle_probe_satellite_link(request).await,
            "ListHubSatelliteSessions" => self.handle_list_hub_satellite_sessions(request).await,
            "GetConversation" => self.handle_get_conversation(request).await,
            "GetSessionDiagnostics" => self.handle_get_session_diagnostics(request).await,
            "GetConversationsSince" => self.handle_get_conversations_since(request).await,
            "GetTurnMetrics" => self.handle_get_turn_metrics(request).await,
            "ListRecursiveTaskGraphs" => self.handle_list_recursive_task_graphs(request).await,
            "GetRecursiveGraphAsWorkflow" => {
                self.handle_get_recursive_graph_as_workflow(request).await
            }
            "EditRecursiveNodeInstructions" => {
                self.handle_edit_recursive_node_instructions(request).await
            }
            "EditRecursiveNodeSettings" => self.handle_edit_recursive_node_settings(request).await,
            "ListRecursiveGraphsForTopology" => {
                self.handle_list_recursive_graphs_for_topology(request)
                    .await
            }
            "GetTopologyRecursiveStatus" => {
                self.handle_get_topology_recursive_status(request).await
            }
            "GetRecursiveTaskGraph" => self.handle_get_recursive_task_graph(request).await,
            "GetRecursiveTask" => self.handle_get_recursive_task(request).await,
            "ListRecursiveTasks" => self.handle_list_recursive_tasks(request).await,
            "ListRecursiveTaskAttempts" => self.handle_list_recursive_task_attempts(request).await,
            "ListRecursiveLifecycleEvents" => {
                self.handle_list_recursive_lifecycle_events(request).await
            }
            "ListRecursiveExecutionArtifacts" => {
                self.handle_list_recursive_execution_artifacts(request)
                    .await
            }
            "GetRecursiveExecutionArtifact" => {
                self.handle_get_recursive_execution_artifact(request).await
            }
            "PreviewRecursiveExecutionArtifact" => {
                self.handle_preview_recursive_execution_artifact(request)
                    .await
            }
            "ListRecursiveExecutionArtifactSummaries" => {
                self.handle_list_recursive_execution_artifact_summaries(request)
                    .await
            }
            "ListRecursiveSchedulerRuns" => {
                self.handle_list_recursive_scheduler_runs(request).await
            }
            "GetRecursiveSchedulerRun" => self.handle_get_recursive_scheduler_run(request).await,
            "ListRecursiveSchedulerRunEvents" => {
                self.handle_list_recursive_scheduler_run_events(request)
                    .await
            }
            "GetRecursiveDagOperationalStatus" => {
                self.handle_get_recursive_dag_operational_status(request)
                    .await
            }
            "GetRecursiveLiveAttempt" => self.handle_get_recursive_live_attempt(request).await,
            "ListRecursiveLiveAttempts" => self.handle_list_recursive_live_attempts(request).await,
            "GetRecursiveLiveAttemptHeartbeatStatus" => {
                self.handle_get_recursive_live_attempt_heartbeat_status(request)
                    .await
            }
            "ListStaleRecursiveLiveAttemptHeartbeats" => {
                self.handle_list_stale_recursive_live_attempt_heartbeats(request)
                    .await
            }
            "GetRecursiveLiveInterruptStatus" => {
                self.handle_get_recursive_live_interrupt_status(request)
                    .await
            }
            "ListRecursiveLiveInterrupts" => {
                self.handle_list_recursive_live_interrupts(request).await
            }
            "GetRecursiveLiveRecoveryStatus" => {
                self.handle_get_recursive_live_recovery_status(request)
                    .await
            }
            "GetRecursiveLiveOutputValidationResult" => {
                self.handle_get_recursive_live_output_validation_result(request)
                    .await
            }
            "ListRecursiveLiveOutputValidationResults" => {
                self.handle_list_recursive_live_output_validation_results(request)
                    .await
            }
            "ListRecursiveLiveValidationIssues" => {
                self.handle_list_recursive_live_validation_issues(request)
                    .await
            }
            "GetRecursiveLiveAttemptArtifacts" => {
                self.handle_get_recursive_live_attempt_artifacts(request)
                    .await
            }
            "ListRecursiveCancellationRequests" => {
                self.handle_list_recursive_cancellation_requests(request)
                    .await
            }
            "GetRecursiveCancellationRequest" => {
                self.handle_get_recursive_cancellation_request(request)
                    .await
            }
            "GetRecursiveRecoveryStatus" | "GetRecursiveDagRecoveryStatus" => {
                self.handle_get_recursive_recovery_status(request).await
            }
            "ListRecursiveDeferredRecoveryGraphs" => {
                self.handle_list_recursive_deferred_recovery_graphs(request)
                    .await
            }
            "ContinueRecursiveRecovery" | "RunRecursiveDagRecoveryPass" => {
                self.handle_continue_recursive_recovery(request).await
            }
            "ContinueTopologyRecursiveRecovery" => {
                self.handle_continue_topology_recursive_recovery(request)
                    .await
            }
            "RequestRecursiveGraphCancellation" | "CancelRecursiveTaskGraph" => {
                self.handle_request_recursive_graph_cancellation(request)
                    .await
            }
            "RequestRecursiveSchedulerRunCancellation" | "CancelRecursiveSchedulerRun" => {
                self.handle_request_recursive_scheduler_run_cancellation(request)
                    .await
            }
            "RequestTopologyRecursiveCancellation" => {
                self.handle_request_topology_recursive_cancellation(request)
                    .await
            }
            "RunRecursiveFakeScheduler" | "RunRecursiveDagFakeScheduler" => {
                self.handle_run_recursive_fake_scheduler(request).await
            }
            "RunRecursiveLiveScheduler" => self.handle_run_recursive_live_scheduler(request).await,
            "CommitRecursiveLiveAttemptOutput" => {
                self.handle_commit_recursive_live_attempt_output(request)
                    .await
            }
            "RunRecursiveTopologyNodeFakeScheduler" => {
                self.handle_run_recursive_topology_node_fake_scheduler(request)
                    .await
            }
            "InterruptSession" => self.handle_interrupt_session(request).await,
            "InterruptSessionNow" => self.handle_interrupt_session_now(request).await,
            "QueueOperatorMessage" => self.handle_queue_operator_message(request).await,
            "ListOperatorMessages" => self.handle_list_operator_messages(request).await,
            "EditOperatorMessage" => self.handle_edit_operator_message(request).await,
            "WithdrawOperatorMessage" => self.handle_withdraw_operator_message(request).await,
            "RestartDaemonDrain" => self.handle_restart_daemon_drain(request).await,
            "GetDrainRestartStatus" => self.handle_get_drain_restart_status(request).await,
            "SetOperatorPause" => self.handle_set_operator_pause(request).await,
            "GetOperatorPause" => self.handle_get_operator_pause(request).await,
            "ContinueSession" => self.handle_continue_session(request).await,
            "RotateSession" => self.handle_rotate_session(request).await,
            // #1176 operator-only: deliberately absent from AGENT_VERBS,
            // READ_VERBS, native tools and the agent CLI catalog.
            "AbandonBlockedRotation" => self.handle_abandon_blocked_rotation(request).await,
            "DeleteSession" => self.handle_delete_session(request).await,
            "ArchiveSession" => self.handle_archive_session(request).await,
            "GetArchiveCleanupStatus" => self.handle_get_archive_cleanup_status(request).await,
            "MarkPendingArchive" => self.handle_mark_pending_archive(request).await,
            "ListArchivedSessions" => self.handle_list_archived_sessions(request).await,
            "UnarchiveSession" => self.handle_unarchive_session(request).await,
            "ListDeletedSessions" => self.handle_list_deleted_sessions(request).await,
            "UndeleteSession" => self.handle_undelete_session(request).await,
            "PurgeSession" => self.handle_purge_session(request).await,
            "TogglePin" => self.handle_toggle_pin(request).await,
            "ToggleTestingNeeded" => self.handle_toggle_testing_needed(request).await,
            "ToggleRotationDisabled" => self.handle_toggle_rotation_disabled(request).await,
            "UpdateSessionProject" => self.handle_update_session_project(request).await,
            "UpdateSessionWorkflow" => self.handle_update_session_workflow(request).await,
            "UpdateSessionTitle" => self.handle_update_session_title(request).await,
            "UpdateSessionDescription" => self.handle_update_session_description(request).await,
            "UpdateSessionRating" => self.handle_update_session_rating(request).await,
            "UpdateActiveTask" => self.handle_update_active_task(request).await,
            "AnswerQuestion" => self.handle_answer_question(request).await,
            // Project methods
            "CreateProject" => self.handle_create_project(request).await,
            "UpdateProject" => self.handle_update_project(request).await,
            "DeleteProject" => self.handle_delete_project(request).await,
            "GetProject" => self.handle_get_project(request).await,
            "ListProjects" => self.handle_list_projects(request).await,
            // Label methods
            "CreateLabel" => self.handle_create_label(request).await,
            "UpdateLabel" => self.handle_update_label(request).await,
            "DeleteLabel" => self.handle_delete_label(request).await,
            "GetLabel" => self.handle_get_label(request).await,
            "ListLabels" => self.handle_list_labels(request).await,
            "UpdateSessionLabel" => self.handle_update_session_label(request).await,
            // Hierarchy methods (Group/Epic organizational tree).
            "CreateContainer" => self.handle_create_container(request).await,
            "SetSessionParent" => self.handle_set_session_parent(request).await,
            "ListSessionChildren" => self.handle_list_session_children(request).await,
            "SetEpicLead" => self.handle_set_epic_lead(request).await,
            // Topology methods (DB-stored named templates) — P1.4.
            "ListTopologies" => self.handle_list_topologies(request).await,
            "CreateTopology" => self.handle_create_topology(request).await,
            "UpdateTopology" => self.handle_update_topology(request).await,
            "DeleteTopology" => self.handle_delete_topology(request).await,
            "GetTopology" => self.handle_get_topology(request).await,
            "ExecuteTopology" => self.handle_execute_topology(request).await,
            // Index status sidecar methods (filesystem JSON) — P1.9.
            "UpdateIndexStatus" => self.handle_update_index_status(request).await,
            "GetIndexStatus" => self.handle_get_index_status(request).await,
            // Tag methods (multi-tag join + normalization) — P1.5.
            "UpdateSessionTags" => self.handle_update_session_tags(request).await,
            "AddSessionTag" => self.handle_add_session_tag(request).await,
            "RemoveSessionTag" => self.handle_remove_session_tag(request).await,
            "ListTags" => self.handle_list_tags(request).await,
            "SwitchSessionModel" => Err(DaemonError::Rpc(
                "SwitchSessionModel has been deprecated. Model is locked at session creation."
                    .to_string(),
            )),
            "QueueSessionModelUpdate" => self.handle_queue_session_model_update(request).await,
            "GetSessionModelSwitchOptions" => {
                self.handle_get_session_model_switch_options(request).await
            }
            "GetModelSegments" => self.handle_get_model_segments(request).await,
            // Read-only lifetime usage aggregate (T8) — operator/TUI-only,
            // deliberately NOT added to READ_VERBS (F-010).
            "GetFleetOverview" => self.handle_get_fleet_overview(request).await,
            "GetUsageStats" => self.handle_get_usage_stats(request).await,
            "GetEfficiencyMetrics" => self.handle_get_efficiency_metrics(request).await,
            "GetRollingQueue" => self.handle_get_rolling_queue(request).await,
            // Resource governor (#1014). Deliberately absent from
            // AGENT_VERBS/READ_VERBS: the wrapper client calls these
            // without a session token, and a lease grants a slot only.
            "AcquireAdmission" => self.handle_acquire_admission(request).await,
            "ReleaseAdmission" => self.handle_release_admission(request).await,
            "GetResourceGovernor" => self.handle_get_resource_governor(request).await,
            "GetModelControlStatus" => self.handle_get_model_control_status(request).await,
            "UpdateModelControlPolicy" => self.handle_update_model_control_policy(request).await,
            "ListModelInvocations" => self.handle_list_model_invocations(request).await,
            "CancelModelInvocation" => self.handle_cancel_model_invocation(request).await,
            "DiscoverModels" => self.handle_discover_models(request).await,
            "GetHealthStatus" => self.handle_get_health_status(request).await,
            "GetDaemonCapabilities" => self.handle_get_daemon_capabilities(request).await,
            // Workflow methods
            "ListWorkflows" => self.handle_list_workflows(request).await,
            "GetWorkflow" => self.handle_get_workflow(request).await,
            "GetWorkflowDefinition" => self.handle_get_workflow_definition(request).await,
            "UpsertWorkflowDefinition" => self.handle_upsert_workflow_definition(request).await,
            // Per-project config (FLYWHEEL.md) methods
            "GetProjectWorkflow" => self.handle_get_project_workflow(request).await,
            "ReloadProjectWorkflow" => self.handle_reload_project_workflow(request).await,
            // ESP game methods
            "SaveEspGame" => self.handle_save_esp_game(request).await,
            "ListEspGames" => self.handle_list_esp_games(request).await,
            // Graph generation methods
            "GenerateWorkflow" => self.handle_generate_workflow(request).await,
            "ClearGraphCache" => self.handle_clear_graph_cache(request).await,
            "RefineWorkflow" => self.handle_refine_workflow(request).await,
            // Graph execution methods
            "ExecuteWorkflow" => self.handle_execute_workflow(request).await,
            "StartChainedWorkflow" => self.handle_start_chained_workflow(request).await,
            "GetWorkflowExecution" => self.handle_get_workflow_execution(request).await,
            "InterruptWorkflowExecution" => self.handle_interrupt_workflow_execution(request).await,
            "ResolveTopologyAttempt" => self.handle_resolve_topology_attempt(request).await,
            // Summary methods
            "GetSessionSummary" => self.handle_get_session_summary(request).await,
            // Memory methods
            "MemorySearch" => self.handle_memory_search(request).await,
            "MemoryStatus" => self.handle_memory_status(request).await,
            "MemoryIndex" => self.handle_memory_index(request).await,
            "MemoryRead" => self.handle_memory_read(request).await,
            // Observation methods
            "ListObservations" => self.handle_list_observations(request).await,
            "SearchObservations" => self.handle_search_observations(request).await,
            // Entity card methods
            "GetEntityCard" => self.handle_get_entity_card(request).await,
            "SetEntityCard" => self.handle_set_entity_card(request).await,
            // Retry management
            "CancelRetry" => self.handle_cancel_retry(request).await,
            // Dreamer
            "GetDreamStatus" => self.handle_get_dream_status(request).await,
            "TriggerDream" => self.handle_trigger_dream(request).await,
            // Issue tracker methods
            "GetIssueTrackerStatus" => self.handle_get_issue_tracker_status(request).await,
            "ListDispatchedIssues" => self.handle_list_dispatched_issues(request).await,
            "TriggerIssueTrackerPoll" => self.handle_trigger_issue_tracker_poll(request).await,
            // Local issue tracker (Track C slice C3): operator-only issue
            // CRUD + dependency-edge + ready-work RPC surface over the V72
            // `issues`/`issue_deps` store (crates/rsid-store/src/store/issues.rs).
            // Deliberately absent from AGENT_VERBS/READ_VERBS above — the
            // default-deny agent gate rejects any token-attributed call to
            // these methods (P-003); agent-attributed issue writes are C5.
            "CreateIssue" => self.handle_create_issue(request).await,
            "CreateIssueV2" => self.handle_create_issue_v2(request).await,
            "GetIssueInProject" => self.handle_get_issue_in_project(request).await,
            "ListIssuesPage" => self.handle_list_issues_page(request).await,
            "UpdateIssue" => self.handle_update_issue_v2(request).await,
            "UpdateIssueStatusV2" => self.handle_update_issue_status_v2(request).await,
            "ListIssueDependencies" => self.handle_list_issue_dependencies(request).await,
            "AddIssueDependency" => self.handle_add_issue_dependency(request).await,
            "RemoveIssueDependency" => self.handle_remove_issue_dependency(request).await,
            "ListIssueEventsV2" => self.handle_list_issue_events_v2(request).await,
            "ArchiveIssue" => self.handle_archive_issue_v2(request).await,
            "RestoreIssue" => self.handle_restore_issue_v2(request).await,
            "LinkIssueToIdea" => self.handle_link_issue_to_idea(request).await,
            // D01 bounded Idea read. Deliberately absent from
            // AGENT_VERBS/READ_VERBS: operator-only by default-deny.
            "GetIdea" => self.handle_get_idea(request).await,
            // D05 ProgramRun kernel. These eight methods are operator-only and
            // remain absent from both attributed allowlists.
            "CreateProgramRun" => self.handle_create_program_run(request).await,
            "GetProgramRun" => self.handle_get_program_run(request).await,
            "ListProgramRuns" => self.handle_list_program_runs(request).await,
            "ListProgramRunTransitions" => self.handle_list_program_run_transitions(request).await,
            "GetProgramRunOperationalStatus" => {
                self.handle_get_program_run_operational_status(request)
                    .await
            }
            "CancelProgramRun" => self.handle_cancel_program_run(request).await,
            "ResumeBlockedProgramRun" => self.handle_resume_blocked_program_run(request).await,
            "ReconcileProgramRuns" => self.handle_reconcile_program_runs(request).await,
            // K1 Closure Kernel. All six methods are intentionally absent
            // from both attributed-caller allowlists above.
            "CreateClosureProgram" => self.handle_create_closure_program(request).await,
            "UpdateClosureProgram" => self.handle_update_closure_program(request).await,
            "LaunchClosureSource" => self.handle_launch_closure_source(request).await,
            "ListClosurePrograms" => self.handle_list_closure_programs(request).await,
            "GetClosureProgram" => self.handle_get_closure_program(request).await,
            "RecordClosureEvidence" => self.handle_record_closure_evidence(request).await,
            "GetIssue" => self.handle_get_issue(request).await,
            "ListIssues" => self.handle_list_issues(request).await,
            "UpdateIssueStatus" => self.handle_update_issue_status(request).await,
            "ListIssueEvents" => self.handle_list_issue_events(request).await,
            "AddIssueDep" => self.handle_add_issue_dep(request).await,
            "RemoveIssueDep" => self.handle_remove_issue_dep(request).await,
            "ListReadyIssues" => self.handle_list_ready_issues(request).await,
            // Dialectic query
            "QueryMemory" => self.handle_query_memory(request).await,
            // Scheduled job methods
            "CreateScheduledJob" => self.handle_create_scheduled_job(request).await,
            "ListScheduledJobs" => self.handle_list_scheduled_jobs(request).await,
            "ListScheduledJobHolds" => self.handle_list_scheduled_job_holds(request).await,
            "UpdateScheduledJob" => self.handle_update_scheduled_job(request).await,
            "DeleteScheduledJob" => self.handle_delete_scheduled_job(request).await,
            "ToggleScheduledJob" => self.handle_toggle_scheduled_job(request).await,
            "TriggerScheduledJob" => self.handle_trigger_scheduled_job(request).await,
            // Compiled prompts
            "SaveCompiledPrompt" => self.handle_save_compiled_prompt(request).await,
            "ListCompiledPrompts" => self.handle_list_compiled_prompts(request).await,
            // Runtime config
            // Codegraph operator reads are intentionally absent from both
            // agent allowlists and the agent CLI catalog.
            "GetCodegraphCapabilities" => self.handle_get_codegraph_capabilities(request).await,
            "ListCodegraphWorkspaces" => self.handle_list_codegraph_workspaces(request).await,
            "GetCodegraphStatus" => self.handle_get_codegraph_status(request).await,
            "GetCodegraphSnapshot" => self.handle_get_codegraph_snapshot(request).await,
            "ListCodegraphSnapshots" => self.handle_list_codegraph_snapshots(request).await,
            "SearchCodegraph"
            | "ExplainCodegraph"
            | "GetCodegraphNeighbors"
            | "FindCodegraphPath"
            | "GetCodegraphSubgraph"
            | "GetCodegraphImpact"
            | "DiffCodegraph" => self.handle_codegraph_read(request).await,
            "GetDaemonConfig" => self.handle_get_daemon_config(request).await,
            "UpdateDaemonConfig" => self.handle_update_daemon_config(request).await,
            // Issue #69 storage lifecycle. Both methods are operator-only by
            // default-deny and remain absent from AGENT_VERBS/READ_VERBS.
            "GetSandboxStorageStatus" => self.handle_get_sandbox_storage_status(request).await,
            "RunSandboxBuildCacheReclaim" => {
                self.handle_run_sandbox_build_cache_reclaim(request).await
            }
            "RunSandboxWorktreeReclaim" => self.handle_run_sandbox_worktree_reclaim(request).await,
            // Issue #955: operator-only by default-deny, like the reclaim pair.
            "RunArchivedSandboxPurge" => self.handle_run_archived_sandbox_purge(request).await,
            // V94 source-worktree settlement. All four methods are operator-only
            // by omission from both attributed-caller allowlists.
            "ListSourceWorktreeCohorts" => self.handle_list_source_worktree_cohorts(request).await,
            "AuditSourceWorktreeCohort" => self.handle_audit_source_worktree_cohort(request).await,
            "ApplySourceWorktreeCohort" => self.handle_apply_source_worktree_cohort(request).await,
            "GetSourceWorktreeSettlementRun" => {
                self.handle_get_source_worktree_settlement_run(request)
                    .await
            }
            // Stall classifier (RSI-0XX)
            "GetClassificationStatus" => self.handle_get_classification_status(request).await,
            // Prompt compilation & generic generate
            "CompilePrompt" => self.handle_compile_prompt(request).await,
            "GenerateText" => self.handle_generate_text(request).await,
            // #694 K1 provider key vault. Operator-only: absent from
            // AGENT_VERBS/READ_VERBS/native tools/agent CLI catalog, so the
            // attribution gate above already refuses a tokened caller; the
            // handler refuses one again and returns secret-free metadata.
            method if crate::vault::operator::is_operator_method(method) => {
                crate::vault::operator::handle(
                    &crate::vault::global(),
                    method,
                    request.session_token.is_some(),
                    &request.params,
                )
                .await
            }
            // #1407 first-run AWS setup check. Operator-only like the vault
            // methods; the handler refuses a tokened caller again and returns
            // a secret-free result.
            method if rsi_common::provider_profile::OPERATOR_METHODS.contains(&method) => {
                crate::bedrock_setup::handle(
                    &crate::vault::global(),
                    method,
                    request.session_token.is_some(),
                    &request.params,
                    &crate::bedrock_setup::HttpBedrockInvokeProbe::default(),
                )
                .await
            }
            // #788 MCP operator configuration and credentials. Operator-only:
            // absent from AGENT_VERBS/READ_VERBS/native tools/agent CLI
            // catalog; the handler refuses tokened callers again.
            method if crate::mcp_config::is_operator_method(method) => {
                let store = self.session_manager.store().lock().await;
                crate::mcp_config::handle(
                    &store,
                    &crate::vault::global(),
                    method,
                    request.session_token.is_some(),
                    &request.params,
                )
            }
            _ => {
                return HandleResult::Response(RpcResponse::error(
                    request.id.clone(),
                    RpcError {
                        code: METHOD_NOT_FOUND,
                        message: format!("Method not found: {}", request.method),
                        data: None,
                    },
                ));
            }
        };

        if let Err(error) = &result {
            // #1333 andon: a refused `Agent*` verb is friction data.
            Box::pin(self.note_agent_refusal(request, error)).await;
        }
        HandleResult::Response(match result {
            Ok(value) => RpcResponse::success(request.id.clone(), value),
            Err(e) => {
                let code = match &e {
                    DaemonError::StructuredRpc { rpc_code, .. } => *rpc_code,
                    DaemonError::InvalidParam(_) => INVALID_PARAMS,
                    DaemonError::SessionNotFound(_) => INVALID_PARAMS,
                    _ => INTERNAL_ERROR,
                };
                let data = match &e {
                    DaemonError::StructuredRpc { data, .. } => Some(data.clone()),
                    _ => None,
                };
                // Surface IO and Process errors to the daemon log before
                // they round-trip back to the TUI as an opaque RPC error.
                // Without this the TUI shows e.g.
                // `RPC error (-32603): IO error: No such file or directory`
                // with no corresponding daemon-side trace to debug from.
                match &e {
                    DaemonError::Io(io_err) => {
                        tracing::error!(
                            method = %request.method,
                            request_id = ?request.id,
                            error = %io_err,
                            error_kind = ?io_err.kind(),
                            "RPC handler returned IO error; propagating as INTERNAL_ERROR"
                        );
                    }
                    DaemonError::Process(msg) => {
                        tracing::error!(
                            method = %request.method,
                            request_id = ?request.id,
                            error = %msg,
                            "RPC handler returned Process error; propagating as INTERNAL_ERROR"
                        );
                    }
                    _ => {}
                }
                RpcResponse::error(
                    request.id.clone(),
                    RpcError {
                        code,
                        message: e.to_string(),
                        data,
                    },
                )
            }
        })
    }

    // === Issue tracker RPC handlers ===

    // -- Project handlers --

    // ─── Local issue tracker (Track C slice C3): operator-only RPC ──────
    //
    // Mirrors the Label family's handler shape exactly (deser -> Invalid
    // params via `DaemonError::Rpc`; store call via
    // `self.session_manager.store().lock().await`; `to_value` envelope).
    // No production store method is new here (C1 `store/issues.rs` already
    // has the whole surface) — these handlers are thin RPC adapters only.

    // --- Scheduled job handlers ---

    // -- Summary handlers --

    // -- Memory handlers --

    // -- Observation handlers --

    // ─── Index status sidecar handlers (filesystem JSON) — P1.9 ─────────────
}

/// Extract the event type string from a DaemonEvent (without allocating).
fn event_type_str(event: &DaemonEvent) -> &'static str {
    match event {
        DaemonEvent::SessionCreated { .. } => "session_created",
        DaemonEvent::SessionQuestionRaised { .. } => "session_question_raised",
        DaemonEvent::ManagerNoticeQueued { .. } => "manager_notice_queued",
        DaemonEvent::SessionStatusChanged { .. } => "session_status_changed",
        DaemonEvent::ConversationEvent { .. } => "conversation_event",
        DaemonEvent::GraphExecution { .. } => crate::bus::GRAPH_EXECUTION_EVENT,
        DaemonEvent::CodegraphIndexStatus { .. } => "codegraph.index.status",
        DaemonEvent::AgentMessageState { .. } => crate::bus::AGENT_MESSAGE_STATE_EVENT,
        DaemonEvent::SystemMessage { .. } => "system_message",
        DaemonEvent::SessionDeleted { .. } => "session_deleted",
        DaemonEvent::SessionArchived { .. } => "session_archived",
        DaemonEvent::SessionUnarchived { .. } => "session_unarchived",
        DaemonEvent::SessionMetadataChanged { .. } => "session_metadata_changed",
        DaemonEvent::MemoryIndexUpdated { .. } => "memory_index_updated",
        DaemonEvent::SessionRetrying { .. } => "session_retrying",
        DaemonEvent::SessionHealScheduled { .. } => "session_heal_scheduled",
        DaemonEvent::ContextUsageUpdated { .. } => "context_usage_updated",
        DaemonEvent::SessionStalled { .. } => "session_stalled",
        DaemonEvent::QueueTaskCompleted { .. } => "queue_task_completed",
        DaemonEvent::QueueTaskFailed { .. } => "queue_task_failed",
        DaemonEvent::SessionSummaryUpdated { .. } => "session_summary_updated",
        DaemonEvent::ObservationsExtracted { .. } => "observations_extracted",
        DaemonEvent::SessionReconciled { .. } => "session_reconciled",
        DaemonEvent::DreamStarted => "dream_started",
        DaemonEvent::DreamCompleted { .. } => "dream_completed",
        DaemonEvent::IssueDispatched { .. } => "issue_dispatched",
        DaemonEvent::IssueTrackerPolled { .. } => "issue_tracker_polled",
        DaemonEvent::IssueReconciled { .. } => "issue_reconciled",
        DaemonEvent::ScheduledJobFired { .. } => "scheduled_job_fired",
        DaemonEvent::CompilePromptChunk { .. } => "compile_prompt_chunk",
        DaemonEvent::CompilePromptCompleted { .. } => "compile_prompt_completed",
        DaemonEvent::CompilePromptFailed { .. } => "compile_prompt_failed",
        DaemonEvent::SandboxOrphanCleaned { .. } => "sandbox_orphan_cleaned",
        DaemonEvent::ChildSpawned { .. } => "child_spawned",
        DaemonEvent::HaltDirective { .. } => "halt_directive",
        DaemonEvent::SessionSpawnDeduped { .. } => "session_spawn_deduped",
        DaemonEvent::SessionClassified { .. } => "session_classified",
        DaemonEvent::ModelInvocationAdmitted { .. } => "model_invocation_admitted",
        DaemonEvent::ModelInvocationDenied { .. } => "model_invocation_denied",
        DaemonEvent::ModelBudgetNearLimit { .. } => "model_budget_near_limit",
        DaemonEvent::ModelControlModeChanged { .. } => "model_control_mode_changed",
        DaemonEvent::ModelControlCircuitChanged { .. } => "model_control_circuit_changed",
        DaemonEvent::ModelInvocationCancellationRequested { .. } => {
            "model_invocation_cancellation_requested"
        }
        DaemonEvent::ModelInvocationCancellationSkipped { .. } => {
            "model_invocation_cancellation_skipped"
        }
        DaemonEvent::ModelInvocationCancelled { .. } => "model_invocation_cancelled",
        DaemonEvent::ModelInvocationCompleted { .. } => "model_invocation_completed",
        DaemonEvent::ProviderRateLimitUpdated { .. } => "provider_rate_limit_updated",
    }
}

/// Check if a DaemonEvent is associated with a specific session.
fn event_matches_session(event: &DaemonEvent, session_id: Uuid) -> bool {
    match event {
        DaemonEvent::SessionCreated { session } => session.id == session_id,
        DaemonEvent::SessionQuestionRaised {
            session_id: sid, ..
        } => *sid == session_id,
        DaemonEvent::ManagerNoticeQueued { .. } => false,
        DaemonEvent::SessionStatusChanged {
            session_id: sid, ..
        } => *sid == session_id,
        DaemonEvent::ConversationEvent {
            session_id: sid, ..
        } => *sid == session_id,
        DaemonEvent::SessionDeleted {
            session_id: sid, ..
        } => *sid == session_id,
        DaemonEvent::SessionArchived {
            session_id: sid, ..
        } => *sid == session_id,
        DaemonEvent::SessionUnarchived {
            session_id: sid, ..
        } => *sid == session_id,
        DaemonEvent::SessionMetadataChanged {
            session_id: sid, ..
        } => *sid == session_id,
        DaemonEvent::ContextUsageUpdated {
            session_id: sid, ..
        } => *sid == session_id,
        DaemonEvent::SessionRetrying {
            session_id: sid, ..
        } => *sid == session_id,
        DaemonEvent::SessionHealScheduled {
            session_id: sid,
            owner_session_id,
            ..
        } => *sid == session_id || *owner_session_id == Some(session_id),
        DaemonEvent::GraphExecution { .. } => true,
        DaemonEvent::CodegraphIndexStatus { .. } => true,
        // AgentMessageState is scoped to the message's immutable logical
        // target root, never a delivery tip — a subscriber filtering on the
        // root it knows must still see every state change for its mail.
        DaemonEvent::AgentMessageState { event } => event.target_session_id == session_id,
        DaemonEvent::SystemMessage { .. } => true, // System messages are always delivered
        DaemonEvent::MemoryIndexUpdated { .. } => true, // Global event, always delivered
        DaemonEvent::SessionStalled {
            session_id: sid, ..
        } => *sid == session_id,
        DaemonEvent::QueueTaskCompleted { .. } => false, // Queue events are not session-scoped
        DaemonEvent::QueueTaskFailed { .. } => false,
        DaemonEvent::SessionSummaryUpdated {
            session_id: sid, ..
        } => *sid == session_id,
        DaemonEvent::ObservationsExtracted {
            session_id: sid, ..
        } => *sid == session_id,
        DaemonEvent::SessionReconciled {
            session_id: sid, ..
        } => *sid == session_id,
        DaemonEvent::DreamStarted => true, // Global event, always delivered
        DaemonEvent::DreamCompleted { .. } => true, // Global event, always delivered
        DaemonEvent::IssueDispatched {
            session_id: sid, ..
        } => *sid == session_id,
        DaemonEvent::IssueTrackerPolled { .. } => true, // Global event
        DaemonEvent::IssueReconciled { .. } => true,    // Global event
        DaemonEvent::ScheduledJobFired { .. } => true,  // Global event
        DaemonEvent::CompilePromptChunk { .. } => true, // Caller filters by request_id
        DaemonEvent::CompilePromptCompleted { .. } => true,
        DaemonEvent::CompilePromptFailed { .. } => true,
        // Orphan cleanup events are daemon-global; always deliver.
        DaemonEvent::SandboxOrphanCleaned { .. } => true,
        // Child-spawned events are global (TUI updates hierarchy view).
        DaemonEvent::ChildSpawned { .. } => true,
        // HaltDirective: scoped to the emitting session but loop executor
        // subscribes globally; always deliver so it can filter by session_id.
        DaemonEvent::HaltDirective { .. } => true,
        // SessionSpawnDeduped: scoped to the session whose duplicate spawn was
        // suppressed — deliver to that session's subscriber, like other
        // session-scoped lifecycle events.
        DaemonEvent::SessionSpawnDeduped {
            session_id: sid, ..
        } => *sid == session_id,
        // SessionClassified: scoped to the classified session — TUI renders
        // an ambient notification for the focused session.
        DaemonEvent::SessionClassified {
            session_id: sid, ..
        } => *sid == session_id,
        DaemonEvent::ModelInvocationAdmitted {
            session_id: Some(sid),
            ..
        } => *sid == session_id,
        DaemonEvent::ModelInvocationAdmitted {
            session_id: None, ..
        } => true,
        DaemonEvent::ModelInvocationDenied {
            session_id: Some(sid),
            ..
        } => *sid == session_id,
        DaemonEvent::ModelInvocationDenied {
            session_id: None, ..
        } => true,
        DaemonEvent::ModelBudgetNearLimit { .. } => true,
        // Plan-window utilization is an ACCOUNT fact shared by every session,
        // so it is daemon-global: always deliver.
        DaemonEvent::ProviderRateLimitUpdated { .. } => true,
        DaemonEvent::ModelControlModeChanged { .. } => true,
        DaemonEvent::ModelControlCircuitChanged { .. } => true,
        DaemonEvent::ModelInvocationCancellationRequested {
            session_id: Some(sid),
            ..
        } => *sid == session_id,
        DaemonEvent::ModelInvocationCancellationRequested {
            session_id: None, ..
        } => true,
        DaemonEvent::ModelInvocationCancellationSkipped {
            session_id: Some(sid),
            ..
        } => *sid == session_id,
        DaemonEvent::ModelInvocationCancellationSkipped {
            session_id: None, ..
        } => true,
        DaemonEvent::ModelInvocationCancelled {
            session_id: Some(sid),
            ..
        } => *sid == session_id,
        DaemonEvent::ModelInvocationCancelled {
            session_id: None, ..
        } => true,
        DaemonEvent::ModelInvocationCompleted { .. } => true,
    }
}

#[cfg(test)]
mod scheduled_jobs_list_tests; // #954 B RPC paging tests

#[cfg(test)]
mod tests;

/// Production source of the RPC router plus every family module, for
/// source-scanning tests: `rpc.rs` alone no longer holds the handlers.
#[cfg(test)]
pub(crate) fn rpc_production_source() -> String {
    let router = include_str!("rpc.rs");
    let end = router
        .find("#[cfg(test)]\nmod scheduled_jobs_list_tests")
        .expect("router test module marker");
    let mut source = router[..end].to_string();
    for family in [
        include_str!("rpc/agent_issues.rs"),
        include_str!("rpc/agent_verbs.rs"),
        include_str!("rpc/codegraph.rs"),
        include_str!("rpc/common.rs"),
        include_str!("rpc/issues.rs"),
        include_str!("rpc/manager.rs"),
        include_str!("rpc/memory.rs"),
        include_str!("rpc/models.rs"),
        include_str!("rpc/program_runs.rs"),
        include_str!("rpc/projects.rs"),
        include_str!("rpc/recursive_control.rs"),
        include_str!("rpc/recursive_read.rs"),
        include_str!("rpc/remote.rs"),
        include_str!("rpc/satellites.rs"),
        include_str!("rpc/scheduled_jobs.rs"),
        include_str!("rpc/sessions.rs"),
        include_str!("rpc/settings.rs"),
        include_str!("rpc/storage.rs"),
        include_str!("rpc/topology.rs"),
    ] {
        source.push('\n');
        source.push_str(family);
    }
    source
}
