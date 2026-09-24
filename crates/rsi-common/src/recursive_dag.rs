//! Shared read-model types for daemon-owned recursive task DAG persistence.
//!
//! These types intentionally mirror the deterministic eval gate's durable
//! contract while keeping production identifiers and read models independent of
//! the eval crate.

use crate::types::{SandboxKind, SessionKind, SessionProvider, SessionStatus};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

macro_rules! uuid_newtype {
    ($name:ident) => {
        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            PartialOrd,
            Ord,
            Hash,
            Serialize,
            Deserialize,
            Default,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self(value)
            }
        }

        impl From<$name> for Uuid {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

uuid_newtype!(RecursiveTaskGraphId);
uuid_newtype!(RecursiveTaskId);
uuid_newtype!(RecursiveAttemptId);
uuid_newtype!(RecursiveInjectionBatchId);
uuid_newtype!(RecursiveSchedulerRunId);
uuid_newtype!(RecursiveCancellationRequestId);
uuid_newtype!(RecursiveRecoveryPassId);
uuid_newtype!(RecursiveLiveAttemptId);
uuid_newtype!(RecursiveLiveInterruptId);
uuid_newtype!(RecursiveLiveOutputValidationId);
uuid_newtype!(RecursiveTestResultId);
uuid_newtype!(RecursiveDiffId);
uuid_newtype!(RecursiveDiffFileId);
uuid_newtype!(RecursiveSchedulerReportId);

pub const RECURSIVE_LIVE_OUTPUT_SCHEMA_VERSION: u32 = 1;
pub const RECURSIVE_TOPOLOGY_EXECUTION_OWNER_FAKE: &str = "recursive_dag_fake";
pub const RECURSIVE_TOPOLOGY_CREATION_MODE_NODE_PREREQ_CLOSURE: &str =
    "topology_node_prereq_closure";

const fn default_include_prerequisite_closure() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveGraphStatus {
    Active,
    Terminal,
    Blocked,
    Failed,
    Cancelled,
    Malformed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveExecutionMode {
    Fake,
    LiveSession,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecursiveTopologyGraphCreateRequest {
    pub topology_id: Uuid,
    pub node_id: String,
    #[serde(default)]
    pub topology_iteration: u32,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub parent_session_id: Option<Uuid>,
    #[serde(default)]
    pub workflow_id: Option<Uuid>,
    #[serde(default)]
    pub workflow_execution_id: Option<Uuid>,
    #[serde(default = "default_include_prerequisite_closure")]
    pub include_prerequisite_closure: bool,
    #[serde(default)]
    pub max_depth: Option<u32>,
    #[serde(default)]
    pub max_fanout: Option<u32>,
    #[serde(default)]
    pub max_descendants: Option<u32>,
    #[serde(default)]
    pub step_limit: Option<u32>,
    #[serde(default)]
    pub policy_overrides: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecursiveTopologyGraphLink {
    pub graph_id: RecursiveTaskGraphId,
    pub execution_owner: String,
    pub owner_key: String,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    pub request_fingerprint: String,
    pub topology_id: Uuid,
    #[serde(default)]
    pub workflow_id: Option<Uuid>,
    #[serde(default)]
    pub workflow_execution_id: Option<Uuid>,
    pub source_topology_node_id: String,
    pub source_topology_iteration: u32,
    #[serde(default)]
    pub parent_session_id: Option<Uuid>,
    #[serde(default)]
    pub project_id: Option<Uuid>,
    pub creation_mode: String,
    pub include_prerequisite_closure: bool,
    pub topology_name_snapshot: String,
    pub topology_updated_at_snapshot: DateTime<Utc>,
    pub topology_snapshot: serde_json::Value,
    pub selected_slice: serde_json::Value,
    pub policy_snapshot: serde_json::Value,
    #[serde(default)]
    pub workflow_execution_linkage: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecursiveTopologyTaskLink {
    pub graph_id: RecursiveTaskGraphId,
    pub task_id: RecursiveTaskId,
    pub topology_id: Uuid,
    pub topology_node_id: String,
    pub topology_iteration: u32,
    pub topology_node_kind: SessionKind,
    #[serde(default)]
    pub source_params: serde_json::Value,
    pub topology_node_snapshot: serde_json::Value,
    pub policy_snapshot: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecursiveTopologyGraphCreateResponse {
    pub graph: RecursiveTaskGraphSummary,
    pub root_task_id: RecursiveTaskId,
    pub graph_link: RecursiveTopologyGraphLink,
    #[serde(default)]
    pub task_links: Vec<RecursiveTopologyTaskLink>,
    pub reused_existing: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunRecursiveTopologyNodeFakeSchedulerResponse {
    pub scheduler_run: RecursiveSchedulerRunSummary,
    #[serde(default)]
    pub report_artifact: Option<RecursiveExecutionArtifact>,
    pub graph_link: RecursiveTopologyGraphLink,
    #[serde(default)]
    pub task_links: Vec<RecursiveTopologyTaskLink>,
    pub status: TopologyRecursiveStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecursiveLiveSchedulerResponse {
    pub scheduler_run: RecursiveSchedulerRunSummary,
    pub stop_reason: RecursiveSchedulerStopReason,
    pub step_count: u32,
    #[serde(default)]
    pub selected_task_order: Vec<RecursiveTaskId>,
    #[serde(default)]
    pub live_attempts: Vec<RecursiveLiveAttemptReadback>,
    #[serde(default)]
    pub validation_summaries: Vec<RecursiveLiveOutputValidationSummary>,
    #[serde(default)]
    pub report_artifact: Option<RecursiveExecutionArtifact>,
    #[serde(default)]
    pub cancellation_request_id: Option<RecursiveCancellationRequestId>,
    #[serde(default)]
    pub warnings: Vec<RecursiveReadbackWarning>,
}

/// Params for `GetRecursiveGraphAsWorkflow`: the recursive graph to bridge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetRecursiveGraphAsWorkflowParams {
    pub graph_id: Uuid,
}

/// Response for `GetRecursiveGraphAsWorkflow`: the bridged workflow definition.
///
/// Carries the definition as `serde_json::Value` (not a typed
/// `WorkflowDefinition`) because `rsi-common` cannot depend on `rsi-graph`. The
/// TUI deserializes it into a `WorkflowDefinition`. Kept `PartialEq`-only (no
/// `Eq`): the payload is a free-form `serde_json::Value` whose equality is
/// intentionally partial.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GetRecursiveGraphAsWorkflowResponse {
    pub definition: serde_json::Value,
}

/// Params for `EditRecursiveNodeInstructions`: edit a non-running recursive
/// node's instructions (`RecursiveTaskNode.objective`) via native mutation.
///
/// `rsi-common`-native (`Uuid`/`String` primitives only) so `rsi-common` need
/// not depend on `rsi-graph`. The response is the bare updated
/// `RecursiveTaskNode` (mirrors `transition_recursive_task_state`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditRecursiveNodeInstructionsParams {
    pub graph_id: Uuid,
    pub task_id: Uuid,
    pub instructions: String,
}

/// Params for `EditRecursiveNodeSettings`: edit a non-running recursive node's
/// integration/verification strategy via native mutation.
///
/// Both strategies are submitted together (read-modify-write of both columns):
/// `None` means "cleared", not "unchanged", so the gv affordance pre-populates
/// the form from the node's current values before submitting. The response is
/// the bare updated `RecursiveTaskNode`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditRecursiveNodeSettingsParams {
    pub graph_id: Uuid,
    pub task_id: Uuid,
    #[serde(default)]
    pub integration_strategy: Option<String>,
    #[serde(default)]
    pub verification_strategy: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopologyRecursiveVisibleStatus {
    Active,
    Idle,
    Terminal,
    Failed,
    Blocked,
    Cancelled,
    Quarantined,
    RecoveryDeferred,
    Cancelling,
    RunCancelling,
    Error,
    Pending,
    Planning,
    Ready,
    Running,
    Decomposed,
    BlockedOnChildren,
    Integrating,
    Verifying,
    Succeeded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyRecursiveGraphStatus {
    pub graph_id: RecursiveTaskGraphId,
    pub root_task_id: RecursiveTaskId,
    pub owner_key: String,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    pub topology_id: Uuid,
    #[serde(default)]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub workflow_id: Option<Uuid>,
    #[serde(default)]
    pub workflow_execution_id: Option<Uuid>,
    #[serde(default)]
    pub parent_session_id: Option<Uuid>,
    pub source_topology_node_id: String,
    pub source_topology_iteration: u32,
    pub execution_owner: String,
    pub graph_status: RecursiveGraphStatus,
    pub topology_visible_status: TopologyRecursiveVisibleStatus,
    #[serde(default)]
    pub active_run_id: Option<RecursiveSchedulerRunId>,
    #[serde(default)]
    pub latest_run_id: Option<RecursiveSchedulerRunId>,
    #[serde(default)]
    pub open_cancellation_request_ids: Vec<RecursiveCancellationRequestId>,
    #[serde(default)]
    pub recovery_state: Option<RecursiveGraphRecoveryState>,
    #[serde(default)]
    pub recovery: Option<RecursiveGraphRecoveryStatus>,
    #[serde(default)]
    pub quarantined_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub quarantine_reason: Option<String>,
    #[serde(default)]
    pub malformed_reason: Option<String>,
    pub topology_updated_at_snapshot: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyRecursiveNodeStatus {
    pub topology_id: Uuid,
    pub topology_node_id: String,
    pub topology_iteration: u32,
    pub graph_id: RecursiveTaskGraphId,
    pub task_id: RecursiveTaskId,
    #[serde(default)]
    pub recursive_status: Option<RecursiveTaskLifecycleState>,
    pub topology_visible_status: TopologyRecursiveVisibleStatus,
    #[serde(default)]
    pub active_run_id: Option<RecursiveSchedulerRunId>,
    #[serde(default)]
    pub latest_run_id: Option<RecursiveSchedulerRunId>,
    #[serde(default)]
    pub active_live_attempt_id: Option<RecursiveLiveAttemptId>,
    #[serde(default)]
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub child_count: Option<u64>,
    pub artifact_count: u64,
    #[serde(default)]
    pub cancellation_request_id: Option<RecursiveCancellationRequestId>,
    #[serde(default)]
    pub recovery_state: Option<RecursiveGraphRecoveryState>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyRecursiveStatus {
    #[serde(default)]
    pub topology_id: Option<Uuid>,
    #[serde(default)]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub workflow_id: Option<Uuid>,
    #[serde(default)]
    pub workflow_execution_id: Option<Uuid>,
    #[serde(default)]
    pub execution_owner: Option<String>,
    #[serde(default)]
    pub graphs: Vec<TopologyRecursiveGraphStatus>,
    #[serde(default)]
    pub nodes: Vec<TopologyRecursiveNodeStatus>,
    #[serde(default)]
    pub latest_recovery: Option<RecursiveGraphRecoveryStatus>,
    #[serde(default)]
    pub open_cancellations: Vec<RecursiveCancellationRequestSummary>,
    pub live_enabled: bool,
    pub background_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveSchedulerRunStatus {
    Running,
    Cancelling,
    Completed,
    Cancelled,
    Failed,
    Rejected,
    LeaseExpired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveSchedulerRunSource {
    TestHarness,
    ManualRpc,
    StartupRecovery,
    FutureDaemonLoop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveSchedulerStopReason {
    GraphTerminal,
    IdleNoRunnable,
    StepLimitExceeded,
    PartialFailure,
    Quarantined,
    CancellationRequested,
    LeaseExpired,
    ExecutorError,
    RecoveryDeferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveCancellationScope {
    Graph,
    Run,
    Task,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveCancellationRequestStatus {
    Requested,
    Observed,
    Applied,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveCancellationRequestSource {
    TestHarness,
    ManualRpc,
    StartupRecovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveRecoverySource {
    Startup,
    ManualRpc,
    TestHarness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveRecoveryPassStatus {
    Running,
    Completed,
    Deferred,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveRecoveryStopReason {
    Completed,
    MaxGraphs,
    TimeBudget,
    StoreError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveGraphRecoveryState {
    PendingRecovery,
    Recovered,
    Quarantined,
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveLiveAttemptStatus {
    Created,
    Launching,
    Running,
    WaitingApproval,
    Succeeded,
    Decomposed,
    Failed,
    Blocked,
    Cancelled,
    Interrupted,
    Lost,
    RecoveryPending,
}

impl RecursiveLiveAttemptStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded
                | Self::Decomposed
                | Self::Failed
                | Self::Blocked
                | Self::Cancelled
                | Self::Interrupted
                | Self::Lost
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveLiveInterruptStatus {
    Requested,
    Sent,
    Interrupted,
    Failed,
    Rejected,
    Ignored,
}

impl RecursiveLiveInterruptStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Interrupted | Self::Failed | Self::Rejected | Self::Ignored
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveLiveRecoveryStatus {
    None,
    Pending,
    Recovered,
    Lost,
    Quarantined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveLiveAttemptHeartbeatStatus {
    Missing,
    Active,
    Stale,
    Released,
}

impl RecursiveLiveAttemptHeartbeatStatus {
    #[must_use]
    pub const fn is_stale(self) -> bool {
        matches!(self, Self::Stale)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveToolPolicy {
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub denied_tools: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveSandboxPolicy {
    #[serde(default)]
    pub requested_kind: Option<SandboxKind>,
    #[serde(default)]
    pub requested_branch: Option<String>,
    #[serde(default)]
    pub preserve_on_failure: Option<bool>,
    #[serde(default)]
    pub allowed_write_roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveTaskLifecycleState {
    Pending,
    Planning,
    Ready,
    Running,
    Decomposed,
    BlockedOnChildren,
    Integrating,
    Verifying,
    Succeeded,
    Failed,
    Blocked,
    Cancelled,
}

impl RecursiveTaskLifecycleState {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Blocked | Self::Cancelled
        )
    }

    #[must_use]
    pub const fn is_transient_without_attempt(self) -> bool {
        matches!(
            self,
            Self::Planning | Self::Running | Self::Integrating | Self::Verifying
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveTaskEdgeKind {
    ParentChild,
    Dependency,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveAttemptPhase {
    Execute,
    Integrate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveAttemptStatus {
    Running,
    Succeeded,
    Decomposed,
    Failed,
    Blocked,
    Cancelled,
    Interrupted,
}

impl RecursiveAttemptStatus {
    #[must_use]
    pub const fn is_finished(self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveExecutionArtifactKind {
    Inline,
    File,
    SessionEvent,
    WorkflowExecution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveLiveOutputKind {
    Success,
    Decomposition,
    RetryableFailure,
    PermanentFailure,
    Blocked,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveLiveOutputConfidenceLevel {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveLiveAcceptanceStatus {
    Met,
    NotApplicable,
    NotVerified,
    Unmet,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveLiveTestStatus {
    Passed,
    Failed,
    Skipped,
    NotRun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveLiveDiffFileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Unchanged,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveLivePermanentFailureClass {
    InvalidScope,
    ImpossibleRequirement,
    UnsafeRequest,
    MissingIrreplaceableInput,
    ToolingNotAvailable,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveLiveBlockedOn {
    Approval,
    Credential,
    MissingContext,
    ExternalService,
    MergeConflict,
    UnsafeState,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveLiveOutputValidationStatus {
    Valid,
    Invalid,
    Repairable,
    Ambiguous,
    OperatorReviewRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveTypedSource {
    DaemonCollected,
    LiveValidation,
    SchedulerReport,
    LegacyBackfill,
    ModelClaimed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveTrustLevel {
    Verified,
    Normalized,
    SchedulerOwned,
    ModelClaimed,
    LegacyUnverified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveTypedTestStatus {
    Passed,
    Failed,
    Skipped,
    NotRun,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveTypedDiffFileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Unchanged,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveTypedDiffHunkState {
    Deferred,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveSchedulerReportStepOutcomeKind {
    Succeeded,
    Decomposed,
    Failed,
    Blocked,
    Cancelled,
    Stopped,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveLiveValidationIssueSeverity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveLiveValidationIssueClass {
    Malformed,
    Missing,
    Unsafe,
    Ambiguous,
    Invalid,
    Mismatch,
    Policy,
    Cancellation,
    #[default]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveLiveValidationIssueCode {
    MalformedOutput,
    MissingRequiredField,
    UnknownField,
    UnknownSchemaVersion,
    CorrelationMismatch,
    AmbiguousTerminalKind,
    InvalidDecomposition,
    ChildNotSmallerThanParent,
    DependencyCycle,
    UnknownDependency,
    ArtifactMismatch,
    UnsafeToolClaim,
    UnsafeSandboxClaim,
    CancelWithoutRequest,
    SuccessWithFailedRequiredTest,
    SuccessWithUnmetAcceptance,
    IncoherentTestCounts,
    ModelOutputInvalid,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveLiveOutputMappingDecision {
    Success,
    Decomposition,
    RetryFailure,
    PermanentFailure,
    Blocked,
    Cancelled,
    RepairSameLiveAttempt,
    FailAttempt,
    BlockTask,
    OperatorReview,
    NoOp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveLiveOutputRetryDecisionKind {
    NotApplicable,
    RetrySameLiveAttempt,
    RetryTaskAttempt,
    NoRetry,
    OperatorReview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveOutputCorrelation {
    pub graph_id: RecursiveTaskGraphId,
    pub task_id: RecursiveTaskId,
    pub attempt_id: RecursiveAttemptId,
    pub live_attempt_id: RecursiveLiveAttemptId,
    pub scheduler_run_id: RecursiveSchedulerRunId,
    #[serde(default)]
    pub session_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveOutputConfidence {
    pub self_assessment: RecursiveLiveOutputConfidenceLevel,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub known_risks: Vec<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveOutputArtifactReference {
    pub label: String,
    pub kind: RecursiveExecutionArtifactKind,
    #[serde(default)]
    pub task_id: Option<RecursiveTaskId>,
    #[serde(default)]
    pub attempt_id: Option<RecursiveAttemptId>,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default)]
    pub content_digest: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub existing_artifact_id: Option<i64>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveAcceptanceCheck {
    pub criterion: String,
    pub status: RecursiveLiveAcceptanceStatus,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub artifact_ids: Vec<i64>,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveTestResultSummary {
    #[serde(default)]
    pub command: Option<String>,
    pub status: RecursiveLiveTestStatus,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub output_artifact: Option<RecursiveLiveOutputArtifactReference>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveDiffFileSummary {
    pub path: PathBuf,
    pub status: RecursiveLiveDiffFileStatus,
    #[serde(default)]
    pub previous_path: Option<PathBuf>,
    #[serde(default)]
    pub insertions: Option<u64>,
    #[serde(default)]
    pub deletions: Option<u64>,
    #[serde(default)]
    pub inside_allowed_root: Option<bool>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveDiffSummary {
    #[serde(default)]
    pub worktree_root: Option<PathBuf>,
    #[serde(default)]
    pub sandbox_root: Option<PathBuf>,
    #[serde(default)]
    pub files: Vec<RecursiveLiveDiffFileSummary>,
    #[serde(default)]
    pub diff_artifact: Option<RecursiveLiveOutputArtifactReference>,
    #[serde(default)]
    pub clean_worktree: Option<bool>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveDependencyOutputReference {
    pub task_id: RecursiveTaskId,
    pub status: RecursiveTaskLifecycleState,
    #[serde(default)]
    pub artifact_ids: Vec<i64>,
    pub summary: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecursiveLiveTaskDependencyRef {
    ChildLocal { local_id: String },
    ExistingTask { task_id: RecursiveTaskId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveChildTaskSpec {
    pub local_id: String,
    #[serde(default)]
    pub task_id: Option<RecursiveTaskId>,
    pub title: String,
    pub objective: String,
    pub scope: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    pub scope_units: u32,
    pub max_retries: u32,
    #[serde(default)]
    pub dependencies: Vec<RecursiveLiveTaskDependencyRef>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveDependencyEdgeSpec {
    pub from: RecursiveLiveTaskDependencyRef,
    pub to: RecursiveLiveTaskDependencyRef,
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveDecompositionBudgetHints {
    #[serde(default)]
    pub expected_child_count: Option<u32>,
    #[serde(default)]
    pub total_scope_units: Option<u32>,
    #[serde(default)]
    pub max_child_scope_units: Option<u32>,
    #[serde(default)]
    pub retry_budget: Option<u32>,
    #[serde(default)]
    pub scope_notes: Vec<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveSuccessPayload {
    pub result_summary: String,
    #[serde(default)]
    pub acceptance: Vec<RecursiveLiveAcceptanceCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveDecompositionPayload {
    pub reason: String,
    #[serde(default)]
    pub children: Vec<RecursiveLiveChildTaskSpec>,
    #[serde(default)]
    pub dependency_edges: Vec<RecursiveLiveDependencyEdgeSpec>,
    pub integration_strategy: String,
    pub verification_strategy: String,
    #[serde(default)]
    pub budget_hints: Option<RecursiveLiveDecompositionBudgetHints>,
    #[serde(default)]
    pub scope_hints: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveRetryableFailurePayload {
    pub reason: String,
    pub retry_hint: String,
    #[serde(default)]
    pub operator_message: Option<String>,
    #[serde(default)]
    pub evidence: Vec<RecursiveLiveOutputArtifactReference>,
    #[serde(default)]
    pub partial_artifacts: Vec<RecursiveLiveOutputArtifactReference>,
    #[serde(default)]
    pub suggested_next_action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLivePermanentFailurePayload {
    pub reason: String,
    pub failure_class: RecursiveLivePermanentFailureClass,
    #[serde(default)]
    pub operator_message: Option<String>,
    #[serde(default)]
    pub evidence: Vec<RecursiveLiveOutputArtifactReference>,
    #[serde(default)]
    pub suggested_next_action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveBlockedPayload {
    pub reason: String,
    pub requested_operator_input: String,
    pub blocked_on: RecursiveLiveBlockedOn,
    pub safe_to_retry_without_input: bool,
    #[serde(default)]
    pub operator_message: Option<String>,
    #[serde(default)]
    pub evidence: Vec<RecursiveLiveOutputArtifactReference>,
    #[serde(default)]
    pub suggested_next_action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveCancelledPayload {
    pub reason: String,
    #[serde(default)]
    pub cancellation_request_id: Option<RecursiveCancellationRequestId>,
    pub observed_scope: RecursiveCancellationScope,
    #[serde(default)]
    pub operator_message: Option<String>,
    #[serde(default)]
    pub evidence: Vec<RecursiveLiveOutputArtifactReference>,
    #[serde(default)]
    pub suggested_next_action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecursiveLiveOutputOutcome {
    Success(RecursiveLiveSuccessPayload),
    Decomposition(RecursiveLiveDecompositionPayload),
    RetryableFailure(RecursiveLiveRetryableFailurePayload),
    PermanentFailure(RecursiveLivePermanentFailurePayload),
    Blocked(RecursiveLiveBlockedPayload),
    Cancelled(RecursiveLiveCancelledPayload),
}

impl RecursiveLiveOutputOutcome {
    #[must_use]
    pub const fn kind(&self) -> RecursiveLiveOutputKind {
        match self {
            Self::Success(_) => RecursiveLiveOutputKind::Success,
            Self::Decomposition(_) => RecursiveLiveOutputKind::Decomposition,
            Self::RetryableFailure(_) => RecursiveLiveOutputKind::RetryableFailure,
            Self::PermanentFailure(_) => RecursiveLiveOutputKind::PermanentFailure,
            Self::Blocked(_) => RecursiveLiveOutputKind::Blocked,
            Self::Cancelled(_) => RecursiveLiveOutputKind::Cancelled,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveOutputEnvelope {
    #[serde(default = "recursive_live_output_schema_version")]
    pub schema_version: u32,
    pub correlation: RecursiveLiveOutputCorrelation,
    pub summary: String,
    #[serde(default)]
    pub artifacts: Vec<RecursiveLiveOutputArtifactReference>,
    #[serde(default)]
    pub tests: Vec<RecursiveLiveTestResultSummary>,
    #[serde(default)]
    pub diffs: Vec<RecursiveLiveDiffSummary>,
    #[serde(default)]
    pub dependency_outputs: Vec<RecursiveLiveDependencyOutputReference>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    #[serde(default)]
    pub confidence: Option<RecursiveLiveOutputConfidence>,
    #[serde(flatten)]
    pub outcome: RecursiveLiveOutputOutcome,
}

impl RecursiveLiveOutputEnvelope {
    #[must_use]
    pub const fn kind(&self) -> RecursiveLiveOutputKind {
        self.outcome.kind()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecursiveLiveValidationIssueLocation {
    OutputPath {
        path: String,
    },
    Artifact {
        artifact_id: i64,
        #[serde(default)]
        path: Option<String>,
    },
    ConversationEvent {
        event_id: i64,
        #[serde(default)]
        sequence: Option<i64>,
        #[serde(default)]
        path: Option<String>,
    },
    Task {
        task_id: RecursiveTaskId,
    },
    Attempt {
        attempt_id: RecursiveAttemptId,
    },
    ChildLocal {
        local_id: String,
    },
    Dependency {
        reference: RecursiveLiveTaskDependencyRef,
    },
    DiffPath {
        path: PathBuf,
    },
    Other {
        description: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveOutputValidationIssue {
    pub code: RecursiveLiveValidationIssueCode,
    pub severity: RecursiveLiveValidationIssueSeverity,
    #[serde(default)]
    pub class: RecursiveLiveValidationIssueClass,
    #[serde(default)]
    pub location: Option<RecursiveLiveValidationIssueLocation>,
    pub message: String,
    #[serde(default)]
    pub evidence: Vec<RecursiveLiveOutputArtifactReference>,
    #[serde(default)]
    pub suggested_next_action: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveOutputRetryDecision {
    pub decision: RecursiveLiveOutputRetryDecisionKind,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub remaining_repair_attempts: Option<u32>,
    #[serde(default)]
    pub remaining_task_retries: Option<u32>,
    #[serde(default)]
    pub suggested_next_action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecursiveLiveOutputParserSource {
    Artifact {
        artifact_id: i64,
    },
    ConversationEvent {
        event_id: i64,
        #[serde(default)]
        sequence: Option<i64>,
    },
    FinalJsonBlock {
        #[serde(default)]
        event_id: Option<i64>,
        #[serde(default)]
        block_index: Option<u32>,
    },
    Inline {
        #[serde(default)]
        description: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveOutputValidationSummary {
    #[serde(default)]
    pub validation_id: Option<RecursiveLiveOutputValidationId>,
    pub live_attempt_id: RecursiveLiveAttemptId,
    pub graph_id: RecursiveTaskGraphId,
    pub task_id: RecursiveTaskId,
    pub scheduler_run_id: RecursiveSchedulerRunId,
    pub attempt_id: RecursiveAttemptId,
    #[serde(default)]
    pub session_id: Option<Uuid>,
    pub status: RecursiveLiveOutputValidationStatus,
    #[serde(default)]
    pub output_kind: Option<RecursiveLiveOutputKind>,
    #[serde(default)]
    pub mapping_decision: Option<RecursiveLiveOutputMappingDecision>,
    #[serde(default)]
    pub retry_decision: Option<RecursiveLiveOutputRetryDecision>,
    #[serde(default)]
    pub raw_output_artifact_id: Option<i64>,
    #[serde(default)]
    pub normalized_output_artifact_id: Option<i64>,
    #[serde(default)]
    pub validation_artifact_id: Option<i64>,
    #[serde(default)]
    pub normalized_digest: Option<String>,
    #[serde(default)]
    pub issue_count: u32,
    #[serde(default)]
    pub error_count: u32,
    #[serde(default)]
    pub warning_count: u32,
    #[serde(default)]
    pub info_count: u32,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveOutputValidationArtifactLinks {
    #[serde(default)]
    pub raw_output_artifact_id: Option<i64>,
    #[serde(default)]
    pub normalized_output_artifact_id: Option<i64>,
    #[serde(default)]
    pub validation_artifact_id: Option<i64>,
    #[serde(default)]
    pub produced_artifact_ids: Vec<i64>,
    #[serde(default)]
    pub test_artifact_ids: Vec<i64>,
    #[serde(default)]
    pub diff_artifact_ids: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveOutputValidationResult {
    pub summary: RecursiveLiveOutputValidationSummary,
    #[serde(default)]
    pub artifact_links: RecursiveLiveOutputValidationArtifactLinks,
    #[serde(default)]
    pub parser_source: Option<RecursiveLiveOutputParserSource>,
    #[serde(default)]
    pub issues: Vec<RecursiveLiveOutputValidationIssue>,
    #[serde(default)]
    pub normalized_output: Option<RecursiveLiveOutputEnvelope>,
    #[serde(default)]
    pub validation_report: Option<serde_json::Value>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

const fn recursive_live_output_schema_version() -> u32 {
    RECURSIVE_LIVE_OUTPUT_SCHEMA_VERSION
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveDependencySnapshot {
    pub task_id: RecursiveTaskId,
    pub status: RecursiveTaskLifecycleState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveTaskGraphSummary {
    pub id: RecursiveTaskGraphId,
    pub root_task_id: RecursiveTaskId,
    pub title: String,
    pub objective: String,
    pub status: RecursiveGraphStatus,
    #[serde(default)]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub workflow_id: Option<Uuid>,
    #[serde(default)]
    pub topology_id: Option<Uuid>,
    #[serde(default)]
    pub parent_session_id: Option<Uuid>,
    #[serde(default)]
    pub source_execution_id: Option<String>,
    #[serde(default)]
    pub source_eval_id: Option<String>,
    pub execution_mode: RecursiveExecutionMode,
    pub max_depth: u32,
    pub max_fanout: u32,
    pub max_descendants: u32,
    pub step_limit: u32,
    #[serde(default)]
    pub last_stop_reason: Option<String>,
    #[serde(default)]
    pub malformed_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub recovered_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub quarantined_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub quarantine_reason: Option<String>,
    #[serde(default)]
    pub recovery_checked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveTaskNode {
    pub id: RecursiveTaskId,
    pub graph_id: RecursiveTaskGraphId,
    #[serde(default)]
    pub parent_task_id: Option<RecursiveTaskId>,
    pub title: String,
    pub objective: String,
    pub scope: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    pub depth: u32,
    pub scope_units: u32,
    pub max_retries: u32,
    pub status: RecursiveTaskLifecycleState,
    #[serde(default)]
    pub decomposed_once: bool,
    #[serde(default)]
    pub integration_strategy: Option<String>,
    #[serde(default)]
    pub verification_strategy: Option<String>,
    #[serde(default)]
    pub blocked_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveTaskEdge {
    pub id: i64,
    pub graph_id: RecursiveTaskGraphId,
    pub from_task_id: RecursiveTaskId,
    pub to_task_id: RecursiveTaskId,
    pub kind: RecursiveTaskEdgeKind,
    #[serde(default)]
    pub injection_batch_id: Option<RecursiveInjectionBatchId>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveTaskAttempt {
    pub id: RecursiveAttemptId,
    pub graph_id: RecursiveTaskGraphId,
    pub task_id: RecursiveTaskId,
    pub phase: RecursiveAttemptPhase,
    pub attempt_no: u32,
    pub retry_count: u32,
    pub status: RecursiveAttemptStatus,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub failure_reason: Option<String>,
    #[serde(default)]
    pub block_reason: Option<String>,
    #[serde(default)]
    pub dependency_snapshot: Vec<RecursiveDependencySnapshot>,
    pub executor_kind: RecursiveExecutionMode,
    #[serde(default)]
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub workflow_execution_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveSchedulerRunSummary {
    pub id: RecursiveSchedulerRunId,
    pub graph_id: RecursiveTaskGraphId,
    pub status: RecursiveSchedulerRunStatus,
    pub source: RecursiveSchedulerRunSource,
    #[serde(default)]
    pub operator: Option<String>,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub stop_reason: Option<RecursiveSchedulerStopReason>,
    pub step_count: u32,
    pub max_steps: u32,
    pub executor_mode: RecursiveExecutionMode,
    #[serde(default)]
    pub failure_reason: Option<String>,
    #[serde(default)]
    pub cancellation_request_id: Option<RecursiveCancellationRequestId>,
    #[serde(default)]
    pub cancellation_reason: Option<String>,
    #[serde(default)]
    pub lease_owner: Option<String>,
    #[serde(default)]
    pub lease_token: Option<String>,
    #[serde(default)]
    pub lease_heartbeat_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub lease_expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub report_artifact_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveSchedulerRunEvent {
    pub id: i64,
    pub run_id: RecursiveSchedulerRunId,
    pub graph_id: RecursiveTaskGraphId,
    pub event_type: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveCancellationRequestSummary {
    pub id: RecursiveCancellationRequestId,
    pub graph_id: RecursiveTaskGraphId,
    #[serde(default)]
    pub run_id: Option<RecursiveSchedulerRunId>,
    #[serde(default)]
    pub task_id: Option<RecursiveTaskId>,
    pub scope: RecursiveCancellationScope,
    pub status: RecursiveCancellationRequestStatus,
    pub source: RecursiveCancellationRequestSource,
    pub reason: String,
    #[serde(default)]
    pub requested_by: Option<String>,
    pub requested_at: DateTime<Utc>,
    #[serde(default)]
    pub observed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub applied_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub rejection_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_context: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveRecoveryBudget {
    pub max_graphs: u32,
    #[serde(default)]
    pub time_budget_ms: Option<u64>,
    pub source: RecursiveRecoverySource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveRecoveryPassSummary {
    pub id: RecursiveRecoveryPassId,
    pub source: RecursiveRecoverySource,
    pub status: RecursiveRecoveryPassStatus,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
    pub max_graphs: u32,
    #[serde(default)]
    pub time_budget_ms: Option<u64>,
    pub checked: u64,
    pub recovered: u64,
    pub quarantined: u64,
    pub deferred: u64,
    pub skipped: u64,
    pub errors: u64,
    #[serde(default)]
    pub last_graph_id: Option<RecursiveTaskGraphId>,
    #[serde(default)]
    pub last_raw_graph_id: Option<String>,
    #[serde(default)]
    pub stop_reason: Option<RecursiveRecoveryStopReason>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_context: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveDeferredRecoveryGraph {
    #[serde(default)]
    pub graph_id: Option<RecursiveTaskGraphId>,
    pub raw_graph_id: String,
    pub pass_id: RecursiveRecoveryPassId,
    pub state: RecursiveGraphRecoveryState,
    pub deferred_at: DateTime<Utc>,
    pub reason: String,
    #[serde(default)]
    pub next_after: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_attempted_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveGraphRecoveryStatus {
    #[serde(default)]
    pub graph_id: Option<RecursiveTaskGraphId>,
    pub raw_graph_id: String,
    pub state: RecursiveGraphRecoveryState,
    #[serde(default)]
    pub pass_id: Option<RecursiveRecoveryPassId>,
    #[serde(default)]
    pub last_attempted_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub deferred_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveInjectionBatch {
    pub id: RecursiveInjectionBatchId,
    pub graph_id: RecursiveTaskGraphId,
    pub parent_task_id: RecursiveTaskId,
    #[serde(default)]
    pub attempt_id: Option<RecursiveAttemptId>,
    pub reason_for_decomposition: String,
    #[serde(default)]
    pub child_task_ids: Vec<RecursiveTaskId>,
    pub edge_count: u32,
    pub committed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLifecycleEvent {
    pub id: i64,
    pub graph_id: RecursiveTaskGraphId,
    pub task_id: RecursiveTaskId,
    pub from_status: RecursiveTaskLifecycleState,
    pub to_status: RecursiveTaskLifecycleState,
    #[serde(default)]
    pub reason: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveExecutionArtifact {
    pub id: i64,
    pub graph_id: RecursiveTaskGraphId,
    pub task_id: RecursiveTaskId,
    #[serde(default)]
    pub attempt_id: Option<RecursiveAttemptId>,
    pub kind: RecursiveExecutionArtifactKind,
    pub label: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveReadPage<T> {
    pub items: Vec<T>,
    pub limit: u32,
    #[serde(default)]
    pub next_cursor: Option<String>,
    pub has_more: bool,
    #[serde(default)]
    pub total_count: Option<u64>,
    #[serde(default)]
    pub warnings: Vec<RecursiveReadbackWarning>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveArtifactRole {
    RawOutput,
    NormalizedOutput,
    ValidationReport,
    ProducedArtifact,
    TestSummary,
    DiffSummary,
    SchedulerReport,
    TopologyRecursiveGraphCreation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveArtifactProvenance {
    Legacy,
    DaemonRecorded,
    LiveOutputValidation,
    SchedulerReport,
    TopologyCreation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveArtifactOwners {
    pub graph_id: RecursiveTaskGraphId,
    pub task_id: RecursiveTaskId,
    #[serde(default)]
    pub attempt_id: Option<RecursiveAttemptId>,
    #[serde(default)]
    pub live_attempt_id: Option<RecursiveLiveAttemptId>,
    #[serde(default)]
    pub scheduler_run_id: Option<RecursiveSchedulerRunId>,
    #[serde(default)]
    pub validation_id: Option<RecursiveLiveOutputValidationId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveArtifactPreviewState {
    Available,
    Truncated,
    Binary,
    Oversized,
    UnsupportedKind,
    UriBlocked,
    UriNotFound,
    MalformedMetadata,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveArtifactPreviewAvailability {
    pub state: RecursiveArtifactPreviewState,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveArtifactRenderHint {
    Plain,
    Json,
    Raw,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveByteRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLineRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveArtifactMetadataState {
    Valid,
    Malformed,
    Legacy,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecursiveArtifactContentPresence {
    Inline,
    Uri,
    Blob,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveExecutionArtifactReadback {
    pub artifact: RecursiveExecutionArtifact,
    #[serde(default)]
    pub role: Option<RecursiveArtifactRole>,
    pub provenance: RecursiveArtifactProvenance,
    pub owners: RecursiveArtifactOwners,
    pub preview: RecursiveArtifactPreviewAvailability,
    pub metadata_state: RecursiveArtifactMetadataState,
    #[serde(default)]
    pub warnings: Vec<RecursiveReadbackWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveExecutionArtifactPreview {
    pub artifact_id: i64,
    pub graph_id: RecursiveTaskGraphId,
    pub kind: RecursiveExecutionArtifactKind,
    pub label: String,
    pub content_state: RecursiveArtifactPreviewState,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub charset: Option<String>,
    #[serde(default)]
    pub digest: Option<String>,
    #[serde(default)]
    pub digest_algorithm: Option<String>,
    #[serde(default)]
    pub total_bytes: Option<u64>,
    #[serde(default)]
    pub total_lines: Option<u64>,
    #[serde(default)]
    pub byte_range: Option<RecursiveByteRange>,
    #[serde(default)]
    pub line_range: Option<RecursiveLineRange>,
    pub shown_bytes: u64,
    pub shown_lines: u64,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub binary_unavailable_reason: Option<String>,
    pub truncated: bool,
    pub truncated_by_bytes: bool,
    pub truncated_by_lines: bool,
    #[serde(default)]
    pub omitted_bytes: Option<u64>,
    #[serde(default)]
    pub omitted_lines: Option<u64>,
    pub applied_max_bytes: u32,
    pub applied_max_lines: u32,
    #[serde(default)]
    pub warnings: Vec<RecursiveReadbackWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveExecutionArtifactSummary {
    pub artifact_id: i64,
    pub graph_id: RecursiveTaskGraphId,
    pub task_id: RecursiveTaskId,
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
    pub kind: RecursiveExecutionArtifactKind,
    pub label: String,
    #[serde(default)]
    pub uri_display: Option<String>,
    pub content_presence: RecursiveArtifactContentPresence,
    #[serde(default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub digest: Option<String>,
    pub preview_state: RecursiveArtifactPreviewState,
    pub metadata_state: RecursiveArtifactMetadataState,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveTextExcerpt {
    pub text: String,
    pub shown_bytes: u64,
    pub truncated: bool,
    #[serde(default)]
    pub omitted_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveTypedTestArtifactLinks {
    #[serde(default)]
    pub stdout_artifact_id: Option<i64>,
    #[serde(default)]
    pub stderr_artifact_id: Option<i64>,
    #[serde(default)]
    pub log_artifact_id: Option<i64>,
    #[serde(default)]
    pub output_artifact_ids: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveTypedTestSummary {
    pub test_id: RecursiveTestResultId,
    pub graph_id: RecursiveTaskGraphId,
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
    pub source: RecursiveTypedSource,
    pub trust_level: RecursiveTrustLevel,
    pub status: RecursiveTypedTestStatus,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    pub display_label: String,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub failure_summary: Option<String>,
    pub metadata_state: RecursiveArtifactMetadataState,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveTypedTestDetail {
    pub summary: RecursiveTypedTestSummary,
    #[serde(default)]
    pub failure_text: Option<RecursiveTextExcerpt>,
    #[serde(default)]
    pub stdout: Option<RecursiveTextExcerpt>,
    #[serde(default)]
    pub stderr: Option<RecursiveTextExcerpt>,
    #[serde(default)]
    pub log: Option<RecursiveTextExcerpt>,
    #[serde(default)]
    pub artifact_links: RecursiveTypedTestArtifactLinks,
    #[serde(default)]
    pub related_validation_issue_ids: Vec<String>,
    #[serde(default)]
    pub retry_decision: Option<RecursiveLiveOutputRetryDecision>,
    #[serde(default)]
    pub suggested_next_action: Option<String>,
    #[serde(default)]
    pub warnings: Vec<RecursiveReadbackWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveTypedDiffSummary {
    pub diff_id: RecursiveDiffId,
    pub graph_id: RecursiveTaskGraphId,
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
    pub source: RecursiveTypedSource,
    pub trust_level: RecursiveTrustLevel,
    pub file_count: u32,
    pub binary_file_count: u32,
    pub truncated_file_count: u32,
    #[serde(default)]
    pub additions: Option<u64>,
    #[serde(default)]
    pub deletions: Option<u64>,
    #[serde(default)]
    pub hunk_count: Option<u32>,
    pub metadata_state: RecursiveArtifactMetadataState,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveTypedDiffFileSummary {
    pub file_id: RecursiveDiffFileId,
    pub diff_id: RecursiveDiffId,
    pub graph_id: RecursiveTaskGraphId,
    pub file_index: u32,
    pub display_path: PathBuf,
    #[serde(default)]
    pub previous_display_path: Option<PathBuf>,
    pub status: RecursiveTypedDiffFileStatus,
    #[serde(default)]
    pub additions: Option<u64>,
    #[serde(default)]
    pub deletions: Option<u64>,
    #[serde(default)]
    pub hunk_count: Option<u32>,
    #[serde(default)]
    pub binary: bool,
    #[serde(default)]
    pub inside_allowed_root: Option<bool>,
    #[serde(default)]
    pub validation_issue_count: u32,
    pub metadata_state: RecursiveArtifactMetadataState,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveTypedDiffDetail {
    pub summary: RecursiveTypedDiffSummary,
    pub files: RecursiveReadPage<RecursiveTypedDiffFileSummary>,
    #[serde(default)]
    pub warnings: Vec<RecursiveReadbackWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveTypedDiffHunkReadback {
    pub graph_id: RecursiveTaskGraphId,
    pub diff_id: RecursiveDiffId,
    pub file_id: RecursiveDiffFileId,
    pub state: RecursiveTypedDiffHunkState,
    #[serde(default)]
    pub reason: Option<String>,
    pub limit: u32,
    #[serde(default)]
    pub next_cursor: Option<String>,
    pub has_more: bool,
    #[serde(default)]
    pub warnings: Vec<RecursiveReadbackWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveSchedulerReportSummary {
    pub report_id: RecursiveSchedulerReportId,
    pub run_id: RecursiveSchedulerRunId,
    pub graph_id: RecursiveTaskGraphId,
    pub source: RecursiveTypedSource,
    pub trust_level: RecursiveTrustLevel,
    pub scheduler_source: RecursiveSchedulerRunSource,
    #[serde(default)]
    pub operator: Option<String>,
    pub execution_mode: RecursiveExecutionMode,
    pub run_status: RecursiveSchedulerRunStatus,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub stop_reason: Option<RecursiveSchedulerStopReason>,
    #[serde(default)]
    pub failure_reason: Option<String>,
    pub step_count: u32,
    pub max_steps: u32,
    pub selected_task_count: u32,
    pub live_attempt_count: u32,
    pub validation_count: u32,
    pub emitted_artifact_count: u32,
    #[serde(default)]
    pub cancellation_observed: bool,
    #[serde(default)]
    pub recovery_observed: bool,
    #[serde(default)]
    pub report_artifact_id: Option<i64>,
    pub metadata_state: RecursiveArtifactMetadataState,
    #[serde(default)]
    pub warnings: Vec<RecursiveReadbackWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveSchedulerReportStep {
    pub report_id: RecursiveSchedulerReportId,
    pub run_id: RecursiveSchedulerRunId,
    pub graph_id: RecursiveTaskGraphId,
    pub step_index: u32,
    pub task_id: RecursiveTaskId,
    pub phase: RecursiveAttemptPhase,
    pub attempt_id: RecursiveAttemptId,
    #[serde(default)]
    pub live_attempt_id: Option<RecursiveLiveAttemptId>,
    #[serde(default)]
    pub validation_id: Option<RecursiveLiveOutputValidationId>,
    pub outcome: RecursiveSchedulerReportStepOutcomeKind,
    #[serde(default)]
    pub message: Option<String>,
    pub final_task_status: RecursiveTaskLifecycleState,
    #[serde(default)]
    pub emitted_artifact_ids: Vec<i64>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    pub metadata_state: RecursiveArtifactMetadataState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveSchedulerReportDetail {
    pub summary: RecursiveSchedulerReportSummary,
    pub steps: RecursiveReadPage<RecursiveSchedulerReportStep>,
    #[serde(default)]
    pub events: Option<RecursiveReadPage<RecursiveSchedulerRunEvent>>,
    #[serde(default)]
    pub warnings: Vec<RecursiveReadbackWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveAttemptSummary {
    pub id: RecursiveLiveAttemptId,
    pub graph_id: RecursiveTaskGraphId,
    pub task_id: RecursiveTaskId,
    pub scheduler_run_id: RecursiveSchedulerRunId,
    pub attempt_id: RecursiveAttemptId,
    pub phase: RecursiveAttemptPhase,
    pub attempt_no: u32,
    #[serde(default)]
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub provider: Option<SessionProvider>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub sandbox_kind: Option<SandboxKind>,
    #[serde(default)]
    pub sandbox_root: Option<PathBuf>,
    #[serde(default)]
    pub sandbox_branch: Option<String>,
    #[serde(default)]
    pub sandbox_worktree_id: Option<String>,
    pub execution_mode: RecursiveExecutionMode,
    pub status: RecursiveLiveAttemptStatus,
    pub recovery_status: RecursiveLiveRecoveryStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub launched_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveAttemptDetail {
    pub summary: RecursiveLiveAttemptSummary,
    #[serde(default)]
    pub workflow_id: Option<Uuid>,
    #[serde(default)]
    pub topology_id: Option<Uuid>,
    #[serde(default)]
    pub workflow_execution_id: Option<String>,
    #[serde(default)]
    pub topology_workflow_id: Option<Uuid>,
    #[serde(default)]
    pub prompt_artifact_id: Option<i64>,
    #[serde(default)]
    pub output_artifact_id: Option<i64>,
    #[serde(default)]
    pub diff_artifact_id: Option<i64>,
    #[serde(default)]
    pub test_artifact_id: Option<i64>,
    #[serde(default)]
    pub cancellation_request_id: Option<RecursiveCancellationRequestId>,
    #[serde(default)]
    pub lease_owner: Option<String>,
    #[serde(default)]
    pub lease_token: Option<String>,
    #[serde(default)]
    pub heartbeat_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub lease_expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub max_wall_time_ms: Option<u64>,
    #[serde(default)]
    pub recovery_checked_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub recovered_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub failure_reason: Option<String>,
    #[serde(default)]
    pub interruption_reason: Option<String>,
    #[serde(default)]
    pub cancellation_reason: Option<String>,
    #[serde(default)]
    pub recovery_reason: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveAttemptHeartbeatState {
    pub live_attempt_id: RecursiveLiveAttemptId,
    pub live_attempt_status: RecursiveLiveAttemptStatus,
    pub heartbeat_status: RecursiveLiveAttemptHeartbeatStatus,
    #[serde(default)]
    pub heartbeat_owner: Option<String>,
    #[serde(default)]
    pub heartbeat_token: Option<String>,
    #[serde(default)]
    pub heartbeat_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub heartbeat_expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub failure_reason: Option<String>,
    #[serde(default)]
    pub interruption_reason: Option<String>,
    #[serde(default)]
    pub cancellation_reason: Option<String>,
    #[serde(default)]
    pub recovery_reason: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

impl RecursiveLiveAttemptHeartbeatState {
    #[must_use]
    pub const fn is_stale(&self) -> bool {
        self.heartbeat_status.is_stale()
    }
}

impl RecursiveLiveAttemptDetail {
    #[must_use]
    pub fn heartbeat_state_at(&self, at: DateTime<Utc>) -> RecursiveLiveAttemptHeartbeatState {
        let heartbeat_status = if self.summary.status.is_terminal() {
            RecursiveLiveAttemptHeartbeatStatus::Released
        } else if self.lease_token.is_none()
            || self.heartbeat_at.is_none()
            || self.lease_expires_at.is_none()
        {
            RecursiveLiveAttemptHeartbeatStatus::Missing
        } else if self
            .lease_expires_at
            .is_some_and(|expires_at| expires_at <= at)
        {
            RecursiveLiveAttemptHeartbeatStatus::Stale
        } else {
            RecursiveLiveAttemptHeartbeatStatus::Active
        };

        RecursiveLiveAttemptHeartbeatState {
            live_attempt_id: self.summary.id,
            live_attempt_status: self.summary.status,
            heartbeat_status,
            heartbeat_owner: self.lease_owner.clone(),
            heartbeat_token: self.lease_token.clone(),
            heartbeat_at: self.heartbeat_at,
            heartbeat_expires_at: self.lease_expires_at,
            session_id: self.summary.session_id,
            failure_reason: self.failure_reason.clone(),
            interruption_reason: self.interruption_reason.clone(),
            cancellation_reason: self.cancellation_reason.clone(),
            recovery_reason: self.recovery_reason.clone(),
            error: self.error.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveInterruptSummary {
    pub id: RecursiveLiveInterruptId,
    pub live_attempt_id: RecursiveLiveAttemptId,
    pub graph_id: RecursiveTaskGraphId,
    pub task_id: RecursiveTaskId,
    pub scheduler_run_id: RecursiveSchedulerRunId,
    pub attempt_id: RecursiveAttemptId,
    #[serde(default)]
    pub session_id: Option<Uuid>,
    #[serde(default)]
    pub cancellation_request_id: Option<RecursiveCancellationRequestId>,
    pub status: RecursiveLiveInterruptStatus,
    pub reason: String,
    #[serde(default)]
    pub failure_reason: Option<String>,
    pub requested_at: DateTime<Utc>,
    #[serde(default)]
    pub sent_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveReadbackWarning {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub resource_type: Option<String>,
    #[serde(default)]
    pub resource_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveLinkedSessionSummary {
    pub session_id: Uuid,
    pub status: SessionStatus,
    pub session_kind: SessionKind,
    pub provider: SessionProvider,
    #[serde(default)]
    pub model: Option<String>,
    pub working_dir: PathBuf,
    #[serde(default)]
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveAttemptArtifactReadback {
    #[serde(default)]
    pub prompt_artifact: Option<RecursiveExecutionArtifact>,
    #[serde(default)]
    pub raw_output_artifacts: Vec<RecursiveExecutionArtifact>,
    #[serde(default)]
    pub normalized_output_artifact: Option<RecursiveExecutionArtifact>,
    #[serde(default)]
    pub validation_artifacts: Vec<RecursiveExecutionArtifact>,
    #[serde(default)]
    pub diff_artifacts: Vec<RecursiveExecutionArtifact>,
    #[serde(default)]
    pub test_artifacts: Vec<RecursiveExecutionArtifact>,
    #[serde(default)]
    pub produced_artifacts: Vec<RecursiveExecutionArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveAttemptReadback {
    pub live_attempt: RecursiveLiveAttemptDetail,
    pub task: RecursiveTaskNode,
    pub recursive_attempt: RecursiveTaskAttempt,
    pub scheduler_run: RecursiveSchedulerRunSummary,
    #[serde(default)]
    pub session: Option<RecursiveLiveLinkedSessionSummary>,
    #[serde(default)]
    pub heartbeat: Option<RecursiveLiveAttemptHeartbeatState>,
    #[serde(default)]
    pub latest_interrupt: Option<RecursiveLiveInterruptSummary>,
    #[serde(default)]
    pub latest_validation: Option<RecursiveLiveOutputValidationSummary>,
    #[serde(default)]
    pub artifacts: Option<RecursiveLiveAttemptArtifactReadback>,
    #[serde(default)]
    pub retry_history: Option<Vec<RecursiveTaskAttempt>>,
    #[serde(default)]
    pub warnings: Vec<RecursiveReadbackWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveAttemptListItem {
    pub summary: RecursiveLiveAttemptSummary,
    #[serde(default)]
    pub heartbeat: Option<RecursiveLiveAttemptHeartbeatState>,
    #[serde(default)]
    pub latest_interrupt: Option<RecursiveLiveInterruptSummary>,
    #[serde(default)]
    pub latest_validation: Option<RecursiveLiveOutputValidationSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveRecoveryReadback {
    #[serde(default)]
    pub graph_recovery: Option<RecursiveGraphRecoveryStatus>,
    #[serde(default)]
    pub live_attempts: Vec<RecursiveLiveAttemptDetail>,
    #[serde(default)]
    pub heartbeat_states: Vec<RecursiveLiveAttemptHeartbeatState>,
    #[serde(default)]
    pub scheduler_runs: Vec<RecursiveSchedulerRunSummary>,
    #[serde(default)]
    pub linked_sessions: Vec<RecursiveLiveLinkedSessionSummary>,
    #[serde(default)]
    pub deferred_graph: Option<RecursiveDeferredRecoveryGraph>,
    #[serde(default)]
    pub operator_review_required: bool,
    #[serde(default)]
    pub warnings: Vec<RecursiveReadbackWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveLiveOutputValidationListItem {
    pub summary: RecursiveLiveOutputValidationSummary,
    pub artifact_links: RecursiveLiveOutputValidationArtifactLinks,
    #[serde(default)]
    pub issues: Option<Vec<RecursiveLiveOutputValidationIssue>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveTaskGraphDetail {
    pub graph: RecursiveTaskGraphSummary,
    #[serde(default)]
    pub nodes: Vec<RecursiveTaskNode>,
    #[serde(default)]
    pub edges: Vec<RecursiveTaskEdge>,
    #[serde(default)]
    pub attempts: Vec<RecursiveTaskAttempt>,
    #[serde(default)]
    pub injection_batches: Vec<RecursiveInjectionBatch>,
    #[serde(default)]
    pub lifecycle_events: Vec<RecursiveLifecycleEvent>,
    #[serde(default)]
    pub artifacts: Vec<RecursiveExecutionArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveSchedulerRunDetail {
    pub run: RecursiveSchedulerRunSummary,
    pub graph: RecursiveTaskGraphSummary,
    #[serde(default)]
    pub active_attempts: Vec<RecursiveTaskAttempt>,
    #[serde(default)]
    pub cancellation_requests: Vec<RecursiveCancellationRequestSummary>,
    #[serde(default)]
    pub events: Vec<RecursiveSchedulerRunEvent>,
    #[serde(default)]
    pub report_artifact: Option<RecursiveExecutionArtifact>,
    #[serde(default)]
    pub live_attempts: Vec<RecursiveLiveAttemptSummary>,
    #[serde(default)]
    pub live_heartbeat_states: Vec<RecursiveLiveAttemptHeartbeatState>,
    #[serde(default)]
    pub live_interrupts: Vec<RecursiveLiveInterruptSummary>,
    #[serde(default)]
    pub latest_live_validations: Vec<RecursiveLiveOutputValidationSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveDagOperationalStatus {
    pub graph: RecursiveTaskGraphSummary,
    #[serde(default)]
    pub active_run: Option<RecursiveSchedulerRunSummary>,
    #[serde(default)]
    pub latest_run: Option<RecursiveSchedulerRunSummary>,
    #[serde(default)]
    pub open_cancellation_requests: Vec<RecursiveCancellationRequestSummary>,
    #[serde(default)]
    pub running_attempts: Vec<RecursiveTaskAttempt>,
    pub recovery: RecursiveGraphRecoveryStatus,
    #[serde(default)]
    pub active_live_attempts: Vec<RecursiveLiveAttemptSummary>,
    #[serde(default)]
    pub active_live_heartbeats: Vec<RecursiveLiveAttemptHeartbeatState>,
    #[serde(default)]
    pub active_live_interrupts: Vec<RecursiveLiveInterruptSummary>,
    #[serde(default)]
    pub live_recovery_pending: Vec<RecursiveLiveAttemptSummary>,
    #[serde(default)]
    pub latest_live_validations: Vec<RecursiveLiveOutputValidationSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecursiveDagRecoveryStatus {
    #[serde(default)]
    pub latest_pass: Option<RecursiveRecoveryPassSummary>,
    #[serde(default)]
    pub graph_status: Option<RecursiveGraphRecoveryStatus>,
    #[serde(default)]
    pub deferred_graphs: Vec<RecursiveDeferredRecoveryGraph>,
    pub deferred_graph_count: u64,
    #[serde(default)]
    pub oldest_deferred_graph: Option<RecursiveDeferredRecoveryGraph>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum RecursiveDagError {
    #[error("recursive DAG not found: {0}")]
    NotFound(RecursiveTaskGraphId),
    #[error("recursive DAG integrity error: {0}")]
    Integrity(String),
    #[error("invalid recursive DAG value: {0}")]
    InvalidValue(String),
}

pub type RecursiveDagResult<T> = std::result::Result<T, RecursiveDagError>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde::de::DeserializeOwned;

    fn output_correlation() -> RecursiveLiveOutputCorrelation {
        RecursiveLiveOutputCorrelation {
            graph_id: RecursiveTaskGraphId::new(),
            task_id: RecursiveTaskId::new(),
            attempt_id: RecursiveAttemptId::new(),
            live_attempt_id: RecursiveLiveAttemptId::new(),
            scheduler_run_id: RecursiveSchedulerRunId::new(),
            session_id: Some(Uuid::new_v4()),
        }
    }

    fn artifact(label: &str) -> RecursiveLiveOutputArtifactReference {
        RecursiveLiveOutputArtifactReference {
            label: label.to_string(),
            kind: RecursiveExecutionArtifactKind::File,
            task_id: Some(RecursiveTaskId::new()),
            attempt_id: Some(RecursiveAttemptId::new()),
            uri: Some(format!("file:///tmp/{label}.txt")),
            content_digest: Some(format!("sha256:{label}")),
            description: Some(format!("{label} artifact")),
            existing_artifact_id: None,
            metadata: serde_json::json!({"label": label}),
        }
    }

    fn output_with(outcome: RecursiveLiveOutputOutcome) -> RecursiveLiveOutputEnvelope {
        let artifact = artifact("primary");
        RecursiveLiveOutputEnvelope {
            schema_version: RECURSIVE_LIVE_OUTPUT_SCHEMA_VERSION,
            correlation: output_correlation(),
            summary: "task reached a terminal live output".to_string(),
            artifacts: vec![artifact.clone()],
            tests: vec![RecursiveLiveTestResultSummary {
                command: Some("cargo test -p rsi-common".to_string()),
                status: RecursiveLiveTestStatus::Passed,
                exit_code: Some(0),
                duration_ms: Some(1200),
                output_artifact: Some(artifact.clone()),
                required: true,
                reason: None,
                metadata: serde_json::json!({"suite": "recursive_dag"}),
            }],
            diffs: vec![RecursiveLiveDiffSummary {
                worktree_root: Some(PathBuf::from("/tmp/worktree")),
                sandbox_root: Some(PathBuf::from("/tmp/sandbox")),
                files: vec![RecursiveLiveDiffFileSummary {
                    path: PathBuf::from("crates/rsi-common/src/recursive_dag.rs"),
                    status: RecursiveLiveDiffFileStatus::Modified,
                    previous_path: None,
                    insertions: Some(10),
                    deletions: Some(2),
                    inside_allowed_root: Some(true),
                    metadata: serde_json::Value::Null,
                }],
                diff_artifact: Some(artifact),
                clean_worktree: Some(false),
                metadata: serde_json::Value::Null,
            }],
            dependency_outputs: vec![RecursiveLiveDependencyOutputReference {
                task_id: RecursiveTaskId::new(),
                status: RecursiveTaskLifecycleState::Succeeded,
                artifact_ids: vec![42],
                summary: "used dependency output".to_string(),
                metadata: serde_json::Value::Null,
            }],
            notes: vec!["operator-visible note".to_string()],
            metadata: serde_json::json!({"source": "test"}),
            confidence: Some(RecursiveLiveOutputConfidence {
                self_assessment: RecursiveLiveOutputConfidenceLevel::Medium,
                evidence: vec!["tests passed".to_string()],
                known_risks: vec!["model-reported only".to_string()],
                metadata: serde_json::Value::Null,
            }),
            outcome,
        }
    }

    fn round_trip_output(
        output: &RecursiveLiveOutputEnvelope,
    ) -> Result<serde_json::Value, serde_json::Error> {
        let value = serde_json::to_value(output)?;
        let parsed: RecursiveLiveOutputEnvelope = serde_json::from_value(value.clone())?;
        assert_eq!(parsed, *output);
        Ok(value)
    }

    fn assert_json_name<T>(value: T, expected: &str) -> Result<(), serde_json::Error>
    where
        T: serde::Serialize + DeserializeOwned + std::fmt::Debug + PartialEq,
    {
        let serialized = serde_json::to_value(&value)?;
        assert_eq!(serialized, serde_json::json!(expected));
        let parsed: T = serde_json::from_value(serialized)?;
        assert_eq!(parsed, value);
        Ok(())
    }

    #[test]
    fn recursive_typed_inspector_enum_wire_names_are_stable() -> Result<(), serde_json::Error> {
        assert_json_name(RecursiveTypedSource::DaemonCollected, "daemon_collected")?;
        assert_json_name(RecursiveTypedSource::LiveValidation, "live_validation")?;
        assert_json_name(RecursiveTypedSource::SchedulerReport, "scheduler_report")?;
        assert_json_name(RecursiveTypedSource::LegacyBackfill, "legacy_backfill")?;
        assert_json_name(RecursiveTypedSource::ModelClaimed, "model_claimed")?;
        assert_unknown_string_rejected::<RecursiveTypedSource>("artifact_claimed");

        assert_json_name(RecursiveTrustLevel::Verified, "verified")?;
        assert_json_name(RecursiveTrustLevel::Normalized, "normalized")?;
        assert_json_name(RecursiveTrustLevel::SchedulerOwned, "scheduler_owned")?;
        assert_json_name(RecursiveTrustLevel::ModelClaimed, "model_claimed")?;
        assert_json_name(RecursiveTrustLevel::LegacyUnverified, "legacy_unverified")?;
        assert_unknown_string_rejected::<RecursiveTrustLevel>("trusted");

        assert_json_name(RecursiveTypedTestStatus::Passed, "passed")?;
        assert_json_name(RecursiveTypedTestStatus::Failed, "failed")?;
        assert_json_name(RecursiveTypedTestStatus::Skipped, "skipped")?;
        assert_json_name(RecursiveTypedTestStatus::NotRun, "not_run")?;
        assert_json_name(RecursiveTypedTestStatus::Unknown, "unknown")?;
        assert_unknown_string_rejected::<RecursiveTypedTestStatus>("flaky");

        assert_json_name(RecursiveTypedDiffFileStatus::Added, "added")?;
        assert_json_name(RecursiveTypedDiffFileStatus::Modified, "modified")?;
        assert_json_name(RecursiveTypedDiffFileStatus::Deleted, "deleted")?;
        assert_json_name(RecursiveTypedDiffFileStatus::Renamed, "renamed")?;
        assert_json_name(RecursiveTypedDiffFileStatus::Copied, "copied")?;
        assert_json_name(RecursiveTypedDiffFileStatus::Unchanged, "unchanged")?;
        assert_json_name(RecursiveTypedDiffFileStatus::Unknown, "unknown")?;
        assert_unknown_string_rejected::<RecursiveTypedDiffFileStatus>("mode_changed");

        assert_json_name(RecursiveTypedDiffHunkState::Deferred, "deferred")?;
        assert_json_name(RecursiveTypedDiffHunkState::Unavailable, "unavailable")?;
        assert_unknown_string_rejected::<RecursiveTypedDiffHunkState>("available");

        assert_json_name(
            RecursiveSchedulerReportStepOutcomeKind::Succeeded,
            "succeeded",
        )?;
        assert_json_name(
            RecursiveSchedulerReportStepOutcomeKind::Decomposed,
            "decomposed",
        )?;
        assert_json_name(RecursiveSchedulerReportStepOutcomeKind::Failed, "failed")?;
        assert_json_name(RecursiveSchedulerReportStepOutcomeKind::Blocked, "blocked")?;
        assert_json_name(
            RecursiveSchedulerReportStepOutcomeKind::Cancelled,
            "cancelled",
        )?;
        assert_json_name(RecursiveSchedulerReportStepOutcomeKind::Stopped, "stopped")?;
        assert_json_name(RecursiveSchedulerReportStepOutcomeKind::Unknown, "unknown")?;
        assert_unknown_string_rejected::<RecursiveSchedulerReportStepOutcomeKind>("retried");

        Ok(())
    }

    fn assert_unknown_string_rejected<T>(unknown: &str)
    where
        T: DeserializeOwned,
    {
        assert!(serde_json::from_value::<T>(serde_json::json!(unknown)).is_err());
    }

    #[test]
    fn recursive_live_output_enum_wire_names_are_stable() -> Result<(), serde_json::Error> {
        assert_json_name(RecursiveLiveOutputKind::Success, "success")?;
        assert_json_name(RecursiveLiveOutputKind::Decomposition, "decomposition")?;
        assert_json_name(
            RecursiveLiveOutputKind::RetryableFailure,
            "retryable_failure",
        )?;
        assert_json_name(
            RecursiveLiveOutputKind::PermanentFailure,
            "permanent_failure",
        )?;
        assert_json_name(RecursiveLiveOutputKind::Blocked, "blocked")?;
        assert_json_name(RecursiveLiveOutputKind::Cancelled, "cancelled")?;

        assert_json_name(RecursiveLiveOutputConfidenceLevel::Low, "low")?;
        assert_json_name(RecursiveLiveOutputConfidenceLevel::Medium, "medium")?;
        assert_json_name(RecursiveLiveOutputConfidenceLevel::High, "high")?;

        assert_json_name(RecursiveLiveAcceptanceStatus::Met, "met")?;
        assert_json_name(
            RecursiveLiveAcceptanceStatus::NotApplicable,
            "not_applicable",
        )?;
        assert_json_name(RecursiveLiveAcceptanceStatus::NotVerified, "not_verified")?;
        assert_json_name(RecursiveLiveAcceptanceStatus::Unmet, "unmet")?;

        assert_json_name(RecursiveLiveTestStatus::Passed, "passed")?;
        assert_json_name(RecursiveLiveTestStatus::Failed, "failed")?;
        assert_json_name(RecursiveLiveTestStatus::Skipped, "skipped")?;
        assert_json_name(RecursiveLiveTestStatus::NotRun, "not_run")?;

        assert_json_name(RecursiveLiveDiffFileStatus::Added, "added")?;
        assert_json_name(RecursiveLiveDiffFileStatus::Modified, "modified")?;
        assert_json_name(RecursiveLiveDiffFileStatus::Deleted, "deleted")?;
        assert_json_name(RecursiveLiveDiffFileStatus::Renamed, "renamed")?;
        assert_json_name(RecursiveLiveDiffFileStatus::Copied, "copied")?;
        assert_json_name(RecursiveLiveDiffFileStatus::Unchanged, "unchanged")?;
        assert_json_name(RecursiveLiveDiffFileStatus::Unknown, "unknown")?;

        assert_json_name(
            RecursiveLivePermanentFailureClass::InvalidScope,
            "invalid_scope",
        )?;
        assert_json_name(
            RecursiveLivePermanentFailureClass::ImpossibleRequirement,
            "impossible_requirement",
        )?;
        assert_json_name(
            RecursiveLivePermanentFailureClass::UnsafeRequest,
            "unsafe_request",
        )?;
        assert_json_name(
            RecursiveLivePermanentFailureClass::MissingIrreplaceableInput,
            "missing_irreplaceable_input",
        )?;
        assert_json_name(
            RecursiveLivePermanentFailureClass::ToolingNotAvailable,
            "tooling_not_available",
        )?;
        assert_json_name(RecursiveLivePermanentFailureClass::Other, "other")?;

        assert_json_name(RecursiveLiveBlockedOn::Approval, "approval")?;
        assert_json_name(RecursiveLiveBlockedOn::Credential, "credential")?;
        assert_json_name(RecursiveLiveBlockedOn::MissingContext, "missing_context")?;
        assert_json_name(RecursiveLiveBlockedOn::ExternalService, "external_service")?;
        assert_json_name(RecursiveLiveBlockedOn::MergeConflict, "merge_conflict")?;
        assert_json_name(RecursiveLiveBlockedOn::UnsafeState, "unsafe_state")?;
        assert_json_name(RecursiveLiveBlockedOn::Other, "other")?;

        assert_json_name(RecursiveLiveOutputValidationStatus::Valid, "valid")?;
        assert_json_name(RecursiveLiveOutputValidationStatus::Invalid, "invalid")?;
        assert_json_name(
            RecursiveLiveOutputValidationStatus::Repairable,
            "repairable",
        )?;
        assert_json_name(RecursiveLiveOutputValidationStatus::Ambiguous, "ambiguous")?;
        assert_json_name(
            RecursiveLiveOutputValidationStatus::OperatorReviewRequired,
            "operator_review_required",
        )?;

        assert_json_name(RecursiveLiveValidationIssueSeverity::Error, "error")?;
        assert_json_name(RecursiveLiveValidationIssueSeverity::Warning, "warning")?;
        assert_json_name(RecursiveLiveValidationIssueSeverity::Info, "info")?;

        assert_json_name(RecursiveLiveValidationIssueClass::Malformed, "malformed")?;
        assert_json_name(RecursiveLiveValidationIssueClass::Missing, "missing")?;
        assert_json_name(RecursiveLiveValidationIssueClass::Unsafe, "unsafe")?;
        assert_json_name(RecursiveLiveValidationIssueClass::Ambiguous, "ambiguous")?;
        assert_json_name(RecursiveLiveValidationIssueClass::Invalid, "invalid")?;
        assert_json_name(RecursiveLiveValidationIssueClass::Mismatch, "mismatch")?;
        assert_json_name(RecursiveLiveValidationIssueClass::Policy, "policy")?;
        assert_json_name(
            RecursiveLiveValidationIssueClass::Cancellation,
            "cancellation",
        )?;
        assert_json_name(RecursiveLiveValidationIssueClass::Other, "other")?;

        assert_json_name(
            RecursiveLiveValidationIssueCode::MalformedOutput,
            "malformed_output",
        )?;
        assert_json_name(
            RecursiveLiveValidationIssueCode::MissingRequiredField,
            "missing_required_field",
        )?;
        assert_json_name(
            RecursiveLiveValidationIssueCode::UnknownField,
            "unknown_field",
        )?;
        assert_json_name(
            RecursiveLiveValidationIssueCode::UnknownSchemaVersion,
            "unknown_schema_version",
        )?;
        assert_json_name(
            RecursiveLiveValidationIssueCode::CorrelationMismatch,
            "correlation_mismatch",
        )?;
        assert_json_name(
            RecursiveLiveValidationIssueCode::AmbiguousTerminalKind,
            "ambiguous_terminal_kind",
        )?;
        assert_json_name(
            RecursiveLiveValidationIssueCode::InvalidDecomposition,
            "invalid_decomposition",
        )?;
        assert_json_name(
            RecursiveLiveValidationIssueCode::ChildNotSmallerThanParent,
            "child_not_smaller_than_parent",
        )?;
        assert_json_name(
            RecursiveLiveValidationIssueCode::DependencyCycle,
            "dependency_cycle",
        )?;
        assert_json_name(
            RecursiveLiveValidationIssueCode::UnknownDependency,
            "unknown_dependency",
        )?;
        assert_json_name(
            RecursiveLiveValidationIssueCode::ArtifactMismatch,
            "artifact_mismatch",
        )?;
        assert_json_name(
            RecursiveLiveValidationIssueCode::UnsafeToolClaim,
            "unsafe_tool_claim",
        )?;
        assert_json_name(
            RecursiveLiveValidationIssueCode::UnsafeSandboxClaim,
            "unsafe_sandbox_claim",
        )?;
        assert_json_name(
            RecursiveLiveValidationIssueCode::CancelWithoutRequest,
            "cancel_without_request",
        )?;
        assert_json_name(
            RecursiveLiveValidationIssueCode::SuccessWithFailedRequiredTest,
            "success_with_failed_required_test",
        )?;
        assert_json_name(
            RecursiveLiveValidationIssueCode::SuccessWithUnmetAcceptance,
            "success_with_unmet_acceptance",
        )?;
        assert_json_name(
            RecursiveLiveValidationIssueCode::IncoherentTestCounts,
            "incoherent_test_counts",
        )?;
        assert_json_name(
            RecursiveLiveValidationIssueCode::ModelOutputInvalid,
            "model_output_invalid",
        )?;
        assert_json_name(RecursiveLiveValidationIssueCode::Other, "other")?;

        assert_json_name(RecursiveLiveOutputMappingDecision::Success, "success")?;
        assert_json_name(
            RecursiveLiveOutputMappingDecision::Decomposition,
            "decomposition",
        )?;
        assert_json_name(
            RecursiveLiveOutputMappingDecision::RetryFailure,
            "retry_failure",
        )?;
        assert_json_name(
            RecursiveLiveOutputMappingDecision::PermanentFailure,
            "permanent_failure",
        )?;
        assert_json_name(RecursiveLiveOutputMappingDecision::Blocked, "blocked")?;
        assert_json_name(RecursiveLiveOutputMappingDecision::Cancelled, "cancelled")?;
        assert_json_name(
            RecursiveLiveOutputMappingDecision::RepairSameLiveAttempt,
            "repair_same_live_attempt",
        )?;
        assert_json_name(
            RecursiveLiveOutputMappingDecision::FailAttempt,
            "fail_attempt",
        )?;
        assert_json_name(RecursiveLiveOutputMappingDecision::BlockTask, "block_task")?;
        assert_json_name(
            RecursiveLiveOutputMappingDecision::OperatorReview,
            "operator_review",
        )?;
        assert_json_name(RecursiveLiveOutputMappingDecision::NoOp, "no_op")?;

        assert_json_name(
            RecursiveLiveOutputRetryDecisionKind::NotApplicable,
            "not_applicable",
        )?;
        assert_json_name(
            RecursiveLiveOutputRetryDecisionKind::RetrySameLiveAttempt,
            "retry_same_live_attempt",
        )?;
        assert_json_name(
            RecursiveLiveOutputRetryDecisionKind::RetryTaskAttempt,
            "retry_task_attempt",
        )?;
        assert_json_name(RecursiveLiveOutputRetryDecisionKind::NoRetry, "no_retry")?;
        assert_json_name(
            RecursiveLiveOutputRetryDecisionKind::OperatorReview,
            "operator_review",
        )?;

        Ok(())
    }

    #[test]
    fn recursive_live_output_parser_source_wire_names_are_stable() -> Result<(), serde_json::Error>
    {
        let sources = [
            (
                RecursiveLiveOutputParserSource::Artifact { artifact_id: 7 },
                "artifact",
            ),
            (
                RecursiveLiveOutputParserSource::ConversationEvent {
                    event_id: 8,
                    sequence: Some(9),
                },
                "conversation_event",
            ),
            (
                RecursiveLiveOutputParserSource::FinalJsonBlock {
                    event_id: Some(10),
                    block_index: Some(0),
                },
                "final_json_block",
            ),
            (
                RecursiveLiveOutputParserSource::Inline {
                    description: Some("test source".to_string()),
                },
                "inline",
            ),
        ];

        for (source, expected_kind) in sources {
            let value = serde_json::to_value(&source)?;
            assert_eq!(value["kind"], serde_json::json!(expected_kind));
            let parsed: RecursiveLiveOutputParserSource = serde_json::from_value(value)?;
            assert_eq!(parsed, source);
        }

        Ok(())
    }

    #[test]
    fn recursive_live_attempt_status_serde_round_trips() -> Result<(), serde_json::Error> {
        let value = serde_json::to_value(RecursiveLiveAttemptStatus::WaitingApproval)?;
        assert_eq!(value, serde_json::json!("waiting_approval"));
        let parsed: RecursiveLiveAttemptStatus = serde_json::from_value(value)?;
        assert_eq!(parsed, RecursiveLiveAttemptStatus::WaitingApproval);
        assert!(RecursiveLiveAttemptStatus::Succeeded.is_terminal());
        assert!(!RecursiveLiveAttemptStatus::RecoveryPending.is_terminal());
        Ok(())
    }

    #[test]
    fn recursive_live_interrupt_status_serde_round_trips() -> Result<(), serde_json::Error> {
        let value = serde_json::to_value(RecursiveLiveInterruptStatus::Sent)?;
        assert_eq!(value, serde_json::json!("sent"));
        let parsed: RecursiveLiveInterruptStatus = serde_json::from_value(value)?;
        assert_eq!(parsed, RecursiveLiveInterruptStatus::Sent);
        assert!(RecursiveLiveInterruptStatus::Interrupted.is_terminal());
        assert!(!RecursiveLiveInterruptStatus::Requested.is_terminal());
        Ok(())
    }

    #[test]
    fn recursive_live_attempt_heartbeat_status_serde_round_trips() -> Result<(), serde_json::Error>
    {
        let value = serde_json::to_value(RecursiveLiveAttemptHeartbeatStatus::Stale)?;
        assert_eq!(value, serde_json::json!("stale"));
        let parsed: RecursiveLiveAttemptHeartbeatStatus = serde_json::from_value(value)?;
        assert_eq!(parsed, RecursiveLiveAttemptHeartbeatStatus::Stale);
        assert!(RecursiveLiveAttemptHeartbeatStatus::Stale.is_stale());
        assert!(!RecursiveLiveAttemptHeartbeatStatus::Active.is_stale());
        Ok(())
    }

    #[test]
    fn recursive_live_execution_mode_serde_round_trips() -> Result<(), serde_json::Error> {
        let value = serde_json::to_value(RecursiveExecutionMode::LiveSession)?;
        assert_eq!(value, serde_json::json!("live_session"));
        let parsed: RecursiveExecutionMode = serde_json::from_value(value)?;
        assert_eq!(parsed, RecursiveExecutionMode::LiveSession);
        Ok(())
    }

    #[test]
    fn recursive_live_output_success_payload_serde_round_trips() -> Result<(), serde_json::Error> {
        let output = output_with(RecursiveLiveOutputOutcome::Success(
            RecursiveLiveSuccessPayload {
                result_summary: "implemented the selected task".to_string(),
                acceptance: vec![RecursiveLiveAcceptanceCheck {
                    criterion: "shared types exist".to_string(),
                    status: RecursiveLiveAcceptanceStatus::Met,
                    evidence: vec!["serde round trip passed".to_string()],
                    artifact_ids: vec![1],
                    notes: Some("covered by rsi-common tests".to_string()),
                }],
            },
        ));

        let value = round_trip_output(&output)?;

        assert_eq!(value["schema_version"], serde_json::json!(1));
        assert_eq!(value["kind"], serde_json::json!("success"));
        assert_eq!(
            value["confidence"]["self_assessment"],
            serde_json::json!("medium")
        );
        assert_eq!(value["tests"][0]["status"], serde_json::json!("passed"));
        assert_eq!(value["diffs"][0]["files"][0]["status"], "modified");
        Ok(())
    }

    #[test]
    fn recursive_live_output_decomposition_payload_serde_round_trips()
    -> Result<(), serde_json::Error> {
        let child_id = RecursiveTaskId::new();
        let child_ref = RecursiveLiveTaskDependencyRef::ChildLocal {
            local_id: "child-a".to_string(),
        };
        let output = output_with(RecursiveLiveOutputOutcome::Decomposition(
            RecursiveLiveDecompositionPayload {
                reason: "parent scope is too broad".to_string(),
                children: vec![RecursiveLiveChildTaskSpec {
                    local_id: "child-a".to_string(),
                    task_id: Some(child_id),
                    title: "Add shared output types".to_string(),
                    objective: "Define serializable live output envelopes".to_string(),
                    scope: "rsi-common recursive DAG shared types".to_string(),
                    acceptance_criteria: vec!["types compile".to_string()],
                    scope_units: 1,
                    max_retries: 1,
                    dependencies: vec![RecursiveLiveTaskDependencyRef::ExistingTask {
                        task_id: RecursiveTaskId::new(),
                    }],
                    metadata: serde_json::json!({"local": true}),
                }],
                dependency_edges: vec![RecursiveLiveDependencyEdgeSpec {
                    from: child_ref.clone(),
                    to: child_ref,
                    rationale: Some("self edge is validation input only".to_string()),
                    metadata: serde_json::json!({"edge": "self-validation"}),
                }],
                integration_strategy: "integrate children after they succeed".to_string(),
                verification_strategy: "run rsi-common tests".to_string(),
                budget_hints: Some(RecursiveLiveDecompositionBudgetHints {
                    expected_child_count: Some(1),
                    total_scope_units: Some(1),
                    max_child_scope_units: Some(1),
                    retry_budget: Some(1),
                    scope_notes: vec!["smaller than parent".to_string()],
                    metadata: serde_json::Value::Null,
                }),
                scope_hints: vec!["type-only phase".to_string()],
            },
        ));

        let value = round_trip_output(&output)?;

        assert_eq!(value["kind"], serde_json::json!("decomposition"));
        assert_eq!(value["children"][0]["local_id"], "child-a");
        assert_eq!(
            value["dependency_edges"][0]["metadata"]["edge"],
            serde_json::json!("self-validation")
        );
        Ok(())
    }

    #[test]
    fn recursive_live_output_retryable_failure_payload_serde_round_trips()
    -> Result<(), serde_json::Error> {
        let output = output_with(RecursiveLiveOutputOutcome::RetryableFailure(
            RecursiveLiveRetryableFailurePayload {
                reason: "transient test infrastructure failure".to_string(),
                retry_hint: "rerun after cache is restored".to_string(),
                operator_message: Some("Retry is safe once the cache is available".to_string()),
                evidence: vec![artifact("retry-evidence")],
                partial_artifacts: vec![artifact("partial-output")],
                suggested_next_action: Some("retry same task".to_string()),
            },
        ));

        let value = round_trip_output(&output)?;

        assert_eq!(value["kind"], serde_json::json!("retryable_failure"));
        assert_eq!(value["partial_artifacts"][0]["label"], "partial-output");
        Ok(())
    }

    #[test]
    fn recursive_live_output_permanent_failure_payload_serde_round_trips()
    -> Result<(), serde_json::Error> {
        let output = output_with(RecursiveLiveOutputOutcome::PermanentFailure(
            RecursiveLivePermanentFailurePayload {
                reason: "required input cannot be recovered".to_string(),
                failure_class: RecursiveLivePermanentFailureClass::MissingIrreplaceableInput,
                operator_message: Some("The task needs missing external input".to_string()),
                evidence: vec![artifact("permanent-evidence")],
                suggested_next_action: Some("revise task scope".to_string()),
            },
        ));

        let value = round_trip_output(&output)?;

        assert_eq!(value["kind"], serde_json::json!("permanent_failure"));
        assert_eq!(
            value["failure_class"],
            serde_json::json!("missing_irreplaceable_input")
        );
        Ok(())
    }

    #[test]
    fn recursive_live_output_blocked_payload_serde_round_trips() -> Result<(), serde_json::Error> {
        let output = output_with(RecursiveLiveOutputOutcome::Blocked(
            RecursiveLiveBlockedPayload {
                reason: "operator approval is required".to_string(),
                requested_operator_input: "Approve the credential use".to_string(),
                blocked_on: RecursiveLiveBlockedOn::Approval,
                safe_to_retry_without_input: false,
                operator_message: Some("Waiting for an explicit approval".to_string()),
                evidence: vec![artifact("blocked-evidence")],
                suggested_next_action: Some("request approval".to_string()),
            },
        ));

        let value = round_trip_output(&output)?;

        assert_eq!(value["kind"], serde_json::json!("blocked"));
        assert_eq!(value["blocked_on"], serde_json::json!("approval"));
        assert_eq!(
            value["safe_to_retry_without_input"],
            serde_json::json!(false)
        );
        Ok(())
    }

    #[test]
    fn recursive_live_output_cancelled_payload_serde_round_trips() -> Result<(), serde_json::Error>
    {
        let cancellation_request_id = RecursiveCancellationRequestId::new();
        let output = output_with(RecursiveLiveOutputOutcome::Cancelled(
            RecursiveLiveCancelledPayload {
                reason: "graph cancellation was observed".to_string(),
                cancellation_request_id: Some(cancellation_request_id),
                observed_scope: RecursiveCancellationScope::Graph,
                operator_message: Some("Cancellation accepted".to_string()),
                evidence: vec![artifact("cancel-evidence")],
                suggested_next_action: Some("stop scheduling descendants".to_string()),
            },
        ));

        let value = round_trip_output(&output)?;

        assert_eq!(value["kind"], serde_json::json!("cancelled"));
        assert_eq!(value["observed_scope"], serde_json::json!("graph"));
        assert_eq!(
            value["cancellation_request_id"],
            serde_json::json!(cancellation_request_id)
        );
        Ok(())
    }

    #[test]
    fn recursive_live_output_validation_issue_and_result_serde_round_trips()
    -> Result<(), serde_json::Error> {
        let correlation = output_correlation();
        let issue = RecursiveLiveOutputValidationIssue {
            code: RecursiveLiveValidationIssueCode::MissingRequiredField,
            severity: RecursiveLiveValidationIssueSeverity::Error,
            class: RecursiveLiveValidationIssueClass::Missing,
            location: Some(RecursiveLiveValidationIssueLocation::OutputPath {
                path: "/result_summary".to_string(),
            }),
            message: "result_summary is required".to_string(),
            evidence: vec![artifact("validation-evidence")],
            suggested_next_action: Some("repair final JSON output".to_string()),
            metadata: serde_json::json!({"field": "result_summary"}),
        };
        let result = RecursiveLiveOutputValidationResult {
            summary: RecursiveLiveOutputValidationSummary {
                validation_id: Some(RecursiveLiveOutputValidationId::new()),
                live_attempt_id: correlation.live_attempt_id,
                graph_id: correlation.graph_id,
                task_id: correlation.task_id,
                scheduler_run_id: correlation.scheduler_run_id,
                attempt_id: correlation.attempt_id,
                session_id: correlation.session_id,
                status: RecursiveLiveOutputValidationStatus::Repairable,
                output_kind: Some(RecursiveLiveOutputKind::Success),
                mapping_decision: Some(RecursiveLiveOutputMappingDecision::RepairSameLiveAttempt),
                retry_decision: Some(RecursiveLiveOutputRetryDecision {
                    decision: RecursiveLiveOutputRetryDecisionKind::RetrySameLiveAttempt,
                    reason: Some("repair budget remains".to_string()),
                    remaining_repair_attempts: Some(1),
                    remaining_task_retries: Some(2),
                    suggested_next_action: Some("ask model for corrected JSON".to_string()),
                }),
                raw_output_artifact_id: Some(11),
                normalized_output_artifact_id: None,
                validation_artifact_id: Some(12),
                normalized_digest: Some("sha256:normalized".to_string()),
                issue_count: 1,
                error_count: 1,
                warning_count: 0,
                info_count: 0,
                created_at: Some(Utc::now()),
            },
            artifact_links: RecursiveLiveOutputValidationArtifactLinks::default(),
            parser_source: Some(RecursiveLiveOutputParserSource::FinalJsonBlock {
                event_id: Some(20),
                block_index: Some(0),
            }),
            issues: vec![issue],
            normalized_output: None,
            validation_report: Some(serde_json::json!({"status": "repairable"})),
            metadata: serde_json::Value::Null,
        };

        let value = serde_json::to_value(&result)?;
        assert_eq!(value["summary"]["status"], serde_json::json!("repairable"));
        assert_eq!(
            value["issues"][0]["code"],
            serde_json::json!("missing_required_field")
        );
        let parsed: RecursiveLiveOutputValidationResult = serde_json::from_value(value)?;
        assert_eq!(parsed, result);
        Ok(())
    }

    #[test]
    fn recursive_live_output_optional_fields_default_for_backward_compatibility()
    -> Result<(), serde_json::Error> {
        let correlation = output_correlation();
        let value = serde_json::json!({
            "kind": "retryable_failure",
            "correlation": correlation,
            "summary": "minimal retryable failure",
            "reason": "temporary missing service",
            "retry_hint": "try after the service is back"
        });

        let parsed: RecursiveLiveOutputEnvelope = serde_json::from_value(value)?;

        assert_eq!(parsed.schema_version, RECURSIVE_LIVE_OUTPUT_SCHEMA_VERSION);
        assert!(parsed.artifacts.is_empty());
        assert!(parsed.tests.is_empty());
        assert!(parsed.diffs.is_empty());
        assert!(parsed.dependency_outputs.is_empty());
        assert!(parsed.notes.is_empty());
        assert_eq!(parsed.metadata, serde_json::Value::Null);
        assert_eq!(parsed.confidence, None);
        assert_eq!(parsed.kind(), RecursiveLiveOutputKind::RetryableFailure);

        let RecursiveLiveOutputOutcome::RetryableFailure(payload) = parsed.outcome else {
            panic!("expected retryable failure payload");
        };
        assert!(payload.evidence.is_empty());
        assert!(payload.partial_artifacts.is_empty());
        Ok(())
    }

    #[test]
    fn recursive_live_output_validation_result_minimal_json_defaults()
    -> Result<(), serde_json::Error> {
        let correlation = output_correlation();
        let value = serde_json::json!({
            "summary": {
                "live_attempt_id": correlation.live_attempt_id,
                "graph_id": correlation.graph_id,
                "task_id": correlation.task_id,
                "scheduler_run_id": correlation.scheduler_run_id,
                "attempt_id": correlation.attempt_id,
                "status": "valid"
            }
        });

        let parsed: RecursiveLiveOutputValidationResult = serde_json::from_value(value)?;

        assert!(parsed.summary.validation_id.is_none());
        assert!(parsed.summary.session_id.is_none());
        assert!(parsed.summary.output_kind.is_none());
        assert!(parsed.summary.mapping_decision.is_none());
        assert!(parsed.summary.retry_decision.is_none());
        assert_eq!(parsed.summary.issue_count, 0);
        assert_eq!(parsed.summary.error_count, 0);
        assert_eq!(parsed.summary.warning_count, 0);
        assert_eq!(parsed.summary.info_count, 0);
        assert!(parsed.summary.created_at.is_none());
        assert_eq!(
            parsed.artifact_links,
            RecursiveLiveOutputValidationArtifactLinks::default()
        );
        assert!(parsed.parser_source.is_none());
        assert!(parsed.issues.is_empty());
        assert!(parsed.normalized_output.is_none());
        assert!(parsed.validation_report.is_none());
        assert_eq!(parsed.metadata, serde_json::Value::Null);
        Ok(())
    }

    #[test]
    fn recursive_live_output_metadata_round_trips() -> Result<(), serde_json::Error> {
        let mut output = output_with(RecursiveLiveOutputOutcome::Success(
            RecursiveLiveSuccessPayload {
                result_summary: "metadata survives normalization boundaries".to_string(),
                acceptance: vec![RecursiveLiveAcceptanceCheck {
                    criterion: "metadata can be inspected".to_string(),
                    status: RecursiveLiveAcceptanceStatus::Met,
                    evidence: Vec::new(),
                    artifact_ids: Vec::new(),
                    notes: None,
                }],
            },
        ));
        output.metadata = serde_json::json!({
            "future": {
                "contract_metadata": true
            }
        });
        output.artifacts[0].metadata = serde_json::json!({
            "future": {
                "artifact_metadata": true
            }
        });
        output.confidence.as_mut().expect("confidence").metadata = serde_json::json!({
            "future": {
                "confidence_metadata": true
            }
        });

        let value = round_trip_output(&output)?;

        assert_eq!(value["metadata"]["future"]["contract_metadata"], true);
        assert_eq!(
            value["artifacts"][0]["metadata"]["future"]["artifact_metadata"],
            true
        );
        assert_eq!(
            value["confidence"]["metadata"]["future"]["confidence_metadata"],
            true
        );

        let issue = RecursiveLiveOutputValidationIssue {
            code: RecursiveLiveValidationIssueCode::Other,
            severity: RecursiveLiveValidationIssueSeverity::Info,
            class: RecursiveLiveValidationIssueClass::Other,
            location: None,
            message: "metadata survives validation readback".to_string(),
            evidence: Vec::new(),
            suggested_next_action: None,
            metadata: serde_json::json!({"future": {"issue_metadata": true}}),
        };
        let issue_value = serde_json::to_value(&issue)?;
        let parsed_issue: RecursiveLiveOutputValidationIssue =
            serde_json::from_value(issue_value.clone())?;
        assert_eq!(parsed_issue, issue);
        assert_eq!(issue_value["metadata"]["future"]["issue_metadata"], true);
        Ok(())
    }

    #[test]
    fn recursive_live_decomposition_child_and_dependency_refs_round_trip()
    -> Result<(), serde_json::Error> {
        let child_task_id = RecursiveTaskId::new();
        let dependency_task_id = RecursiveTaskId::new();
        let child_value = serde_json::json!({
            "local_id": "child-a",
            "task_id": child_task_id,
            "title": "Implement validator types",
            "objective": "Keep child shape serializable",
            "scope": "rsi-common only",
            "acceptance_criteria": ["round trips"],
            "scope_units": 1,
            "max_retries": 2,
            "dependencies": [
                {
                    "kind": "child_local",
                    "local_id": "child-b"
                },
                {
                    "kind": "existing_task",
                    "task_id": dependency_task_id
                }
            ],
            "metadata": {
                "future": "child"
            }
        });

        let child: RecursiveLiveChildTaskSpec = serde_json::from_value(child_value.clone())?;
        assert_eq!(child.task_id, Some(child_task_id));
        assert_eq!(child.dependencies.len(), 2);
        assert_eq!(child.metadata["future"], "child");
        assert_eq!(serde_json::to_value(&child)?, child_value);

        let edge_value = serde_json::json!({
            "from": {
                "kind": "child_local",
                "local_id": "child-a"
            },
            "to": {
                "kind": "existing_task",
                "task_id": dependency_task_id
            },
            "rationale": "child-a consumes the existing task output"
        });
        let edge: RecursiveLiveDependencyEdgeSpec = serde_json::from_value(edge_value)?;
        assert!(matches!(
            edge.from,
            RecursiveLiveTaskDependencyRef::ChildLocal { .. }
        ));
        assert!(matches!(
            edge.to,
            RecursiveLiveTaskDependencyRef::ExistingTask { task_id }
                if task_id == dependency_task_id
        ));
        assert_eq!(edge.metadata, serde_json::Value::Null);
        Ok(())
    }

    #[test]
    fn recursive_live_validation_issue_location_variants_round_trip()
    -> Result<(), serde_json::Error> {
        let task_id = RecursiveTaskId::new();
        let attempt_id = RecursiveAttemptId::new();
        let dependency_id = RecursiveTaskId::new();
        let locations = [
            (
                RecursiveLiveValidationIssueLocation::OutputPath {
                    path: "/summary".to_string(),
                },
                "output_path",
            ),
            (
                RecursiveLiveValidationIssueLocation::Artifact {
                    artifact_id: 7,
                    path: Some("/artifacts/0".to_string()),
                },
                "artifact",
            ),
            (
                RecursiveLiveValidationIssueLocation::ConversationEvent {
                    event_id: 8,
                    sequence: Some(2),
                    path: Some("/content".to_string()),
                },
                "conversation_event",
            ),
            (
                RecursiveLiveValidationIssueLocation::Task { task_id },
                "task",
            ),
            (
                RecursiveLiveValidationIssueLocation::Attempt { attempt_id },
                "attempt",
            ),
            (
                RecursiveLiveValidationIssueLocation::ChildLocal {
                    local_id: "child-a".to_string(),
                },
                "child_local",
            ),
            (
                RecursiveLiveValidationIssueLocation::Dependency {
                    reference: RecursiveLiveTaskDependencyRef::ExistingTask {
                        task_id: dependency_id,
                    },
                },
                "dependency",
            ),
            (
                RecursiveLiveValidationIssueLocation::DiffPath {
                    path: PathBuf::from("crates/rsi-common/src/recursive_dag.rs"),
                },
                "diff_path",
            ),
            (
                RecursiveLiveValidationIssueLocation::Other {
                    description: "operator-visible fallback".to_string(),
                },
                "other",
            ),
        ];

        for (location, expected_kind) in locations {
            let value = serde_json::to_value(&location)?;
            assert_eq!(value["kind"], serde_json::json!(expected_kind));
            let parsed: RecursiveLiveValidationIssueLocation = serde_json::from_value(value)?;
            assert_eq!(parsed, location);
        }

        Ok(())
    }

    #[test]
    fn recursive_live_validation_issue_minimal_json_defaults() -> Result<(), serde_json::Error> {
        let value = serde_json::json!({
            "code": "other",
            "severity": "warning",
            "message": "minimal issue"
        });

        let parsed: RecursiveLiveOutputValidationIssue = serde_json::from_value(value)?;

        assert_eq!(parsed.code, RecursiveLiveValidationIssueCode::Other);
        assert_eq!(
            parsed.severity,
            RecursiveLiveValidationIssueSeverity::Warning
        );
        assert_eq!(parsed.class, RecursiveLiveValidationIssueClass::Other);
        assert!(parsed.location.is_none());
        assert!(parsed.evidence.is_empty());
        assert!(parsed.suggested_next_action.is_none());
        assert_eq!(parsed.metadata, serde_json::Value::Null);
        Ok(())
    }

    #[test]
    fn recursive_live_failure_outcomes_do_not_duplicate_classification()
    -> Result<(), serde_json::Error> {
        let retryable = output_with(RecursiveLiveOutputOutcome::RetryableFailure(
            RecursiveLiveRetryableFailurePayload {
                reason: "transient failure".to_string(),
                retry_hint: "retry after dependency is healthy".to_string(),
                operator_message: None,
                evidence: Vec::new(),
                partial_artifacts: Vec::new(),
                suggested_next_action: None,
            },
        ));
        let permanent = output_with(RecursiveLiveOutputOutcome::PermanentFailure(
            RecursiveLivePermanentFailurePayload {
                reason: "irrecoverable input is missing".to_string(),
                failure_class: RecursiveLivePermanentFailureClass::MissingIrreplaceableInput,
                operator_message: None,
                evidence: Vec::new(),
                suggested_next_action: None,
            },
        ));

        let retryable_value = serde_json::to_value(&retryable)?;
        let permanent_value = serde_json::to_value(&permanent)?;

        assert_eq!(retryable_value["kind"], "retryable_failure");
        assert_eq!(permanent_value["kind"], "permanent_failure");
        assert!(
            !retryable_value
                .as_object()
                .expect("retryable object")
                .contains_key("classification")
        );
        assert!(
            !permanent_value
                .as_object()
                .expect("permanent object")
                .contains_key("classification")
        );
        Ok(())
    }

    #[test]
    fn recursive_topology_create_request_defaults_round_trip() -> Result<(), serde_json::Error> {
        let topology_id = Uuid::new_v4();
        let value = serde_json::json!({
            "topology_id": topology_id,
            "node_id": "verify"
        });

        let request: RecursiveTopologyGraphCreateRequest = serde_json::from_value(value)?;
        assert_eq!(request.topology_id, topology_id);
        assert_eq!(request.node_id, "verify");
        assert_eq!(request.topology_iteration, 0);
        assert!(request.include_prerequisite_closure);
        assert!(request.idempotency_key.is_none());

        let encoded = serde_json::to_value(&request)?;
        assert_eq!(encoded["include_prerequisite_closure"], true);
        Ok(())
    }

    #[test]
    fn recursive_topology_link_models_round_trip() -> Result<(), serde_json::Error> {
        let graph_id = RecursiveTaskGraphId::new();
        let root_task_id = RecursiveTaskId::new();
        let task_id = RecursiveTaskId::new();
        let topology_id = Uuid::new_v4();
        let workflow_id = Uuid::new_v4();
        let workflow_execution_id = Uuid::new_v4();
        let now = Utc::now();

        let graph = RecursiveTaskGraphSummary {
            id: graph_id,
            root_task_id,
            title: "Topology demo".to_string(),
            objective: "Create graph only".to_string(),
            status: RecursiveGraphStatus::Active,
            project_id: None,
            workflow_id: Some(workflow_id),
            topology_id: Some(topology_id),
            parent_session_id: None,
            source_execution_id: Some(workflow_execution_id.to_string()),
            source_eval_id: None,
            execution_mode: RecursiveExecutionMode::Fake,
            max_depth: 1,
            max_fanout: 3,
            max_descendants: 3,
            step_limit: 100,
            last_stop_reason: None,
            malformed_reason: None,
            created_at: now,
            updated_at: now,
            recovered_at: None,
            quarantined_at: None,
            quarantine_reason: None,
            recovery_checked_at: None,
        };
        let graph_link = RecursiveTopologyGraphLink {
            graph_id,
            execution_owner: RECURSIVE_TOPOLOGY_EXECUTION_OWNER_FAKE.to_string(),
            owner_key: "v1|mode=topology_node_prereq_closure".to_string(),
            idempotency_key: Some("retry-key".to_string()),
            request_fingerprint: "sha256:abc".to_string(),
            topology_id,
            workflow_id: Some(workflow_id),
            workflow_execution_id: Some(workflow_execution_id),
            source_topology_node_id: "verify".to_string(),
            source_topology_iteration: 0,
            parent_session_id: None,
            project_id: None,
            creation_mode: RECURSIVE_TOPOLOGY_CREATION_MODE_NODE_PREREQ_CLOSURE.to_string(),
            include_prerequisite_closure: true,
            topology_name_snapshot: "demo".to_string(),
            topology_updated_at_snapshot: now,
            topology_snapshot: serde_json::json!({"nodes": []}),
            selected_slice: serde_json::json!({"node_ids": ["verify"]}),
            policy_snapshot: serde_json::json!({"schema_version": 1}),
            workflow_execution_linkage: serde_json::json!({"source": "explicit"}),
            created_at: now,
            updated_at: now,
        };
        let task_link = RecursiveTopologyTaskLink {
            graph_id,
            task_id,
            topology_id,
            topology_node_id: "verify".to_string(),
            topology_iteration: 0,
            topology_node_kind: SessionKind::Task,
            source_params: serde_json::json!({"model": "gpt-5"}),
            topology_node_snapshot: serde_json::json!({"id": "verify"}),
            policy_snapshot: serde_json::json!({"max_retries": 0}),
            created_at: now,
        };
        let response = RecursiveTopologyGraphCreateResponse {
            graph,
            root_task_id,
            graph_link,
            task_links: vec![task_link],
            reused_existing: false,
        };

        let value = serde_json::to_value(&response)?;
        assert_eq!(
            value["graph_link"]["execution_owner"],
            RECURSIVE_TOPOLOGY_EXECUTION_OWNER_FAKE
        );
        assert_eq!(value["task_links"][0]["topology_node_kind"], "Task");
        let decoded: RecursiveTopologyGraphCreateResponse = serde_json::from_value(value)?;
        assert_eq!(decoded.task_links.len(), 1);
        assert_eq!(
            decoded.graph_link.workflow_execution_id,
            Some(workflow_execution_id)
        );

        let run_id = RecursiveSchedulerRunId::new();
        let run_response = RunRecursiveTopologyNodeFakeSchedulerResponse {
            scheduler_run: RecursiveSchedulerRunSummary {
                id: run_id,
                graph_id,
                status: RecursiveSchedulerRunStatus::Completed,
                source: RecursiveSchedulerRunSource::ManualRpc,
                operator: Some("operator".to_string()),
                started_at: now,
                completed_at: Some(now),
                stop_reason: Some(RecursiveSchedulerStopReason::IdleNoRunnable),
                step_count: 0,
                max_steps: 5,
                executor_mode: RecursiveExecutionMode::Fake,
                failure_reason: None,
                cancellation_request_id: None,
                cancellation_reason: None,
                lease_owner: None,
                lease_token: None,
                lease_heartbeat_at: None,
                lease_expires_at: None,
                report_artifact_id: None,
            },
            report_artifact: None,
            graph_link: response.graph_link.clone(),
            task_links: response.task_links.clone(),
            status: TopologyRecursiveStatus {
                topology_id: Some(topology_id),
                project_id: None,
                workflow_id: Some(workflow_id),
                workflow_execution_id: Some(workflow_execution_id),
                execution_owner: Some(RECURSIVE_TOPOLOGY_EXECUTION_OWNER_FAKE.to_string()),
                graphs: Vec::new(),
                nodes: Vec::new(),
                latest_recovery: None,
                open_cancellations: Vec::new(),
                live_enabled: false,
                background_enabled: false,
            },
        };
        let value = serde_json::to_value(&run_response)?;
        assert_eq!(value["scheduler_run"]["id"], run_id.to_string());
        assert_eq!(
            value["graph_link"]["execution_owner"],
            RECURSIVE_TOPOLOGY_EXECUTION_OWNER_FAKE
        );
        let decoded: RunRecursiveTopologyNodeFakeSchedulerResponse = serde_json::from_value(value)?;
        assert_eq!(decoded.scheduler_run.id, run_id);
        assert_eq!(decoded.graph_link.graph_id, graph_id);
        Ok(())
    }

    #[test]
    fn recursive_live_scheduler_response_wire_shape_round_trip() -> Result<(), serde_json::Error> {
        let graph_id = RecursiveTaskGraphId::new();
        let task_id = RecursiveTaskId::new();
        let run_id = RecursiveSchedulerRunId::new();
        let now = Utc::now();
        let response = RunRecursiveLiveSchedulerResponse {
            scheduler_run: RecursiveSchedulerRunSummary {
                id: run_id,
                graph_id,
                status: RecursiveSchedulerRunStatus::Completed,
                source: RecursiveSchedulerRunSource::ManualRpc,
                operator: Some("operator".to_string()),
                started_at: now,
                completed_at: Some(now),
                stop_reason: Some(RecursiveSchedulerStopReason::IdleNoRunnable),
                step_count: 1,
                max_steps: 5,
                executor_mode: RecursiveExecutionMode::LiveSession,
                failure_reason: None,
                cancellation_request_id: None,
                cancellation_reason: None,
                lease_owner: None,
                lease_token: None,
                lease_heartbeat_at: None,
                lease_expires_at: None,
                report_artifact_id: None,
            },
            stop_reason: RecursiveSchedulerStopReason::IdleNoRunnable,
            step_count: 1,
            selected_task_order: vec![task_id],
            live_attempts: Vec::new(),
            validation_summaries: Vec::new(),
            report_artifact: None,
            cancellation_request_id: None,
            warnings: vec![RecursiveReadbackWarning {
                code: "slice_1_non_reachable".to_string(),
                message: "shared response type only".to_string(),
                resource_type: Some("recursive_live_scheduler".to_string()),
                resource_id: Some(run_id.to_string()),
            }],
        };

        let value = serde_json::to_value(&response)?;
        assert_eq!(value["scheduler_run"]["id"], run_id.to_string());
        assert_eq!(value["scheduler_run"]["executor_mode"], "live_session");
        assert_eq!(value["stop_reason"], "idle_no_runnable");
        assert_eq!(value["selected_task_order"][0], task_id.to_string());
        assert_eq!(value["live_attempts"], serde_json::json!([]));
        assert_eq!(value["validation_summaries"], serde_json::json!([]));
        assert_eq!(value["warnings"][0]["code"], "slice_1_non_reachable");

        let decoded: RunRecursiveLiveSchedulerResponse = serde_json::from_value(value)?;
        assert_eq!(decoded.scheduler_run.id, run_id);
        assert_eq!(
            decoded.stop_reason,
            RecursiveSchedulerStopReason::IdleNoRunnable
        );
        assert_eq!(decoded.selected_task_order, vec![task_id]);
        Ok(())
    }

    #[test]
    fn get_recursive_graph_as_workflow_params_response_round_trip() -> Result<(), serde_json::Error>
    {
        let graph_id = Uuid::new_v4();
        let params = GetRecursiveGraphAsWorkflowParams { graph_id };
        let params_value = serde_json::to_value(&params)?;
        assert_eq!(params_value["graph_id"], graph_id.to_string());
        let decoded_params: GetRecursiveGraphAsWorkflowParams =
            serde_json::from_value(params_value)?;
        assert_eq!(decoded_params, params);

        let response = GetRecursiveGraphAsWorkflowResponse {
            definition: serde_json::json!({
                "nodes": [{ "id": graph_id.to_string(), "kind": "action" }],
                "edges": [],
                "metadata": { "source_recursive_graph_id": graph_id.to_string() },
            }),
        };
        let response_value = serde_json::to_value(&response)?;
        assert_eq!(
            response_value["definition"]["nodes"][0]["id"],
            graph_id.to_string()
        );
        let decoded_response: GetRecursiveGraphAsWorkflowResponse =
            serde_json::from_value(response_value)?;
        // PartialEq (not Eq) — the definition carries a serde_json::Value.
        assert_eq!(decoded_response, response);
        Ok(())
    }

    #[test]
    fn edit_recursive_node_params_round_trip() -> Result<(), serde_json::Error> {
        let graph_id = Uuid::new_v4();
        let task_id = Uuid::new_v4();

        // Instructions params — all fields are `Eq`, so the type derives `Eq`.
        let instr = EditRecursiveNodeInstructionsParams {
            graph_id,
            task_id,
            instructions: "do the thing".to_string(),
        };
        let instr_value = serde_json::to_value(&instr)?;
        assert_eq!(instr_value["graph_id"], graph_id.to_string());
        assert_eq!(instr_value["task_id"], task_id.to_string());
        assert_eq!(instr_value["instructions"], "do the thing");
        let decoded_instr: EditRecursiveNodeInstructionsParams =
            serde_json::from_value(instr_value)?;
        assert_eq!(decoded_instr, instr);
        // `Eq` is derivable (compile-time proof the bound is satisfied).
        fn assert_eq_bound<T: Eq>(_: &T) {}
        assert_eq_bound(&decoded_instr);

        // Settings params — both strategies present.
        let settings = EditRecursiveNodeSettingsParams {
            graph_id,
            task_id,
            integration_strategy: Some("rebase".to_string()),
            verification_strategy: Some("cargo test".to_string()),
        };
        let settings_value = serde_json::to_value(&settings)?;
        assert_eq!(settings_value["integration_strategy"], "rebase");
        assert_eq!(settings_value["verification_strategy"], "cargo test");
        let decoded_settings: EditRecursiveNodeSettingsParams =
            serde_json::from_value(settings_value)?;
        assert_eq!(decoded_settings, settings);
        assert_eq_bound(&decoded_settings);

        // Settings params — both strategies cleared (None survives the round-trip).
        let cleared = EditRecursiveNodeSettingsParams {
            graph_id,
            task_id,
            integration_strategy: None,
            verification_strategy: None,
        };
        let cleared_value = serde_json::to_value(&cleared)?;
        let decoded_cleared: EditRecursiveNodeSettingsParams =
            serde_json::from_value(cleared_value)?;
        assert_eq!(decoded_cleared, cleared);
        assert_eq!(decoded_cleared.integration_strategy, None);
        assert_eq!(decoded_cleared.verification_strategy, None);

        Ok(())
    }

    #[test]
    fn recursive_topology_status_models_round_trip() -> Result<(), serde_json::Error> {
        let graph_id = RecursiveTaskGraphId::new();
        let task_id = RecursiveTaskId::new();
        let run_id = RecursiveSchedulerRunId::new();
        let cancellation_id = RecursiveCancellationRequestId::new();
        let topology_id = Uuid::new_v4();
        let workflow_id = Uuid::new_v4();
        let workflow_execution_id = Uuid::new_v4();
        let now = Utc::now();

        let graph_status = TopologyRecursiveGraphStatus {
            graph_id,
            root_task_id: task_id,
            owner_key: "v1|topology".to_string(),
            idempotency_key: Some("idem".to_string()),
            topology_id,
            project_id: None,
            workflow_id: Some(workflow_id),
            workflow_execution_id: Some(workflow_execution_id),
            parent_session_id: None,
            source_topology_node_id: "verify".to_string(),
            source_topology_iteration: 1,
            execution_owner: RECURSIVE_TOPOLOGY_EXECUTION_OWNER_FAKE.to_string(),
            graph_status: RecursiveGraphStatus::Active,
            topology_visible_status: TopologyRecursiveVisibleStatus::RunCancelling,
            active_run_id: Some(run_id),
            latest_run_id: Some(run_id),
            open_cancellation_request_ids: vec![cancellation_id],
            recovery_state: Some(RecursiveGraphRecoveryState::PendingRecovery),
            recovery: None,
            quarantined_at: None,
            quarantine_reason: None,
            malformed_reason: None,
            topology_updated_at_snapshot: now,
            created_at: now,
            updated_at: now,
        };
        let node_status = TopologyRecursiveNodeStatus {
            topology_id,
            topology_node_id: "verify".to_string(),
            topology_iteration: 1,
            graph_id,
            task_id,
            recursive_status: Some(RecursiveTaskLifecycleState::Running),
            topology_visible_status: TopologyRecursiveVisibleStatus::Running,
            active_run_id: Some(run_id),
            latest_run_id: Some(run_id),
            active_live_attempt_id: None,
            session_id: None,
            child_count: Some(2),
            artifact_count: 3,
            cancellation_request_id: Some(cancellation_id),
            recovery_state: Some(RecursiveGraphRecoveryState::PendingRecovery),
            error: None,
        };
        let recovery = RecursiveGraphRecoveryStatus {
            graph_id: Some(graph_id),
            raw_graph_id: graph_id.to_string(),
            state: RecursiveGraphRecoveryState::PendingRecovery,
            pass_id: None,
            last_attempted_at: None,
            completed_at: None,
            deferred_at: None,
            reason: None,
            last_error: None,
            updated_at: now,
        };
        let cancellation = RecursiveCancellationRequestSummary {
            id: cancellation_id,
            graph_id,
            run_id: Some(run_id),
            task_id: None,
            scope: RecursiveCancellationScope::Run,
            status: RecursiveCancellationRequestStatus::Requested,
            source: RecursiveCancellationRequestSource::ManualRpc,
            reason: "stop".to_string(),
            requested_by: Some("operator".to_string()),
            requested_at: now,
            observed_at: None,
            applied_at: None,
            rejection_reason: None,
            idempotency_key: None,
            request_fingerprint: None,
            source_context: None,
        };
        let status = TopologyRecursiveStatus {
            topology_id: Some(topology_id),
            project_id: None,
            workflow_id: Some(workflow_id),
            workflow_execution_id: Some(workflow_execution_id),
            execution_owner: Some(RECURSIVE_TOPOLOGY_EXECUTION_OWNER_FAKE.to_string()),
            graphs: vec![graph_status],
            nodes: vec![node_status],
            latest_recovery: Some(recovery),
            open_cancellations: vec![cancellation],
            live_enabled: false,
            background_enabled: false,
        };

        let value = serde_json::to_value(&status)?;
        assert_eq!(
            value["graphs"][0]["topology_visible_status"],
            "run_cancelling"
        );
        assert!(value["nodes"][0]["active_live_attempt_id"].is_null());
        assert_eq!(value["live_enabled"], false);
        assert_eq!(value["background_enabled"], false);
        let decoded: TopologyRecursiveStatus = serde_json::from_value(value)?;
        assert_eq!(decoded.graphs.len(), 1);
        assert_eq!(
            decoded.nodes[0].topology_visible_status,
            TopologyRecursiveVisibleStatus::Running
        );
        Ok(())
    }

    #[test]
    fn recursive_artifact_inspector_readbacks_use_snake_case_wire_names()
    -> Result<(), serde_json::Error> {
        let graph_id = RecursiveTaskGraphId::new();
        let task_id = RecursiveTaskId::new();
        let attempt_id = RecursiveAttemptId::new();
        let live_attempt_id = RecursiveLiveAttemptId::new();
        let scheduler_run_id = RecursiveSchedulerRunId::new();
        let validation_id = RecursiveLiveOutputValidationId::new();
        let now = Utc::now();
        let summary = RecursiveExecutionArtifactSummary {
            artifact_id: 7,
            graph_id,
            task_id,
            attempt_id: Some(attempt_id),
            live_attempt_id: Some(live_attempt_id),
            scheduler_run_id: Some(scheduler_run_id),
            validation_id: Some(validation_id),
            role: Some(RecursiveArtifactRole::DiffSummary),
            kind: RecursiveExecutionArtifactKind::Inline,
            label: "diff".to_string(),
            uri_display: None,
            content_presence: RecursiveArtifactContentPresence::Inline,
            content_type: None,
            size_bytes: Some(12),
            digest: None,
            preview_state: RecursiveArtifactPreviewState::Available,
            metadata_state: RecursiveArtifactMetadataState::Valid,
            created_at: now,
        };
        let page = RecursiveReadPage {
            items: vec![summary],
            limit: 1,
            next_cursor: Some("opaque".to_string()),
            has_more: true,
            total_count: Some(2),
            warnings: vec![RecursiveReadbackWarning {
                code: "RECURSIVE_METADATA_MALFORMED".to_string(),
                message: "legacy metadata could not be parsed".to_string(),
                resource_type: Some("recursive_execution_artifact".to_string()),
                resource_id: Some("7".to_string()),
            }],
        };
        let value = serde_json::to_value(&page)?;
        assert_eq!(value["items"][0]["role"], "diff_summary");
        assert_eq!(value["items"][0]["content_presence"], "inline");
        assert_eq!(value["items"][0]["preview_state"], "available");
        assert_eq!(value["items"][0]["metadata_state"], "valid");
        assert_eq!(value["has_more"], true);
        assert_eq!(value["next_cursor"], "opaque");

        let decoded: RecursiveReadPage<RecursiveExecutionArtifactSummary> =
            serde_json::from_value(value)?;
        assert!(decoded.has_more);
        assert_eq!(
            decoded.items[0].role,
            Some(RecursiveArtifactRole::DiffSummary)
        );

        let preview = RecursiveExecutionArtifactPreview {
            artifact_id: 7,
            graph_id,
            kind: RecursiveExecutionArtifactKind::Inline,
            label: "diff".to_string(),
            content_state: RecursiveArtifactPreviewState::Truncated,
            content_type: Some("text/plain".to_string()),
            charset: Some("utf-8".to_string()),
            digest: Some("sha256:abc".to_string()),
            digest_algorithm: Some("sha256".to_string()),
            total_bytes: Some(12),
            total_lines: Some(3),
            byte_range: Some(RecursiveByteRange { start: 0, end: 5 }),
            line_range: Some(RecursiveLineRange { start: 0, end: 2 }),
            shown_bytes: 5,
            shown_lines: 2,
            text: Some("one\nt".to_string()),
            binary_unavailable_reason: None,
            truncated: true,
            truncated_by_bytes: true,
            truncated_by_lines: false,
            omitted_bytes: Some(7),
            omitted_lines: Some(1),
            applied_max_bytes: 5,
            applied_max_lines: 2,
            warnings: Vec::new(),
        };
        let value = serde_json::to_value(&preview)?;
        assert_eq!(value["content_state"], "truncated");
        assert_eq!(value["byte_range"]["start"], 0);
        assert_eq!(value["line_range"]["end"], 2);
        assert_eq!(value["charset"], "utf-8");

        let decoded: RecursiveExecutionArtifactPreview = serde_json::from_value(value)?;
        assert_eq!(
            decoded.content_state,
            RecursiveArtifactPreviewState::Truncated
        );
        assert!(decoded.truncated_by_bytes);
        Ok(())
    }

    #[test]
    fn recursive_typed_test_readback_contract_round_trips() -> Result<(), serde_json::Error> {
        let graph_id = RecursiveTaskGraphId::new();
        let task_id = RecursiveTaskId::new();
        let test_id = RecursiveTestResultId::new();
        let validation_id = RecursiveLiveOutputValidationId::new();
        let now = Utc::now();
        let summary = RecursiveTypedTestSummary {
            test_id,
            graph_id,
            task_id: Some(task_id),
            attempt_id: Some(RecursiveAttemptId::new()),
            live_attempt_id: Some(RecursiveLiveAttemptId::new()),
            scheduler_run_id: Some(RecursiveSchedulerRunId::new()),
            validation_id: Some(validation_id),
            artifact_id: Some(100),
            source: RecursiveTypedSource::LiveValidation,
            trust_level: RecursiveTrustLevel::Normalized,
            status: RecursiveTypedTestStatus::Failed,
            required: true,
            name: None,
            command: Some("cargo test -p rsi-common".to_string()),
            display_label: "cargo test -p rsi-common".to_string(),
            duration_ms: Some(3210),
            exit_code: Some(101),
            failure_summary: Some("one test failed".to_string()),
            metadata_state: RecursiveArtifactMetadataState::Valid,
            created_at: now,
        };
        let detail = RecursiveTypedTestDetail {
            summary: summary.clone(),
            failure_text: Some(RecursiveTextExcerpt {
                text: "assertion failed".to_string(),
                shown_bytes: 16,
                truncated: false,
                omitted_bytes: None,
            }),
            stdout: None,
            stderr: None,
            log: None,
            artifact_links: RecursiveTypedTestArtifactLinks {
                stdout_artifact_id: Some(101),
                stderr_artifact_id: Some(102),
                log_artifact_id: None,
                output_artifact_ids: vec![103],
            },
            related_validation_issue_ids: vec!["issue-1".to_string()],
            retry_decision: None,
            suggested_next_action: Some("fix failing assertion".to_string()),
            warnings: vec![RecursiveReadbackWarning {
                code: "RECURSIVE_TEST_OUTPUT_TRUNCATED".to_string(),
                message: "failure output was bounded".to_string(),
                resource_type: Some("recursive_test_result".to_string()),
                resource_id: Some(test_id.to_string()),
            }],
        };

        let value = serde_json::to_value(&detail)?;
        assert_eq!(value["summary"]["source"], "live_validation");
        assert_eq!(value["summary"]["trust_level"], "normalized");
        assert_eq!(value["summary"]["status"], "failed");
        assert_eq!(value["summary"]["metadata_state"], "valid");
        assert_eq!(value["artifact_links"]["stdout_artifact_id"], 101);

        let decoded: RecursiveTypedTestDetail = serde_json::from_value(value)?;
        assert_eq!(decoded, detail);

        let minimal: RecursiveTypedTestDetail = serde_json::from_value(serde_json::json!({
            "summary": summary
        }))?;
        assert!(minimal.failure_text.is_none());
        assert!(minimal.artifact_links.output_artifact_ids.is_empty());
        assert!(minimal.related_validation_issue_ids.is_empty());
        assert!(minimal.warnings.is_empty());
        Ok(())
    }

    #[test]
    fn recursive_typed_diff_readback_contract_round_trips_without_hunk_text()
    -> Result<(), serde_json::Error> {
        let graph_id = RecursiveTaskGraphId::new();
        let diff_id = RecursiveDiffId::new();
        let file_id = RecursiveDiffFileId::new();
        let now = Utc::now();
        let summary = RecursiveTypedDiffSummary {
            diff_id,
            graph_id,
            task_id: Some(RecursiveTaskId::new()),
            attempt_id: Some(RecursiveAttemptId::new()),
            live_attempt_id: Some(RecursiveLiveAttemptId::new()),
            scheduler_run_id: Some(RecursiveSchedulerRunId::new()),
            validation_id: Some(RecursiveLiveOutputValidationId::new()),
            artifact_id: Some(201),
            source: RecursiveTypedSource::DaemonCollected,
            trust_level: RecursiveTrustLevel::Verified,
            file_count: 1,
            binary_file_count: 0,
            truncated_file_count: 0,
            additions: Some(4),
            deletions: Some(1),
            hunk_count: None,
            metadata_state: RecursiveArtifactMetadataState::Valid,
            created_at: now,
        };
        let file = RecursiveTypedDiffFileSummary {
            file_id,
            diff_id,
            graph_id,
            file_index: 0,
            display_path: PathBuf::from("crates/rsi-common/src/recursive_dag.rs"),
            previous_display_path: None,
            status: RecursiveTypedDiffFileStatus::Modified,
            additions: Some(4),
            deletions: Some(1),
            hunk_count: None,
            binary: false,
            inside_allowed_root: Some(true),
            validation_issue_count: 0,
            metadata_state: RecursiveArtifactMetadataState::Valid,
            created_at: now,
        };
        let detail = RecursiveTypedDiffDetail {
            summary: summary.clone(),
            files: RecursiveReadPage {
                items: vec![file.clone()],
                limit: 100,
                next_cursor: None,
                has_more: false,
                total_count: Some(1),
                warnings: Vec::new(),
            },
            warnings: Vec::new(),
        };
        let value = serde_json::to_value(&detail)?;
        assert_eq!(value["summary"]["source"], "daemon_collected");
        assert_eq!(value["files"]["items"][0]["status"], "modified");
        assert_eq!(
            value["files"]["items"][0]["binary"],
            serde_json::json!(false)
        );
        let decoded: RecursiveTypedDiffDetail = serde_json::from_value(value)?;
        assert_eq!(decoded, detail);

        let hunk = RecursiveTypedDiffHunkReadback {
            graph_id,
            diff_id,
            file_id,
            state: RecursiveTypedDiffHunkState::Deferred,
            reason: Some("trusted hunk materialization is not available".to_string()),
            limit: 50,
            next_cursor: None,
            has_more: false,
            warnings: Vec::new(),
        };
        let value = serde_json::to_value(&hunk)?;
        assert_eq!(value["state"], "deferred");
        assert!(value.get("text").is_none());
        assert!(value.get("lines").is_none());
        assert!(value.get("hunks").is_none());
        let decoded: RecursiveTypedDiffHunkReadback = serde_json::from_value(value)?;
        assert_eq!(decoded, hunk);

        let minimal: RecursiveTypedDiffDetail = serde_json::from_value(serde_json::json!({
            "summary": summary,
            "files": {
                "items": [file],
                "limit": 100,
                "has_more": false
            }
        }))?;
        assert!(minimal.files.warnings.is_empty());
        assert!(minimal.warnings.is_empty());
        Ok(())
    }

    #[test]
    fn recursive_scheduler_report_readback_contract_round_trips() -> Result<(), serde_json::Error> {
        let graph_id = RecursiveTaskGraphId::new();
        let run_id = RecursiveSchedulerRunId::new();
        let report_id = RecursiveSchedulerReportId::new();
        let task_id = RecursiveTaskId::new();
        let attempt_id = RecursiveAttemptId::new();
        let now = Utc::now();
        let summary = RecursiveSchedulerReportSummary {
            report_id,
            run_id,
            graph_id,
            source: RecursiveTypedSource::SchedulerReport,
            trust_level: RecursiveTrustLevel::SchedulerOwned,
            scheduler_source: RecursiveSchedulerRunSource::ManualRpc,
            operator: Some("operator".to_string()),
            execution_mode: RecursiveExecutionMode::Fake,
            run_status: RecursiveSchedulerRunStatus::Completed,
            started_at: now,
            completed_at: Some(now),
            stop_reason: Some(RecursiveSchedulerStopReason::GraphTerminal),
            failure_reason: None,
            step_count: 1,
            max_steps: 10,
            selected_task_count: 1,
            live_attempt_count: 0,
            validation_count: 0,
            emitted_artifact_count: 2,
            cancellation_observed: false,
            recovery_observed: false,
            report_artifact_id: Some(301),
            metadata_state: RecursiveArtifactMetadataState::Valid,
            warnings: Vec::new(),
        };
        let step = RecursiveSchedulerReportStep {
            report_id,
            run_id,
            graph_id,
            step_index: 0,
            task_id,
            phase: RecursiveAttemptPhase::Execute,
            attempt_id,
            live_attempt_id: None,
            validation_id: None,
            outcome: RecursiveSchedulerReportStepOutcomeKind::Succeeded,
            message: Some("task succeeded".to_string()),
            final_task_status: RecursiveTaskLifecycleState::Succeeded,
            emitted_artifact_ids: vec![301, 302],
            created_at: Some(now),
            metadata_state: RecursiveArtifactMetadataState::Valid,
        };
        let detail = RecursiveSchedulerReportDetail {
            summary: summary.clone(),
            steps: RecursiveReadPage {
                items: vec![step.clone()],
                limit: 100,
                next_cursor: None,
                has_more: false,
                total_count: None,
                warnings: Vec::new(),
            },
            events: None,
            warnings: Vec::new(),
        };
        let value = serde_json::to_value(&detail)?;
        assert_eq!(value["summary"]["source"], "scheduler_report");
        assert_eq!(value["summary"]["trust_level"], "scheduler_owned");
        assert_eq!(value["summary"]["scheduler_source"], "manual_rpc");
        assert_eq!(value["steps"]["items"][0]["outcome"], "succeeded");
        let decoded: RecursiveSchedulerReportDetail = serde_json::from_value(value)?;
        assert_eq!(decoded, detail);

        let minimal: RecursiveSchedulerReportDetail = serde_json::from_value(serde_json::json!({
            "summary": summary,
            "steps": {
                "items": [step],
                "limit": 100,
                "has_more": false
            }
        }))?;
        assert!(minimal.events.is_none());
        assert!(minimal.warnings.is_empty());
        Ok(())
    }
}
