//! Daemon client: Unix socket JSON-RPC.

use rsi_common::TagWithCount;
use rsi_common::archive_cleanup::{
    ArchiveCleanupErrorV1, ArchiveCleanupStatusV1, ArchiveSessionResultV1,
};
use rsi_common::cohort_settlement::{
    ApplySourceWorktreeCohortParams, AuditSourceWorktreeCohortParams,
    GetSourceWorktreeSettlementRunParams, ListSourceWorktreeCohortsParams,
    SourceWorktreeCohortAuditV1, SourceWorktreeCohortSummaryV1, SourceWorktreeSettlementRunV1,
};
use rsi_common::issue_workspace::{
    ArchiveIssueRequestV1, CreateIssueV2RequestV1, GetIssueInProjectRequestV1,
    IssueDependencyMutationRequestV1, IssueDependencyMutationResultV1, IssueDependencyPageV1,
    IssueDispatchRecordV1, IssueTrackerStatusV1, IssueTrackerTickResultV1, IssueWorkspaceErrorV1,
    IssueWorkspacePageV1, ListIssueDependenciesRequestV1, ListIssueEventsV2RequestV1,
    ListIssueEventsV2ResultV1, ListIssuesPageRequestV1, OperatorIssueMutationResultV1,
    RestoreIssueRequestV1, UpdateIssueRequestV1, UpdateIssueStatusV2RequestV1,
};
use rsi_common::rpc::{
    CommitRecursiveLiveAttemptOutputParams, CommitRecursiveLiveAttemptOutputResponse,
    ContinueRecursiveRecoveryParams, ConversationBatchResponse, ConversationFetchCursor,
    DaemonCapabilities, GetRecursiveExecutionArtifactParams,
    GetRecursiveLiveAttemptArtifactsParams, GetRecursiveLiveAttemptHeartbeatStatusParams,
    GetRecursiveLiveAttemptParams, GetRecursiveLiveInterruptStatusParams,
    GetRecursiveLiveOutputValidationResultParams, GetRecursiveLiveRecoveryStatusParams,
    GetRecursiveRecoveryStatusParams, HealthStatusResponse,
    ListRecursiveCancellationRequestsParams, ListRecursiveExecutionArtifactSummariesParams,
    ListRecursiveExecutionArtifactsParams, ListRecursiveLifecycleEventsParams,
    ListRecursiveLiveAttemptsParams, ListRecursiveLiveInterruptsParams,
    ListRecursiveLiveOutputValidationResultsParams, ListRecursiveLiveValidationIssuesParams,
    ListRecursiveSchedulerRunEventsParams, ListRecursiveSchedulerRunsParams,
    ListRecursiveTaskAttemptsParams, ListRecursiveTaskGraphsParams, ListRecursiveTasksParams,
    ListStaleRecursiveLiveAttemptHeartbeatsParams, MemoryProviderStatus, MemorySearchResult,
    PreviewRecursiveExecutionArtifactParams, RequestRecursiveGraphCancellationParams,
    RequestRecursiveSchedulerRunCancellationParams, RpcRequest, RpcResponse,
    RunRecursiveFakeSchedulerParams, RunRecursiveLiveSchedulerParams,
};
use rsi_common::sandbox_storage::SandboxBuildCacheReclaimReportWire;
use rsi_common::types::{
    IndexStatusSidecar, IndexStatusValue, Project, Session, SessionProvider, Topology,
    TopologyDefinition,
};
use rsi_common::{
    EditRecursiveNodeInstructionsParams, EditRecursiveNodeSettingsParams,
    GetRecursiveGraphAsWorkflowParams, GetRecursiveGraphAsWorkflowResponse,
    RecursiveCancellationRequestId, RecursiveCancellationRequestSummary,
    RecursiveDagOperationalStatus, RecursiveDagRecoveryStatus, RecursiveDeferredRecoveryGraph,
    RecursiveExecutionArtifact, RecursiveExecutionArtifactPreview,
    RecursiveExecutionArtifactReadback, RecursiveExecutionArtifactSummary, RecursiveLifecycleEvent,
    RecursiveLiveAttemptArtifactReadback, RecursiveLiveAttemptHeartbeatState,
    RecursiveLiveAttemptId, RecursiveLiveAttemptListItem, RecursiveLiveAttemptReadback,
    RecursiveLiveInterruptSummary, RecursiveLiveOutputValidationIssue,
    RecursiveLiveOutputValidationListItem, RecursiveLiveOutputValidationResult,
    RecursiveLiveRecoveryReadback, RecursiveReadPage, RecursiveRecoveryPassSummary,
    RecursiveSchedulerRunDetail, RecursiveSchedulerRunEvent, RecursiveSchedulerRunId,
    RecursiveSchedulerRunSummary, RecursiveTaskAttempt, RecursiveTaskGraphDetail,
    RecursiveTaskGraphId, RecursiveTaskGraphSummary, RecursiveTaskId, RecursiveTaskNode,
    RunRecursiveLiveSchedulerResponse,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Not connected to daemon")]
    NotConnected,

    #[error("RPC error ({code}): {message}")]
    Rpc {
        code: i32,
        message: String,
        data: Option<Value>,
    },

    #[error("Connection closed by daemon")]
    ConnectionClosed,

    #[error("Protocol error: {0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, ClientError>;

impl ClientError {
    pub fn reclaim_prepared_retry_ms(&self) -> Option<u64> {
        let Self::Rpc {
            data: Some(data), ..
        } = self
        else {
            return None;
        };
        (data.get("kind")?.as_str()? == "sandbox_custody"
            && data.pointer("/error/code")?.as_str()? == "reclaim_prepared")
            .then(|| data.get("retry_after_ms")?.as_u64())
            .flatten()
            .map(|delay| delay.clamp(1, 10_000))
    }

    pub fn archive_cleanup_error(&self) -> Option<ArchiveCleanupErrorV1> {
        let Self::Rpc {
            data: Some(data), ..
        } = self
        else {
            return None;
        };
        let decoded: ArchiveCleanupErrorV1 = serde_json::from_value(data.clone()).ok()?;
        decoded.validate_wire().ok()?;
        Some(decoded)
    }

    /// Decode the bounded Issue-workspace error carried by a generic operator RPC.
    pub fn issue_workspace_error(&self) -> Option<IssueWorkspaceErrorV1> {
        let Self::Rpc {
            data: Some(data), ..
        } = self
        else {
            return None;
        };
        serde_json::from_value(data.clone()).ok()
    }
}

fn decode_sandbox_storage_report(value: Value) -> Result<SandboxBuildCacheReclaimReportWire> {
    serde_json::from_value::<SandboxBuildCacheReclaimReportWire>(value)?
        .validate_wire()
        .map_err(|error| ClientError::Protocol(error.to_string()))
}

fn decode_source_worktree_cohorts(value: Value) -> Result<Vec<SourceWorktreeCohortSummaryV1>> {
    serde_json::from_value::<Vec<SourceWorktreeCohortSummaryV1>>(value)?
        .into_iter()
        .map(|summary| summary.validate_wire().map_err(ClientError::Protocol))
        .collect()
}

fn decode_source_worktree_audit(value: Value) -> Result<SourceWorktreeCohortAuditV1> {
    serde_json::from_value::<SourceWorktreeCohortAuditV1>(value)?
        .validate_wire()
        .map_err(ClientError::Protocol)
}

fn decode_source_worktree_run(value: Value) -> Result<SourceWorktreeSettlementRunV1> {
    serde_json::from_value::<SourceWorktreeSettlementRunV1>(value)?
        .validate_wire()
        .map_err(ClientError::Protocol)
}

/// Async client for communicating with flywheeld over Unix socket.
pub struct DaemonClient {
    socket_path: PathBuf,
    stream: Option<UnixStream>,
    next_id: AtomicI64,
    /// True when a fire-and-forget request was sent and the response hasn't been consumed yet.
    pending_response: bool,
}

#[allow(clippy::too_many_arguments)]
fn launch_session_with_opts_custom_provider_params(
    query: &str,
    title: Option<&str>,
    working_dir: Option<&Path>,
    provider: SessionProvider,
    model: Option<&str>,
    system_prompt: Option<&str>,
    session_kind: Option<rsi_common::types::SessionKind>,
    project_id: Option<uuid::Uuid>,
    openai_base_url: &str,
    openai_api_key: &str,
    max_retries: Option<u8>,
    effort: Option<&str>,
    parent_id: Option<uuid::Uuid>,
    tags: &[String],
    workflow_id_override: Option<uuid::Uuid>,
) -> Value {
    serde_json::json!({
        "query": query,
        "title": title,
        "working_dir": working_dir,
        "provider": provider,
        "model": model,
        "system_prompt": system_prompt,
        "session_kind": session_kind,
        "project_id": project_id,
        "openai_base_url": openai_base_url,
        "openai_api_key": openai_api_key,
        "max_retries": max_retries,
        "effort": effort,
        "parent_id": parent_id,
        "tags": tags,
        "workflow_id_override": workflow_id_override,
    })
}

#[allow(clippy::too_many_arguments)]
fn launch_session_with_opts_params(
    query: &str,
    title: Option<&str>,
    working_dir: Option<&Path>,
    provider: SessionProvider,
    model: Option<&str>,
    system_prompt: Option<&str>,
    session_kind: Option<rsi_common::types::SessionKind>,
    project_id: Option<uuid::Uuid>,
    max_retries: Option<u8>,
    effort: Option<&str>,
    parent_id: Option<uuid::Uuid>,
    sandbox: Option<rsi_common::types::SandboxSpec>,
    tags: &[String],
    workflow_id: Option<uuid::Uuid>,
    workflow_id_override: Option<uuid::Uuid>,
) -> Value {
    serde_json::json!({
        "query": query,
        "title": title,
        "working_dir": working_dir,
        "provider": provider,
        "model": model,
        "system_prompt": system_prompt,
        "session_kind": session_kind,
        "project_id": project_id,
        "max_retries": max_retries,
        "effort": effort,
        "parent_id": parent_id,
        "sandbox": sandbox,
        "tags": tags,
        "workflow_id": workflow_id,
        "workflow_id_override": workflow_id_override,
    })
}

fn recursive_graph_id_params(graph_id: RecursiveTaskGraphId) -> Value {
    serde_json::json!({ "graph_id": graph_id.0 })
}

fn recursive_task_id_params(graph_id: RecursiveTaskGraphId, task_id: RecursiveTaskId) -> Value {
    serde_json::json!({
        "graph_id": graph_id.0,
        "task_id": task_id.0,
    })
}

fn recursive_scheduler_run_id_params(run_id: RecursiveSchedulerRunId) -> Value {
    serde_json::json!({ "run_id": run_id.0 })
}

fn recursive_cancellation_request_id_params(request_id: RecursiveCancellationRequestId) -> Value {
    serde_json::json!({ "request_id": request_id.0 })
}

fn recursive_fake_scheduler_params(graph_id: RecursiveTaskGraphId, max_steps: u32) -> Value {
    serde_json::to_value(RunRecursiveFakeSchedulerParams {
        graph_id: graph_id.0,
        max_steps,
        operator: Some("tui".to_string()),
        execution_mode: Some("fake".to_string()),
    })
    .expect("RunRecursiveFakeSchedulerParams should serialize")
}

fn recursive_live_scheduler_params(graph_id: RecursiveTaskGraphId, max_steps: u32) -> Value {
    serde_json::to_value(RunRecursiveLiveSchedulerParams {
        graph_id: graph_id.0,
        max_steps,
        operator: Some("tui".to_string()),
        idempotency_key: None,
        provider: None,
        model: None,
        effort: None,
        working_dir: None,
        sandbox: None,
        approval_mode: None,
        tool_policy: None,
        sandbox_policy: None,
        max_wall_time_ms: None,
        token_budget: None,
        tool_call_budget: None,
        artifact_bytes_budget: None,
        output_repair_attempts: Some(0),
        heartbeat_ttl_ms: None,
    })
    .expect("RunRecursiveLiveSchedulerParams should serialize")
}

fn recursive_graph_cancellation_params(
    graph_id: RecursiveTaskGraphId,
    reason: String,
    requested_by: Option<String>,
) -> Value {
    serde_json::to_value(RequestRecursiveGraphCancellationParams {
        graph_id: graph_id.0,
        reason,
        requested_by,
    })
    .expect("RequestRecursiveGraphCancellationParams should serialize")
}

fn recursive_scheduler_run_cancellation_params(
    run_id: RecursiveSchedulerRunId,
    reason: String,
    requested_by: Option<String>,
) -> Value {
    serde_json::to_value(RequestRecursiveSchedulerRunCancellationParams {
        run_id: run_id.0,
        reason,
        requested_by,
    })
    .expect("RequestRecursiveSchedulerRunCancellationParams should serialize")
}

fn recursive_recovery_continuation_params(max_graphs: u32, time_budget_ms: Option<u64>) -> Value {
    serde_json::to_value(ContinueRecursiveRecoveryParams {
        max_graphs,
        time_budget_ms,
    })
    .expect("ContinueRecursiveRecoveryParams should serialize")
}

#[cfg(test)]
fn recursive_artifact_lookup_params(
    graph_id: RecursiveTaskGraphId,
    artifact_id: i64,
    include_links: bool,
) -> Value {
    serde_json::to_value(GetRecursiveExecutionArtifactParams {
        graph_id,
        artifact_id,
        include_links,
    })
    .expect("GetRecursiveExecutionArtifactParams should serialize")
}

#[cfg(test)]
fn recursive_artifact_preview_params(
    graph_id: RecursiveTaskGraphId,
    artifact_id: i64,
    max_bytes: u32,
    max_lines: u32,
) -> Value {
    serde_json::to_value(PreviewRecursiveExecutionArtifactParams {
        graph_id,
        artifact_id,
        byte_offset: None,
        line_offset: None,
        max_bytes: Some(max_bytes),
        max_lines: Some(max_lines),
        render_hint: None,
        require_complete: false,
    })
    .expect("PreviewRecursiveExecutionArtifactParams should serialize")
}

#[cfg(test)]
fn recursive_artifact_summary_list_params(
    graph_id: RecursiveTaskGraphId,
    limit: u32,
    cursor: Option<String>,
) -> Value {
    serde_json::to_value(ListRecursiveExecutionArtifactSummariesParams {
        graph_id: Some(graph_id),
        cursor,
        limit: Some(limit),
        include_total: false,
        ..Default::default()
    })
    .expect("ListRecursiveExecutionArtifactSummariesParams should serialize")
}

impl DaemonClient {
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            stream: None,
            next_id: AtomicI64::new(1),
            pending_response: false,
        }
    }

    /// Default socket path: ~/.flywheel/daemon.sock
    pub fn default_socket_path() -> PathBuf {
        rsi_common::identity::default_socket_path()
    }

    /// Connect to the daemon socket.
    pub async fn connect(&mut self) -> Result<()> {
        let stream = UnixStream::connect(&self.socket_path).await?;
        self.stream = Some(stream);
        Ok(())
    }

    /// Get the socket path (for spawning independent connections).
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Check if connected.
    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    /// Disconnect from daemon.
    pub fn disconnect(&mut self) {
        self.stream = None;
    }

    /// Send an RPC request and wait for the response.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        // Drain any pending fire-and-forget response first
        self.drain_pending().await?;

        let stream = self.stream.as_mut().ok_or(ClientError::NotConnected)?;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::Number(id.into())),
            method: method.to_string(),
            params,
            session_token: None,
        };

        let json = serde_json::to_string(&request)?;
        stream.write_all(format!("{}\n", json).as_bytes()).await?;
        stream.flush().await?;

        let mut buf = String::new();
        let mut reader = BufReader::new(&mut *stream);
        let n = reader.read_line(&mut buf).await?;
        if n == 0 {
            self.stream = None;
            return Err(ClientError::ConnectionClosed);
        }

        let response: RpcResponse = serde_json::from_str(buf.trim())?;

        if let Some(error) = response.error {
            return Err(ClientError::Rpc {
                code: error.code,
                message: error.message,
                data: error.data,
            });
        }

        Ok(response.result.unwrap_or(Value::Null))
    }

    /// Send an RPC request without waiting for the response.
    /// The response is consumed lazily before the next `request()` call.
    /// Use for operations where the TUI doesn't need the result (launch, continue)
    /// to avoid blocking the render loop.
    async fn fire_and_forget(&mut self, method: &str, params: Value) -> Result<()> {
        // Drain any previous pending response first
        self.drain_pending().await?;

        let stream = self.stream.as_mut().ok_or(ClientError::NotConnected)?;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::Number(id.into())),
            method: method.to_string(),
            params,
            session_token: None,
        };

        let json = serde_json::to_string(&request)?;
        stream.write_all(format!("{}\n", json).as_bytes()).await?;
        self.pending_response = true;
        Ok(())
    }

    /// Consume a pending fire-and-forget response. Errors are logged, not propagated
    /// to the caller (since the original caller already returned).
    async fn drain_pending(&mut self) -> Result<()> {
        if !self.pending_response {
            return Ok(());
        }
        self.pending_response = false;

        let stream = self.stream.as_mut().ok_or(ClientError::NotConnected)?;
        let mut buf = String::new();
        let mut reader = BufReader::new(&mut *stream);
        let n = reader.read_line(&mut buf).await?;
        if n == 0 {
            self.stream = None;
            return Err(ClientError::ConnectionClosed);
        }

        let response: RpcResponse = serde_json::from_str(buf.trim())?;
        if let Some(error) = response.error {
            tracing::warn!(
                "Fire-and-forget RPC error: {} (code {})",
                error.message,
                error.code
            );
        }

        Ok(())
    }

    async fn request_launch_session(&mut self, params: Value) -> Result<uuid::Uuid> {
        #[derive(serde::Deserialize)]
        struct LaunchSessionResult {
            session_id: uuid::Uuid,
        }

        let result = self.request("LaunchSession", params).await?;
        Ok(serde_json::from_value::<LaunchSessionResult>(result)?.session_id)
    }

    /// List all sessions.
    pub async fn list_sessions(&mut self) -> Result<Vec<Session>> {
        let result = self.request("ListSessions", Value::Null).await?;
        let sessions: Vec<Session> = serde_json::from_value(result)?;
        Ok(sessions)
    }

    /// Get a specific session by ID.
    pub async fn get_session(&mut self, session_id: uuid::Uuid) -> Result<Session> {
        let result = self
            .request(
                "GetSession",
                serde_json::json!({ "session_id": session_id }),
            )
            .await?;
        let session: Session = serde_json::from_value(result)?;
        Ok(session)
    }

    /// Read the operator-appointed manager and its versioned scope.
    pub async fn get_harness_manager(
        &mut self,
        project_id: uuid::Uuid,
    ) -> Result<Option<rsi_common::harness_manager::HarnessManagerConfigV1>> {
        let result = self
            .request(
                "GetHarnessManager",
                serde_json::to_value(rsi_common::harness_manager::GetHarnessManagerRequestV1 {
                    project_id,
                })?,
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn list_harness_manager_epics(
        &mut self,
        request: rsi_common::harness_manager::ListHarnessManagerEpicsRequestV1,
    ) -> Result<rsi_common::harness_manager::ListHarnessManagerEpicsResultV1> {
        let result = self
            .request("ListHarnessManagerEpics", serde_json::to_value(request)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn list_harness_manager_scope(
        &mut self,
        request: rsi_common::harness_manager::ListHarnessManagerScopeRequestV1,
    ) -> Result<rsi_common::harness_manager::ListHarnessManagerScopeResultV1> {
        let result = self
            .request("ListHarnessManagerScope", serde_json::to_value(request)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Replace the manager scope using the version observed by the operator.
    pub async fn configure_harness_manager(
        &mut self,
        request: rsi_common::harness_manager::ConfigureHarnessManagerRequestV1,
    ) -> Result<rsi_common::harness_manager::HarnessManagerConfigV1> {
        let result = self
            .request("ConfigureHarnessManager", serde_json::to_value(request)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn get_harness_manager_policy(
        &mut self,
        project_id: uuid::Uuid,
    ) -> Result<Option<rsi_common::harness_manager_v2::HarnessManagerPolicyConfigV2>> {
        let result = self
            .request(
                "GetHarnessManagerPolicy",
                serde_json::json!({"project_id": project_id}),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }
    pub async fn configure_harness_manager_policy(
        &mut self,
        request: rsi_common::harness_manager_v2::ConfigureHarnessManagerPolicyRequestV2,
    ) -> Result<rsi_common::harness_manager_v2::HarnessManagerPolicyConfigV2> {
        let result = self
            .request(
                "ConfigureHarnessManagerPolicy",
                serde_json::to_value(request)?,
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }
    pub async fn get_harness_manager_state(
        &mut self,
        request: rsi_common::harness_manager_v2::GetHarnessManagerStateRequestV2,
    ) -> Result<rsi_common::harness_manager_v2::ManagerInspectionV2> {
        let result = self
            .request("GetHarnessManagerState", serde_json::to_value(request)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }
    pub async fn answer_harness_manager_decision(
        &mut self,
        request: rsi_common::harness_manager_v2::AnswerHarnessManagerDecisionRequestV2,
    ) -> Result<serde_json::Value> {
        let result = self
            .request(
                "AnswerHarnessManagerDecision",
                serde_json::to_value(request)?,
            )
            .await?;
        Ok(result)
    }

    /// Launch a new session (fire-and-forget).
    /// Sends the request without blocking on the response. The TUI discovers
    /// the new session via polling. Any daemon-side errors are logged when
    /// the response is drained before the next RPC call.
    #[allow(clippy::too_many_arguments)]
    pub async fn launch_session(
        &mut self,
        query: &str,
        working_dir: Option<&Path>,
        provider: SessionProvider,
        model: Option<&str>,
        system_prompt: Option<&str>,
        project_id: Option<uuid::Uuid>,
        workflow_id: Option<uuid::Uuid>,
        effort: Option<&str>,
        sandbox: Option<rsi_common::types::SandboxSpec>,
        tags: &[String],
        workflow_id_override: Option<uuid::Uuid>,
    ) -> Result<()> {
        let params = serde_json::json!({
            "query": query,
            "working_dir": working_dir,
            "provider": provider,
            "model": model,
            "system_prompt": system_prompt,
            "project_id": project_id,
            "workflow_id": workflow_id,
            "effort": effort,
            "sandbox": sandbox,
            "tags": tags,
            "workflow_id_override": workflow_id_override,
        });
        self.fire_and_forget("LaunchSession", params).await
    }

    /// Launch a session with an inline OpenAI-compatible provider config (custom provider).
    #[allow(clippy::too_many_arguments)]
    pub async fn launch_session_custom_provider(
        &mut self,
        query: &str,
        title: Option<&str>,
        working_dir: Option<&Path>,
        provider: SessionProvider,
        model: Option<&str>,
        system_prompt: Option<&str>,
        project_id: Option<uuid::Uuid>,
        openai_base_url: &str,
        openai_api_key: &str,
        workflow_id: Option<uuid::Uuid>,
        effort: Option<&str>,
        tags: &[String],
        workflow_id_override: Option<uuid::Uuid>,
    ) -> Result<()> {
        let params = serde_json::json!({
            "query": query,
            "title": title,
            "working_dir": working_dir,
            "provider": provider,
            "model": model,
            "system_prompt": system_prompt,
            "project_id": project_id,
            "openai_base_url": openai_base_url,
            "openai_api_key": openai_api_key,
            "workflow_id": workflow_id,
            "effort": effort,
            "tags": tags,
            "workflow_id_override": workflow_id_override,
        });
        self.fire_and_forget("LaunchSession", params).await
    }

    /// Launch with inline OpenAI-compatible credentials and wait for the
    /// daemon's semantic response, returning its assigned session ID.
    #[allow(clippy::too_many_arguments)]
    pub async fn launch_session_custom_provider_response(
        &mut self,
        query: &str,
        title: Option<&str>,
        working_dir: Option<&Path>,
        provider: SessionProvider,
        model: Option<&str>,
        system_prompt: Option<&str>,
        project_id: Option<uuid::Uuid>,
        openai_base_url: &str,
        openai_api_key: &str,
        workflow_id: Option<uuid::Uuid>,
        effort: Option<&str>,
        tags: &[String],
        workflow_id_override: Option<uuid::Uuid>,
    ) -> Result<uuid::Uuid> {
        let params = serde_json::json!({
            "query": query,
            "title": title,
            "working_dir": working_dir,
            "provider": provider,
            "model": model,
            "system_prompt": system_prompt,
            "project_id": project_id,
            "openai_base_url": openai_base_url,
            "openai_api_key": openai_api_key,
            "workflow_id": workflow_id,
            "effort": effort,
            "tags": tags,
            "workflow_id_override": workflow_id_override,
        });
        self.request_launch_session(params).await
    }

    /// Launch a session with opts + inline OpenAI-compatible provider config (custom provider).
    #[allow(clippy::too_many_arguments)]
    pub async fn launch_session_with_opts_custom_provider(
        &mut self,
        query: &str,
        title: Option<&str>,
        working_dir: Option<&Path>,
        provider: SessionProvider,
        model: Option<&str>,
        system_prompt: Option<&str>,
        session_kind: Option<rsi_common::types::SessionKind>,
        project_id: Option<uuid::Uuid>,
        openai_base_url: &str,
        openai_api_key: &str,
        max_retries: Option<u8>,
        effort: Option<&str>,
        parent_id: Option<uuid::Uuid>,
        tags: &[String],
        workflow_id_override: Option<uuid::Uuid>,
    ) -> Result<()> {
        let params = launch_session_with_opts_custom_provider_params(
            query,
            title,
            working_dir,
            provider,
            model,
            system_prompt,
            session_kind,
            project_id,
            openai_base_url,
            openai_api_key,
            max_retries,
            effort,
            parent_id,
            tags,
            workflow_id_override,
        );
        self.fire_and_forget("LaunchSession", params).await
    }

    /// Launch a session with the full custom-provider option surface and wait
    /// for semantic daemon acceptance. The returned ID is the identity from
    /// the `LaunchSession` response, not a session inferred from later polling.
    #[allow(clippy::too_many_arguments)]
    pub async fn launch_session_with_opts_custom_provider_response(
        &mut self,
        query: &str,
        title: Option<&str>,
        working_dir: Option<&Path>,
        provider: SessionProvider,
        model: Option<&str>,
        system_prompt: Option<&str>,
        session_kind: Option<rsi_common::types::SessionKind>,
        project_id: Option<uuid::Uuid>,
        openai_base_url: &str,
        openai_api_key: &str,
        max_retries: Option<u8>,
        effort: Option<&str>,
        parent_id: Option<uuid::Uuid>,
        tags: &[String],
        workflow_id_override: Option<uuid::Uuid>,
    ) -> Result<uuid::Uuid> {
        let params = launch_session_with_opts_custom_provider_params(
            query,
            title,
            working_dir,
            provider,
            model,
            system_prompt,
            session_kind,
            project_id,
            openai_base_url,
            openai_api_key,
            max_retries,
            effort,
            parent_id,
            tags,
            workflow_id_override,
        );
        self.request_launch_session(params).await
    }

    /// Launch a new session with additional options (fire-and-forget).
    /// Supports system prompt and session kind parameters for specialized sessions.
    ///
    /// This carries the full `LaunchSessionParams` surface (including
    /// `workflow_id`). The response-bearing sibling below uses the identical
    /// payload for launch paths whose UI lifecycle depends on semantic daemon
    /// acceptance.
    #[allow(clippy::too_many_arguments)]
    pub async fn launch_session_with_opts(
        &mut self,
        query: &str,
        title: Option<&str>,
        working_dir: Option<&Path>,
        provider: SessionProvider,
        model: Option<&str>,
        system_prompt: Option<&str>,
        session_kind: Option<rsi_common::types::SessionKind>,
        project_id: Option<uuid::Uuid>,
        max_retries: Option<u8>,
        effort: Option<&str>,
        parent_id: Option<uuid::Uuid>,
        sandbox: Option<rsi_common::types::SandboxSpec>,
        tags: &[String],
        workflow_id: Option<uuid::Uuid>,
        workflow_id_override: Option<uuid::Uuid>,
    ) -> Result<()> {
        let params = launch_session_with_opts_params(
            query,
            title,
            working_dir,
            provider,
            model,
            system_prompt,
            session_kind,
            project_id,
            max_retries,
            effort,
            parent_id,
            sandbox,
            tags,
            workflow_id,
            workflow_id_override,
        );
        self.fire_and_forget("LaunchSession", params).await
    }

    /// Launch a session with the full option surface and wait for semantic
    /// daemon acceptance, returning the daemon-assigned session identity.
    #[allow(clippy::too_many_arguments)]
    pub async fn launch_session_with_opts_response(
        &mut self,
        query: &str,
        title: Option<&str>,
        working_dir: Option<&Path>,
        provider: SessionProvider,
        model: Option<&str>,
        system_prompt: Option<&str>,
        session_kind: Option<rsi_common::types::SessionKind>,
        project_id: Option<uuid::Uuid>,
        max_retries: Option<u8>,
        effort: Option<&str>,
        parent_id: Option<uuid::Uuid>,
        sandbox: Option<rsi_common::types::SandboxSpec>,
        tags: &[String],
        workflow_id: Option<uuid::Uuid>,
        workflow_id_override: Option<uuid::Uuid>,
    ) -> Result<uuid::Uuid> {
        let params = launch_session_with_opts_params(
            query,
            title,
            working_dir,
            provider,
            model,
            system_prompt,
            session_kind,
            project_id,
            max_retries,
            effort,
            parent_id,
            sandbox,
            tags,
            workflow_id,
            workflow_id_override,
        );
        self.request_launch_session(params).await
    }

    /// Interrupt a running session.
    pub async fn interrupt_session(&mut self, session_id: uuid::Uuid) -> Result<()> {
        self.request(
            "InterruptSession",
            serde_json::json!({ "session_id": session_id }),
        )
        .await?;
        Ok(())
    }

    /// Cancel a pending retry for a failed session.
    /// Returns true if a retry was actually cancelled, false if no retry was pending.
    pub async fn cancel_retry(&mut self, session_id: uuid::Uuid) -> Result<bool> {
        let response = self
            .request(
                "CancelRetry",
                serde_json::json!({ "session_id": session_id }),
            )
            .await?;
        Ok(response
            .get("cancelled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false))
    }

    /// Delete a session from the daemon.
    pub async fn delete_session(&mut self, session_id: uuid::Uuid) -> Result<()> {
        self.request(
            "DeleteSession",
            serde_json::json!({ "session_id": session_id }),
        )
        .await?;
        Ok(())
    }

    /// Archive a session (soft delete — hides from list, preserves data).
    pub async fn archive_session(
        &mut self,
        session_id: uuid::Uuid,
    ) -> Result<ArchiveSessionResultV1> {
        let value = self
            .request(
                "ArchiveSession",
                serde_json::json!({ "session_id": session_id }),
            )
            .await?;
        let result: ArchiveSessionResultV1 = serde_json::from_value(value)?;
        result.validate_wire().map_err(ClientError::Protocol)?;
        Ok(result)
    }

    pub async fn get_archive_cleanup_status(
        &mut self,
        session_id: uuid::Uuid,
    ) -> Result<ArchiveCleanupStatusV1> {
        let value = self
            .request(
                "GetArchiveCleanupStatus",
                serde_json::json!({ "session_id": session_id }),
            )
            .await?;
        let status: ArchiveCleanupStatusV1 = serde_json::from_value(value)?;
        status.validate_wire().map_err(ClientError::Protocol)?;
        Ok(status)
    }

    /// Set or clear an active session's auto-archive-on-completion flag.
    pub async fn mark_pending_archive(
        &mut self,
        session_id: uuid::Uuid,
        pending: bool,
    ) -> Result<()> {
        self.request(
            "MarkPendingArchive",
            serde_json::json!({ "session_id": session_id, "pending": pending }),
        )
        .await?;
        Ok(())
    }

    /// Toggle pin state of a session. Returns the new pinned_at value (None = unpinned).
    pub async fn toggle_pin(&mut self, session_id: uuid::Uuid) -> Result<Option<String>> {
        let result = self
            .request("TogglePin", serde_json::json!({ "session_id": session_id }))
            .await?;
        Ok(result
            .get("pinned_at")
            .and_then(|v| v.as_str())
            .map(String::from))
    }

    /// Toggle "testing needed" state of a session. Returns the new testing_needed_at value.
    pub async fn toggle_testing_needed(
        &mut self,
        session_id: uuid::Uuid,
    ) -> Result<Option<String>> {
        let result = self
            .request(
                "ToggleTestingNeeded",
                serde_json::json!({ "session_id": session_id }),
            )
            .await?;
        Ok(result
            .get("testing_needed_at")
            .and_then(|v| v.as_str())
            .map(String::from))
    }

    /// Toggle auto-rotation disabled state. Returns the new rotation_disabled_at value.
    pub async fn toggle_rotation_disabled(
        &mut self,
        session_id: uuid::Uuid,
    ) -> Result<Option<String>> {
        let result = self
            .request(
                "ToggleRotationDisabled",
                serde_json::json!({ "session_id": session_id }),
            )
            .await?;
        Ok(result
            .get("rotation_disabled_at")
            .and_then(|v| v.as_str())
            .map(String::from))
    }

    /// Update a session's project assignment.
    pub async fn update_session_project(
        &mut self,
        session_id: uuid::Uuid,
        project_id: Option<uuid::Uuid>,
    ) -> Result<()> {
        self.request(
            "UpdateSessionProject",
            serde_json::json!({
                "session_id": session_id,
                "project_id": project_id,
            }),
        )
        .await?;
        Ok(())
    }

    /// Update a session's workflow assignment.
    pub async fn update_session_workflow(
        &mut self,
        session_id: uuid::Uuid,
        workflow_id: Option<uuid::Uuid>,
    ) -> Result<()> {
        self.request(
            "UpdateSessionWorkflow",
            serde_json::json!({
                "session_id": session_id,
                "workflow_id": workflow_id,
            }),
        )
        .await?;
        Ok(())
    }

    /// Update a session's rating on the 1–10 scale. `None` clears it.
    pub async fn update_session_rating(
        &mut self,
        session_id: uuid::Uuid,
        rating: Option<i16>,
    ) -> Result<()> {
        self.request(
            "UpdateSessionRating",
            serde_json::json!({
                "session_id": session_id,
                "rating": rating,
            }),
        )
        .await?;
        Ok(())
    }

    /// Update a session's title.
    pub async fn update_session_title(
        &mut self,
        session_id: uuid::Uuid,
        title: &str,
    ) -> Result<()> {
        self.request(
            "UpdateSessionTitle",
            serde_json::json!({
                "session_id": session_id,
                "title": title,
            }),
        )
        .await?;
        Ok(())
    }

    /// Update a session's description.
    pub async fn update_session_description(
        &mut self,
        session_id: uuid::Uuid,
        description: &str,
    ) -> Result<()> {
        self.request(
            "UpdateSessionDescription",
            serde_json::json!({
                "session_id": session_id,
                "description": description,
            }),
        )
        .await?;
        Ok(())
    }

    /// Update a session's active task context.
    pub async fn update_active_task(
        &mut self,
        session_id: uuid::Uuid,
        active_task: Option<&str>,
    ) -> Result<()> {
        self.request(
            "UpdateActiveTask",
            serde_json::json!({
                "session_id": session_id,
                "active_task": active_task,
            }),
        )
        .await?;
        Ok(())
    }

    /// Trigger context rotation on a running session (fire-and-forget).
    pub async fn rotate_session(&mut self, session_id: uuid::Uuid) -> Result<()> {
        self.fire_and_forget(
            "RotateSession",
            serde_json::json!({ "session_id": session_id }),
        )
        .await
    }

    /// List archived sessions, optionally filtered by project.
    pub async fn list_archived_sessions(
        &mut self,
        project_id: Option<uuid::Uuid>,
    ) -> Result<Vec<Session>> {
        let params = match project_id {
            Some(pid) => serde_json::json!({ "project_id": pid }),
            None => serde_json::json!({}),
        };
        let result = self.request("ListArchivedSessions", params).await?;
        let sessions: Vec<Session> = serde_json::from_value(result)?;
        Ok(sessions)
    }

    /// Unarchive a session (restore to Completed status).
    pub async fn unarchive_session(&mut self, session_id: uuid::Uuid) -> Result<Session> {
        let result = self
            .request(
                "UnarchiveSession",
                serde_json::json!({ "session_id": session_id }),
            )
            .await?;
        let session: Session = serde_json::from_value(result)?;
        Ok(session)
    }

    /// List deleted (trash) sessions, optionally filtered by project.
    pub async fn list_deleted_sessions(
        &mut self,
        project_id: Option<uuid::Uuid>,
    ) -> Result<Vec<Session>> {
        let params = match project_id {
            Some(pid) => serde_json::json!({ "project_id": pid }),
            None => serde_json::json!({}),
        };
        let result = self.request("ListDeletedSessions", params).await?;
        let sessions: Vec<Session> = serde_json::from_value(result)?;
        Ok(sessions)
    }

    /// Restore a session from trash (set status back to Completed).
    pub async fn undelete_session(&mut self, session_id: uuid::Uuid) -> Result<Session> {
        let result = self
            .request(
                "UndeleteSession",
                serde_json::json!({ "session_id": session_id }),
            )
            .await?;
        let session: Session = serde_json::from_value(result)?;
        Ok(session)
    }

    /// Permanently purge a session from trash (hard delete).
    pub async fn purge_session(&mut self, session_id: uuid::Uuid) -> Result<()> {
        self.request(
            "PurgeSession",
            serde_json::json!({ "session_id": session_id }),
        )
        .await?;
        Ok(())
    }

    pub async fn answer_question(
        &mut self,
        session_id: uuid::Uuid,
        response_text: String,
    ) -> Result<()> {
        let params = serde_json::json!({
            "session_id": session_id,
            "response_text": response_text,
        });
        self.fire_and_forget("AnswerQuestion", params).await
    }

    /// Continue a completed/interrupted session with a follow-up query.
    /// Uses request/response so daemon errors surface to the caller.
    pub async fn continue_session(&mut self, session_id: uuid::Uuid, query: &str) -> Result<()> {
        self.request(
            "ContinueSession",
            serde_json::json!({
                "session_id": session_id,
                "query": query,
            }),
        )
        .await?;
        Ok(())
    }

    /// Get model segments for a session (for rendering switch dividers).
    pub async fn get_model_segments(
        &mut self,
        session_id: uuid::Uuid,
    ) -> Result<Vec<rsi_common::types::ModelSegment>> {
        let result = self
            .request(
                "GetModelSegments",
                serde_json::json!({ "session_id": session_id }),
            )
            .await?;
        let segments: Vec<rsi_common::types::ModelSegment> = serde_json::from_value(result)?;
        Ok(segments)
    }

    /// Get the lifetime usage aggregate for the Settings -> Stats category,
    /// optionally scoped to a single project (tab-scoped filter, D1).
    ///
    /// # Errors
    /// Returns an error if the daemon RPC call fails or the response body
    /// doesn't deserialize into `UsageStats`.
    pub async fn get_usage_stats(
        &mut self,
        project_id: Option<uuid::Uuid>,
    ) -> Result<rsi_common::types::UsageStats> {
        let result = self
            .request(
                "GetUsageStats",
                serde_json::json!({ "project_id": project_id }),
            )
            .await?;
        let stats: rsi_common::types::UsageStats = serde_json::from_value(result)?;
        Ok(stats)
    }

    pub async fn get_model_control_status(
        &mut self,
        recent_limit: Option<u32>,
    ) -> Result<rsi_common::model_control::ModelControlStatusReport> {
        let result = self
            .request(
                "GetModelControlStatus",
                serde_json::json!({ "recent_limit": recent_limit.unwrap_or(8) }),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn update_model_control_policy(
        &mut self,
        mode: rsi_common::model_control::ModelControlMode,
        interrupt_active: bool,
    ) -> Result<rsi_common::model_control::ModelControlPolicyUpdateReport> {
        let result = self
            .request(
                "UpdateModelControlPolicy",
                serde_json::json!({
                    "mode": mode,
                    "interrupt_active": interrupt_active
                }),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Sibling wrapper around the SAME `UpdateModelControlPolicy` daemon RPC
    /// as `update_model_control_policy`, but replaces the full budget-policy
    /// list instead of touching the breaker mode. Callers pass the CURRENT
    /// mode through unchanged (read from
    /// `app.cached_model_control_status.mode`) so this call has no side
    /// effect on the breaker mode — only on policies. `interrupt_active` is
    /// always `false` here: a budget-policy edit is not an emergency-stop
    /// action and must never interrupt running sessions as a side effect.
    pub async fn update_model_budget_policies(
        &mut self,
        mode: rsi_common::model_control::ModelControlMode,
        policies: Vec<rsi_common::model_control::ModelBudgetPolicy>,
    ) -> Result<rsi_common::model_control::ModelControlPolicyUpdateReport> {
        let result = self
            .request(
                "UpdateModelControlPolicy",
                serde_json::json!({
                    "mode": mode,
                    "interrupt_active": false,
                    "replace_policies": true,
                    "policies": policies
                }),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn list_model_invocations(
        &mut self,
        limit: u32,
        active_only: bool,
    ) -> Result<rsi_common::model_control::ModelInvocationList> {
        let result = self
            .request(
                "ListModelInvocations",
                serde_json::json!({
                    "limit": limit,
                    "active_only": active_only
                }),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn cancel_model_invocation(
        &mut self,
        invocation_id: uuid::Uuid,
    ) -> Result<rsi_common::model_control::CancelModelInvocationReport> {
        let result = self
            .request(
                "CancelModelInvocation",
                serde_json::json!({ "invocation_id": invocation_id }),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Get conversation events for a session.
    /// If `since_sequence` is Some, only events with sequence > that value are returned.
    pub async fn get_conversation(
        &mut self,
        session_id: uuid::Uuid,
        since_sequence: Option<i32>,
    ) -> Result<Vec<rsi_common::types::ConversationEvent>> {
        let mut params = serde_json::json!({ "session_id": session_id });
        if let Some(seq) = since_sequence {
            params["since_sequence"] = serde_json::json!(seq);
        }
        let result = self.request("GetConversation", params).await?;
        let events: Vec<rsi_common::types::ConversationEvent> = serde_json::from_value(result)?;
        Ok(events)
    }

    /// Get turn metrics for a session.
    pub async fn get_turn_metrics(
        &mut self,
        session_id: uuid::Uuid,
    ) -> Result<Vec<rsi_common::types::TurnMetric>> {
        let result = self
            .request(
                "GetTurnMetrics",
                serde_json::json!({ "session_id": session_id }),
            )
            .await?;
        let metrics: Vec<rsi_common::types::TurnMetric> = serde_json::from_value(result)?;
        Ok(metrics)
    }

    /// Fetch multiple conversations in a single RPC call.
    pub async fn get_conversations_batch(
        &mut self,
        cursors: Vec<ConversationFetchCursor>,
    ) -> Result<Vec<(uuid::Uuid, Vec<rsi_common::types::ConversationEvent>)>> {
        if cursors.is_empty() {
            return Ok(Vec::new());
        }
        let params = serde_json::json!({
            "requests": cursors,
        });
        let result = self.request("GetConversationsSince", params).await?;
        let response: ConversationBatchResponse = serde_json::from_value(result)?;
        Ok(response
            .conversations
            .into_iter()
            .map(|entry| (entry.session_id, entry.events))
            .collect())
    }

    // --- Project operations ---

    /// List all projects.
    pub async fn list_projects(&mut self) -> Result<Vec<Project>> {
        let result = self.request("ListProjects", Value::Null).await?;
        let projects: Vec<Project> = serde_json::from_value(result)?;
        Ok(projects)
    }

    /// List workflows, optionally filtered by project.
    pub async fn list_workflows(
        &mut self,
        project_id: Option<uuid::Uuid>,
    ) -> Result<Vec<rsi_common::types::Workflow>> {
        let params = match project_id {
            Some(pid) => serde_json::json!({ "project_id": pid.to_string() }),
            None => serde_json::json!({}),
        };
        let result = self.request("ListWorkflows", params).await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Load workflow metadata plus the stored definition payload.
    pub async fn get_workflow_definition(
        &mut self,
        workflow_id: uuid::Uuid,
    ) -> Result<rsi_common::types::WorkflowDocument> {
        let result = self
            .request(
                "GetWorkflowDefinition",
                serde_json::json!({ "workflow_id": workflow_id }),
            )
            .await?;
        let response: rsi_common::rpc::GetWorkflowDefinitionResponse =
            serde_json::from_value(result)?;
        Ok(response.document)
    }

    /// Insert or update workflow metadata plus definition in one RPC.
    pub async fn upsert_workflow_definition(
        &mut self,
        document: &rsi_common::types::WorkflowDocument,
    ) -> Result<rsi_common::types::WorkflowDocument> {
        let result = self
            .request(
                "UpsertWorkflowDefinition",
                serde_json::json!({ "document": document }),
            )
            .await?;
        let response: rsi_common::rpc::UpsertWorkflowDefinitionResponse =
            serde_json::from_value(result)?;
        Ok(response.document)
    }

    /// Start workflow execution and return the execution acknowledgment.
    ///
    /// `parent_id` (P1.12): when `Some`, spawned executor sessions are placed
    /// under the given container session (Group or Epic). When `None`,
    /// sessions spawn as top-level orphans (legacy behavior).
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_workflow(
        &mut self,
        workflow_id: uuid::Uuid,
        workflow: &serde_json::Value,
        input: Option<&serde_json::Value>,
        dry_run: bool,
        project_id: Option<uuid::Uuid>,
        working_dir: Option<&str>,
        parent_id: Option<uuid::Uuid>,
    ) -> Result<rsi_common::rpc::ExecuteWorkflowResponse> {
        let result = self
            .request(
                "ExecuteWorkflow",
                serde_json::json!({
                    "workflow_id": workflow_id,
                    "workflow": workflow,
                    "input": input,
                    "dry_run": dry_run,
                    "project_id": project_id,
                    "working_dir": working_dir,
                    "parent_id": parent_id,
                }),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Execute a stored topology and return the execution id (P1.10).
    ///
    /// Bridges the stored topology to a `WorkflowDefinition` via
    /// `topology_bridge` in the daemon and dispatches it to `graph_runner`.
    /// Acyclic topologies only — loop-bearing topologies return an error until
    /// P1.11 ships the loop-aware executor.
    ///
    /// Returns `execution_id` — poll via `get_workflow_execution`.
    pub async fn execute_topology(
        &mut self,
        topology_id: uuid::Uuid,
        project_id: Option<uuid::Uuid>,
        inputs: serde_json::Value,
        parent_id: Option<uuid::Uuid>,
    ) -> Result<uuid::Uuid> {
        let result = self
            .request(
                "ExecuteTopology",
                serde_json::json!({
                    "topology_id": topology_id,
                    "project_id": project_id,
                    "inputs": inputs,
                    "parent_id": parent_id,
                }),
            )
            .await?;
        let execution_id: uuid::Uuid = serde_json::from_value(result["execution_id"].clone())?;
        Ok(execution_id)
    }

    /// Fetch the latest snapshot for a workflow execution.
    pub async fn get_workflow_execution(
        &mut self,
        execution_id: uuid::Uuid,
    ) -> Result<rsi_common::types::WorkflowExecutionLookup> {
        let result = self
            .request(
                "GetWorkflowExecution",
                serde_json::json!({ "execution_id": execution_id }),
            )
            .await?;
        let response: rsi_common::rpc::GetWorkflowExecutionResponse =
            serde_json::from_value(result)?;
        Ok(response.lookup)
    }

    /// Request interruption for a workflow execution.
    pub async fn interrupt_workflow_execution(
        &mut self,
        execution_id: uuid::Uuid,
    ) -> Result<rsi_common::rpc::InterruptWorkflowExecutionResponse> {
        let result = self
            .request(
                "InterruptWorkflowExecution",
                serde_json::json!({ "execution_id": execution_id }),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Operator-only resolution of preserved topology work (#634).
    ///
    /// # Errors
    ///
    /// Transport failures, or the daemon's typed resolution refusal
    /// (`stale_row_version`, `preserved_commit_mismatch`, ...).
    pub async fn resolve_topology_attempt(
        &mut self,
        params: &rsi_common::rpc::ResolveTopologyAttemptParams,
    ) -> Result<rsi_common::rpc::ResolveTopologyAttemptResponse> {
        let result = self
            .request("ResolveTopologyAttempt", serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Fetch daemon health + provider availability info.
    pub async fn get_health_status(&mut self) -> Result<HealthStatusResponse> {
        let result = self.request("GetHealthStatus", Value::Null).await?;
        let status: HealthStatusResponse = serde_json::from_value(result)?;
        Ok(status)
    }

    /// Fetch daemon protocol capabilities (feature flags for incremental polling, batch fetch, etc.).
    pub async fn get_daemon_capabilities(&mut self) -> Result<DaemonCapabilities> {
        let result = self.request("GetDaemonCapabilities", Value::Null).await?;
        let caps: DaemonCapabilities = serde_json::from_value(result)?;
        Ok(caps)
    }

    // --- Recursive DAG read-only methods ---

    pub async fn list_recursive_task_graphs(
        &mut self,
        params: ListRecursiveTaskGraphsParams,
    ) -> Result<Vec<RecursiveTaskGraphSummary>> {
        let result = self
            .request("ListRecursiveTaskGraphs", serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Fetch a recursive task graph bridged (read-only) into a serialized
    /// `WorkflowDefinition`. Gated server-side on `gv_render_recursive_origin`.
    pub async fn get_recursive_graph_as_workflow(
        &mut self,
        graph_id: uuid::Uuid,
    ) -> Result<GetRecursiveGraphAsWorkflowResponse> {
        let params = GetRecursiveGraphAsWorkflowParams { graph_id };
        let result = self
            .request("GetRecursiveGraphAsWorkflow", serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn edit_recursive_node_instructions(
        &mut self,
        graph_id: uuid::Uuid,
        task_id: uuid::Uuid,
        instructions: String,
    ) -> Result<RecursiveTaskNode> {
        let params = EditRecursiveNodeInstructionsParams {
            graph_id,
            task_id,
            instructions,
        };
        let result = self
            .request(
                "EditRecursiveNodeInstructions",
                serde_json::to_value(params)?,
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn edit_recursive_node_settings(
        &mut self,
        graph_id: uuid::Uuid,
        task_id: uuid::Uuid,
        integration_strategy: Option<String>,
        verification_strategy: Option<String>,
    ) -> Result<RecursiveTaskNode> {
        let params = EditRecursiveNodeSettingsParams {
            graph_id,
            task_id,
            integration_strategy,
            verification_strategy,
        };
        let result = self
            .request("EditRecursiveNodeSettings", serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn get_recursive_task_graph(
        &mut self,
        graph_id: RecursiveTaskGraphId,
    ) -> Result<RecursiveTaskGraphDetail> {
        let result = self
            .request("GetRecursiveTaskGraph", recursive_graph_id_params(graph_id))
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn list_recursive_tasks(
        &mut self,
        params: ListRecursiveTasksParams,
    ) -> Result<Vec<RecursiveTaskNode>> {
        let result = self
            .request("ListRecursiveTasks", serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn get_recursive_task(
        &mut self,
        graph_id: RecursiveTaskGraphId,
        task_id: RecursiveTaskId,
    ) -> Result<RecursiveTaskNode> {
        let result = self
            .request(
                "GetRecursiveTask",
                recursive_task_id_params(graph_id, task_id),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn list_recursive_task_attempts(
        &mut self,
        params: ListRecursiveTaskAttemptsParams,
    ) -> Result<Vec<RecursiveTaskAttempt>> {
        let result = self
            .request("ListRecursiveTaskAttempts", serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn list_recursive_lifecycle_events(
        &mut self,
        params: ListRecursiveLifecycleEventsParams,
    ) -> Result<Vec<RecursiveLifecycleEvent>> {
        let result = self
            .request(
                "ListRecursiveLifecycleEvents",
                serde_json::to_value(params)?,
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn list_recursive_execution_artifacts(
        &mut self,
        params: ListRecursiveExecutionArtifactsParams,
    ) -> Result<Vec<RecursiveExecutionArtifact>> {
        let result = self
            .request(
                "ListRecursiveExecutionArtifacts",
                serde_json::to_value(params)?,
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn get_recursive_execution_artifact(
        &mut self,
        params: GetRecursiveExecutionArtifactParams,
    ) -> Result<RecursiveExecutionArtifactReadback> {
        let result = self
            .request(
                "GetRecursiveExecutionArtifact",
                serde_json::to_value(params)?,
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn preview_recursive_execution_artifact(
        &mut self,
        params: PreviewRecursiveExecutionArtifactParams,
    ) -> Result<RecursiveExecutionArtifactPreview> {
        let result = self
            .request(
                "PreviewRecursiveExecutionArtifact",
                serde_json::to_value(params)?,
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn list_recursive_execution_artifact_summaries(
        &mut self,
        params: ListRecursiveExecutionArtifactSummariesParams,
    ) -> Result<RecursiveReadPage<RecursiveExecutionArtifactSummary>> {
        let result = self
            .request(
                "ListRecursiveExecutionArtifactSummaries",
                serde_json::to_value(params)?,
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn list_recursive_scheduler_runs(
        &mut self,
        params: ListRecursiveSchedulerRunsParams,
    ) -> Result<Vec<RecursiveSchedulerRunSummary>> {
        let result = self
            .request("ListRecursiveSchedulerRuns", serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn get_recursive_scheduler_run(
        &mut self,
        run_id: RecursiveSchedulerRunId,
    ) -> Result<RecursiveSchedulerRunDetail> {
        let result = self
            .request(
                "GetRecursiveSchedulerRun",
                recursive_scheduler_run_id_params(run_id),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn list_recursive_scheduler_run_events(
        &mut self,
        params: ListRecursiveSchedulerRunEventsParams,
    ) -> Result<Vec<RecursiveSchedulerRunEvent>> {
        let result = self
            .request(
                "ListRecursiveSchedulerRunEvents",
                serde_json::to_value(params)?,
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn run_recursive_fake_scheduler(
        &mut self,
        graph_id: RecursiveTaskGraphId,
        max_steps: u32,
    ) -> Result<RecursiveSchedulerRunSummary> {
        let result = self
            .request(
                "RunRecursiveFakeScheduler",
                recursive_fake_scheduler_params(graph_id, max_steps),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn run_recursive_live_scheduler(
        &mut self,
        graph_id: RecursiveTaskGraphId,
        max_steps: u32,
    ) -> Result<RunRecursiveLiveSchedulerResponse> {
        let result = self
            .request(
                "RunRecursiveLiveScheduler",
                recursive_live_scheduler_params(graph_id, max_steps),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn request_recursive_graph_cancellation(
        &mut self,
        graph_id: RecursiveTaskGraphId,
        reason: String,
        requested_by: Option<String>,
    ) -> Result<RecursiveCancellationRequestSummary> {
        let result = self
            .request(
                "RequestRecursiveGraphCancellation",
                recursive_graph_cancellation_params(graph_id, reason, requested_by),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn request_recursive_scheduler_run_cancellation(
        &mut self,
        run_id: RecursiveSchedulerRunId,
        reason: String,
        requested_by: Option<String>,
    ) -> Result<RecursiveCancellationRequestSummary> {
        let result = self
            .request(
                "RequestRecursiveSchedulerRunCancellation",
                recursive_scheduler_run_cancellation_params(run_id, reason, requested_by),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn continue_recursive_recovery(
        &mut self,
        max_graphs: u32,
        time_budget_ms: Option<u64>,
    ) -> Result<RecursiveRecoveryPassSummary> {
        let result = self
            .request(
                "ContinueRecursiveRecovery",
                recursive_recovery_continuation_params(max_graphs, time_budget_ms),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn get_recursive_dag_operational_status(
        &mut self,
        graph_id: RecursiveTaskGraphId,
    ) -> Result<RecursiveDagOperationalStatus> {
        let result = self
            .request(
                "GetRecursiveDagOperationalStatus",
                recursive_graph_id_params(graph_id),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn list_recursive_cancellation_requests(
        &mut self,
        params: ListRecursiveCancellationRequestsParams,
    ) -> Result<Vec<RecursiveCancellationRequestSummary>> {
        let result = self
            .request(
                "ListRecursiveCancellationRequests",
                serde_json::to_value(params)?,
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn get_recursive_cancellation_request(
        &mut self,
        request_id: RecursiveCancellationRequestId,
    ) -> Result<RecursiveCancellationRequestSummary> {
        let result = self
            .request(
                "GetRecursiveCancellationRequest",
                recursive_cancellation_request_id_params(request_id),
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn get_recursive_recovery_status(
        &mut self,
        params: GetRecursiveRecoveryStatusParams,
    ) -> Result<RecursiveDagRecoveryStatus> {
        let result = self
            .request("GetRecursiveRecoveryStatus", serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn list_recursive_deferred_recovery_graphs(
        &mut self,
    ) -> Result<Vec<RecursiveDeferredRecoveryGraph>> {
        let result = self
            .request("ListRecursiveDeferredRecoveryGraphs", Value::Null)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn get_recursive_live_attempt(
        &mut self,
        params: GetRecursiveLiveAttemptParams,
    ) -> Result<RecursiveLiveAttemptReadback> {
        let result = self
            .request("GetRecursiveLiveAttempt", serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn list_recursive_live_attempts(
        &mut self,
        params: ListRecursiveLiveAttemptsParams,
    ) -> Result<Vec<RecursiveLiveAttemptListItem>> {
        let result = self
            .request("ListRecursiveLiveAttempts", serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn commit_recursive_live_attempt_output(
        &mut self,
        params: CommitRecursiveLiveAttemptOutputParams,
    ) -> Result<CommitRecursiveLiveAttemptOutputResponse> {
        let result = self
            .request(
                "CommitRecursiveLiveAttemptOutput",
                serde_json::to_value(params)?,
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn get_recursive_live_attempt_heartbeat_status(
        &mut self,
        live_attempt_id: RecursiveLiveAttemptId,
    ) -> Result<RecursiveLiveAttemptHeartbeatState> {
        let params = GetRecursiveLiveAttemptHeartbeatStatusParams { live_attempt_id };
        let result = self
            .request(
                "GetRecursiveLiveAttemptHeartbeatStatus",
                serde_json::to_value(params)?,
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn list_stale_recursive_live_attempt_heartbeats(
        &mut self,
        params: ListStaleRecursiveLiveAttemptHeartbeatsParams,
    ) -> Result<Vec<RecursiveLiveAttemptHeartbeatState>> {
        let result = self
            .request(
                "ListStaleRecursiveLiveAttemptHeartbeats",
                serde_json::to_value(params)?,
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn get_recursive_live_interrupt_status(
        &mut self,
        params: GetRecursiveLiveInterruptStatusParams,
    ) -> Result<Option<RecursiveLiveInterruptSummary>> {
        let result = self
            .request(
                "GetRecursiveLiveInterruptStatus",
                serde_json::to_value(params)?,
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn list_recursive_live_interrupts(
        &mut self,
        params: ListRecursiveLiveInterruptsParams,
    ) -> Result<Vec<RecursiveLiveInterruptSummary>> {
        let result = self
            .request("ListRecursiveLiveInterrupts", serde_json::to_value(params)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn get_recursive_live_recovery_status(
        &mut self,
        params: GetRecursiveLiveRecoveryStatusParams,
    ) -> Result<RecursiveLiveRecoveryReadback> {
        let result = self
            .request(
                "GetRecursiveLiveRecoveryStatus",
                serde_json::to_value(params)?,
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn get_recursive_live_output_validation_result(
        &mut self,
        params: GetRecursiveLiveOutputValidationResultParams,
    ) -> Result<Option<RecursiveLiveOutputValidationResult>> {
        let result = self
            .request(
                "GetRecursiveLiveOutputValidationResult",
                serde_json::to_value(params)?,
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn list_recursive_live_output_validation_results(
        &mut self,
        params: ListRecursiveLiveOutputValidationResultsParams,
    ) -> Result<Vec<RecursiveLiveOutputValidationListItem>> {
        let result = self
            .request(
                "ListRecursiveLiveOutputValidationResults",
                serde_json::to_value(params)?,
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn list_recursive_live_validation_issues(
        &mut self,
        params: ListRecursiveLiveValidationIssuesParams,
    ) -> Result<Vec<RecursiveLiveOutputValidationIssue>> {
        let result = self
            .request(
                "ListRecursiveLiveValidationIssues",
                serde_json::to_value(params)?,
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn get_recursive_live_attempt_artifacts(
        &mut self,
        params: GetRecursiveLiveAttemptArtifactsParams,
    ) -> Result<RecursiveLiveAttemptArtifactReadback> {
        let result = self
            .request(
                "GetRecursiveLiveAttemptArtifacts",
                serde_json::to_value(params)?,
            )
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    /// Create a new project.
    pub async fn create_project(
        &mut self,
        name: &str,
        path: Option<&Path>,
        description: Option<&str>,
        color: Option<&str>,
    ) -> Result<Project> {
        let params = serde_json::json!({
            "name": name,
            "path": path,
            "description": description,
            "color": color,
        });
        let result = self.request("CreateProject", params).await?;
        let project: Project = serde_json::from_value(result)?;
        Ok(project)
    }

    /// Update an existing project.
    pub async fn update_project(
        &mut self,
        id: uuid::Uuid,
        name: Option<&str>,
        path: Option<&Path>,
        description: Option<&str>,
        color: Option<&str>,
    ) -> Result<Project> {
        let params = serde_json::json!({
            "id": id,
            "name": name,
            "path": path,
            "description": description,
            "color": color,
        });
        let result = self.request("UpdateProject", params).await?;
        let project: Project = serde_json::from_value(result)?;
        Ok(project)
    }

    /// Delete a project.
    pub async fn delete_project(&mut self, id: uuid::Uuid) -> Result<()> {
        self.request("DeleteProject", serde_json::json!({ "id": id }))
            .await?;
        Ok(())
    }

    /// Get FLYWHEEL.md workflow status for a project.
    /// Returns a JSON object with `exists`, `healthy`, `last_error`, `loaded_at`, `settings`, `template_length`.
    pub async fn get_project_workflow(
        &mut self,
        project_id: uuid::Uuid,
    ) -> Result<serde_json::Value> {
        self.request(
            "GetProjectWorkflow",
            serde_json::json!({ "id": project_id }),
        )
        .await
    }

    /// Force re-read and re-parse of FLYWHEEL.md for a project.
    pub async fn reload_project_workflow(&mut self, project_id: uuid::Uuid) -> Result<()> {
        self.request(
            "ReloadProjectWorkflow",
            serde_json::json!({ "id": project_id }),
        )
        .await?;
        Ok(())
    }

    /// Create a new session label.
    pub async fn create_label(
        &mut self,
        name: &str,
        description: Option<&str>,
        project_id: Option<uuid::Uuid>,
        color: Option<&str>,
    ) -> Result<rsi_common::types::SessionLabel> {
        let params = serde_json::json!({
            "name": name,
            "description": description,
            "project_id": project_id,
            "color": color,
        });
        let result = self.request("CreateLabel", params).await?;
        let label: rsi_common::types::SessionLabel = serde_json::from_value(result)?;
        Ok(label)
    }

    /// Update an existing session label.
    pub async fn update_label(
        &mut self,
        id: uuid::Uuid,
        name: Option<&str>,
        description: Option<&str>,
        color: Option<&str>,
    ) -> Result<rsi_common::types::SessionLabel> {
        let params = serde_json::json!({
            "id": id,
            "name": name,
            "description": description,
            "color": color,
        });
        let result = self.request("UpdateLabel", params).await?;
        let label: rsi_common::types::SessionLabel = serde_json::from_value(result)?;
        Ok(label)
    }

    /// Delete a session label.
    pub async fn delete_label(&mut self, id: uuid::Uuid) -> Result<()> {
        self.request("DeleteLabel", serde_json::json!({ "id": id }))
            .await?;
        Ok(())
    }

    /// List all session labels.
    pub async fn list_labels(&mut self) -> Result<Vec<rsi_common::types::SessionLabel>> {
        let result = self.request("ListLabels", Value::Null).await?;
        let labels: Vec<rsi_common::types::SessionLabel> = serde_json::from_value(result)?;
        Ok(labels)
    }

    /// Update a session's label assignment.
    pub async fn update_session_label(
        &mut self,
        session_id: uuid::Uuid,
        group_id: Option<uuid::Uuid>,
    ) -> Result<()> {
        let params = serde_json::json!({
            "session_id": session_id,
            "group_id": group_id,
        });
        self.fire_and_forget("UpdateSessionLabel", params).await
    }

    /// Create a hierarchy container (Group or Epic). The daemon rejects leaf
    /// kinds and any (parent, kind) pair the containment matrix forbids.
    /// Returns the new container's UUID on success.
    pub async fn create_container(
        &mut self,
        kind: rsi_common::types::SessionKind,
        name: &str,
        parent_id: Option<uuid::Uuid>,
        project_id: Option<uuid::Uuid>,
        tags: &[String],
        topology_id: Option<uuid::Uuid>,
    ) -> Result<uuid::Uuid> {
        let params = serde_json::json!({
            "kind": kind,
            "name": name,
            "parent_id": parent_id,
            "project_id": project_id,
            "tags": tags,
            "topology_id": topology_id,
        });
        let result = self.request("CreateContainer", params).await?;
        let id: uuid::Uuid =
            serde_json::from_value(result.get("id").cloned().ok_or_else(|| ClientError::Rpc {
                code: -32603,
                message: "CreateContainer: missing id in response".to_string(),
                data: None,
            })?)?;
        Ok(id)
    }

    /// Reparent a session in the hierarchy. `new_parent_id = None` moves the
    /// session to the root. The daemon validates containment + cycles.
    pub async fn set_session_parent(
        &mut self,
        session_id: uuid::Uuid,
        new_parent_id: Option<uuid::Uuid>,
    ) -> Result<()> {
        let params = serde_json::json!({
            "session_id": session_id,
            "new_parent_id": new_parent_id,
        });
        self.request("SetSessionParent", params).await?;
        Ok(())
    }

    /// Set the lead session for an Epic container. `new_lead_session_id = None`
    /// clears the lead pointer.
    pub async fn set_epic_lead(
        &mut self,
        epic_id: uuid::Uuid,
        new_lead_session_id: Option<uuid::Uuid>,
    ) -> Result<()> {
        let params = serde_json::json!({
            "epic_id": epic_id,
            "new_lead_session_id": new_lead_session_id,
        });
        self.request("SetEpicLead", params).await?;
        Ok(())
    }

    /// List the direct children of a hierarchy parent. `parent_id = None`
    /// returns top-level sessions.
    pub async fn list_session_children(
        &mut self,
        parent_id: Option<uuid::Uuid>,
    ) -> Result<Vec<Session>> {
        let params = serde_json::json!({ "parent_id": parent_id });
        let result = self.request("ListSessionChildren", params).await?;
        let sessions: Vec<Session> = serde_json::from_value(result)?;
        Ok(sessions)
    }

    // ─── Tag RPC wrappers (P1.5) ───────────────────────────────────────

    /// Replace the full tag set for a session. Tags are normalized and
    /// deduplicated by the daemon. `tags` must be non-empty.
    pub async fn update_session_tags(
        &mut self,
        session_id: uuid::Uuid,
        tags: Vec<String>,
    ) -> Result<()> {
        self.request(
            "UpdateSessionTags",
            serde_json::json!({ "session_id": session_id, "tags": tags }),
        )
        .await?;
        Ok(())
    }

    /// Add a single tag to a session (idempotent).
    pub async fn add_session_tag(&mut self, session_id: uuid::Uuid, tag: &str) -> Result<()> {
        self.request(
            "AddSessionTag",
            serde_json::json!({ "session_id": session_id, "tag": tag }),
        )
        .await?;
        Ok(())
    }

    /// Remove a single tag from a session (idempotent).
    pub async fn remove_session_tag(&mut self, session_id: uuid::Uuid, tag: &str) -> Result<()> {
        self.request(
            "RemoveSessionTag",
            serde_json::json!({ "session_id": session_id, "tag": tag }),
        )
        .await?;
        Ok(())
    }

    /// List tags with session-counts, optionally filtered by prefix and/or
    /// project. Returns up to 100 tags ordered by count descending.
    pub async fn list_tags(
        &mut self,
        prefix: Option<&str>,
        project_id: Option<uuid::Uuid>,
    ) -> Result<Vec<TagWithCount>> {
        let params = serde_json::json!({ "prefix": prefix, "project_id": project_id });
        let result = self.request("ListTags", params).await?;
        let tags: Vec<TagWithCount> = serde_json::from_value(result)?;
        Ok(tags)
    }

    // ─── Topology RPC wrappers (P1.4) ──────────────────────────────────

    /// List all named topology templates, optionally filtered by a
    /// case-sensitive name prefix.
    pub async fn list_topologies(&mut self, name_prefix: Option<String>) -> Result<Vec<Topology>> {
        let params = serde_json::json!({ "name_prefix": name_prefix });
        let result = self.request("ListTopologies", params).await?;
        let topologies: Vec<Topology> = serde_json::from_value(result)?;
        Ok(topologies)
    }

    /// Create a new named topology. The daemon validates DAG well-formedness,
    /// kind legality, name uniqueness, and the MAX_ITERATIONS cap before
    /// insert. Returns the new topology's UUID.
    pub async fn create_topology(
        &mut self,
        name: &str,
        definition: TopologyDefinition,
    ) -> Result<uuid::Uuid> {
        let params = serde_json::json!({
            "name": name,
            "definition": definition,
        });
        let result = self.request("CreateTopology", params).await?;
        let id: uuid::Uuid =
            serde_json::from_value(result.get("id").cloned().ok_or_else(|| ClientError::Rpc {
                code: -32603,
                message: "CreateTopology: missing id in response".to_string(),
                data: None,
            })?)?;
        Ok(id)
    }

    /// Update a topology's name and/or definition. The daemon validates
    /// the new definition (if provided) and rejects rename-to-existing.
    pub async fn update_topology(
        &mut self,
        id: uuid::Uuid,
        name: Option<String>,
        definition: Option<TopologyDefinition>,
    ) -> Result<()> {
        let params = serde_json::json!({
            "id": id,
            "name": name,
            "definition": definition,
        });
        self.request("UpdateTopology", params).await?;
        Ok(())
    }

    /// Delete a topology. The daemon rejects when any Epic still
    /// references the topology via `workflow_id`. Clear references
    /// first; per-session `workflow_id_override` references are NOT
    /// considered (they survive as dangling pointers that resolve to
    /// `None` at read time).
    pub async fn delete_topology(&mut self, id: uuid::Uuid) -> Result<()> {
        let params = serde_json::json!({ "id": id });
        self.request("DeleteTopology", params).await?;
        Ok(())
    }

    /// Fetch a single topology by id. Returns an error if no row matches.
    pub async fn get_topology(&mut self, id: uuid::Uuid) -> Result<Topology> {
        let params = serde_json::json!({ "id": id });
        let result = self.request("GetTopology", params).await?;
        let topology: Topology = serde_json::from_value(result)?;
        Ok(topology)
    }

    /// Discover available models for the selected provider by querying the daemon.
    /// Returns a list of (model_id, display_name) tuples.
    pub async fn discover_models(
        &mut self,
        provider: SessionProvider,
    ) -> Result<Vec<(String, String)>> {
        let result = self
            .request(
                "DiscoverModels",
                serde_json::json!({ "provider": provider }),
            )
            .await?;
        let models: Vec<(String, String)> = serde_json::from_value(result)?;
        Ok(models)
    }

    /// Fetch current memory provider status from daemon.
    pub async fn memory_status(&mut self) -> Result<MemoryProviderStatus> {
        let result = self.request("MemoryStatus", Value::Null).await?;
        let status: MemoryProviderStatus = serde_json::from_value(result)?;
        Ok(status)
    }

    /// Search the memory index, optionally scoped to a project.
    ///
    /// When `project_id` is `Some`, the daemon restricts results to that
    /// project. When `None`, the search is global (matches pre-scoping
    /// behavior for manual TUI search / debug surfaces).
    pub async fn memory_search(
        &mut self,
        query: &str,
        max_results: Option<usize>,
        project_id: Option<uuid::Uuid>,
    ) -> Result<Vec<MemorySearchResult>> {
        let params = serde_json::json!({
            "query": query,
            "max_results": max_results,
            "project_id": project_id,
        });
        let result = self.request("MemorySearch", params).await?;
        let results: Vec<MemorySearchResult> = serde_json::from_value(result)?;
        Ok(results)
    }

    /// Save a completed ESP game (fire-and-forget).
    pub async fn save_esp_game(
        &mut self,
        score: u8,
        rounds_played: u8,
        total_rounds: u8,
        p_value: f64,
        round_details: &str,
    ) -> Result<()> {
        let params = serde_json::json!({
            "score": score,
            "rounds_played": rounds_played,
            "total_rounds": total_rounds,
            "p_value": p_value,
            "round_details": round_details,
        });
        self.fire_and_forget("SaveEspGame", params).await
    }

    /// List saved ESP games.
    pub async fn list_esp_games(
        &mut self,
        limit: usize,
    ) -> Result<Vec<rsi_common::types::EspGame>> {
        let params = serde_json::json!({ "limit": limit });
        let result = self.request("ListEspGames", params).await?;
        Ok(serde_json::from_value(result)?)
    }

    // --- Compiled Prompts ---

    pub async fn save_compiled_prompt(
        &mut self,
        params: rsi_common::rpc::SaveCompiledPromptParams,
    ) -> Result<()> {
        self.fire_and_forget("SaveCompiledPrompt", serde_json::to_value(params)?)
            .await
    }

    // --- Entity Cards ---

    /// Get an entity card by type and ID.
    pub async fn get_entity_card(
        &mut self,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<Option<rsi_common::types::EntityCard>> {
        let params = serde_json::json!({
            "entity_type": entity_type,
            "entity_id": entity_id,
        });
        let result = self.request("GetEntityCard", params).await?;
        let card: Option<rsi_common::types::EntityCard> = serde_json::from_value(result)?;
        Ok(card)
    }

    /// Set (replace) an entity card's facts.
    pub async fn set_entity_card(
        &mut self,
        entity_type: &str,
        entity_id: &str,
        facts: Vec<String>,
    ) -> Result<rsi_common::types::EntityCard> {
        let params = serde_json::json!({
            "entity_type": entity_type,
            "entity_id": entity_id,
            "facts": facts,
        });
        let result = self.request("SetEntityCard", params).await?;
        let card: rsi_common::types::EntityCard = serde_json::from_value(result)?;
        Ok(card)
    }

    // --- Issue workspace (operator-only generic RPCs) ---

    pub async fn create_issue_v2(
        &mut self,
        request: CreateIssueV2RequestV1,
    ) -> Result<OperatorIssueMutationResultV1> {
        let value = self
            .request("CreateIssueV2", serde_json::to_value(request)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    pub async fn get_issue_in_project(
        &mut self,
        request: GetIssueInProjectRequestV1,
    ) -> Result<Option<rsi_common::issue_workspace::IssueWorkspaceRowV1>> {
        let value = self
            .request("GetIssueInProject", serde_json::to_value(request)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    pub async fn list_issues_page(
        &mut self,
        request: ListIssuesPageRequestV1,
    ) -> Result<IssueWorkspacePageV1> {
        let value = self
            .request("ListIssuesPage", serde_json::to_value(request)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    pub async fn update_issue(
        &mut self,
        request: UpdateIssueRequestV1,
    ) -> Result<OperatorIssueMutationResultV1> {
        let value = self
            .request("UpdateIssue", serde_json::to_value(request)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    pub async fn update_issue_status_v2(
        &mut self,
        request: UpdateIssueStatusV2RequestV1,
    ) -> Result<OperatorIssueMutationResultV1> {
        let value = self
            .request("UpdateIssueStatusV2", serde_json::to_value(request)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    pub async fn list_issue_dependencies(
        &mut self,
        request: ListIssueDependenciesRequestV1,
    ) -> Result<IssueDependencyPageV1> {
        let value = self
            .request("ListIssueDependencies", serde_json::to_value(request)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    pub async fn add_issue_dependency(
        &mut self,
        request: IssueDependencyMutationRequestV1,
    ) -> Result<IssueDependencyMutationResultV1> {
        let value = self
            .request("AddIssueDependency", serde_json::to_value(request)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    pub async fn remove_issue_dependency(
        &mut self,
        request: IssueDependencyMutationRequestV1,
    ) -> Result<IssueDependencyMutationResultV1> {
        let value = self
            .request("RemoveIssueDependency", serde_json::to_value(request)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    pub async fn list_issue_events_v2(
        &mut self,
        request: ListIssueEventsV2RequestV1,
    ) -> Result<ListIssueEventsV2ResultV1> {
        let value = self
            .request("ListIssueEventsV2", serde_json::to_value(request)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    pub async fn archive_issue(
        &mut self,
        request: ArchiveIssueRequestV1,
    ) -> Result<OperatorIssueMutationResultV1> {
        let value = self
            .request("ArchiveIssue", serde_json::to_value(request)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    pub async fn restore_issue(
        &mut self,
        request: RestoreIssueRequestV1,
    ) -> Result<OperatorIssueMutationResultV1> {
        let value = self
            .request("RestoreIssue", serde_json::to_value(request)?)
            .await?;
        Ok(serde_json::from_value(value)?)
    }

    // --- External Issue tracker ---

    /// Get issue tracker polling status.
    pub async fn get_issue_tracker_status(&mut self) -> Result<IssueTrackerStatusV1> {
        let value = self.request("GetIssueTrackerStatus", Value::Null).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// List dispatched issues.
    pub async fn list_dispatched_issues(&mut self) -> Result<Vec<IssueDispatchRecordV1>> {
        let value = self.request("ListDispatchedIssues", Value::Null).await?;
        Ok(serde_json::from_value(value)?)
    }

    // --- Daemon Config ---

    /// Fetch daemon runtime configuration (feature flags).
    pub async fn get_daemon_config(&mut self) -> Result<Value> {
        self.request("GetDaemonConfig", serde_json::json!({})).await
    }

    /// Update a single daemon config field by name.
    pub async fn update_daemon_config(&mut self, field: &str, value: Value) -> Result<()> {
        self.request(
            "UpdateDaemonConfig",
            serde_json::json!({ "field": field, "value": value }),
        )
        .await?;
        Ok(())
    }

    /// Fetch one authenticated dry-run report for sandbox target-cache
    /// maintenance. Operator/TUI-only; session-attributed callers are denied
    /// by rsid before dispatch.
    pub async fn get_sandbox_storage_status(
        &mut self,
    ) -> Result<SandboxBuildCacheReclaimReportWire> {
        let value = self
            .request("GetSandboxStorageStatus", serde_json::json!({}))
            .await?;
        decode_sandbox_storage_report(value)
    }

    /// Run one bounded sandbox target-cache pass. `dry_run=true` traverses the
    /// same custody boundary without deleting cache data.
    pub async fn run_sandbox_build_cache_reclaim(
        &mut self,
        dry_run: bool,
    ) -> Result<SandboxBuildCacheReclaimReportWire> {
        let value = self
            .request(
                "RunSandboxBuildCacheReclaim",
                serde_json::json!({ "dry_run": dry_run }),
            )
            .await?;
        decode_sandbox_storage_report(value)
    }

    pub async fn list_source_worktree_cohorts(
        &mut self,
    ) -> Result<Vec<SourceWorktreeCohortSummaryV1>> {
        let value = self
            .request(
                "ListSourceWorktreeCohorts",
                serde_json::to_value(ListSourceWorktreeCohortsParams::default())?,
            )
            .await?;
        decode_source_worktree_cohorts(value)
    }

    pub async fn audit_source_worktree_cohort(
        &mut self,
        repository_identity: String,
    ) -> Result<SourceWorktreeCohortAuditV1> {
        let value = self
            .request(
                "AuditSourceWorktreeCohort",
                serde_json::to_value(AuditSourceWorktreeCohortParams {
                    repository_identity,
                })?,
            )
            .await?;
        decode_source_worktree_audit(value)
    }

    pub async fn apply_source_worktree_cohort(
        &mut self,
        params: ApplySourceWorktreeCohortParams,
    ) -> Result<SourceWorktreeSettlementRunV1> {
        let value = self
            .request("ApplySourceWorktreeCohort", serde_json::to_value(params)?)
            .await?;
        decode_source_worktree_run(value)
    }

    pub async fn get_source_worktree_settlement_run(
        &mut self,
        run_id: uuid::Uuid,
    ) -> Result<Option<SourceWorktreeSettlementRunV1>> {
        let value = self
            .request(
                "GetSourceWorktreeSettlementRun",
                serde_json::to_value(GetSourceWorktreeSettlementRunParams { run_id })?,
            )
            .await?;
        serde_json::from_value::<Option<SourceWorktreeSettlementRunV1>>(value)?
            .map(|run| run.validate_wire().map_err(ClientError::Protocol))
            .transpose()
    }

    /// Trigger a manual issue tracker poll.
    pub async fn trigger_issue_tracker_poll(&mut self) -> Result<IssueTrackerTickResultV1> {
        let value = self.request("TriggerIssueTrackerPoll", Value::Null).await?;
        Ok(serde_json::from_value(value)?)
    }

    // --- Dialectic Query ---

    /// Query the dialectic engine for natural-language answers about accumulated knowledge.
    pub async fn query_memory(
        &mut self,
        query: &str,
        project_id: Option<uuid::Uuid>,
        conversation_history: &[(String, String)],
    ) -> Result<rsi_common::rpc::QueryMemoryResponse> {
        let params = serde_json::json!({
            "query": query,
            "project_id": project_id,
            "conversation_history": conversation_history,
        });
        let result = self.request("QueryMemory", params).await?;
        let response: rsi_common::rpc::QueryMemoryResponse = serde_json::from_value(result)?;
        Ok(response)
    }

    // --- Scheduled job methods ---

    pub async fn list_scheduled_jobs(&mut self) -> Result<Vec<rsi_common::types::ScheduledJob>> {
        let result = self.request("ListScheduledJobs", Value::Null).await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn create_scheduled_job(
        &mut self,
        params: rsi_common::rpc::CreateScheduledJobParams,
    ) -> Result<rsi_common::types::ScheduledJob> {
        let result = self
            .request("CreateScheduledJob", serde_json::to_value(&params)?)
            .await?;
        Ok(serde_json::from_value(result)?)
    }

    pub async fn update_scheduled_job(
        &mut self,
        params: rsi_common::rpc::UpdateScheduledJobParams,
    ) -> Result<()> {
        self.request("UpdateScheduledJob", serde_json::to_value(&params)?)
            .await?;
        Ok(())
    }

    pub async fn delete_scheduled_job(&mut self, id: uuid::Uuid) -> Result<()> {
        self.request(
            "DeleteScheduledJob",
            serde_json::json!({"id": id.to_string()}),
        )
        .await?;
        Ok(())
    }

    pub async fn toggle_scheduled_job(&mut self, id: uuid::Uuid) -> Result<bool> {
        let result = self
            .request(
                "ToggleScheduledJob",
                serde_json::json!({"id": id.to_string()}),
            )
            .await?;
        Ok(result
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false))
    }

    pub async fn trigger_scheduled_job(&mut self, id: uuid::Uuid) -> Result<()> {
        self.fire_and_forget(
            "TriggerScheduledJob",
            serde_json::json!({"id": id.to_string()}),
        )
        .await
    }

    /// Update (or create) a single ticket entry in the INDEX.status.json sidecar
    /// for the given project.
    pub async fn update_index_status(
        &mut self,
        project: &str,
        ticket_id: &str,
        status: IndexStatusValue,
        last_shipped_commit: Option<String>,
        last_shipped_branch: Option<String>,
    ) -> Result<()> {
        self.fire_and_forget(
            "UpdateIndexStatus",
            serde_json::json!({
                "project": project,
                "ticket_id": ticket_id,
                "status": status,
                "last_shipped_commit": last_shipped_commit,
                "last_shipped_branch": last_shipped_branch,
            }),
        )
        .await
    }

    /// Fetch the full INDEX.status.json sidecar for a project.
    pub async fn get_index_status(&mut self, project: &str) -> Result<IndexStatusSidecar> {
        let result = self
            .request("GetIndexStatus", serde_json::json!({"project": project}))
            .await?;
        Ok(serde_json::from_value(result)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reclaim_prepared_hint_is_narrow_and_bounded() {
        let pending = ClientError::Rpc {
            code: -32603,
            message: "sandbox_custody:reclaim_prepared".into(),
            data: Some(serde_json::json!({
                "kind": "sandbox_custody",
                "error": { "code": "reclaim_prepared" },
                "retry_after_ms": 1_000
            })),
        };
        assert_eq!(pending.reclaim_prepared_retry_ms(), Some(1_000));
        let unrelated = ClientError::Rpc {
            code: -32603,
            message: "sandbox_custody:custody_changed".into(),
            data: Some(serde_json::json!({
                "kind": "sandbox_custody",
                "error": { "code": "custody_changed" },
                "retry_after_ms": 1_000
            })),
        };
        assert_eq!(unrelated.reclaim_prepared_retry_ms(), None);
    }

    #[test]
    fn archive_cleanup_error_decoder_is_narrow_and_validated() {
        let safe = ArchiveCleanupErrorV1 {
            version: rsi_common::archive_cleanup::ARCHIVE_CLEANUP_SCHEMA_VERSION,
            safe_code: rsi_common::archive_cleanup::ArchiveCleanupSafeCodeV1::WorktreeDirty,
            run_id: None,
            phase: None,
            retryable: true,
            next_action: "Stop competing maintenance, then retry archive.".into(),
        };
        let error = ClientError::Rpc {
            code: -32071,
            message: "archive_cleanup_worktree_dirty".into(),
            data: Some(serde_json::to_value(&safe).unwrap()),
        };
        assert_eq!(error.archive_cleanup_error(), Some(safe));

        let unrelated = ClientError::Rpc {
            code: -32071,
            message: "unrelated".into(),
            data: Some(serde_json::json!({"version": 1, "detail": "raw"})),
        };
        assert!(unrelated.archive_cleanup_error().is_none());
    }

    #[test]
    fn test_default_socket_path() {
        let path = DaemonClient::default_socket_path();
        assert!(path.to_string_lossy().contains("rsi"));
    }

    #[test]
    fn test_client_initially_disconnected() {
        let client = DaemonClient::new(PathBuf::from("/tmp/test.sock"));
        assert!(!client.is_connected());
    }

    #[test]
    fn test_client_disconnect() {
        let mut client = DaemonClient::new(PathBuf::from("/tmp/test.sock"));
        client.disconnect();
        assert!(!client.is_connected());
    }

    #[tokio::test]
    async fn get_issue_in_project_decodes_option_row_and_typed_foreign_error_over_socket() {
        use rsi_common::issue_workspace::{
            IssueWorkspaceErrorCodeV1, IssueWorkspaceReadinessV1, IssueWorkspaceRowV1,
        };
        use rsi_common::types::{Issue, IssueStatus};
        use tokio::net::UnixListener;
        use uuid::Uuid;

        let directory = crate::test_support::short_socket_dir("rsi-issue-workspace-");
        let socket_path = directory.path().join("issue-workspace.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let project_id = Uuid::new_v4();
        let issue_id = Uuid::new_v4();
        let missing_id = Uuid::new_v4();
        let foreign_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        let row = IssueWorkspaceRowV1 {
            issue: Issue {
                id: issue_id,
                project_id,
                display_number: 91,
                title: "Socket-decoded Issue row".to_string(),
                body: "Exact nested workspace row".to_string(),
                priority: Some(2),
                labels: vec!["wire".to_string()],
                assignee: Some("operator".to_string()),
                status: IssueStatus::Open,
                created_by_session_id: None,
                idea_id: None,
                source_event_id: None,
                source_finding_ref: None,
                created_at: now,
                updated_at: now,
                closed_at: None,
                archived_at: None,
                row_version: 1,
            },
            readiness: IssueWorkspaceReadinessV1::Ready,
            open_blocker_count: 0,
            dependent_count: 2,
        };
        let expected_row = row.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            for (expected_issue_id, response) in [
                (issue_id, Ok(serde_json::to_value(Some(row)).unwrap())),
                (missing_id, Ok(Value::Null)),
                (
                    foreign_id,
                    Err(IssueWorkspaceErrorV1 {
                        code: IssueWorkspaceErrorCodeV1::NotFoundInProject,
                        issue_id: Some(foreign_id),
                        expected_row_version: None,
                        actual_row_version: None,
                        retryable: false,
                        next_action: "refresh the project Issue list".to_string(),
                    }),
                ),
            ] {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let request: RpcRequest = serde_json::from_str(line.trim()).unwrap();
                assert_eq!(request.method, "GetIssueInProject");
                assert_eq!(
                    request.params.get("project_id"),
                    Some(&serde_json::json!(project_id))
                );
                assert_eq!(
                    request.params.get("issue_id"),
                    Some(&serde_json::json!(expected_issue_id))
                );
                let response = match response {
                    Ok(result) => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request.id,
                        "result": result
                    }),
                    Err(error) => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request.id,
                        "error": {
                            "code": -32602,
                            "message": "Issue is not in this project",
                            "data": error
                        }
                    }),
                };
                write_half
                    .write_all(format!("{}\n", response).as_bytes())
                    .await
                    .unwrap();
            }
        });

        let mut client = DaemonClient::new(socket_path);
        client.connect().await.unwrap();
        let selected = client
            .get_issue_in_project(GetIssueInProjectRequestV1 {
                project_id,
                issue_id,
            })
            .await
            .unwrap();
        assert_eq!(selected, Some(expected_row));
        let missing = client
            .get_issue_in_project(GetIssueInProjectRequestV1 {
                project_id,
                issue_id: missing_id,
            })
            .await
            .unwrap();
        assert_eq!(missing, None);
        let foreign = client
            .get_issue_in_project(GetIssueInProjectRequestV1 {
                project_id,
                issue_id: foreign_id,
            })
            .await
            .expect_err("foreign-project Issue must remain typed");
        assert_eq!(
            foreign.issue_workspace_error().map(|error| error.code),
            Some(IssueWorkspaceErrorCodeV1::NotFoundInProject)
        );
        server.await.unwrap();
    }

    #[test]
    fn settlement_client_accepts_v1_and_rejects_future_wire_versions() {
        let valid = serde_json::json!([{
            "schema_version": 1,
            "policy_version": 1,
            "repository_identity": "/tmp/repository/.git",
            "canonical_repo_dir": "/tmp/repository",
            "live_roots": 2,
            "terminal_roots": 1
        }]);
        let decoded = decode_source_worktree_cohorts(valid.clone()).expect("V1 cohort response");
        assert_eq!(decoded.len(), 1);

        let mut future = valid;
        future[0]["schema_version"] = serde_json::json!(2);
        assert!(
            decode_source_worktree_cohorts(future).is_err(),
            "future settlement schema must fail closed"
        );
    }

    #[test]
    fn sandbox_build_cache_client_decodes_v1_v2_and_rejects_malformed_success_report() {
        use rsi_common::sandbox_storage::{
            SANDBOX_BUILD_CACHE_REPORT_VERSION_V2, SandboxBuildCacheReclaimConfig,
            SandboxBuildCacheReclaimReport, SandboxBuildCacheReclaimReportV2,
            SandboxBuildCacheReclaimStopReason, SandboxFilesystemStats,
            SandboxTargetReclaimSweepV2, SandboxTargetRecoverySweepV2,
        };

        let filesystem = SandboxFilesystemStats {
            total_bytes: 100,
            available_bytes: 40,
            used_bytes: 60,
            used_percent: 60,
        };
        let report = SandboxBuildCacheReclaimReport {
            version: 1,
            dry_run: true,
            enabled: true,
            config: SandboxBuildCacheReclaimConfig {
                enabled: true,
                ttl_secs: 21_600,
                interval_secs: 3_600,
                high_watermark_pct: 85,
                low_watermark_pct: 75,
                max_candidates: 64,
            },
            pressure_active_before: false,
            pressure_active_after: false,
            filesystem_before: filesystem,
            filesystem_after: filesystem,
            candidates_considered: 0,
            eligible_candidates: 0,
            skip_counts: Default::default(),
            would_reclaim_count: 0,
            would_reclaim_bytes: 0,
            staged_count: 0,
            staged_bytes: 0,
            newly_staged_count: 0,
            newly_staged_bytes: 0,
            recovered_count: 0,
            recovered_bytes: 0,
            pending_count: 0,
            pending_bytes: 0,
            fully_removed_count: 0,
            reclaimed_bytes: 0,
            stopped_at_low_watermark: false,
            candidate_budget_exhausted: false,
            stop_reason: SandboxBuildCacheReclaimStopReason::NoCandidates,
        };
        assert!(matches!(
            decode_sandbox_storage_report(serde_json::to_value(&report).unwrap()).unwrap(),
            SandboxBuildCacheReclaimReportWire::V1(_)
        ));
        let v2 = SandboxBuildCacheReclaimReportV2 {
            version: SANDBOX_BUILD_CACHE_REPORT_VERSION_V2,
            report: report.clone(),
            candidate_sweep: SandboxTargetReclaimSweepV2 {
                cycle_before: 0,
                cycle_after: 0,
                cursor_before: None,
                cursor_after: None,
                upper_bound: None,
                page_key_digest: format!("sha256:{}", "0".repeat(64)),
                reserved: false,
                wrapped: false,
            },
            recovery_sweep: SandboxTargetRecoverySweepV2::default(),
            pass_contention: None,
        };
        let decoded = decode_sandbox_storage_report(serde_json::to_value(v2).unwrap()).unwrap();
        assert!(matches!(decoded, SandboxBuildCacheReclaimReportWire::V2(_)));
        assert!(decoded.report().dry_run);

        let mut malformed = serde_json::to_value(&report).unwrap();
        malformed["future_field"] = serde_json::json!(true);
        assert!(matches!(
            decode_sandbox_storage_report(malformed),
            Err(ClientError::Json(_))
        ));
        let mut wrong_version = serde_json::to_value(report).unwrap();
        wrong_version["version"] = serde_json::json!(2);
        assert!(matches!(
            decode_sandbox_storage_report(wrong_version),
            Err(ClientError::Protocol(_))
        ));
    }

    #[test]
    fn launch_session_with_opts_serializes_parent_id() {
        let parent_id = uuid::Uuid::new_v4();
        let params = launch_session_with_opts_params(
            "query",
            None,
            Some(Path::new("/tmp/project")),
            SessionProvider::Claude,
            Some("sonnet"),
            Some("system"),
            Some(rsi_common::types::SessionKind::Story),
            None,
            None,
            Some("high"),
            Some(parent_id),
            None,
            &["ci".to_string()],
            None,
            None,
        );

        let decoded: rsi_common::rpc::LaunchSessionParams = serde_json::from_value(params).unwrap();
        assert_eq!(decoded.parent_id, Some(parent_id));
        assert_eq!(
            decoded.session_kind,
            Some(rsi_common::types::SessionKind::Story)
        );
    }

    #[test]
    fn launch_session_with_opts_keeps_parent_id_null_when_absent() {
        let params = launch_session_with_opts_params(
            "query",
            None,
            None,
            SessionProvider::Claude,
            None,
            None,
            Some(rsi_common::types::SessionKind::TaskRabbit),
            None,
            Some(3),
            None,
            None,
            None,
            &["ci".to_string()],
            None,
            None,
        );

        let decoded: rsi_common::rpc::LaunchSessionParams = serde_json::from_value(params).unwrap();
        assert_eq!(decoded.parent_id, None);
        assert_eq!(decoded.max_retries, Some(3));
    }

    /// ST-NEWSESSION-UNIFY: the unified launch path threads `workflow_id`
    /// through `launch_session_with_opts` (it previously lived only on the
    /// `launch_session` builder). Pin that both workflow fields round-trip and
    /// stay independent.
    #[test]
    fn launch_session_with_opts_threads_workflow_id() {
        let workflow_id = uuid::Uuid::new_v4();
        let params = launch_session_with_opts_params(
            "query",
            None,
            None,
            SessionProvider::Claude,
            None,
            None,
            Some(rsi_common::types::SessionKind::Standard),
            None,
            None,
            None,
            None,
            None,
            &["ci".to_string()],
            Some(workflow_id),
            None,
        );

        let decoded: rsi_common::rpc::LaunchSessionParams = serde_json::from_value(params).unwrap();
        assert_eq!(decoded.workflow_id, Some(workflow_id));
        assert_eq!(decoded.workflow_id_override, None);
    }

    #[test]
    fn custom_provider_launch_opts_serializes_parent_id() {
        let parent_id = uuid::Uuid::new_v4();
        let params = launch_session_with_opts_custom_provider_params(
            "query",
            Some("explicit title"),
            None,
            SessionProvider::Local,
            Some("custom-model"),
            None,
            Some(rsi_common::types::SessionKind::Bug),
            None,
            "http://localhost:11434/v1",
            "test-key",
            None,
            None,
            Some(parent_id),
            &["ci".to_string()],
            None,
        );

        let decoded: rsi_common::rpc::LaunchSessionParams = serde_json::from_value(params).unwrap();
        assert_eq!(decoded.parent_id, Some(parent_id));
        assert_eq!(decoded.title.as_deref(), Some("explicit title"));
        assert_eq!(
            decoded.session_kind,
            Some(rsi_common::types::SessionKind::Bug)
        );
        assert_eq!(
            decoded.openai_base_url.as_deref(),
            Some("http://localhost:11434/v1")
        );
    }

    #[test]
    fn recursive_graph_id_params_uses_rpc_field_name() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(
            recursive_graph_id_params(RecursiveTaskGraphId(id)),
            serde_json::json!({ "graph_id": id })
        );
    }

    #[test]
    fn recursive_task_id_params_uses_rpc_field_names() {
        let graph_id = uuid::Uuid::new_v4();
        let task_id = uuid::Uuid::new_v4();
        assert_eq!(
            recursive_task_id_params(RecursiveTaskGraphId(graph_id), RecursiveTaskId(task_id)),
            serde_json::json!({
                "graph_id": graph_id,
                "task_id": task_id,
            })
        );
    }

    #[test]
    fn recursive_scheduler_run_id_params_uses_rpc_field_name() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(
            recursive_scheduler_run_id_params(RecursiveSchedulerRunId(id)),
            serde_json::json!({ "run_id": id })
        );
    }

    #[test]
    fn recursive_cancellation_request_id_params_uses_rpc_field_name() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(
            recursive_cancellation_request_id_params(RecursiveCancellationRequestId(id)),
            serde_json::json!({ "request_id": id })
        );
    }

    #[test]
    fn recursive_fake_scheduler_params_are_fake_only_and_bounded() {
        let graph_id = RecursiveTaskGraphId::new();
        let params = recursive_fake_scheduler_params(graph_id, 7);

        assert_eq!(
            params,
            serde_json::json!({
                "graph_id": graph_id.0,
                "max_steps": 7,
                "operator": "tui",
                "execution_mode": "fake",
            })
        );

        let decoded: RunRecursiveFakeSchedulerParams = serde_json::from_value(params).unwrap();
        assert_eq!(decoded.graph_id, graph_id.0);
        assert_eq!(decoded.max_steps, 7);
        assert_eq!(decoded.operator.as_deref(), Some("tui"));
        assert_eq!(decoded.execution_mode.as_deref(), Some("fake"));
    }

    #[test]
    fn recursive_fake_scheduler_response_decodes_top_level_run_summary() {
        let graph_id = RecursiveTaskGraphId::new();
        let run_id = RecursiveSchedulerRunId::new();
        let result = serde_json::json!({
            "id": run_id,
            "graph_id": graph_id,
            "status": "completed",
            "source": "manual_rpc",
            "operator": "tui",
            "started_at": "2026-05-27T12:00:00.000000000Z",
            "completed_at": "2026-05-27T12:00:01.000000000Z",
            "stop_reason": null,
            "step_count": 1,
            "max_steps": 1,
            "executor_mode": "fake",
            "failure_reason": null,
            "cancellation_request_id": null,
            "cancellation_reason": null,
            "lease_owner": null,
            "lease_token": null,
            "lease_heartbeat_at": null,
            "lease_expires_at": null,
            "report_artifact_id": 87,
        });

        let decoded: RecursiveSchedulerRunSummary = serde_json::from_value(result.clone()).unwrap();
        assert_eq!(decoded.id, run_id);
        assert_eq!(decoded.graph_id, graph_id);
        assert_eq!(
            decoded.status,
            rsi_common::RecursiveSchedulerRunStatus::Completed
        );
        assert_eq!(decoded.operator.as_deref(), Some("tui"));
        assert_eq!(
            decoded.executor_mode,
            rsi_common::RecursiveExecutionMode::Fake
        );
        assert_eq!(decoded.report_artifact_id, Some(87));

        let old_shape_error = serde_json::from_value::<RecursiveSchedulerRunDetail>(result)
            .expect_err("top-level run is not a RecursiveSchedulerRunDetail");
        assert!(old_shape_error.to_string().contains("missing field `run`"));
    }

    #[test]
    fn recursive_cancellation_params_use_typed_rpc_models_and_requested_by() {
        let graph_id = RecursiveTaskGraphId::new();
        let run_id = RecursiveSchedulerRunId::new();

        let graph_params = recursive_graph_cancellation_params(
            graph_id,
            "stop graph".to_string(),
            Some("rsi-tui".to_string()),
        );
        let decoded_graph: RequestRecursiveGraphCancellationParams =
            serde_json::from_value(graph_params).unwrap();
        assert_eq!(decoded_graph.graph_id, graph_id.0);
        assert_eq!(decoded_graph.reason, "stop graph");
        assert_eq!(decoded_graph.requested_by.as_deref(), Some("rsi-tui"));

        let run_params = recursive_scheduler_run_cancellation_params(
            run_id,
            "stop run".to_string(),
            Some("rsi-tui".to_string()),
        );
        let decoded_run: RequestRecursiveSchedulerRunCancellationParams =
            serde_json::from_value(run_params).unwrap();
        assert_eq!(decoded_run.run_id, run_id.0);
        assert_eq!(decoded_run.reason, "stop run");
        assert_eq!(decoded_run.requested_by.as_deref(), Some("rsi-tui"));
    }

    #[test]
    fn recursive_recovery_params_preserve_omitted_and_zero_time_budget() {
        let omitted = recursive_recovery_continuation_params(2, None);
        let decoded_omitted: ContinueRecursiveRecoveryParams =
            serde_json::from_value(omitted).unwrap();
        assert_eq!(decoded_omitted.max_graphs, 2);
        assert_eq!(decoded_omitted.time_budget_ms, None);

        let zero = recursive_recovery_continuation_params(2, Some(0));
        let decoded_zero: ContinueRecursiveRecoveryParams = serde_json::from_value(zero).unwrap();
        assert_eq!(decoded_zero.max_graphs, 2);
        assert_eq!(decoded_zero.time_budget_ms, Some(0));
    }

    #[test]
    fn recursive_artifact_lookup_params_are_graph_guarded() {
        let graph_id = RecursiveTaskGraphId::new();
        let params = recursive_artifact_lookup_params(graph_id, 42, true);

        assert_eq!(
            params,
            serde_json::json!({
                "graph_id": graph_id,
                "artifact_id": 42,
                "include_links": true,
            })
        );

        let decoded: GetRecursiveExecutionArtifactParams = serde_json::from_value(params).unwrap();
        assert_eq!(decoded.graph_id, graph_id);
        assert_eq!(decoded.artifact_id, 42);
        assert!(decoded.include_links);
    }

    #[test]
    fn recursive_artifact_preview_params_are_graph_guarded_and_bounded() {
        let graph_id = RecursiveTaskGraphId::new();
        let params = recursive_artifact_preview_params(graph_id, 43, 16_384, 80);

        assert_eq!(
            params,
            serde_json::json!({
                "graph_id": graph_id,
                "artifact_id": 43,
                "byte_offset": null,
                "line_offset": null,
                "max_bytes": 16_384,
                "max_lines": 80,
                "render_hint": null,
                "require_complete": false,
            })
        );

        let decoded: PreviewRecursiveExecutionArtifactParams =
            serde_json::from_value(params).unwrap();
        assert_eq!(decoded.graph_id, graph_id);
        assert_eq!(decoded.artifact_id, 43);
        assert!(decoded.byte_offset.is_none());
        assert!(decoded.line_offset.is_none());
        assert_eq!(decoded.max_bytes, Some(16_384));
        assert_eq!(decoded.max_lines, Some(80));
        assert!(decoded.render_hint.is_none());
        assert!(!decoded.require_complete);
    }

    #[test]
    fn recursive_artifact_summary_list_params_are_graph_guarded_and_bounded() {
        let graph_id = RecursiveTaskGraphId::new();
        let params =
            recursive_artifact_summary_list_params(graph_id, 24, Some("cursor".to_string()));

        assert_eq!(
            params,
            serde_json::json!({
                "graph_id": graph_id,
                "task_id": null,
                "attempt_id": null,
                "live_attempt_id": null,
                "scheduler_run_id": null,
                "validation_id": null,
                "role": null,
                "kind": null,
                "cursor": "cursor",
                "limit": 24,
                "include_total": false,
            })
        );

        let decoded: ListRecursiveExecutionArtifactSummariesParams =
            serde_json::from_value(params).unwrap();
        assert_eq!(decoded.graph_id, Some(graph_id));
        assert_eq!(decoded.limit, Some(24));
        assert_eq!(decoded.cursor.as_deref(), Some("cursor"));
        assert!(!decoded.include_total);
        assert_eq!(decoded.role, None);
        assert_eq!(decoded.kind, None);
    }
}
