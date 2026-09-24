pub use crate::agent_coordination::*;
pub use crate::archive_cleanup::{ArchiveSessionParamsV1, GetArchiveCleanupStatusParamsV1};
pub use crate::cohort_settlement::{
    ApplySourceWorktreeBatchParamsV2, ApplySourceWorktreeCohortParams,
    AuditSourceWorktreeBatchParamsV2, AuditSourceWorktreeCohortParams,
    GetSourceWorktreeSettlementRunParams, ListSourceWorktreeCohortsParams,
};
use crate::program_runs::{
    CancelProgramRunRequestV1, CreateProgramRunRequestV1, ProgramRunMutationResultV1,
    ProgramRunOperationalStatusV1, ProgramRunPageCursorV1, ProgramRunReconciliationPageV1,
    ProgramRunStatusV1, ProgramRunTransitionPageV1, ProgramRunV1, ResumeBlockedProgramRunRequestV1,
};
use crate::types::{
    ConversationEvent, IndexStatusValue, Issue, IssueArchiveFilterV1, IssueEventPageRequestV1,
    IssueEventPageV1, IssueSourceFindingRef, IssueStatus, OffloadEntry, PermissionLevel,
    PermissionRule, PermissionScope, SandboxSpec, SessionKind, SessionProvider, TopologyDefinition,
    WorkflowDocument, WorkflowExecutionLookup, WorkflowExecutionStatus,
};
use crate::{
    RecursiveArtifactRenderHint, RecursiveArtifactRole, RecursiveAttemptId,
    RecursiveCancellationRequestId, RecursiveCancellationRequestStatus,
    RecursiveCancellationRequestSummary, RecursiveDiffFileId, RecursiveDiffId,
    RecursiveExecutionArtifact, RecursiveExecutionArtifactKind, RecursiveGraphRecoveryStatus,
    RecursiveGraphStatus, RecursiveLiveAttemptId, RecursiveLiveAttemptReadback,
    RecursiveLiveAttemptStatus, RecursiveLiveInterruptId, RecursiveLiveInterruptStatus,
    RecursiveLiveOutputKind, RecursiveLiveOutputValidationId, RecursiveLiveOutputValidationResult,
    RecursiveLiveOutputValidationStatus, RecursiveLiveRecoveryStatus, RecursiveLiveSandboxPolicy,
    RecursiveLiveToolPolicy, RecursiveLiveValidationIssueClass, RecursiveLiveValidationIssueCode,
    RecursiveLiveValidationIssueSeverity, RecursiveRecoveryPassSummary, RecursiveSchedulerReportId,
    RecursiveSchedulerRunId, RecursiveSchedulerRunSource, RecursiveSchedulerRunStatus,
    RecursiveTaskGraphId, RecursiveTaskId, RecursiveTestResultId, RecursiveTrustLevel,
    RecursiveTypedSource, RecursiveTypedTestStatus, TopologyRecursiveStatus,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use uuid::Uuid;

/// JSON-RPC 2.0 Request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
    /// Per-session authority token (P0 attribution gate). Sibling of
    /// `params`, never nested inside the agent-typed payload — this is what
    /// keeps the token out of persisted request params. `rsi-rpc` sets this
    /// from `$RSI_SESSION_TOKEN`; unattributed (TUI/operator) callers leave
    /// it `None` and are unaffected by the gate in `handle_request_inner`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
}

impl RpcRequest {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::Number(1.into())),
            method: method.into(),
            params,
            session_token: None,
        }
    }
}

/// JSON-RPC 2.0 Response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    pub jsonrpc: String,
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    pub fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: Option<Value>, error: RpcError) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcErrorData {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

// Standard JSON-RPC error codes
pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

/// RPC Method Parameters
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaunchSessionParams {
    pub query: String,
    #[serde(default)]
    pub title: Option<String>,
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub provider: Option<crate::types::SessionProvider>,
    #[serde(default)]
    pub model: Option<String>,
    /// Optional raw provider context-window override. Codex applies its
    /// catalog effective percentage before reporting the live denominator.
    #[serde(default)]
    pub configured_context_window: Option<u64>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub session_kind: Option<crate::types::SessionKind>,
    /// Explicit project assignment. Used as fallback when path-based resolution
    /// finds no match. Allows the TUI to assign sessions to the active project.
    #[serde(default)]
    pub project_id: Option<Uuid>,
    /// Parent session ID for context rotation chains.
    /// When set, the child session's continued_from points to this parent.
    #[serde(default)]
    pub continued_from: Option<Uuid>,
    /// Hierarchical parent (Group/Epic container). Independent of
    /// `continued_from`. Validated server-side via
    /// `hierarchy::validate_containment` before launch.
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    /// Override base URL for OpenAI-compatible providers (custom provider config).
    #[serde(default)]
    pub openai_base_url: Option<String>,
    /// Override API key for OpenAI-compatible providers (custom provider config).
    #[serde(default)]
    pub openai_api_key: Option<String>,
    /// Workflow to associate this session with (pipeline propagation).
    #[serde(default)]
    pub workflow_id: Option<Uuid>,
    /// Maximum retry attempts on transient failure. Default 0 (no retry).
    /// TaskRabbit sessions use 3 by default.
    #[serde(default)]
    pub max_retries: Option<u8>,
    /// Optional label to assign this session to (field name preserved for schema compat).
    #[serde(default)]
    pub group_id: Option<Uuid>,
    /// Effort level for reasoning-capable sessions.
    /// Claude accepts "low", "medium", "high", "max"; Codex accepts
    /// "low", "medium", "high", "xhigh", "max", and (on GPT-6 Astra)
    /// "ultra" on supported models.
    /// None = provider default.
    #[serde(default)]
    pub effort: Option<String>,
    /// Per-session sandbox request. `None` = canonical working_dir (zero change
    /// vs. pre-sandbox behavior). Daemons that lack sandbox support ignore this.
    #[serde(default)]
    pub sandbox: Option<SandboxSpec>,
    /// Tag this session as an eval replay. `None` / `Some(false)` = production
    /// session (default). `Some(true)` = excluded from production analytics
    /// and default `ListSessions`. Used by `rsi-eval`. See RSI-006.
    #[serde(default)]
    pub is_eval: Option<bool>,
    /// Eval-only: bypass the daemon's ContextPipeline assembly. When `Some(true)`,
    /// `system_prompt` is used verbatim — no preamble injection, no project
    /// context, no memory bundle, no git log, no FLYWHEEL.md. Required for
    /// hermetic eval replays. Default `None` / `Some(false)` preserves the
    /// production assembly path. See RSI-006.
    #[serde(default)]
    pub skip_context_pipeline: Option<bool>,
    // ─── P1.6: tag + topology override ─────────────────────
    /// Mandatory non-empty tag set for this session. Daemon normalizes each
    /// entry via `normalize_tag` and rejects the request if normalization
    /// fails or the resulting set is empty. NOT serde(default) — pre-1.6
    /// callers that omit this field get a clean deserialization failure.
    pub tags: Vec<String>,
    /// Per-leaf topology override. When Some, the spawned session row carries
    /// `workflow_id_override` (supersedes the Epic-derived topology resolved
    /// by `effective_topology_with_override`). Not consumed at execution time
    /// in Phase 1 — wired for Phase 5. `serde(default)` = None for callers
    /// that omit it.
    #[serde(default)]
    pub workflow_id_override: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinueSessionParams {
    pub session_id: Uuid,
    pub query: String,
}

/// Params for setting/clearing a session's active task context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateActiveTaskParams {
    pub session_id: Uuid,
    /// None clears the active task. Some(text) sets it (max 500 chars enforced by TUI).
    pub active_task: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RotateSessionParams {
    pub session_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwitchSessionModelParams {
    pub session_id: Uuid,
    pub new_model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnswerQuestionParams {
    pub session_id: Uuid,
    pub response_text: String,
}

/// Params for updating session rating (1–10 scale).
/// `rating: None` clears the rating on the target session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateSessionRatingParams {
    pub session_id: Uuid,
    /// Rating on 1–10 scale. None clears the stored rating.
    #[serde(default)]
    pub rating: Option<i16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeParams {
    #[serde(default)]
    pub event_types: Vec<String>,
    pub session_id: Option<Uuid>,
}

/// Cursor describing which conversation slice to fetch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationFetchCursor {
    pub session_id: Uuid,
    #[serde(default)]
    pub since_sequence: Option<i32>,
}

/// Batched conversation fetch params.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetConversationsSinceParams {
    pub requests: Vec<ConversationFetchCursor>,
}

/// Batched conversation response entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationBatchEntry {
    pub session_id: Uuid,
    pub events: Vec<ConversationEvent>,
}

/// Response payload for GetConversationsSince.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationBatchResponse {
    pub conversations: Vec<ConversationBatchEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecursiveTaskGraphIdParams {
    pub graph_id: Uuid,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListRecursiveTaskGraphsParams {
    #[serde(default)]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub workflow_id: Option<Uuid>,
    #[serde(default)]
    pub topology_id: Option<Uuid>,
    #[serde(default)]
    pub parent_session_id: Option<Uuid>,
    #[serde(default)]
    pub status: Option<RecursiveGraphStatus>,
    #[serde(default = "default_include_quarantined_recursive_graphs")]
    pub include_quarantined: bool,
}

fn default_include_quarantined_recursive_graphs() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListRecursiveGraphsForTopologyParams {
    #[serde(default)]
    pub topology_id: Option<Uuid>,
    #[serde(default)]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub workflow_id: Option<Uuid>,
    #[serde(default)]
    pub workflow_execution_id: Option<Uuid>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub topology_iteration: Option<u32>,
    #[serde(default)]
    pub parent_session_id: Option<Uuid>,
    #[serde(default)]
    pub execution_owner: Option<String>,
    #[serde(default = "default_include_quarantined_recursive_graphs")]
    pub include_quarantined: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GetTopologyRecursiveStatusParams {
    #[serde(default)]
    pub topology_id: Option<Uuid>,
    #[serde(default)]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub workflow_id: Option<Uuid>,
    #[serde(default)]
    pub workflow_execution_id: Option<Uuid>,
    #[serde(default)]
    pub graph_id: Option<Uuid>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub topology_iteration: Option<u32>,
    #[serde(default)]
    pub parent_session_id: Option<Uuid>,
    #[serde(default)]
    pub execution_owner: Option<String>,
    #[serde(default)]
    pub include_dynamic_children: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopologyRecursiveCancellationScope {
    #[default]
    Graph,
    Run,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopologyRecursiveCancellationApplyTo {
    #[default]
    Single,
    AllMatching,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopologyRecursiveCancellationRunSelection {
    #[default]
    Active,
    Latest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopologyRecursiveCancellationOutcome {
    Requested,
    Reused,
    AlreadyCancelled,
    SkippedTerminal,
    Rejected,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestTopologyRecursiveCancellationParams {
    #[serde(default)]
    pub graph_id: Option<Uuid>,
    #[serde(default)]
    pub topology_id: Option<Uuid>,
    #[serde(default)]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub workflow_id: Option<Uuid>,
    #[serde(default)]
    pub workflow_execution_id: Option<Uuid>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub topology_iteration: Option<u32>,
    #[serde(default)]
    pub parent_session_id: Option<Uuid>,
    #[serde(default)]
    pub execution_owner: Option<String>,
    #[serde(default)]
    pub scope: TopologyRecursiveCancellationScope,
    #[serde(default)]
    pub apply_to: TopologyRecursiveCancellationApplyTo,
    #[serde(default)]
    pub run_id: Option<Uuid>,
    #[serde(default)]
    pub run_selection: TopologyRecursiveCancellationRunSelection,
    pub reason: String,
    #[serde(default)]
    pub requested_by: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyRecursiveCancellationTargetResult {
    pub graph_id: RecursiveTaskGraphId,
    #[serde(default)]
    pub run_id: Option<RecursiveSchedulerRunId>,
    pub topology_id: Uuid,
    pub source_topology_node_id: String,
    pub source_topology_iteration: u32,
    pub execution_owner: String,
    #[serde(default)]
    pub cancellation_request: Option<RecursiveCancellationRequestSummary>,
    pub outcome: TopologyRecursiveCancellationOutcome,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyRecursiveCancellationResponse {
    pub scope: TopologyRecursiveCancellationScope,
    pub apply_to: TopologyRecursiveCancellationApplyTo,
    pub matched_graph_count: u32,
    pub requested_count: u32,
    pub reused_count: u32,
    pub skipped_count: u32,
    pub rejected_count: u32,
    #[serde(default)]
    pub targets: Vec<TopologyRecursiveCancellationTargetResult>,
    pub status: TopologyRecursiveStatus,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopologyRecursiveRecoveryApplyTo {
    #[default]
    Single,
    AllMatching,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopologyRecursiveRecoveryOutcome {
    Recovered,
    Quarantined,
    Deferred,
    SkippedComplete,
    SkippedTerminal,
    SkippedQuarantined,
    Rejected,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContinueTopologyRecursiveRecoveryParams {
    #[serde(default)]
    pub graph_id: Option<Uuid>,
    #[serde(default)]
    pub topology_id: Option<Uuid>,
    #[serde(default)]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub workflow_id: Option<Uuid>,
    #[serde(default)]
    pub workflow_execution_id: Option<Uuid>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub topology_iteration: Option<u32>,
    #[serde(default)]
    pub parent_session_id: Option<Uuid>,
    #[serde(default)]
    pub execution_owner: Option<String>,
    #[serde(default)]
    pub apply_to: TopologyRecursiveRecoveryApplyTo,
    pub max_graphs: u32,
    #[serde(default)]
    pub time_budget_ms: Option<u64>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyRecursiveRecoveryTargetResult {
    pub graph_id: RecursiveTaskGraphId,
    pub topology_id: Uuid,
    pub source_topology_node_id: String,
    pub source_topology_iteration: u32,
    pub execution_owner: String,
    pub before_graph_status: RecursiveGraphStatus,
    pub after_graph_status: RecursiveGraphStatus,
    pub before: RecursiveGraphRecoveryStatus,
    pub after: RecursiveGraphRecoveryStatus,
    pub outcome: TopologyRecursiveRecoveryOutcome,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyRecursiveRecoveryResponse {
    pub apply_to: TopologyRecursiveRecoveryApplyTo,
    pub matched_graph_count: u32,
    pub eligible_count: u32,
    pub checked_count: u32,
    pub recovered_count: u32,
    pub failed_graph_count: u32,
    pub quarantined_count: u32,
    pub deferred_count: u32,
    pub skipped_count: u32,
    pub rejected_count: u32,
    pub no_op_count: u32,
    pub replayed: bool,
    #[serde(default)]
    pub pass: Option<RecursiveRecoveryPassSummary>,
    #[serde(default)]
    pub targets: Vec<TopologyRecursiveRecoveryTargetResult>,
    pub status: TopologyRecursiveStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecursiveTaskIdParams {
    pub graph_id: Uuid,
    pub task_id: Uuid,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListRecursiveTasksParams {
    pub graph_id: Uuid,
    #[serde(default)]
    pub parent_task_id: Option<Uuid>,
    #[serde(default)]
    pub status: Option<crate::RecursiveTaskLifecycleState>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListRecursiveTaskAttemptsParams {
    pub graph_id: Uuid,
    #[serde(default)]
    pub task_id: Option<Uuid>,
    #[serde(default)]
    pub status: Option<crate::RecursiveAttemptStatus>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListRecursiveLifecycleEventsParams {
    pub graph_id: Uuid,
    #[serde(default)]
    pub task_id: Option<Uuid>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListRecursiveExecutionArtifactsParams {
    pub graph_id: Uuid,
    #[serde(default)]
    pub task_id: Option<Uuid>,
    #[serde(default)]
    pub attempt_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRecursiveExecutionArtifactParams {
    pub graph_id: RecursiveTaskGraphId,
    pub artifact_id: i64,
    #[serde(default = "default_true")]
    pub include_links: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreviewRecursiveExecutionArtifactParams {
    pub graph_id: RecursiveTaskGraphId,
    pub artifact_id: i64,
    #[serde(default)]
    pub byte_offset: Option<u64>,
    #[serde(default)]
    pub line_offset: Option<u64>,
    #[serde(default)]
    pub max_bytes: Option<u32>,
    #[serde(default)]
    pub max_lines: Option<u32>,
    #[serde(default)]
    pub render_hint: Option<RecursiveArtifactRenderHint>,
    #[serde(default)]
    pub require_complete: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListRecursiveExecutionArtifactSummariesParams {
    #[serde(default)]
    pub graph_id: Option<RecursiveTaskGraphId>,
    #[serde(default)]
    pub task_id: Option<RecursiveTaskId>,
    #[serde(default)]
    pub attempt_id: Option<RecursiveAttemptId>,
    #[serde(default)]
    pub live_attempt_id: Option<RecursiveLiveAttemptId>,
    #[serde(default)]
    pub scheduler_run_id: Option<RecursiveSchedulerRunId>,
    #[serde(default)]
    pub validation_id: Option<RecursiveLiveOutputValidationId>,
    #[serde(default)]
    pub role: Option<RecursiveArtifactRole>,
    #[serde(default)]
    pub kind: Option<RecursiveExecutionArtifactKind>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub include_total: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListRecursiveTestSummariesParams {
    #[serde(default)]
    pub graph_id: Option<RecursiveTaskGraphId>,
    #[serde(default)]
    pub task_id: Option<RecursiveTaskId>,
    #[serde(default)]
    pub attempt_id: Option<RecursiveAttemptId>,
    #[serde(default)]
    pub live_attempt_id: Option<RecursiveLiveAttemptId>,
    #[serde(default)]
    pub scheduler_run_id: Option<RecursiveSchedulerRunId>,
    #[serde(default)]
    pub validation_id: Option<RecursiveLiveOutputValidationId>,
    #[serde(default)]
    pub artifact_id: Option<i64>,
    #[serde(default)]
    pub source: Option<RecursiveTypedSource>,
    #[serde(default)]
    pub trust_level: Option<RecursiveTrustLevel>,
    #[serde(default)]
    pub status: Option<RecursiveTypedTestStatus>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub include_total: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRecursiveTestDetailParams {
    pub graph_id: RecursiveTaskGraphId,
    pub test_id: RecursiveTestResultId,
    #[serde(default)]
    pub max_failure_bytes: Option<u32>,
    #[serde(default)]
    pub include_output: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListRecursiveDiffSummariesParams {
    #[serde(default)]
    pub graph_id: Option<RecursiveTaskGraphId>,
    #[serde(default)]
    pub task_id: Option<RecursiveTaskId>,
    #[serde(default)]
    pub attempt_id: Option<RecursiveAttemptId>,
    #[serde(default)]
    pub live_attempt_id: Option<RecursiveLiveAttemptId>,
    #[serde(default)]
    pub scheduler_run_id: Option<RecursiveSchedulerRunId>,
    #[serde(default)]
    pub validation_id: Option<RecursiveLiveOutputValidationId>,
    #[serde(default)]
    pub artifact_id: Option<i64>,
    #[serde(default)]
    pub source: Option<RecursiveTypedSource>,
    #[serde(default)]
    pub trust_level: Option<RecursiveTrustLevel>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub include_total: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRecursiveDiffDetailParams {
    pub graph_id: RecursiveTaskGraphId,
    pub diff_id: RecursiveDiffId,
    #[serde(default)]
    pub file_cursor: Option<String>,
    #[serde(default)]
    pub file_limit: Option<u32>,
    #[serde(default)]
    pub include_total: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRecursiveDiffFileHunksParams {
    pub graph_id: RecursiveTaskGraphId,
    pub diff_id: RecursiveDiffId,
    pub file_id: RecursiveDiffFileId,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub max_lines: Option<u32>,
    #[serde(default)]
    pub max_bytes: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRecursiveSchedulerReportSummaryParams {
    pub graph_id: RecursiveTaskGraphId,
    #[serde(default)]
    pub run_id: Option<RecursiveSchedulerRunId>,
    #[serde(default)]
    pub report_id: Option<RecursiveSchedulerReportId>,
    #[serde(default)]
    pub artifact_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRecursiveSchedulerReportDetailParams {
    pub graph_id: RecursiveTaskGraphId,
    #[serde(default)]
    pub run_id: Option<RecursiveSchedulerRunId>,
    #[serde(default)]
    pub report_id: Option<RecursiveSchedulerReportId>,
    #[serde(default)]
    pub artifact_id: Option<i64>,
    #[serde(default)]
    pub step_cursor: Option<String>,
    #[serde(default)]
    pub step_limit: Option<u32>,
    #[serde(default)]
    pub include_events: bool,
    #[serde(default)]
    pub event_cursor: Option<String>,
    #[serde(default)]
    pub event_limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecursiveSchedulerRunIdParams {
    pub run_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecursiveCancellationRequestIdParams {
    pub request_id: Uuid,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListRecursiveSchedulerRunsParams {
    pub graph_id: Uuid,
    #[serde(default)]
    pub status: Option<RecursiveSchedulerRunStatus>,
    #[serde(default)]
    pub source: Option<RecursiveSchedulerRunSource>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default = "default_true")]
    pub include_terminal: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListRecursiveSchedulerRunEventsParams {
    pub run_id: Uuid,
    #[serde(default)]
    pub since_id: Option<i64>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListRecursiveCancellationRequestsParams {
    #[serde(default)]
    pub graph_id: Option<Uuid>,
    #[serde(default)]
    pub run_id: Option<Uuid>,
    #[serde(default)]
    pub status: Option<RecursiveCancellationRequestStatus>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GetRecursiveRecoveryStatusParams {
    #[serde(default)]
    pub graph_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinueRecursiveRecoveryParams {
    pub max_graphs: u32,
    #[serde(default)]
    pub time_budget_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecursiveGraphCancellationParams {
    pub graph_id: Uuid,
    pub reason: String,
    #[serde(default)]
    pub requested_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecursiveSchedulerRunCancellationParams {
    pub run_id: Uuid,
    pub reason: String,
    #[serde(default)]
    pub requested_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecursiveFakeSchedulerParams {
    pub graph_id: Uuid,
    pub max_steps: u32,
    #[serde(default)]
    pub operator: Option<String>,
    #[serde(default)]
    pub execution_mode: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunRecursiveTopologyNodeFakeSchedulerParams {
    #[serde(default)]
    pub graph_id: Option<Uuid>,
    #[serde(default)]
    pub topology_id: Option<Uuid>,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub topology_iteration: Option<u32>,
    #[serde(default)]
    pub workflow_execution_id: Option<Uuid>,
    #[serde(default)]
    pub max_steps: Option<u32>,
    #[serde(default)]
    pub operator: Option<String>,
    #[serde(default)]
    pub execution_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecursiveLiveSchedulerParams {
    pub graph_id: Uuid,
    pub max_steps: u32,
    #[serde(default)]
    pub operator: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub provider: Option<crate::types::SessionProvider>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub sandbox: Option<SandboxSpec>,
    #[serde(default)]
    pub approval_mode: Option<String>,
    #[serde(default)]
    pub tool_policy: Option<RecursiveLiveToolPolicy>,
    #[serde(default)]
    pub sandbox_policy: Option<RecursiveLiveSandboxPolicy>,
    #[serde(default)]
    pub max_wall_time_ms: Option<u64>,
    #[serde(default)]
    pub token_budget: Option<u64>,
    #[serde(default)]
    pub tool_call_budget: Option<u32>,
    #[serde(default)]
    pub artifact_bytes_budget: Option<u64>,
    #[serde(default)]
    pub output_repair_attempts: Option<u32>,
    #[serde(default)]
    pub heartbeat_ttl_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitRecursiveLiveAttemptOutputParams {
    pub live_attempt_id: RecursiveLiveAttemptId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitRecursiveLiveAttemptOutputResponse {
    pub readback: RecursiveLiveAttemptReadback,
    pub validation_result: RecursiveLiveOutputValidationResult,
    #[serde(default)]
    pub raw_output_artifact: Option<RecursiveExecutionArtifact>,
    #[serde(default)]
    pub normalized_output_artifact: Option<RecursiveExecutionArtifact>,
    pub validation_artifact: RecursiveExecutionArtifact,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRecursiveLiveAttemptParams {
    pub live_attempt_id: RecursiveLiveAttemptId,
    #[serde(default = "default_true")]
    pub include_session: bool,
    #[serde(default = "default_true")]
    pub include_heartbeat: bool,
    #[serde(default = "default_true")]
    pub include_interrupt: bool,
    #[serde(default = "default_true")]
    pub include_validation: bool,
    #[serde(default)]
    pub include_artifacts: bool,
    #[serde(default)]
    pub include_retry_history: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListRecursiveLiveAttemptsParams {
    #[serde(default)]
    pub graph_id: Option<RecursiveTaskGraphId>,
    #[serde(default)]
    pub task_id: Option<RecursiveTaskId>,
    #[serde(default)]
    pub scheduler_run_id: Option<RecursiveSchedulerRunId>,
    #[serde(default)]
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub status: Option<RecursiveLiveAttemptStatus>,
    #[serde(default)]
    pub recovery_status: Option<RecursiveLiveRecoveryStatus>,
    #[serde(default = "default_true")]
    pub include_terminal: bool,
    #[serde(default)]
    pub include_status: bool,
    #[serde(default)]
    pub limit: Option<u32>,
}

impl Default for ListRecursiveLiveAttemptsParams {
    fn default() -> Self {
        Self {
            graph_id: None,
            task_id: None,
            scheduler_run_id: None,
            session_id: None,
            status: None,
            recovery_status: None,
            include_terminal: true,
            include_status: false,
            limit: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRecursiveLiveAttemptHeartbeatStatusParams {
    pub live_attempt_id: RecursiveLiveAttemptId,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListStaleRecursiveLiveAttemptHeartbeatsParams {
    #[serde(default)]
    pub graph_id: Option<RecursiveTaskGraphId>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GetRecursiveLiveInterruptStatusParams {
    #[serde(default)]
    pub interrupt_id: Option<RecursiveLiveInterruptId>,
    #[serde(default)]
    pub live_attempt_id: Option<RecursiveLiveAttemptId>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListRecursiveLiveInterruptsParams {
    #[serde(default)]
    pub live_attempt_id: Option<RecursiveLiveAttemptId>,
    #[serde(default)]
    pub cancellation_request_id: Option<RecursiveCancellationRequestId>,
    #[serde(default)]
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub status: Option<RecursiveLiveInterruptStatus>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRecursiveLiveRecoveryStatusParams {
    #[serde(default)]
    pub graph_id: Option<RecursiveTaskGraphId>,
    #[serde(default)]
    pub live_attempt_id: Option<RecursiveLiveAttemptId>,
    #[serde(default)]
    pub session_id: Option<Uuid>,
    #[serde(default = "default_true")]
    pub include_recovery_pending: bool,
    #[serde(default = "default_true")]
    pub include_deferred_graph: bool,
}

impl Default for GetRecursiveLiveRecoveryStatusParams {
    fn default() -> Self {
        Self {
            graph_id: None,
            live_attempt_id: None,
            session_id: None,
            include_recovery_pending: true,
            include_deferred_graph: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRecursiveLiveOutputValidationResultParams {
    #[serde(default)]
    pub validation_id: Option<RecursiveLiveOutputValidationId>,
    #[serde(default)]
    pub live_attempt_id: Option<RecursiveLiveAttemptId>,
    #[serde(default = "default_true")]
    pub latest: bool,
    #[serde(default = "default_true")]
    pub include_issues: bool,
    #[serde(default)]
    pub include_normalized_output: bool,
    #[serde(default)]
    pub include_validation_report: bool,
}

impl Default for GetRecursiveLiveOutputValidationResultParams {
    fn default() -> Self {
        Self {
            validation_id: None,
            live_attempt_id: None,
            latest: true,
            include_issues: true,
            include_normalized_output: false,
            include_validation_report: false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListRecursiveLiveOutputValidationResultsParams {
    #[serde(default)]
    pub graph_id: Option<RecursiveTaskGraphId>,
    #[serde(default)]
    pub task_id: Option<RecursiveTaskId>,
    #[serde(default)]
    pub scheduler_run_id: Option<RecursiveSchedulerRunId>,
    #[serde(default)]
    pub attempt_id: Option<RecursiveAttemptId>,
    #[serde(default)]
    pub live_attempt_id: Option<RecursiveLiveAttemptId>,
    #[serde(default)]
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub status: Option<RecursiveLiveOutputValidationStatus>,
    #[serde(default)]
    pub output_kind: Option<RecursiveLiveOutputKind>,
    #[serde(default)]
    pub include_issues: bool,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListRecursiveLiveValidationIssuesParams {
    #[serde(default)]
    pub validation_id: Option<RecursiveLiveOutputValidationId>,
    #[serde(default)]
    pub live_attempt_id: Option<RecursiveLiveAttemptId>,
    #[serde(default)]
    pub severity: Option<RecursiveLiveValidationIssueSeverity>,
    #[serde(default)]
    pub class: Option<RecursiveLiveValidationIssueClass>,
    #[serde(default)]
    pub code: Option<RecursiveLiveValidationIssueCode>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRecursiveLiveAttemptArtifactsParams {
    pub live_attempt_id: RecursiveLiveAttemptId,
    #[serde(default = "default_true")]
    pub include_prompt: bool,
    #[serde(default)]
    pub include_raw_output: bool,
    #[serde(default = "default_true")]
    pub include_normalized_output: bool,
    #[serde(default)]
    pub include_validation_report: bool,
    #[serde(default = "default_true")]
    pub include_diff: bool,
    #[serde(default = "default_true")]
    pub include_tests: bool,
    #[serde(default = "default_true")]
    pub include_produced_artifacts: bool,
}

/// Durable evidence of the most recent watchdog-triggered daemon restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonRestartRecordV1 {
    pub id: Uuid,
    pub observed_at: DateTime<Utc>,
    pub last_healthy_at: DateTime<Utc>,
    pub failed_probes: Vec<String>,
}

/// Response payload for GetHealthStatus.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthStatusResponse {
    pub persistence_queue_depth: usize,
    pub persistence_queue_capacity: usize,
    pub last_command_duration_ms: u64,
    pub project_cache_size: usize,
    #[serde(default)]
    pub project_cache_hits: u64,
    #[serde(default)]
    pub project_cache_misses: u64,
    #[serde(default)]
    pub last_poll_payload_bytes: u64,
    #[serde(default)]
    pub last_poll_event_count: u64,
    #[serde(default)]
    pub provider_claude_available: bool,
    #[serde(default)]
    pub provider_codex_available: bool,
    /// Whether the Codex CLI boundary and a supported Pioneer credential are available.
    #[serde(default)]
    pub provider_pioneer_available: bool,
    /// Whether the Codex CLI boundary and an OpenRouter credential are available.
    #[serde(default)]
    pub provider_openrouter_available: bool,
    /// Whether Codex CLI and a Bedrock bearer token are available.
    #[serde(default)]
    pub provider_bedrock_available: bool,
    #[serde(default)]
    pub provider_local_available: bool,
    #[serde(default)]
    pub provider_antigravity_available: bool,
    /// Whether `codex app-server` binary is available for bidirectional JSON-RPC sessions.
    #[serde(default)]
    pub provider_codex_app_server_available: bool,
    #[serde(default)]
    pub provider_harness_available: bool,
    /// Background queue: number of pending items.
    #[serde(default)]
    pub queue_pending: i64,
    /// Background queue: number of claimed (in-progress) items.
    #[serde(default)]
    pub queue_claimed: i64,
    /// Background queue: number of completed items.
    #[serde(default)]
    pub queue_completed: i64,
    /// Background queue: number of failed items.
    #[serde(default)]
    pub queue_failed: i64,
    /// Latest known account-level plan-window utilization per provider (V99,
    /// P1-B). Empty until a provider reports a rate-limit event.
    ///
    /// This is the COLD read: `GetHealthStatus` is already a `READ_VERB` and is
    /// already fetched on connect, so no new RPC verb is needed. The live path
    /// is `DaemonEvent::ProviderRateLimitUpdated`, which matters because
    /// provider availability is refreshed at connect time only and would
    /// otherwise never update during a running session.
    #[serde(default)]
    pub rate_limits: Vec<ProviderRateLimitSnapshot>,
    /// Most recent durable watchdog restart, when the daemon has imported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_daemon_restart: Option<DaemonRestartRecordV1>,
}

/// One plan window (e.g. the rolling five-hour or seven-day window) and how
/// much of it the account has consumed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderRateLimitWindow {
    /// Key from the provider's `unifiedWindows` map, e.g. `"five_hour"`.
    /// Not an enum: a provider adding a third window must be captured, not
    /// dropped on an unknown variant.
    pub window_key: String,
    /// Fraction of the window consumed, `0.0`–`1.0` as the provider reports it.
    pub utilization: f64,
    /// When the window resets, as the provider's raw unix epoch seconds.
    /// Verbatim provider data — deliberately not converted to RFC3339, unlike
    /// `ProviderRateLimitSnapshot::observed_at`, which is RSI's own timestamp.
    pub resets_at_epoch: Option<i64>,
}

/// Account-level rate-limit state for one provider.
///
/// Plan-window utilization is an account fact, not a session fact: every
/// concurrent session reports the same windows. It is therefore modelled as a
/// daemon-wide latest-wins snapshot rather than as session columns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderRateLimitSnapshot {
    pub provider: SessionProvider,
    /// Provider's own verdict for the last request, e.g. `"allowed"`.
    #[serde(default)]
    pub status: Option<String>,
    /// Which window the provider itself currently considers binding, e.g.
    /// `"five_hour"`. Snapshot-level, not per-window: it names one of the
    /// `windows` entries below.
    #[serde(default)]
    pub rate_limit_type: Option<String>,
    /// Overage posture, e.g. `"rejected"` when overage is unavailable.
    #[serde(default)]
    pub overage_status: Option<String>,
    #[serde(default)]
    pub is_using_overage: bool,
    /// When RSI observed this snapshot (RFC3339, nanosecond precision).
    pub observed_at: DateTime<Utc>,
    #[serde(default)]
    pub windows: Vec<ProviderRateLimitWindow>,
}

impl ProviderRateLimitSnapshot {
    /// The most-consumed window, which is the one that will throttle first and
    /// therefore the one worth showing the operator.
    pub fn peak_window(&self) -> Option<&ProviderRateLimitWindow> {
        self.windows
            .iter()
            .max_by(|a, b| a.utilization.total_cmp(&b.utilization))
    }
}

/// Response payload for GetDaemonCapabilities.
/// Version-gated feature flags the TUI can use to opt into newer RPC behaviors.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonCapabilities {
    /// Daemon protocol version (semver string, e.g. "1.2.0").
    pub version: String,
    /// Supports `GetConversationsSince` with incremental `since_sequence` polling.
    pub incremental_polling: bool,
    /// Supports `GetConversationsSince` with batched multi-session requests.
    pub batch_fetch: bool,
    /// Exposes `GetHealthStatus` RPC with queue depth and provider availability.
    pub health_status: bool,
    /// Supports `Subscribe` RPC for real-time push notifications over a dedicated connection.
    #[serde(default)]
    pub push_notifications: bool,
    /// Supports memory search, status, index, and read RPC methods.
    #[serde(default)]
    pub memory_search: bool,
    /// Supports workflow tracking (ListWorkflows, GetWorkflow RPCs).
    #[serde(default)]
    pub workflows: bool,
    /// Daemon supports stall detection and emits `session_stalled` push events.
    #[serde(default)]
    pub stall_detection: bool,
    /// Supports entity cards (project + user cards for context injection).
    #[serde(default)]
    pub entity_cards: bool,
    /// Daemon validates and canonicalizes session working_dir at RPC ingestion.
    /// Invalid paths return INVALID_PARAMS error instead of creating failed sessions.
    /// Symlinked working directories are resolved before project index lookup.
    #[serde(default)]
    pub workspace_safety: bool,
    /// Supports CancelRetry RPC for explicit retry cancellation.
    #[serde(default)]
    pub retry_management: bool,
    /// Supports app-server bidirectional JSON-RPC protocol for CodexAppServer sessions.
    #[serde(default)]
    pub app_server_protocol: bool,
    /// Supports issue tracker polling and dispatch.
    #[serde(default)]
    pub issue_tracker: bool,
    /// Supports lifecycle hook system with interception callbacks.
    #[serde(default)]
    pub lifecycle_hooks: bool,
    /// Supports tool permission guardrails (glob patterns, ALLOW/ASK/DENY).
    #[serde(default)]
    pub tool_permissions: bool,
    /// Supports context compression with offload markers and on-demand reload.
    #[serde(default)]
    pub context_offload: bool,
    /// Supports scheduled job management (create/list/toggle/delete/trigger).
    #[serde(default)]
    pub scheduler: bool,
    /// Daemon can allocate session-scoped git worktrees on launch and clean
    /// them up on terminal transition.
    #[serde(default)]
    pub sandbox: bool,
    /// Supports read-only recursive DAG graph/task/attempt/event/artifact inspection.
    #[serde(default)]
    pub recursive_dag_inspection: bool,
    /// Supports read-only recursive DAG scheduler run and run-event inspection.
    #[serde(default)]
    pub recursive_dag_run_inspection: bool,
    /// Supports read-only recursive DAG recovery pass/deferred/status inspection.
    #[serde(default)]
    pub recursive_dag_recovery_status: bool,
    /// Recovery control RPCs are enabled by daemon config.
    #[serde(default)]
    pub recursive_dag_recovery_control: bool,
    /// Fake scheduler control RPCs are enabled by daemon config.
    #[serde(default)]
    pub recursive_dag_scheduler_control: bool,
    /// Live scheduler control RPC is available for explicit ordinary recursive DAG graphs.
    #[serde(default)]
    pub recursive_dag_live_scheduler_control: bool,
    /// Cancellation control RPCs are enabled by daemon config.
    #[serde(default)]
    pub recursive_dag_cancellation_control: bool,
    /// Supports read-only recursive DAG live attempt, heartbeat, interrupt,
    /// recovery, artifact-linkage, scheduler-linkage, and operational-status inspection.
    #[serde(default)]
    pub recursive_dag_live_status_inspection: bool,
    /// Supports read-only recursive DAG live output validation and issue inspection.
    #[serde(default)]
    pub recursive_dag_live_validation_inspection: bool,
    /// Supports graph-guarded artifact lookup by artifact id.
    #[serde(default)]
    pub recursive_dag_artifact_lookup: bool,
    /// Supports graph-scoped paginated artifact summary lists.
    #[serde(default)]
    pub recursive_dag_artifact_list_pagination: bool,
    /// Supports bounded artifact content preview through the daemon.
    #[serde(default)]
    pub recursive_dag_artifact_preview_inspection: bool,
    /// Supports paginated recursive DAG validation result and issue lists.
    #[serde(default)]
    pub recursive_dag_validation_list_pagination: bool,
    /// Supports typed recursive DAG test inspection readbacks.
    #[serde(default)]
    pub recursive_dag_test_inspection: bool,
    /// Supports typed recursive DAG diff inspection readbacks.
    #[serde(default)]
    pub recursive_dag_diff_inspection: bool,
    /// Supports typed recursive DAG scheduler report inspection readbacks.
    #[serde(default)]
    pub recursive_dag_scheduler_report_inspection: bool,
    /// Supports policy-checked daemon opening of safe artifact URIs.
    #[serde(default)]
    pub recursive_dag_safe_artifact_uri_open: bool,
    /// Live recursive DAG execution is available through an explicit gated path.
    #[serde(default)]
    pub recursive_dag_live_execution: bool,
    /// Recursive DAG background scheduling is available. Always false in Phase 5A.5.
    #[serde(default)]
    pub recursive_dag_background_loop: bool,
    /// gv renders bridged recursive-graph origins (read-only). Phase-1 V2 gate;
    /// dormant in V0. Default false — nothing reachable by default.
    #[serde(default)]
    pub gv_render_recursive_origin: bool,
    /// gv info dashboard on right of graph view. Phase-1 V3 gate.
    /// Default false.
    #[serde(default)]
    pub gv_info_dashboard: bool,
}

impl Default for DaemonCapabilities {
    fn default() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            incremental_polling: true,
            batch_fetch: true,
            health_status: true,
            push_notifications: true,
            memory_search: true,
            workflows: true,
            stall_detection: true,
            entity_cards: true,
            workspace_safety: true,
            retry_management: true,
            app_server_protocol: true,
            issue_tracker: false, // set to true by daemon when configured
            lifecycle_hooks: true,
            tool_permissions: true,
            context_offload: true,
            scheduler: false, // set to true by daemon when scheduler is active
            sandbox: true,
            recursive_dag_inspection: true,
            recursive_dag_run_inspection: true,
            recursive_dag_recovery_status: true,
            recursive_dag_recovery_control: false,
            recursive_dag_scheduler_control: false,
            recursive_dag_live_scheduler_control: false,
            recursive_dag_cancellation_control: false,
            recursive_dag_live_status_inspection: false,
            recursive_dag_live_validation_inspection: false,
            recursive_dag_artifact_lookup: false,
            recursive_dag_artifact_list_pagination: false,
            recursive_dag_artifact_preview_inspection: false,
            recursive_dag_validation_list_pagination: false,
            recursive_dag_test_inspection: false,
            recursive_dag_diff_inspection: false,
            recursive_dag_scheduler_report_inspection: false,
            recursive_dag_safe_artifact_uri_open: false,
            recursive_dag_live_execution: false,
            recursive_dag_background_loop: false,
            gv_render_recursive_origin: false,
            gv_info_dashboard: false,
        }
    }
}

/// RPC method name constants for memory operations.
pub const METHOD_MEMORY_SEARCH: &str = "MemorySearch";
pub const METHOD_MEMORY_STATUS: &str = "MemoryStatus";
pub const METHOD_MEMORY_INDEX: &str = "MemoryIndex";
pub const METHOD_MEMORY_READ: &str = "MemoryRead";
pub const METHOD_LIST_RECURSIVE_TEST_SUMMARIES: &str = "ListRecursiveTestSummaries";
pub const METHOD_GET_RECURSIVE_TEST_DETAIL: &str = "GetRecursiveTestDetail";
pub const METHOD_LIST_RECURSIVE_DIFF_SUMMARIES: &str = "ListRecursiveDiffSummaries";
pub const METHOD_GET_RECURSIVE_DIFF_DETAIL: &str = "GetRecursiveDiffDetail";
pub const METHOD_GET_RECURSIVE_DIFF_FILE_HUNKS: &str = "GetRecursiveDiffFileHunks";
pub const METHOD_GET_RECURSIVE_SCHEDULER_REPORT_SUMMARY: &str =
    "GetRecursiveSchedulerReportSummary";
pub const METHOD_GET_RECURSIVE_SCHEDULER_REPORT_DETAIL: &str = "GetRecursiveSchedulerReportDetail";

/// RPC method name constant for `StartChainedWorkflow` (`master_improve` loop).
pub const METHOD_START_CHAINED_WORKFLOW: &str = "StartChainedWorkflow";

/// A single memory search result returned via RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySearchResult {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub score: f64,
    pub snippet: String,
    pub source: String,
}

/// Status of the memory subsystem returned via RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryProviderStatus {
    pub enabled: bool,
    pub backend: String,
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    pub search_mode: String,
    pub file_count: usize,
    pub chunk_count: usize,
    pub dirty: bool,
    pub memory_dir: String,
    pub db_path: String,
    #[serde(default)]
    pub vector_available: bool,
    #[serde(default)]
    pub fts_available: bool,
    #[serde(default)]
    pub cache_entries: usize,
}

/// Parameters for `GenerateText` RPC — thin wrapper over Ollama `/api/generate`.
/// Used by the TUI for AI command / chat / grammar paths that previously went
/// through the `ollama run` subprocess.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerateTextParams {
    pub system: String,
    pub prompt: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<crate::types::SessionProvider>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
}

/// Response for `GenerateText` RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerateTextResponse {
    pub text: String,
}

/// Parameters for `CompilePrompt` RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompilePromptParams {
    /// The raw user input to compile.
    pub input: String,
    /// Optional explicit model override; daemon falls back to
    /// `RuntimeConfig::prompt_compile_model_local` when None.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<crate::types::SessionProvider>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    /// TUI-stable identity for supersede cancellation. A second CompilePrompt
    /// request from the same caller aborts the first.
    pub caller_id: Uuid,
}

/// Response from `CompilePrompt` RPC.
///
/// When `cached` is `Some(...)`, the result is a cache hit and the caller may
/// render immediately (the daemon also emits a synthetic chunk+completed on
/// the bus for consistency). When `None`, the caller must listen for
/// `CompilePromptCompleted` / `CompilePromptFailed` events on the Subscribe
/// stream, filtered by `request_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompilePromptResponse {
    pub request_id: Uuid,
    #[serde(default)]
    pub cached: Option<crate::prompt_compile::CompileResult>,
}

/// Event Bus Events (for pub/sub notifications)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusEvent {
    pub event_type: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub data: serde_json::Value,
}

/// Parameters for GetWorkflowDefinition RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetWorkflowDefinitionParams {
    pub workflow_id: Uuid,
}

/// Response for GetWorkflowDefinition RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetWorkflowDefinitionResponse {
    pub document: WorkflowDocument,
}

/// Parameters for UpsertWorkflowDefinition RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpsertWorkflowDefinitionParams {
    pub document: WorkflowDocument,
}

/// Response for UpsertWorkflowDefinition RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpsertWorkflowDefinitionResponse {
    pub document: WorkflowDocument,
}

/// Parameters for GenerateWorkflow RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerateWorkflowParams {
    pub intent: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub use_cache: Option<bool>,
    /// Complexity hint: "simple" or "complex" or None (auto-detect).
    #[serde(default)]
    pub complexity: Option<String>,
}

/// Response for GenerateWorkflow RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerateWorkflowResponse {
    pub workflow: serde_json::Value, // WorkflowDefinition as JSON Value
    pub reasoning: String,
    #[serde(default)]
    pub cache_hit: bool,
}

/// Parameters for RefineWorkflow RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefineWorkflowParams {
    pub workflow: serde_json::Value, // Current WorkflowDefinition as JSON
    pub instruction: String,
    #[serde(default)]
    pub context: Option<serde_json::Value>, // RefinementContext
}

/// Response for RefineWorkflow RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefineWorkflowResponse {
    pub workflow: serde_json::Value,
    pub changes_made: Vec<String>,
    pub valid: bool,
}

/// Parameters for ExecuteWorkflow RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteWorkflowParams {
    pub workflow_id: Uuid,
    pub workflow: serde_json::Value,
    #[serde(default)]
    pub input: Option<serde_json::Value>,
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// Project ID for resolving default working directory.
    #[serde(default)]
    pub project_id: Option<Uuid>,
    /// Explicit working directory for the workflow execution.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Parent container (Group or Epic) under which spawned sessions are
    /// placed. When None, sessions spawn as top-level orphans (legacy
    /// behavior; preserves backward compat with pre-P1.12 callers).
    #[serde(default)]
    pub parent_id: Option<Uuid>,
}

/// Response for ExecuteWorkflow RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteWorkflowResponse {
    pub execution_id: Uuid,
    pub workflow_id: Uuid,
    pub accepted_at: DateTime<Utc>,
    #[serde(default)]
    pub dry_run: bool,
}

/// Parameters for `StartChainedWorkflow` — explicit chain start for the
/// `master_improve` convergence loop.
///
/// v1 only accepts `topology_name == "master_improve"`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartChainedWorkflowParams {
    pub workflow_id: Uuid,
    /// Starter-template name. v1 only accepts `"master_improve"`.
    pub topology_name: String,
    /// User's initial goal. Stamped into entry node's instructions for iteration 0.
    pub initial_goal: String,
    #[serde(default)]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Per-chain hard iteration cap. None = daemon default.
    #[serde(default)]
    pub cap_override: Option<u32>,
}

/// Response for `StartChainedWorkflow` RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartChainedWorkflowResponse {
    pub chain_id: Uuid,
    pub first_execution_id: Uuid,
    pub accepted_at: DateTime<Utc>,
}

/// Parameters for GetWorkflowExecution RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetWorkflowExecutionParams {
    pub execution_id: Uuid,
}

/// Response for GetWorkflowExecution RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetWorkflowExecutionResponse {
    pub lookup: WorkflowExecutionLookup,
}

/// Parameters for InterruptWorkflowExecution RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterruptWorkflowExecutionParams {
    pub execution_id: Uuid,
}

/// Response for InterruptWorkflowExecution RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterruptWorkflowExecutionResponse {
    pub execution_id: Uuid,
    pub status: WorkflowExecutionStatus,
    pub interrupt_requested_at: DateTime<Utc>,
}

/// Operator resolution of a preserved-work topology attempt (#634, plan §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopologyAttemptAction {
    Inspect,
    Accept,
    Retry,
    Discard,
}

impl TopologyAttemptAction {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inspect => "inspect",
            Self::Accept => "accept",
            Self::Retry => "retry",
            Self::Discard => "discard",
        }
    }
}

/// Parameters for the operator-only `ResolveTopologyAttempt` RPC. The same
/// schema is reused by the T4 agent verb; caller identity is never a field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveTopologyAttemptParams {
    pub execution_id: Uuid,
    pub attempt_id: Uuid,
    pub action: TopologyAttemptAction,
    pub expected_row_version: i64,
    pub idempotency_key: String,
    /// Full 40-hex preserved commit; required iff `action=discard`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm_preserved_commit: Option<String>,
}

/// Bounded view of one topology node attempt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TopologyAttemptSummary {
    pub attempt_id: Uuid,
    pub node_id: String,
    pub iteration: u32,
    pub attempt_no: u32,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    pub base_commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserved_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserved_commit: Option<String>,
}

/// `inspect` report: at most 64 dirty paths and 64 diffstat rows.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TopologyAttemptInspection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    pub base_commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preserved_commit: Option<String>,
    #[serde(default)]
    pub dirty_paths: Vec<String>,
    #[serde(default)]
    pub diffstat: Vec<String>,
}

/// Response for `ResolveTopologyAttempt`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolveTopologyAttemptResponse {
    pub attempt: TopologyAttemptSummary,
    pub execution_status: WorkflowExecutionStatus,
    pub row_version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<TopologyAttemptInspection>,
    #[serde(default)]
    pub deduplicated: bool,
}

fn default_true() -> bool {
    true
}

/// Parameters for GetEntityCard RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetEntityCardParams {
    pub entity_type: String,
    pub entity_id: String,
}

/// Parameters for SetEntityCard RPC (full replacement).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetEntityCardParams {
    pub entity_type: String,
    pub entity_id: String,
    pub facts: Vec<String>,
}

// Permission Rules CRUD
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListPermissionRulesParams {
    #[serde(default)]
    pub project_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListPermissionRulesResponse {
    pub rules: Vec<PermissionRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatePermissionRuleParams {
    pub tool_pattern: String,
    pub level: PermissionLevel,
    pub scope: PermissionScope,
    #[serde(default)]
    pub scope_id: Option<Uuid>,
    pub priority: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletePermissionRuleParams {
    pub id: i64,
}

// Offload management
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReloadOffloadParams {
    pub session_id: Uuid,
    pub event_sequence: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReloadOffloadResponse {
    pub original_content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetOffloadMetadataParams {
    pub session_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetOffloadMetadataResponse {
    pub entries: Vec<OffloadEntry>,
}

/// Parameters for the QueryMemory RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryMemoryParams {
    /// The natural-language query.
    pub query: String,
    /// Optional project scope (only search within this project's sessions).
    #[serde(default)]
    pub project_id: Option<Uuid>,
    /// Optional conversation context from prior turns in the same overlay session.
    /// Each entry is (role, content) where role is "user" or "assistant".
    #[serde(default)]
    pub conversation_history: Vec<(String, String)>,
}

/// A single source citation in a dialectic response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DialecticSource {
    /// What kind of source: "memory", "session", "project".
    pub kind: String,
    /// Human-readable label (e.g., file path, session title).
    pub label: String,
    /// Optional detail (e.g., score, line range).
    #[serde(default)]
    pub detail: Option<String>,
}

/// Response from the QueryMemory RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryMemoryResponse {
    /// The synthesized answer text.
    pub answer: String,
    /// Sources consulted to produce the answer.
    pub sources: Vec<DialecticSource>,
    /// Number of tool calls the agent made.
    pub tool_calls: u32,
}

// --- Scheduled Job RPC params ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateScheduledJobParams {
    pub name: String,
    pub message: String,
    pub schedule: crate::types::ScheduleSpec,
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    #[serde(default)]
    pub provider: Option<crate::types::SessionProvider>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub project_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateScheduledJobParams {
    pub id: Uuid,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub schedule: Option<crate::types::ScheduleSpec>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub working_dir: Option<Option<PathBuf>>,
    #[serde(default)]
    pub provider: Option<Option<crate::types::SessionProvider>>,
    #[serde(default)]
    pub model: Option<Option<String>>,
    #[serde(default)]
    pub project_id: Option<Option<Uuid>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteScheduledJobParams {
    pub id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToggleScheduledJobParams {
    pub id: Uuid,
}

// --- Hierarchy RPC params (RSI hierarchical session organization) ---

/// Parameters for `CreateContainer` RPC. Creates a Group/Epic organizational
/// node — never spawns a provider subprocess. `parent_id = None` places the
/// node at the root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateContainerParams {
    /// Container kind. Must be `Group` or `Epic`; the daemon rejects leaf kinds.
    pub kind: SessionKind,
    /// Display name for the container (stored as `query`/`title`).
    pub name: String,
    /// Hierarchical parent. `None` = top-level. Must satisfy `legal_children`.
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    /// Optional project assignment.
    #[serde(default)]
    pub project_id: Option<Uuid>,
    // ─── P1.6: tag + topology ──────────────────────────────
    /// Mandatory non-empty tag set. Same normalization + rejection rules
    /// as LaunchSessionParams. NOT serde(default).
    pub tags: Vec<String>,
    /// Optional topology to attach to this container. Only meaningful when
    /// `kind == SessionKind::Epic`. Daemon rejects (`InvalidParam`) when
    /// `topology_id.is_some() && kind != Epic`. Persisted to
    /// `session.workflow_id` (no new column needed).
    #[serde(default)]
    pub topology_id: Option<Uuid>,
}

/// Parameters for `SetSessionParent` RPC. Reparents a session in the
/// hierarchy. `new_parent_id = None` moves the session to the root.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetSessionParentParams {
    pub session_id: Uuid,
    #[serde(default)]
    pub new_parent_id: Option<Uuid>,
}

/// Parameters for `ListSessionChildren` RPC. Returns the direct children of
/// a hierarchy parent (or top-level rows when `parent_id = None`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListSessionChildrenParams {
    #[serde(default)]
    pub parent_id: Option<Uuid>,
}

/// Parameters for `SetEpicLead` RPC. Sets or clears the lead-session
/// pointer on a Group/Epic container row.
///
/// `new_lead_session_id = Some(uuid)` requires the target session to
/// satisfy `parent_id == epic_id` and `is_leaf_kind(target.session_kind)`.
/// `new_lead_session_id = None` clears the pointer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetEpicLeadParams {
    pub epic_id: Uuid,
    #[serde(default)]
    pub new_lead_session_id: Option<Uuid>,
}

// ─── Topology RPC params (P1.4) ──────────────────────────────────────────
// DB-stored named topology templates owned by Epics. Mutations are
// daemon-mediated: every Create/Update validates DAG well-formedness,
// kind legality, name uniqueness, and the MAX_ITERATIONS = 32 cap.

/// Parameters for `ListTopologies` RPC. Returns all named topologies,
/// optionally filtered by a case-sensitive name prefix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListTopologiesParams {
    #[serde(default)]
    pub name_prefix: Option<String>,
}

/// Parameters for `CreateTopology` RPC.
///
/// The daemon validates `definition` (DAG well-formedness via Kahn's
/// algorithm, kind legality against `legal_children(Some(Epic))`,
/// `MAX_ITERATIONS` cap, loop termination guards) before insert.
/// Returns `{ id: Uuid }` of the new row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateTopologyParams {
    pub name: String,
    pub definition: TopologyDefinition,
}

/// Parameters for `UpdateTopology` RPC. Either or both of `name` and
/// `definition` may be `None` (no-op for that field). When `name` is
/// `Some`, the daemon enforces uniqueness against rows other than `id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateTopologyParams {
    pub id: Uuid,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub definition: Option<TopologyDefinition>,
}

/// Parameters for `DeleteTopology` RPC. The daemon rejects when any Epic
/// row's `workflow_id` still references this topology. The operator must
/// clear those references first.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteTopologyParams {
    pub id: Uuid,
}

/// Parameters for `GetTopology` RPC. Returns the full `Topology` row or
/// an `InvalidParam` error if no row matches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetTopologyParams {
    pub id: Uuid,
}

/// Parameters for `ExecuteTopology` RPC (P1.10).
///
/// Loads the named topology from the `topologies` table, validates it,
/// bridges it to a `WorkflowDefinition`, and executes it via the
/// `graph_runner`. Acyclic topologies only — loop-bearing topologies
/// (any edge with `loop_edge: true`) are rejected with `InvalidParam`
/// until P1.11 ships the loop-aware executor.
///
/// `inputs` is `serde_json::Value` (not `Option<Value>`). The RPC handler
/// converts to `Option<Value>` by checking `is_null()` before passing to
/// the executor, so callers may omit the field entirely (JSON null default).
///
/// Returns `{ "execution_id": Uuid }` — poll via `GetWorkflowExecution`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteTopologyParams {
    pub topology_id: Uuid,
    /// Project ID for resolving the default working directory.
    #[serde(default)]
    pub project_id: Option<Uuid>,
    /// Optional input value forwarded to the executor. Null = no input.
    #[serde(default)]
    pub inputs: serde_json::Value,
    /// Parent container (Group or Epic) under which spawned sessions are
    /// placed. When None, sessions spawn as top-level orphans (legacy
    /// behavior; preserves backward compat with pre-P1.12 callers).
    #[serde(default)]
    pub parent_id: Option<Uuid>,
}

// ─── Tag RPC params (P1.5) ──────────────────────────────────────────

/// Parameters for `UpdateSessionTags` RPC. Replaces the full tag set for
/// a session. Rejected with `InvalidParam` if `tags` is empty.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateSessionTagsParams {
    pub session_id: Uuid,
    pub tags: Vec<String>,
}

/// Parameters for `AddSessionTag` RPC. Adds a single tag (idempotent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddSessionTagParams {
    pub session_id: Uuid,
    pub tag: String,
}

/// Parameters for `RemoveSessionTag` RPC. Removes a single tag (idempotent).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoveSessionTagParams {
    pub session_id: Uuid,
    pub tag: String,
}

/// Parameters for `ListTags` RPC. Returns tag-with-count rows, ordered by
/// count descending then alphabetically. Optional `prefix` filters by tag
/// prefix; optional `project_id` restricts to sessions in a project.
/// Capped at 100 rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListTagsParams {
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub project_id: Option<Uuid>,
}

/// A tag with its session-count across the filtered scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TagWithCount {
    pub tag: String,
    pub count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveCompiledPromptParams {
    #[serde(default)]
    pub session_id: Option<Uuid>,
    pub original_input: String,
    pub compiled_output: String,
    pub contract_status: String,
    pub layer_semantic: bool,
    pub layer_syntactic: bool,
    pub layer_deictic: bool,
    pub layer_discourse: bool,
    pub layer_pragmatic: bool,
    pub accepted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListCompiledPromptsParams {
    #[serde(default)]
    pub session_id: Option<Uuid>,
    #[serde(default = "default_compiled_prompt_limit")]
    pub limit: u32,
}

fn default_compiled_prompt_limit() -> u32 {
    100
}

// ─── Index status sidecar RPC params (P1.9) ─────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateIndexStatusParams {
    pub project: String,
    pub ticket_id: String,
    pub status: IndexStatusValue,
    #[serde(default)]
    pub last_shipped_commit: Option<String>,
    #[serde(default)]
    pub last_shipped_branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetIndexStatusParams {
    pub project: String,
}

// ─── Usage stats aggregate RPC params (T8) ──────────────────────────────────

/// Params for `GetUsageStats`. `project_id` is `#[serde(default)]` so an
/// object missing the key (`{}`) deserializes to `project_id: None`.
///
/// A derived struct `Deserialize` impl does NOT accept a bare top-level
/// JSON `null` regardless of `#[serde(default)]` placement (field or
/// container) — that attribute only fills in fields absent from a *present*
/// object; `null` itself is a different token the derived visitor rejects.
/// Since `RpcRequest.params` defaults to `Value::Null` (repo rule), the
/// null-tolerant fallback lives at the call site
/// (`handle_get_usage_stats`), which checks `request.params.is_null()`
/// before deserializing and uses `Self::default()` directly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GetUsageStatsParams {
    #[serde(default)]
    pub project_id: Option<Uuid>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GetModelControlStatusParams {
    #[serde(default = "default_model_control_recent_limit")]
    pub recent_limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateModelControlPolicyParams {
    pub mode: crate::model_control::ModelControlMode,
    #[serde(default = "default_true")]
    pub interrupt_active: bool,
    #[serde(default)]
    pub replace_policies: bool,
    #[serde(default)]
    pub policies: Vec<crate::model_control::ModelBudgetPolicy>,
    #[serde(default)]
    pub circuit_updates: Vec<ModelCircuitUpdate>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListModelInvocationsParams {
    #[serde(default = "default_model_control_list_limit")]
    pub limit: u32,
    #[serde(default)]
    pub active_only: bool,
    #[serde(default)]
    pub purpose: Option<crate::model_control::ModelInvocationPurpose>,
    #[serde(default)]
    pub session_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelModelInvocationParams {
    pub invocation_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCircuitUpdate {
    pub scope_kind: crate::model_control::BudgetScopeKind,
    #[serde(default)]
    pub scope_id: Option<String>,
    pub state: String,
    pub reason: String,
    #[serde(default)]
    pub error_class: Option<String>,
    #[serde(default)]
    pub cooldown_secs: Option<u64>,
}

fn default_model_control_recent_limit() -> u32 {
    8
}

fn default_model_control_list_limit() -> u32 {
    50
}

/// RPC params for `CreateIssue` (Track C slice C3, local issue tracker,
/// V72 `issues` table). Operator-only surface — deliberately no
/// `created_by_session_id`: agent-attributed issue creation is a separate
/// slice (C5) via the guarded `AgentControlHandle`, never this verb.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateIssueParams {
    pub project_id: Uuid,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub priority: Option<u8>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub assignee: Option<String>,
}

/// Strict create-only issue request available to a session-attributed agent.
/// The caller and creator are deliberately absent: transports bind them
/// server-side before this request reaches the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentCreateIssueParams {
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub priority: Option<u8>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    pub idempotency_key: String,
}

/// Result envelope shared by tokened RPC and native agent transports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCreateIssueResult {
    pub issue: Issue,
    pub deduplicated: bool,
}

/// Strict lead-scoped Issue list request. The project is intentionally absent:
/// it is derived from the authenticated caller's persisted owning Epic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentListIssuesRequestV1 {
    #[serde(default)]
    pub status: Option<IssueStatus>,
    #[serde(default)]
    pub archive: IssueArchiveFilterV1,
    #[serde(default)]
    pub cursor: Option<crate::types::IssueListCursorV1>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub ready: bool,
}

impl Default for AgentListIssuesRequestV1 {
    fn default() -> Self {
        Self {
            status: None,
            archive: IssueArchiveFilterV1::Active,
            cursor: None,
            limit: None,
            ready: false,
        }
    }
}

impl AgentListIssuesRequestV1 {
    pub fn validated_limit(&self) -> Result<u32, String> {
        if let Some(cursor) = &self.cursor {
            if cursor.display_number < 1 || cursor.issue_id.is_nil() {
                return Err("Issue list cursor is invalid".to_string());
            }
        }
        let limit = self.limit.unwrap_or(64);
        if !(1..=256).contains(&limit) {
            return Err("Issue list limit must be 1..=256".to_string());
        }
        Ok(limit)
    }
}

/// Strict lead-scoped Issue lookup request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentGetIssueRequestV1 {
    pub issue_id: Uuid,
}

impl AgentGetIssueRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_non_nil_issue_id(self.issue_id)
    }
}

/// Strict lead-scoped content mutation. Nullable fields use explicit clear
/// flags because serde's ordinary `Option<Option<T>>` wire shape is ambiguous.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentUpdateIssueRequestV1 {
    pub issue_id: Uuid,
    pub expected_row_version: i64,
    pub idempotency_key: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    #[serde(default)]
    pub priority: Option<u8>,
    #[serde(default)]
    pub clear_priority: bool,
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub clear_assignee: bool,
}

impl AgentUpdateIssueRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_issue_mutation_identity(
            self.issue_id,
            self.expected_row_version,
            &self.idempotency_key,
        )?;
        self.content_patch().validate()
    }

    #[must_use]
    pub fn content_patch(&self) -> crate::types::IssueContentPatchV1 {
        crate::types::IssueContentPatchV1 {
            title: self.title.clone(),
            body: self.body.clone(),
            labels: self.labels.clone(),
            priority: self.priority,
            clear_priority: self.clear_priority,
            assignee: self.assignee.clone(),
            clear_assignee: self.clear_assignee,
        }
    }

    pub fn semantic_request(&self) -> Result<crate::types::IssueSemanticRequestV1, String> {
        self.validate()?;
        Ok(crate::types::IssueSemanticRequestV1::new(
            crate::types::IssueSemanticOperationV1::ContentUpdated {
                issue_id: self.issue_id,
                expected_row_version: self.expected_row_version,
                patch: self.content_patch(),
            },
        ))
    }
}

/// The one explicit status/lifecycle mutation available to owning-Epic leads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentUpdateIssueStatusRequestV1 {
    pub issue_id: Uuid,
    pub status: IssueStatus,
    pub expected_row_version: i64,
    pub idempotency_key: String,
}

impl AgentUpdateIssueStatusRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_issue_mutation_identity(
            self.issue_id,
            self.expected_row_version,
            &self.idempotency_key,
        )
    }

    pub fn semantic_request(&self) -> Result<crate::types::IssueSemanticRequestV1, String> {
        self.validate()?;
        Ok(crate::types::IssueSemanticRequestV1::new(
            crate::types::IssueSemanticOperationV1::StatusUpdated {
                issue_id: self.issue_id,
                expected_row_version: self.expected_row_version,
                status: self.status,
            },
        ))
    }
}

/// Strict archive request. It remains distinct from restore so an audit event
/// never has to infer intent from a boolean.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentArchiveIssueRequestV1 {
    pub issue_id: Uuid,
    pub expected_row_version: i64,
    pub idempotency_key: String,
}

impl AgentArchiveIssueRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_issue_mutation_identity(
            self.issue_id,
            self.expected_row_version,
            &self.idempotency_key,
        )
    }

    pub fn semantic_request(&self) -> Result<crate::types::IssueSemanticRequestV1, String> {
        self.validate()?;
        Ok(crate::types::IssueSemanticRequestV1::new(
            crate::types::IssueSemanticOperationV1::Archived {
                issue_id: self.issue_id,
                expected_row_version: self.expected_row_version,
            },
        ))
    }
}

/// Strict restore request, intentionally not a boolean option on archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRestoreIssueRequestV1 {
    pub issue_id: Uuid,
    pub expected_row_version: i64,
    pub idempotency_key: String,
}

impl AgentRestoreIssueRequestV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_issue_mutation_identity(
            self.issue_id,
            self.expected_row_version,
            &self.idempotency_key,
        )
    }

    pub fn semantic_request(&self) -> Result<crate::types::IssueSemanticRequestV1, String> {
        self.validate()?;
        Ok(crate::types::IssueSemanticRequestV1::new(
            crate::types::IssueSemanticOperationV1::Restored {
                issue_id: self.issue_id,
                expected_row_version: self.expected_row_version,
            },
        ))
    }
}

/// Immutable result receipt for every V95 lead-scoped Issue mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentIssueMutationResultV1 {
    pub issue: Issue,
    pub event: crate::types::IssueEventV1,
    pub deduplicated: bool,
}

/// Typed safe error codes for the lead-scoped Issue surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentIssueErrorCodeV1 {
    InvalidRequest,
    AuthorityDenied,
    NotFoundInScope,
    IdempotencyConflict,
    StaleVersion,
    NoSemanticChange,
    InvalidTransition,
    Archived,
    NotArchived,
    StorageFailure,
}

impl AgentIssueErrorCodeV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::AuthorityDenied => "authority_denied",
            Self::NotFoundInScope => "not_found_in_scope",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::StaleVersion => "stale_version",
            Self::NoSemanticChange => "no_semantic_change",
            Self::InvalidTransition => "invalid_transition",
            Self::Archived => "archived",
            Self::NotArchived => "not_archived",
            Self::StorageFailure => "storage_failure",
        }
    }
}

/// Closed, non-diagnostic classification for a malformed guarded Issue request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentIssueValidationClassV1 {
    MissingField,
    UnknownField,
    InvalidField,
    InvalidShape,
}

/// Public top-level fields that may be named by a guarded Issue validation hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentIssueValidationFieldV1 {
    Status,
    Archive,
    Cursor,
    Limit,
    Ready,
    IssueId,
    ExpectedRowVersion,
    IdempotencyKey,
    Title,
    Body,
    Labels,
    Priority,
    ClearPriority,
    Assignee,
    ClearAssignee,
    AfterSequence,
}

/// One bounded, allowlisted hint for a malformed guarded Issue request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentIssueValidationV1 {
    pub class: AgentIssueValidationClassV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<AgentIssueValidationFieldV1>,
}

/// Safe wire envelope shared by RPC and both native provider transports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentIssueErrorV1 {
    pub code: AgentIssueErrorCodeV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_row_version: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_row_version: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<AgentIssueValidationV1>,
    pub next_action: String,
}

/// Operator-only bounded immutable Issue-history read uses exactly the same
/// request/page contract as the attributed lead surface.
pub type ListIssueEventsParams = IssueEventPageRequestV1;
pub type ListIssueEventsResult = IssueEventPageV1;

fn validate_non_nil_issue_id(issue_id: Uuid) -> Result<(), String> {
    if issue_id.is_nil() {
        Err("issue_id must not be nil".to_string())
    } else {
        Ok(())
    }
}

fn validate_issue_mutation_identity(
    issue_id: Uuid,
    expected_row_version: i64,
    idempotency_key: &str,
) -> Result<(), String> {
    validate_non_nil_issue_id(issue_id)?;
    if expected_row_version < 1 {
        return Err("expected_row_version must be positive".to_string());
    }
    if idempotency_key.is_empty()
        || idempotency_key.len() > 128
        || idempotency_key.as_bytes().contains(&0)
    {
        return Err("idempotency_key must be 1..=128 NUL-free bytes".to_string());
    }
    Ok(())
}

/// RPC params identifying a single issue by id. Shared by `GetIssue` and
/// any other issue verb whose only input is the identity key.
#[derive(Debug, Deserialize)]
pub struct IssueIdParams {
    pub issue_id: Uuid,
}

/// Operator-only bounded read key for `GetIdea`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetIdeaParams {
    pub idea_id: Uuid,
}

/// Strict operator-only `ProgramRun` creation request.
pub type CreateProgramRunParams = CreateProgramRunRequestV1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateProgramRunResult {
    pub mutation: ProgramRunMutationResultV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetProgramRunParams {
    pub program_run_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListProgramRunsParams {
    #[serde(default)]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub idea_id: Option<Uuid>,
    #[serde(default)]
    pub status: Option<ProgramRunStatusV1>,
    #[serde(default)]
    pub cursor: Option<ProgramRunPageCursorV1>,
    #[serde(default)]
    pub limit: Option<u32>,
}

impl ListProgramRunsParams {
    /// Validate and return the bounded page size.
    ///
    /// # Errors
    ///
    /// Returns a wire-safe message when the requested limit is outside 1..=200.
    pub fn validated_limit(&self) -> Result<u32, String> {
        let limit = self.limit.unwrap_or(50);
        if !(1..=200).contains(&limit) {
            return Err("ProgramRun list limit must be 1..=200".to_string());
        }
        Ok(limit)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListProgramRunsResult {
    pub items: Vec<ProgramRunV1>,
    #[serde(default)]
    pub next_cursor: Option<ProgramRunPageCursorV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListProgramRunTransitionsParams {
    pub program_run_id: Uuid,
    #[serde(default)]
    pub after_sequence: Option<u64>,
    #[serde(default)]
    pub limit: Option<u32>,
}

impl ListProgramRunTransitionsParams {
    /// Validate and return the bounded transition-page size.
    ///
    /// # Errors
    ///
    /// Returns a wire-safe message when the requested limit is outside 1..=200.
    pub fn validated_limit(&self) -> Result<u32, String> {
        let limit = self.limit.unwrap_or(50);
        if !(1..=200).contains(&limit) {
            return Err("ProgramRun transition limit must be 1..=200".to_string());
        }
        Ok(limit)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListProgramRunTransitionsResult {
    pub page: ProgramRunTransitionPageV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetProgramRunOperationalStatusParams {
    pub program_run_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetProgramRunOperationalStatusResult {
    pub status: ProgramRunOperationalStatusV1,
}

pub type CancelProgramRunParams = CancelProgramRunRequestV1;
pub type ResumeBlockedProgramRunParams = ResumeBlockedProgramRunRequestV1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReconcileProgramRunsParams {
    #[serde(default)]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub cursor: Option<ProgramRunPageCursorV1>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub time_budget_ms: Option<u32>,
    #[serde(default)]
    pub dry_run: bool,
}

impl ReconcileProgramRunsParams {
    /// Validate and return the reconciliation row and time bounds.
    ///
    /// # Errors
    ///
    /// Returns a wire-safe message when either bound is outside its closed range.
    pub fn validated_bounds(&self) -> Result<(u32, u32), String> {
        let limit = self.limit.unwrap_or(64);
        let time_budget_ms = self.time_budget_ms.unwrap_or(500);
        if !(1..=256).contains(&limit) {
            return Err("ProgramRun reconciliation limit must be 1..=256".to_string());
        }
        if !(1..=2_000).contains(&time_budget_ms) {
            return Err("ProgramRun reconciliation time_budget_ms must be 1..=2000".to_string());
        }
        Ok((limit, time_budget_ms))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileProgramRunsResult {
    pub page: ProgramRunReconciliationPageV1,
}

/// RPC params for `UpdateIssueStatus`.
#[derive(Debug, Deserialize)]
pub struct UpdateIssueStatusParams {
    pub issue_id: Uuid,
    pub status: IssueStatus,
}

/// RPC params for `AddIssueDep` / `RemoveIssueDep`. Read as "`issue_id`
/// depends on (is blocked by) `depends_on_id`", matching `IssueDep`'s and
/// the store's own parameter naming.
#[derive(Debug, Deserialize)]
pub struct IssueDepParams {
    pub issue_id: Uuid,
    pub depends_on_id: Uuid,
}

/// RPC params for `ListReadyIssues`. `None` limit = unbounded.
#[derive(Debug, Default, Deserialize)]
pub struct ListReadyIssuesParams {
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub project_id: Option<Uuid>,
}

/// Operator-only, strict, link-once request. Project and actor authority are
/// deliberately derived from the persisted Issue and RPC boundary.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkIssueToIdeaParams {
    pub issue_id: Uuid,
    pub idea_id: Uuid,
    pub expected_idea_row_version: i64,
    pub idempotency_key: String,
    #[serde(default)]
    pub source_event_id: Option<Uuid>,
    #[serde(default)]
    pub source_finding_ref: Option<IssueSourceFindingRef>,
}

/// Result of an operator Issue-to-Idea link semantic operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkIssueToIdeaResult {
    pub issue: Issue,
    pub idea: crate::types::Idea,
    pub event: crate::types::IdeaEvent,
    pub deduplicated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idea_kernel_get_idea_params_round_trip_and_reject_unknown_fields() -> Result<(), String> {
        let params = GetIdeaParams {
            idea_id: Uuid::new_v4(),
        };
        let value = serde_json::to_value(&params).map_err(|error| error.to_string())?;
        let decoded =
            serde_json::from_value::<GetIdeaParams>(value).map_err(|error| error.to_string())?;
        assert_eq!(decoded, params);
        assert!(
            serde_json::from_value::<GetIdeaParams>(serde_json::json!({
                "idea_id": params.idea_id,
                "session_id": Uuid::new_v4()
            }))
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn test_memory_search_result_roundtrip() {
        let result = MemorySearchResult {
            path: "memory/2026-02-28.md".to_string(),
            start_line: 10,
            end_line: 20,
            score: 0.85,
            snippet: "test snippet".to_string(),
            source: "memory".to_string(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let deser: MemorySearchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.path, result.path);
        assert_eq!(deser.start_line, result.start_line);
        assert!((deser.score - result.score).abs() < f64::EPSILON);
    }

    #[test]
    fn test_memory_provider_status_roundtrip() {
        let status = MemoryProviderStatus {
            enabled: true,
            backend: "builtin".to_string(),
            provider: "ollama".to_string(),
            model: Some("nomic-embed-text".to_string()),
            search_mode: "hybrid".to_string(),
            file_count: 42,
            chunk_count: 1000,
            dirty: false,
            memory_dir: "/home/user/.rsi/memory".to_string(),
            db_path: "/home/user/.rsi/memory.sqlite".to_string(),
            vector_available: true,
            fts_available: true,
            cache_entries: 500,
        };
        let json = serde_json::to_string(&status).unwrap();
        let deser: MemoryProviderStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.backend, status.backend);
        assert_eq!(deser.file_count, status.file_count);
    }

    #[test]
    fn test_memory_provider_status_serde_defaults() {
        // Minimal JSON without optional/default fields
        let json = r#"{"enabled":true,"backend":"builtin","provider":"none","search_mode":"fts","file_count":0,"chunk_count":0,"dirty":false,"memory_dir":"/tmp","db_path":"/tmp/m.db"}"#;
        let deser: MemoryProviderStatus = serde_json::from_str(json).unwrap();
        assert!(deser.model.is_none());
        assert!(!deser.vector_available);
        assert!(!deser.fts_available);
        assert_eq!(deser.cache_entries, 0);
    }

    #[test]
    fn test_daemon_capabilities_default_has_memory_search() {
        let caps = DaemonCapabilities::default();
        assert!(caps.memory_search);
    }

    #[test]
    fn test_execute_workflow_params_default_to_dry_run() {
        let workflow_id = Uuid::new_v4();
        let json = serde_json::json!({
            "workflow_id": workflow_id,
            "workflow": {
                "version": "1.0",
                "name": "test",
                "nodes": [],
                "edges": [],
                "metadata": {}
            }
        });
        let params: ExecuteWorkflowParams = serde_json::from_value(json).unwrap();
        assert!(params.dry_run);
        assert_eq!(params.workflow_id, workflow_id);
        // P1.12: parent_id defaults to None when omitted.
        assert!(params.parent_id.is_none());
    }

    /// P1.12: omitting `parent_id` from ExecuteTopologyParams JSON deserializes to None.
    #[test]
    fn execute_topology_params_parent_id_default() {
        let topology_id = Uuid::new_v4();
        let json = serde_json::json!({ "topology_id": topology_id });
        let params: ExecuteTopologyParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.topology_id, topology_id);
        assert!(params.parent_id.is_none());
        assert!(params.project_id.is_none());
        assert!(params.inputs.is_null());
    }

    /// P1.12: explicit `parent_id: null` deserializes to None.
    #[test]
    fn execute_topology_params_parent_id_explicit_null() {
        let topology_id = Uuid::new_v4();
        let json = serde_json::json!({
            "topology_id": topology_id,
            "parent_id": serde_json::Value::Null,
        });
        let params: ExecuteTopologyParams = serde_json::from_value(json).unwrap();
        assert!(params.parent_id.is_none());
    }

    /// P1.12: `parent_id: "<uuid>"` deserializes to Some(uuid).
    #[test]
    fn execute_topology_params_parent_id_some() {
        let topology_id = Uuid::new_v4();
        let parent_id = Uuid::new_v4();
        let json = serde_json::json!({
            "topology_id": topology_id,
            "parent_id": parent_id,
        });
        let params: ExecuteTopologyParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.parent_id, Some(parent_id));
    }

    /// P1.12: ExecuteWorkflowParams `parent_id` accepts an explicit Some(uuid).
    #[test]
    fn execute_workflow_params_parent_id_some() {
        let workflow_id = Uuid::new_v4();
        let parent_id = Uuid::new_v4();
        let json = serde_json::json!({
            "workflow_id": workflow_id,
            "workflow": {
                "version": "1.0",
                "name": "test",
                "nodes": [],
                "edges": [],
                "metadata": {}
            },
            "parent_id": parent_id,
        });
        let params: ExecuteWorkflowParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.parent_id, Some(parent_id));
    }

    #[test]
    fn test_get_entity_card_params_roundtrip() {
        let params = GetEntityCardParams {
            entity_type: "project".to_string(),
            entity_id: "abc-123".to_string(),
        };
        let json = serde_json::to_string(&params).unwrap();
        let deser: GetEntityCardParams = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.entity_type, "project");
        assert_eq!(deser.entity_id, "abc-123");
    }

    #[test]
    fn test_set_entity_card_params_roundtrip() {
        let params = SetEntityCardParams {
            entity_type: "user".to_string(),
            entity_id: "self".to_string(),
            facts: vec!["Prefers dark theme".to_string()],
        };
        let json = serde_json::to_string(&params).unwrap();
        let deser: SetEntityCardParams = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.entity_type, "user");
        assert_eq!(deser.facts.len(), 1);
    }

    #[test]
    fn test_daemon_capabilities_includes_entity_cards() {
        let caps = DaemonCapabilities::default();
        assert!(caps.entity_cards);
    }

    #[test]
    fn test_daemon_capabilities_includes_recursive_dag_inspection() {
        let caps = DaemonCapabilities::default();
        assert!(caps.recursive_dag_inspection);
        assert!(!caps.recursive_dag_live_status_inspection);
        assert!(!caps.recursive_dag_live_validation_inspection);
        assert!(!caps.recursive_dag_live_scheduler_control);
        assert!(!caps.recursive_dag_artifact_lookup);
        assert!(!caps.recursive_dag_artifact_list_pagination);
        assert!(!caps.recursive_dag_artifact_preview_inspection);
        assert!(!caps.recursive_dag_validation_list_pagination);
        assert!(!caps.recursive_dag_test_inspection);
        assert!(!caps.recursive_dag_diff_inspection);
        assert!(!caps.recursive_dag_scheduler_report_inspection);
        assert!(!caps.recursive_dag_safe_artifact_uri_open);
        assert!(!caps.recursive_dag_live_execution);
        assert!(!caps.recursive_dag_background_loop);
        assert!(!caps.gv_render_recursive_origin);
        assert!(!caps.gv_info_dashboard);
    }

    #[test]
    fn recursive_dag_inspector_rpc_params_and_defaults_round_trip() {
        let graph_id = RecursiveTaskGraphId::new();
        let task_id = RecursiveTaskId::new();
        let attempt_id = RecursiveAttemptId::new();

        let lookup: GetRecursiveExecutionArtifactParams =
            serde_json::from_value(serde_json::json!({
                "graph_id": graph_id,
                "artifact_id": 42
            }))
            .unwrap();
        assert_eq!(lookup.graph_id, graph_id);
        assert_eq!(lookup.artifact_id, 42);
        assert!(lookup.include_links);

        let preview: PreviewRecursiveExecutionArtifactParams =
            serde_json::from_value(serde_json::json!({
                "graph_id": graph_id,
                "artifact_id": 43
            }))
            .unwrap();
        assert_eq!(preview.graph_id, graph_id);
        assert_eq!(preview.artifact_id, 43);
        assert!(preview.byte_offset.is_none());
        assert!(preview.line_offset.is_none());
        assert!(preview.max_bytes.is_none());
        assert!(preview.max_lines.is_none());
        assert!(preview.render_hint.is_none());
        assert!(!preview.require_complete);

        let preview_with_caps: PreviewRecursiveExecutionArtifactParams =
            serde_json::from_value(serde_json::json!({
                "graph_id": graph_id,
                "artifact_id": 43,
                "byte_offset": 8,
                "line_offset": 2,
                "max_bytes": 128,
                "max_lines": 4,
                "render_hint": "json",
                "require_complete": true
            }))
            .unwrap();
        assert_eq!(preview_with_caps.byte_offset, Some(8));
        assert_eq!(preview_with_caps.line_offset, Some(2));
        assert_eq!(preview_with_caps.max_bytes, Some(128));
        assert_eq!(preview_with_caps.max_lines, Some(4));
        assert_eq!(
            preview_with_caps.render_hint,
            Some(RecursiveArtifactRenderHint::Json)
        );
        assert!(preview_with_caps.require_complete);

        let list: ListRecursiveExecutionArtifactSummariesParams =
            serde_json::from_value(serde_json::json!({
                "graph_id": graph_id,
                "task_id": task_id,
                "attempt_id": attempt_id,
                "kind": "inline",
                "limit": 25
            }))
            .unwrap();
        assert_eq!(list.graph_id, Some(graph_id));
        assert_eq!(list.task_id, Some(task_id));
        assert_eq!(list.attempt_id, Some(attempt_id));
        assert_eq!(list.kind, Some(RecursiveExecutionArtifactKind::Inline));
        assert_eq!(list.limit, Some(25));
        assert!(list.cursor.is_none());
        assert!(list.role.is_none());
        assert!(!list.include_total);
    }

    #[test]
    fn recursive_typed_inspector_rpc_names_are_stable() {
        assert_eq!(
            METHOD_LIST_RECURSIVE_TEST_SUMMARIES,
            "ListRecursiveTestSummaries"
        );
        assert_eq!(METHOD_GET_RECURSIVE_TEST_DETAIL, "GetRecursiveTestDetail");
        assert_eq!(
            METHOD_LIST_RECURSIVE_DIFF_SUMMARIES,
            "ListRecursiveDiffSummaries"
        );
        assert_eq!(METHOD_GET_RECURSIVE_DIFF_DETAIL, "GetRecursiveDiffDetail");
        assert_eq!(
            METHOD_GET_RECURSIVE_DIFF_FILE_HUNKS,
            "GetRecursiveDiffFileHunks"
        );
        assert_eq!(
            METHOD_GET_RECURSIVE_SCHEDULER_REPORT_SUMMARY,
            "GetRecursiveSchedulerReportSummary"
        );
        assert_eq!(
            METHOD_GET_RECURSIVE_SCHEDULER_REPORT_DETAIL,
            "GetRecursiveSchedulerReportDetail"
        );
    }

    #[test]
    fn recursive_typed_inspector_rpc_params_and_defaults_round_trip() {
        let graph_id = RecursiveTaskGraphId::new();
        let task_id = RecursiveTaskId::new();
        let attempt_id = RecursiveAttemptId::new();
        let live_attempt_id = RecursiveLiveAttemptId::new();
        let scheduler_run_id = RecursiveSchedulerRunId::new();
        let validation_id = RecursiveLiveOutputValidationId::new();
        let test_id = RecursiveTestResultId::new();
        let diff_id = RecursiveDiffId::new();
        let file_id = RecursiveDiffFileId::new();
        let report_id = RecursiveSchedulerReportId::new();

        let test_list: ListRecursiveTestSummariesParams =
            serde_json::from_value(serde_json::json!({
                "graph_id": graph_id,
                "task_id": task_id,
                "attempt_id": attempt_id,
                "live_attempt_id": live_attempt_id,
                "scheduler_run_id": scheduler_run_id,
                "validation_id": validation_id,
                "artifact_id": 41,
                "source": "live_validation",
                "trust_level": "normalized",
                "status": "failed",
                "cursor": "opaque",
                "limit": 25,
                "include_total": true
            }))
            .unwrap();
        assert_eq!(test_list.graph_id, Some(graph_id));
        assert_eq!(test_list.task_id, Some(task_id));
        assert_eq!(test_list.attempt_id, Some(attempt_id));
        assert_eq!(test_list.live_attempt_id, Some(live_attempt_id));
        assert_eq!(test_list.scheduler_run_id, Some(scheduler_run_id));
        assert_eq!(test_list.validation_id, Some(validation_id));
        assert_eq!(test_list.artifact_id, Some(41));
        assert_eq!(test_list.source, Some(RecursiveTypedSource::LiveValidation));
        assert_eq!(test_list.trust_level, Some(RecursiveTrustLevel::Normalized));
        assert_eq!(test_list.status, Some(RecursiveTypedTestStatus::Failed));
        assert_eq!(test_list.cursor.as_deref(), Some("opaque"));
        assert_eq!(test_list.limit, Some(25));
        assert!(test_list.include_total);

        let test_list_default: ListRecursiveTestSummariesParams =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(test_list_default.graph_id.is_none());
        assert!(test_list_default.cursor.is_none());
        assert!(test_list_default.limit.is_none());
        assert!(!test_list_default.include_total);

        let test_detail: GetRecursiveTestDetailParams = serde_json::from_value(serde_json::json!({
            "graph_id": graph_id,
            "test_id": test_id
        }))
        .unwrap();
        assert_eq!(test_detail.graph_id, graph_id);
        assert_eq!(test_detail.test_id, test_id);
        assert!(test_detail.max_failure_bytes.is_none());
        assert!(!test_detail.include_output);

        let diff_list: ListRecursiveDiffSummariesParams =
            serde_json::from_value(serde_json::json!({
                "graph_id": graph_id,
                "artifact_id": 42,
                "source": "daemon_collected",
                "trust_level": "verified"
            }))
            .unwrap();
        assert_eq!(diff_list.graph_id, Some(graph_id));
        assert_eq!(diff_list.artifact_id, Some(42));
        assert_eq!(
            diff_list.source,
            Some(RecursiveTypedSource::DaemonCollected)
        );
        assert_eq!(diff_list.trust_level, Some(RecursiveTrustLevel::Verified));
        assert!(diff_list.cursor.is_none());
        assert!(diff_list.limit.is_none());
        assert!(!diff_list.include_total);

        let diff_detail: GetRecursiveDiffDetailParams = serde_json::from_value(serde_json::json!({
            "graph_id": graph_id,
            "diff_id": diff_id
        }))
        .unwrap();
        assert_eq!(diff_detail.graph_id, graph_id);
        assert_eq!(diff_detail.diff_id, diff_id);
        assert!(diff_detail.file_cursor.is_none());
        assert!(diff_detail.file_limit.is_none());
        assert!(!diff_detail.include_total);

        let hunks: GetRecursiveDiffFileHunksParams = serde_json::from_value(serde_json::json!({
            "graph_id": graph_id,
            "diff_id": diff_id,
            "file_id": file_id,
            "limit": 50,
            "max_lines": 1000,
            "max_bytes": 262_144
        }))
        .unwrap();
        assert_eq!(hunks.graph_id, graph_id);
        assert_eq!(hunks.diff_id, diff_id);
        assert_eq!(hunks.file_id, file_id);
        assert!(hunks.cursor.is_none());
        assert_eq!(hunks.limit, Some(50));
        assert_eq!(hunks.max_lines, Some(1000));
        assert_eq!(hunks.max_bytes, Some(262_144));

        let report_summary: GetRecursiveSchedulerReportSummaryParams =
            serde_json::from_value(serde_json::json!({
                "graph_id": graph_id,
                "run_id": scheduler_run_id
            }))
            .unwrap();
        assert_eq!(report_summary.graph_id, graph_id);
        assert_eq!(report_summary.run_id, Some(scheduler_run_id));
        assert!(report_summary.report_id.is_none());
        assert!(report_summary.artifact_id.is_none());

        let report_detail: GetRecursiveSchedulerReportDetailParams =
            serde_json::from_value(serde_json::json!({
                "graph_id": graph_id,
                "report_id": report_id,
                "step_cursor": "steps",
                "step_limit": 100,
                "include_events": true,
                "event_cursor": "events",
                "event_limit": 50
            }))
            .unwrap();
        assert_eq!(report_detail.graph_id, graph_id);
        assert_eq!(report_detail.report_id, Some(report_id));
        assert_eq!(report_detail.step_cursor.as_deref(), Some("steps"));
        assert_eq!(report_detail.step_limit, Some(100));
        assert!(report_detail.include_events);
        assert_eq!(report_detail.event_cursor.as_deref(), Some("events"));
        assert_eq!(report_detail.event_limit, Some(50));
    }

    #[test]
    fn recursive_dag_inspector_capability_missing_fields_default_false() {
        let mut value = serde_json::to_value(DaemonCapabilities::default()).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("recursive_dag_artifact_lookup");
        object.remove("recursive_dag_artifact_list_pagination");
        object.remove("recursive_dag_artifact_preview_inspection");
        object.remove("recursive_dag_validation_list_pagination");
        object.remove("recursive_dag_test_inspection");
        object.remove("recursive_dag_diff_inspection");
        object.remove("recursive_dag_scheduler_report_inspection");
        object.remove("recursive_dag_safe_artifact_uri_open");
        object.remove("recursive_dag_live_scheduler_control");

        let caps: DaemonCapabilities = serde_json::from_value(value).unwrap();
        assert!(!caps.recursive_dag_artifact_lookup);
        assert!(!caps.recursive_dag_artifact_list_pagination);
        assert!(!caps.recursive_dag_artifact_preview_inspection);
        assert!(!caps.recursive_dag_validation_list_pagination);
        assert!(!caps.recursive_dag_test_inspection);
        assert!(!caps.recursive_dag_diff_inspection);
        assert!(!caps.recursive_dag_scheduler_report_inspection);
        assert!(!caps.recursive_dag_safe_artifact_uri_open);
        assert!(!caps.recursive_dag_live_scheduler_control);
    }

    #[test]
    fn rpc_error_data_wire_names_round_trip() {
        let data = RpcErrorData {
            code: "RECURSIVE_CURSOR_INVALID".to_string(),
            resource_type: Some("recursive_execution_artifact".to_string()),
            resource_id: Some("42".to_string()),
            retryable: false,
            details: Some(serde_json::json!({"scope": "graph"})),
        };
        let value = serde_json::to_value(&data).unwrap();
        assert_eq!(value["code"], "RECURSIVE_CURSOR_INVALID");
        assert_eq!(value["resource_type"], "recursive_execution_artifact");
        assert_eq!(value["resource_id"], "42");
        assert_eq!(value["retryable"], false);
        assert_eq!(value["details"]["scope"], "graph");
        let decoded: RpcErrorData = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn recursive_dag_live_status_params_defaults_round_trip() {
        let live_attempt_id = RecursiveLiveAttemptId::new();
        let params: GetRecursiveLiveAttemptParams = serde_json::from_value(serde_json::json!({
            "live_attempt_id": live_attempt_id,
        }))
        .unwrap();
        assert_eq!(params.live_attempt_id, live_attempt_id);
        assert!(params.include_session);
        assert!(params.include_heartbeat);
        assert!(params.include_interrupt);
        assert!(params.include_validation);
        assert!(!params.include_artifacts);
        assert!(!params.include_retry_history);

        let graph_id = RecursiveTaskGraphId::new();
        let task_id = RecursiveTaskId::new();
        let list: ListRecursiveLiveAttemptsParams = serde_json::from_value(serde_json::json!({
            "graph_id": graph_id,
            "task_id": task_id,
            "include_status": true,
            "limit": 25
        }))
        .unwrap();
        assert_eq!(list.graph_id, Some(graph_id));
        assert_eq!(list.task_id, Some(task_id));
        assert!(list.include_terminal);
        assert!(list.include_status);
        assert_eq!(list.limit, Some(25));

        let default_list = ListRecursiveLiveAttemptsParams::default();
        assert!(default_list.include_terminal);
        assert!(!default_list.include_status);

        let default_recovery = GetRecursiveLiveRecoveryStatusParams::default();
        assert!(default_recovery.include_recovery_pending);
        assert!(default_recovery.include_deferred_graph);
    }

    #[test]
    fn recursive_dag_live_output_commit_params_round_trip() {
        let live_attempt_id = RecursiveLiveAttemptId::new();
        let params: CommitRecursiveLiveAttemptOutputParams =
            serde_json::from_value(serde_json::json!({
                "live_attempt_id": live_attempt_id,
            }))
            .unwrap();
        assert_eq!(params.live_attempt_id, live_attempt_id);

        let value = serde_json::to_value(&params).unwrap();
        assert_eq!(value["live_attempt_id"], serde_json::json!(live_attempt_id));
    }

    #[test]
    fn recursive_dag_live_scheduler_params_defaults_and_wire_shape() {
        let graph_id = Uuid::new_v4();
        let minimal: RunRecursiveLiveSchedulerParams = serde_json::from_value(serde_json::json!({
            "graph_id": graph_id,
            "max_steps": 3
        }))
        .unwrap();
        assert_eq!(minimal.graph_id, graph_id);
        assert_eq!(minimal.max_steps, 3);
        assert!(minimal.operator.is_none());
        assert!(minimal.idempotency_key.is_none());
        assert!(minimal.provider.is_none());
        assert!(minimal.model.is_none());
        assert!(minimal.effort.is_none());
        assert!(minimal.working_dir.is_none());
        assert!(minimal.sandbox.is_none());
        assert!(minimal.approval_mode.is_none());
        assert!(minimal.tool_policy.is_none());
        assert!(minimal.sandbox_policy.is_none());
        assert!(minimal.max_wall_time_ms.is_none());
        assert!(minimal.token_budget.is_none());
        assert!(minimal.tool_call_budget.is_none());
        assert!(minimal.artifact_bytes_budget.is_none());
        assert!(minimal.output_repair_attempts.is_none());
        assert!(minimal.heartbeat_ttl_ms.is_none());

        let params = RunRecursiveLiveSchedulerParams {
            graph_id,
            max_steps: 7,
            operator: Some("operator".to_string()),
            idempotency_key: Some("retry-key".to_string()),
            provider: Some(crate::types::SessionProvider::Codex),
            model: Some("gpt-5".to_string()),
            effort: Some("high".to_string()),
            working_dir: Some(PathBuf::from("/tmp/rsi")),
            sandbox: Some(SandboxSpec {
                kind: Some(crate::types::SandboxKind::GitWorktree),
                branch: Some("rsi/live-scheduler".to_string()),
            }),
            approval_mode: Some("manual".to_string()),
            tool_policy: Some(RecursiveLiveToolPolicy {
                allowed_tools: vec!["Read".to_string()],
                denied_tools: vec!["Write".to_string()],
            }),
            sandbox_policy: Some(RecursiveLiveSandboxPolicy {
                requested_kind: Some(crate::types::SandboxKind::GitWorktree),
                requested_branch: Some("rsi/live-scheduler".to_string()),
                preserve_on_failure: Some(true),
                allowed_write_roots: vec![PathBuf::from("/tmp/rsi")],
            }),
            max_wall_time_ms: Some(60_000),
            token_budget: Some(100_000),
            tool_call_budget: Some(32),
            artifact_bytes_budget: Some(1_048_576),
            output_repair_attempts: Some(0),
            heartbeat_ttl_ms: Some(30_000),
        };

        let value = serde_json::to_value(&params).unwrap();
        assert_eq!(value["provider"], serde_json::json!("Codex"));
        assert_eq!(value["tool_policy"]["allowed_tools"][0], "Read");
        assert_eq!(
            value["sandbox_policy"]["requested_kind"],
            serde_json::json!("GitWorktree")
        );
        assert_eq!(value["output_repair_attempts"], serde_json::json!(0));

        let decoded: RunRecursiveLiveSchedulerParams = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.graph_id, graph_id);
        assert_eq!(decoded.max_steps, 7);
        assert_eq!(decoded.provider, Some(crate::types::SessionProvider::Codex));
        assert_eq!(decoded.output_repair_attempts, Some(0));
    }

    #[test]
    fn recursive_topology_status_params_defaults_round_trip() {
        let topology_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let workflow_id = Uuid::new_v4();
        let workflow_execution_id = Uuid::new_v4();

        let list: ListRecursiveGraphsForTopologyParams =
            serde_json::from_value(serde_json::json!({
                "topology_id": topology_id,
                "project_id": project_id,
                "workflow_id": workflow_id,
                "workflow_execution_id": workflow_execution_id,
                "node_id": "verify",
                "topology_iteration": 2,
                "execution_owner": "recursive_dag_fake"
            }))
            .unwrap();
        assert_eq!(list.topology_id, Some(topology_id));
        assert_eq!(list.project_id, Some(project_id));
        assert_eq!(list.workflow_id, Some(workflow_id));
        assert_eq!(list.workflow_execution_id, Some(workflow_execution_id));
        assert_eq!(list.node_id.as_deref(), Some("verify"));
        assert_eq!(list.topology_iteration, Some(2));
        assert!(list.include_quarantined);

        let status: GetTopologyRecursiveStatusParams = serde_json::from_value(serde_json::json!({
            "topology_id": topology_id,
            "graph_id": RecursiveTaskGraphId::new(),
            "parent_session_id": Uuid::new_v4(),
            "include_dynamic_children": true
        }))
        .unwrap();
        assert_eq!(status.topology_id, Some(topology_id));
        assert!(status.parent_session_id.is_some());
        assert!(status.include_dynamic_children);

        let default_status = GetTopologyRecursiveStatusParams::default();
        assert!(!default_status.include_dynamic_children);

        let run_topology: RunRecursiveTopologyNodeFakeSchedulerParams =
            serde_json::from_value(serde_json::json!({
                "topology_id": topology_id,
                "node_id": "verify",
                "topology_iteration": 2,
                "workflow_execution_id": workflow_execution_id,
                "max_steps": 5,
                "operator": "test"
            }))
            .unwrap();
        assert_eq!(run_topology.graph_id, None);
        assert_eq!(run_topology.topology_id, Some(topology_id));
        assert_eq!(run_topology.node_id.as_deref(), Some("verify"));
        assert_eq!(run_topology.topology_iteration, Some(2));
        assert_eq!(
            run_topology.workflow_execution_id,
            Some(workflow_execution_id)
        );
        assert_eq!(run_topology.max_steps, Some(5));

        let missing_steps: RunRecursiveTopologyNodeFakeSchedulerParams =
            serde_json::from_value(serde_json::json!({
                "graph_id": RecursiveTaskGraphId::new()
            }))
            .unwrap();
        assert!(missing_steps.max_steps.is_none());
    }

    #[test]
    fn recursive_topology_cancellation_params_defaults_and_wire_names() {
        let topology_id = Uuid::new_v4();
        let graph_id = RecursiveTaskGraphId::new();
        let run_id = RecursiveSchedulerRunId::new();

        let graph: RequestTopologyRecursiveCancellationParams =
            serde_json::from_value(serde_json::json!({
                "topology_id": topology_id,
                "node_id": "verify",
                "topology_iteration": 2,
                "reason": "stop topology graph"
            }))
            .unwrap();
        assert_eq!(graph.topology_id, Some(topology_id));
        assert_eq!(graph.scope, TopologyRecursiveCancellationScope::Graph);
        assert_eq!(graph.apply_to, TopologyRecursiveCancellationApplyTo::Single);
        assert_eq!(
            graph.run_selection,
            TopologyRecursiveCancellationRunSelection::Active
        );

        let run: RequestTopologyRecursiveCancellationParams =
            serde_json::from_value(serde_json::json!({
                "graph_id": graph_id,
                "scope": "run",
                "apply_to": "single",
                "run_id": run_id,
                "run_selection": "latest",
                "reason": "stop run",
                "requested_by": "operator",
                "idempotency_key": "caller-key"
            }))
            .unwrap();
        assert_eq!(run.graph_id, Some(graph_id.0));
        assert_eq!(run.run_id, Some(run_id.0));
        assert_eq!(run.scope, TopologyRecursiveCancellationScope::Run);
        assert_eq!(
            run.run_selection,
            TopologyRecursiveCancellationRunSelection::Latest
        );
        assert_eq!(run.idempotency_key.as_deref(), Some("caller-key"));

        let all_matching: RequestTopologyRecursiveCancellationParams =
            serde_json::from_value(serde_json::json!({
                "topology_id": topology_id,
                "apply_to": "all_matching",
                "reason": "stop all graphs"
            }))
            .unwrap();
        assert_eq!(
            all_matching.apply_to,
            TopologyRecursiveCancellationApplyTo::AllMatching
        );

        let invalid_scope = serde_json::from_value::<RequestTopologyRecursiveCancellationParams>(
            serde_json::json!({
                "scope": "task",
                "reason": "not allowed"
            }),
        );
        assert!(invalid_scope.is_err());
    }

    #[test]
    fn recursive_topology_cancellation_response_wire_names_round_trip() {
        let now = Utc::now();
        let graph_id = RecursiveTaskGraphId::new();
        let run_id = RecursiveSchedulerRunId::new();
        let topology_id = Uuid::new_v4();
        let request_id = RecursiveCancellationRequestId::new();
        let cancellation = RecursiveCancellationRequestSummary {
            id: request_id,
            graph_id,
            run_id: Some(run_id),
            task_id: None,
            scope: crate::RecursiveCancellationScope::Run,
            status: RecursiveCancellationRequestStatus::Requested,
            source: crate::RecursiveCancellationRequestSource::ManualRpc,
            reason: "stop".to_string(),
            requested_by: Some("operator".to_string()),
            requested_at: now,
            observed_at: None,
            applied_at: None,
            rejection_reason: None,
            idempotency_key: Some("v1|topology_recursive_cancel|scope=run".to_string()),
            request_fingerprint: Some("v1|scope=run".to_string()),
            source_context: Some(serde_json::json!({
                "kind": "topology_recursive_cancellation"
            })),
        };
        let response = TopologyRecursiveCancellationResponse {
            scope: TopologyRecursiveCancellationScope::Run,
            apply_to: TopologyRecursiveCancellationApplyTo::Single,
            matched_graph_count: 1,
            requested_count: 1,
            reused_count: 0,
            skipped_count: 0,
            rejected_count: 0,
            targets: vec![TopologyRecursiveCancellationTargetResult {
                graph_id,
                run_id: Some(run_id),
                topology_id,
                source_topology_node_id: "verify".to_string(),
                source_topology_iteration: 0,
                execution_owner: "recursive_dag_fake".to_string(),
                cancellation_request: Some(cancellation),
                outcome: TopologyRecursiveCancellationOutcome::Requested,
                message: None,
            }],
            status: TopologyRecursiveStatus {
                topology_id: Some(topology_id),
                project_id: None,
                workflow_id: None,
                workflow_execution_id: None,
                execution_owner: Some("recursive_dag_fake".to_string()),
                graphs: Vec::new(),
                nodes: Vec::new(),
                latest_recovery: None,
                open_cancellations: Vec::new(),
                live_enabled: false,
                background_enabled: false,
            },
        };

        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["scope"], "run");
        assert_eq!(value["apply_to"], "single");
        assert_eq!(value["targets"][0]["outcome"], "requested");
        assert_eq!(
            value["targets"][0]["cancellation_request"]["idempotency_key"],
            "v1|topology_recursive_cancel|scope=run"
        );

        let decoded: TopologyRecursiveCancellationResponse = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.scope, TopologyRecursiveCancellationScope::Run);
        assert_eq!(
            decoded.targets[0].outcome,
            TopologyRecursiveCancellationOutcome::Requested
        );
        assert_eq!(
            decoded.targets[0]
                .cancellation_request
                .as_ref()
                .and_then(|request| request.source_context.as_ref())
                .and_then(|context| context["kind"].as_str()),
            Some("topology_recursive_cancellation")
        );
    }

    #[test]
    fn recursive_topology_recovery_params_defaults_and_wire_names() {
        let topology_id = Uuid::new_v4();
        let workflow_execution_id = Uuid::new_v4();

        let params: ContinueTopologyRecursiveRecoveryParams =
            serde_json::from_value(serde_json::json!({
                "topology_id": topology_id,
                "workflow_execution_id": workflow_execution_id,
                "node_id": "verify",
                "topology_iteration": 2,
                "apply_to": "all_matching",
                "max_graphs": 3,
                "time_budget_ms": 0,
                "idempotency_key": "caller-key"
            }))
            .unwrap();
        assert_eq!(params.topology_id, Some(topology_id));
        assert_eq!(params.workflow_execution_id, Some(workflow_execution_id));
        assert_eq!(params.node_id.as_deref(), Some("verify"));
        assert_eq!(params.topology_iteration, Some(2));
        assert_eq!(
            params.apply_to,
            TopologyRecursiveRecoveryApplyTo::AllMatching
        );
        assert_eq!(params.max_graphs, 3);
        assert_eq!(params.time_budget_ms, Some(0));
        assert_eq!(params.idempotency_key.as_deref(), Some("caller-key"));

        let default_apply_to: ContinueTopologyRecursiveRecoveryParams =
            serde_json::from_value(serde_json::json!({
                "graph_id": RecursiveTaskGraphId::new(),
                "max_graphs": 1
            }))
            .unwrap();
        assert_eq!(
            default_apply_to.apply_to,
            TopologyRecursiveRecoveryApplyTo::Single
        );

        let invalid_apply_to =
            serde_json::from_value::<ContinueTopologyRecursiveRecoveryParams>(serde_json::json!({
                "apply_to": "everything",
                "max_graphs": 1
            }));
        assert!(invalid_apply_to.is_err());
    }

    #[test]
    fn recursive_topology_recovery_response_wire_names_round_trip() {
        let now = Utc::now();
        let graph_id = RecursiveTaskGraphId::new();
        let topology_id = Uuid::new_v4();
        let recovery = crate::RecursiveGraphRecoveryStatus {
            graph_id: Some(graph_id),
            raw_graph_id: graph_id.to_string(),
            state: crate::RecursiveGraphRecoveryState::Deferred,
            pass_id: None,
            last_attempted_at: None,
            completed_at: None,
            deferred_at: Some(now),
            reason: Some("time_budget".to_string()),
            last_error: None,
            updated_at: now,
        };
        let pass = crate::RecursiveRecoveryPassSummary {
            id: crate::RecursiveRecoveryPassId::new(),
            source: crate::RecursiveRecoverySource::ManualRpc,
            status: crate::RecursiveRecoveryPassStatus::Deferred,
            started_at: now,
            completed_at: Some(now),
            max_graphs: 1,
            time_budget_ms: Some(0),
            checked: 0,
            recovered: 0,
            quarantined: 0,
            deferred: 1,
            skipped: 0,
            errors: 0,
            last_graph_id: None,
            last_raw_graph_id: None,
            stop_reason: Some(crate::RecursiveRecoveryStopReason::TimeBudget),
            error: None,
            idempotency_key: Some("v1|topology_recursive_recovery|caller=key".to_string()),
            request_fingerprint: Some("sha256:abc".to_string()),
            source_context: Some(serde_json::json!({
                "kind": "topology_recursive_recovery"
            })),
        };
        let response = TopologyRecursiveRecoveryResponse {
            apply_to: TopologyRecursiveRecoveryApplyTo::AllMatching,
            matched_graph_count: 1,
            eligible_count: 1,
            checked_count: 0,
            recovered_count: 0,
            failed_graph_count: 0,
            quarantined_count: 0,
            deferred_count: 1,
            skipped_count: 0,
            rejected_count: 0,
            no_op_count: 0,
            replayed: true,
            pass: Some(pass),
            targets: vec![TopologyRecursiveRecoveryTargetResult {
                graph_id,
                topology_id,
                source_topology_node_id: "verify".to_string(),
                source_topology_iteration: 0,
                execution_owner: "recursive_dag_fake".to_string(),
                before_graph_status: crate::RecursiveGraphStatus::Active,
                after_graph_status: crate::RecursiveGraphStatus::Active,
                before: recovery.clone(),
                after: recovery,
                outcome: TopologyRecursiveRecoveryOutcome::Deferred,
                message: Some("time_budget".to_string()),
            }],
            status: TopologyRecursiveStatus {
                topology_id: Some(topology_id),
                project_id: None,
                workflow_id: None,
                workflow_execution_id: None,
                execution_owner: Some("recursive_dag_fake".to_string()),
                graphs: Vec::new(),
                nodes: Vec::new(),
                latest_recovery: None,
                open_cancellations: Vec::new(),
                live_enabled: false,
                background_enabled: false,
            },
        };

        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["apply_to"], "all_matching");
        assert_eq!(value["targets"][0]["outcome"], "deferred");
        assert_eq!(value["replayed"], true);
        assert_eq!(
            value["pass"]["source_context"]["kind"],
            "topology_recursive_recovery"
        );

        let decoded: TopologyRecursiveRecoveryResponse = serde_json::from_value(value).unwrap();
        assert_eq!(
            decoded.targets[0].outcome,
            TopologyRecursiveRecoveryOutcome::Deferred
        );
        assert!(decoded.replayed);
        assert_eq!(
            decoded
                .pass
                .as_ref()
                .and_then(|pass| pass.source_context.as_ref())
                .and_then(|context| context["kind"].as_str()),
            Some("topology_recursive_recovery")
        );
    }

    #[test]
    fn recursive_dag_live_validation_params_defaults_round_trip() {
        let validation_id = RecursiveLiveOutputValidationId::new();
        let params: GetRecursiveLiveOutputValidationResultParams =
            serde_json::from_value(serde_json::json!({
                "validation_id": validation_id
            }))
            .unwrap();
        assert_eq!(params.validation_id, Some(validation_id));
        assert!(params.latest);
        assert!(params.include_issues);
        assert!(!params.include_normalized_output);
        assert!(!params.include_validation_report);

        let default_params = GetRecursiveLiveOutputValidationResultParams::default();
        assert!(default_params.latest);
        assert!(default_params.include_issues);
        assert!(!default_params.include_normalized_output);
        assert!(!default_params.include_validation_report);

        let live_attempt_id = RecursiveLiveAttemptId::new();
        let issues: ListRecursiveLiveValidationIssuesParams =
            serde_json::from_value(serde_json::json!({
                "live_attempt_id": live_attempt_id,
                "severity": "error",
                "class": "missing",
                "code": "missing_required_field"
            }))
            .unwrap();
        assert_eq!(issues.live_attempt_id, Some(live_attempt_id));
        assert_eq!(
            issues.severity,
            Some(RecursiveLiveValidationIssueSeverity::Error)
        );
        assert_eq!(
            issues.class,
            Some(RecursiveLiveValidationIssueClass::Missing)
        );
        assert_eq!(
            issues.code,
            Some(RecursiveLiveValidationIssueCode::MissingRequiredField)
        );
    }

    // ─── P1.6 serde tests ───────────────────────────────────────────────────

    /// AC5b: LaunchSessionParams must fail to deserialize when `tags` is omitted.
    #[test]
    fn launch_session_params_tags_field_required() {
        // JSON without `tags` field — must return Err (not serde(default))
        let json = serde_json::json!({
            "query": "test query",
            "working_dir": "/tmp"
        });
        let result: std::result::Result<LaunchSessionParams, _> = serde_json::from_value(json);
        assert!(
            result.is_err(),
            "LaunchSessionParams without tags must fail deserialization"
        );
    }

    /// AC5a variant: LaunchSessionParams with tags round-trips + workflow_id_override defaults.
    #[test]
    fn launch_session_params_tags_round_trips() {
        let override_id = Uuid::new_v4();
        let json = serde_json::json!({
            "query": "test",
            "tags": ["ci", "infra"],
            "workflow_id_override": override_id
        });
        let params: LaunchSessionParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.tags, vec!["ci", "infra"]);
        assert_eq!(params.workflow_id_override, Some(override_id));

        // Also test that workflow_id_override defaults to None when omitted
        let json2 = serde_json::json!({
            "query": "test",
            "tags": ["ci"]
        });
        let params2: LaunchSessionParams = serde_json::from_value(json2).unwrap();
        assert_eq!(params2.tags, vec!["ci"]);
        assert!(params2.workflow_id_override.is_none());
    }

    /// AC5a: CreateContainerParams with both new fields round-trips correctly.
    #[test]
    fn create_container_params_round_trips() {
        let topo_id = Uuid::new_v4();
        let json = serde_json::json!({
            "kind": "Epic",
            "name": "My Epic",
            "tags": ["release", "q2"],
            "topology_id": topo_id
        });
        let params: CreateContainerParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.tags, vec!["release", "q2"]);
        assert_eq!(params.topology_id, Some(topo_id));

        // topology_id defaults to None when omitted
        let json2 = serde_json::json!({
            "kind": "Group",
            "name": "My Group",
            "tags": ["team-a"]
        });
        let params2: CreateContainerParams = serde_json::from_value(json2).unwrap();
        assert_eq!(params2.tags, vec!["team-a"]);
        assert!(params2.topology_id.is_none());

        // tags field is mandatory — must fail without it
        let json3 = serde_json::json!({
            "kind": "Group",
            "name": "My Group"
        });
        let result: std::result::Result<CreateContainerParams, _> = serde_json::from_value(json3);
        assert!(
            result.is_err(),
            "CreateContainerParams without tags must fail"
        );
    }

    /// T8 — `GetUsageStatsParams` MUST deserialize from an object missing
    /// `project_id` (repo rule: fields absent from a present object fall
    /// back via `#[serde(default)]`, F-007).
    #[test]
    fn get_usage_stats_params_deserializes_from_empty_object() {
        let params: GetUsageStatsParams = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(params.project_id.is_none());
    }

    /// T8 — a bare `Value::Null` (the `RpcRequest.params` default, F-007)
    /// does NOT deserialize directly into `GetUsageStatsParams` — a derived
    /// struct `Deserialize` impl rejects a top-level `null` regardless of
    /// `#[serde(default)]`. The call site (`handle_get_usage_stats`) is
    /// responsible for checking `request.params.is_null()` and using
    /// `GetUsageStatsParams::default()` directly; this test pins that this
    /// IS still the correct/required behavior contract (a regression here
    /// would silently break the no-params RPC call).
    #[test]
    fn get_usage_stats_params_rejects_bare_null_by_design() {
        let result: std::result::Result<GetUsageStatsParams, _> =
            serde_json::from_value(serde_json::Value::Null);
        assert!(
            result.is_err(),
            "a bare null must NOT deserialize directly — handlers must check \
             request.params.is_null() and fall back to ::default() explicitly"
        );
        // The call-site fallback itself:
        assert!(GetUsageStatsParams::default().project_id.is_none());
    }

    #[test]
    fn get_usage_stats_params_round_trips_with_project_id() {
        let project_id = Uuid::new_v4();
        let json = serde_json::json!({ "project_id": project_id });
        let params: GetUsageStatsParams = serde_json::from_value(json).unwrap();
        assert_eq!(params.project_id, Some(project_id));

        let reserialized = serde_json::to_value(&params).unwrap();
        let round_tripped: GetUsageStatsParams = serde_json::from_value(reserialized).unwrap();
        assert_eq!(round_tripped.project_id, Some(project_id));
    }

    /// C3/D04: `CreateIssueParams` requires project ownership and defaults
    /// optional content fields — and carries
    /// no `created_by_session_id` field at all (operator-only surface).
    #[test]
    fn create_issue_params_deserializes_with_defaults() -> Result<(), String> {
        let project_id = Uuid::new_v4();
        let params: CreateIssueParams = serde_json::from_value(
            serde_json::json!({ "project_id": project_id, "title": "fix the thing" }),
        )
        .map_err(|error| error.to_string())?;
        assert_eq!(params.project_id, project_id);
        assert_eq!(params.title, "fix the thing");
        assert_eq!(params.body, "");
        assert!(params.priority.is_none());
        assert!(params.labels.is_empty());
        assert!(params.assignee.is_none());
        Ok(())
    }

    /// C3: `UpdateIssueStatusParams` rejects an unknown status string at
    /// deserialize time (enum parse failure), rather than accepting it and
    /// failing later at the store layer.
    #[test]
    fn update_issue_status_params_rejects_invalid_status_string() {
        let issue_id = Uuid::new_v4();
        let result: std::result::Result<UpdateIssueStatusParams, _> =
            serde_json::from_value(serde_json::json!({
                "issue_id": issue_id,
                "status": "NotAStatus"
            }));
        assert!(result.is_err());
    }

    /// C3: `IssueDepParams` round-trips `issue_id`/`depends_on_id`.
    #[test]
    fn issue_dep_params_roundtrip() {
        let issue_id = Uuid::new_v4();
        let depends_on_id = Uuid::new_v4();
        let params: IssueDepParams = serde_json::from_value(serde_json::json!({
            "issue_id": issue_id,
            "depends_on_id": depends_on_id
        }))
        .unwrap();
        assert_eq!(params.issue_id, issue_id);
        assert_eq!(params.depends_on_id, depends_on_id);
    }

    /// C3: `ListReadyIssuesParams` deserializes from an empty object with
    /// `limit = None` (matches the `ListLabelsParams`/`GetUsageStatsParams`
    /// `unwrap_or_default()` handler convention).
    #[test]
    fn list_ready_issues_params_deserializes_from_empty_object() {
        let params: ListReadyIssuesParams = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(params.limit.is_none());
    }

    #[test]
    fn agent_issue_v1_dtos_reject_spoof_nil_bounds_clear_and_noop() {
        let issue_id = Uuid::new_v4();
        let valid_update = serde_json::json!({
            "issue_id": issue_id,
            "expected_row_version": 1,
            "idempotency_key": "stable-key",
            "title": "updated"
        });
        let update: AgentUpdateIssueRequestV1 =
            serde_json::from_value(valid_update.clone()).unwrap();
        update.validate().unwrap();
        update.semantic_request().unwrap();

        let mut invalid = Vec::new();
        invalid.push(serde_json::json!({
            "issue_id": Uuid::nil(), "expected_row_version": 1,
            "idempotency_key": "k", "title": "updated"
        }));
        invalid.push(serde_json::json!({
            "issue_id": issue_id, "expected_row_version": 0,
            "idempotency_key": "k", "title": "updated"
        }));
        invalid.push(serde_json::json!({
            "issue_id": issue_id, "expected_row_version": 1,
            "idempotency_key": "", "title": "updated"
        }));
        invalid.push(serde_json::json!({
            "issue_id": issue_id, "expected_row_version": 1,
            "idempotency_key": "k"
        }));
        invalid.push(serde_json::json!({
            "issue_id": issue_id, "expected_row_version": 1,
            "idempotency_key": "k", "priority": 2, "clear_priority": true
        }));
        invalid.push(serde_json::json!({
            "issue_id": issue_id, "expected_row_version": 1,
            "idempotency_key": "k", "assignee": "x", "clear_assignee": true
        }));
        invalid.push(serde_json::json!({
            "issue_id": issue_id, "expected_row_version": 1,
            "idempotency_key": "k", "title": "x", "project_id": Uuid::new_v4()
        }));
        invalid.push(serde_json::json!({
            "issue_id": issue_id, "expected_row_version": 1,
            "idempotency_key": "k", "title": "x", "caller_session_id": Uuid::new_v4()
        }));
        invalid.push(serde_json::json!({
            "issue_id": issue_id, "expected_row_version": 1,
            "idempotency_key": "k", "title": "x", "token": "spoof"
        }));
        for value in invalid {
            match serde_json::from_value::<AgentUpdateIssueRequestV1>(value) {
                Ok(request) => assert!(request.validate().is_err()),
                Err(_) => {}
            }
        }

        for field in ["project_id", "actor_session_id", "owning_epic_id", "token"] {
            let mut value = serde_json::json!({"issue_id": issue_id});
            value[field] = serde_json::json!(Uuid::new_v4());
            assert!(serde_json::from_value::<AgentGetIssueRequestV1>(value).is_err());
        }
        assert!(
            serde_json::from_value::<AgentListIssuesRequestV1>(serde_json::json!({
                "ready":true,"project_id":Uuid::new_v4()
            }))
            .is_err()
        );
        for limit in [0, 257] {
            let request: AgentListIssuesRequestV1 =
                serde_json::from_value(serde_json::json!({"limit": limit})).unwrap();
            assert!(request.validated_limit().is_err());
            let events: IssueEventPageRequestV1 = serde_json::from_value(serde_json::json!({
                "issue_id": issue_id, "limit": limit
            }))
            .unwrap();
            assert!(events.validated_limit().is_err());
        }
    }

    #[test]
    fn agent_issue_v1_defaults_results_and_errors_round_trip() {
        let list: AgentListIssuesRequestV1 = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(list.validated_limit().unwrap(), 64);
        assert_eq!(list.archive, IssueArchiveFilterV1::Active);
        assert!(!list.ready);
        let ready: AgentListIssuesRequestV1 =
            serde_json::from_value(serde_json::json!({"ready":true})).unwrap();
        assert!(ready.ready);
        let issue_id = Uuid::new_v4();
        let events: IssueEventPageRequestV1 = serde_json::from_value(serde_json::json!({
            "issue_id": issue_id
        }))
        .unwrap();
        assert_eq!(events.validated_limit().unwrap(), 64);
        assert_eq!(events.after_sequence, 0);

        for code in [
            AgentIssueErrorCodeV1::InvalidRequest,
            AgentIssueErrorCodeV1::AuthorityDenied,
            AgentIssueErrorCodeV1::NotFoundInScope,
            AgentIssueErrorCodeV1::IdempotencyConflict,
            AgentIssueErrorCodeV1::StaleVersion,
            AgentIssueErrorCodeV1::NoSemanticChange,
            AgentIssueErrorCodeV1::InvalidTransition,
            AgentIssueErrorCodeV1::Archived,
            AgentIssueErrorCodeV1::NotArchived,
            AgentIssueErrorCodeV1::StorageFailure,
        ] {
            let error = AgentIssueErrorV1 {
                code,
                expected_row_version: (code == AgentIssueErrorCodeV1::StaleVersion).then_some(2),
                actual_row_version: (code == AgentIssueErrorCodeV1::StaleVersion).then_some(3),
                validation: (code == AgentIssueErrorCodeV1::InvalidRequest).then_some(
                    AgentIssueValidationV1 {
                        class: AgentIssueValidationClassV1::MissingField,
                        field: Some(AgentIssueValidationFieldV1::IssueId),
                    },
                ),
                next_action: "safe remediation".to_string(),
            };
            let value = serde_json::to_value(&error).unwrap();
            let decoded: AgentIssueErrorV1 = serde_json::from_value(value).unwrap();
            assert_eq!(decoded, error);
        }
    }

    #[test]
    fn health_status_defaults_absent_pioneer_availability_for_older_daemons() {
        let Ok(status) = serde_json::from_value::<HealthStatusResponse>(serde_json::json!({
            "persistence_queue_depth": 0,
            "persistence_queue_capacity": 1,
            "last_command_duration_ms": 0,
            "project_cache_size": 0
        })) else {
            panic!("legacy health payload must deserialize");
        };

        assert!(!status.provider_pioneer_available);
    }
}
