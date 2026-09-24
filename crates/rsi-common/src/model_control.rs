use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModelInvocationPurpose {
    #[serde(rename = "session.launch.fresh")]
    SessionLaunchFresh,
    #[serde(rename = "session.continue.resume")]
    SessionContinueResume,
    #[serde(rename = "session.retry.auto")]
    SessionRetryAuto,
    #[serde(rename = "session.rotate.child")]
    SessionRotateChild,
    #[serde(rename = "session.harness.turn")]
    SessionHarnessTurn,
    #[serde(rename = "session.harness.compaction")]
    SessionHarnessCompaction,
    #[serde(rename = "session.codex_app_server.turn")]
    SessionCodexAppServerTurn,
    #[serde(rename = "session.openai_compatible.turn")]
    SessionOpenAiCompatibleTurn,
    #[serde(rename = "agent.spawn_child")]
    AgentSpawnChild,
    #[serde(rename = "agent.reserve_successor")]
    AgentReserveSuccessor,
    #[serde(rename = "workflow.graph.node")]
    WorkflowGraphNode,
    #[serde(rename = "workflow.chain.iteration")]
    WorkflowChainIteration,
    #[serde(rename = "recursive.live.task")]
    RecursiveLiveTask,
    #[serde(rename = "scheduled.fresh")]
    ScheduledFresh,
    #[serde(rename = "agent.schedule_wake.fresh")]
    AgentScheduleWakeFresh,
    #[serde(rename = "scheduled.resume.watch")]
    ScheduledResumeWatch,
    #[serde(rename = "issue_tracker.dispatch")]
    IssueTrackerDispatch,
    #[serde(rename = "prompt.compile")]
    PromptCompile,
    #[serde(rename = "text.generate.rpc")]
    TextGenerateRpc,
    #[serde(rename = "session.title")]
    SessionTitle,
    #[serde(rename = "session.summary")]
    SessionSummary,
    #[serde(rename = "memory.observation.extract")]
    MemoryObservationExtract,
    #[serde(rename = "memory.embedding.index")]
    MemoryEmbeddingIndex,
    #[serde(rename = "dream.consolidation")]
    DreamConsolidation,
    #[serde(rename = "stall.classifier")]
    StallClassifier,
    #[serde(rename = "dialectic.query")]
    DialecticQuery,
    #[serde(rename = "model.discovery.claude_probe")]
    ModelDiscoveryClaudeProbe,
    #[serde(rename = "queue.deferred_model_task")]
    QueueDeferredModelTask,
}

impl ModelInvocationPurpose {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SessionLaunchFresh => "session.launch.fresh",
            Self::SessionContinueResume => "session.continue.resume",
            Self::SessionRetryAuto => "session.retry.auto",
            Self::SessionRotateChild => "session.rotate.child",
            Self::SessionHarnessTurn => "session.harness.turn",
            Self::SessionHarnessCompaction => "session.harness.compaction",
            Self::SessionCodexAppServerTurn => "session.codex_app_server.turn",
            Self::SessionOpenAiCompatibleTurn => "session.openai_compatible.turn",
            Self::AgentSpawnChild => "agent.spawn_child",
            Self::AgentReserveSuccessor => "agent.reserve_successor",
            Self::WorkflowGraphNode => "workflow.graph.node",
            Self::WorkflowChainIteration => "workflow.chain.iteration",
            Self::RecursiveLiveTask => "recursive.live.task",
            Self::ScheduledFresh => "scheduled.fresh",
            Self::AgentScheduleWakeFresh => "agent.schedule_wake.fresh",
            Self::ScheduledResumeWatch => "scheduled.resume.watch",
            Self::IssueTrackerDispatch => "issue_tracker.dispatch",
            Self::PromptCompile => "prompt.compile",
            Self::TextGenerateRpc => "text.generate.rpc",
            Self::SessionTitle => "session.title",
            Self::SessionSummary => "session.summary",
            Self::MemoryObservationExtract => "memory.observation.extract",
            Self::MemoryEmbeddingIndex => "memory.embedding.index",
            Self::DreamConsolidation => "dream.consolidation",
            Self::StallClassifier => "stall.classifier",
            Self::DialecticQuery => "dialectic.query",
            Self::ModelDiscoveryClaudeProbe => "model.discovery.claude_probe",
            Self::QueueDeferredModelTask => "queue.deferred_model_task",
        }
    }
}

impl std::fmt::Display for ModelInvocationPurpose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelInvocationKind {
    SessionLifecycle,
    Orchestration,
    DirectText,
    Background,
    Embedding,
    Discovery,
    Queue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationForeground {
    Foreground,
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaidRisk {
    PaidCapable,
    LocalOnly,
    CatalogOnly,
    NonInvocation,
}

impl PaidRisk {
    pub const fn is_non_invocation(self) -> bool {
        matches!(self, Self::CatalogOnly | Self::NonInvocation)
    }

    pub const fn is_paid_capable(self) -> bool {
        matches!(self, Self::PaidCapable)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelControlMode {
    Normal,
    PauseBackground,
    DenyPaid,
    LocalOnly,
    StopAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionStatus {
    Admitted,
    Denied,
    Duplicate,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelInvocationStatus {
    Running,
    CancellationRequested,
    Completed,
    Failed,
    Cancelled,
    Denied,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelUsageConfidence {
    Measured,
    Estimated,
    Partial,
    Stale,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    Local,
    Standard,
    Premium,
}

/// Canonical spelling for "the operator has declared no orchestration
/// child-effort ceiling", i.e. fall back to the tree root's own effort (the
/// pre-issue-#34 rule).
///
/// Stored as a real `daemon_settings` row value rather than by deleting the
/// row, so the operator surface reuses the generic write-through path
/// unchanged and the row itself documents the deliberate choice instead of
/// looking like the key was never configured.
pub const ORCHESTRATION_MAX_CHILD_EFFORT_UNSET: &str = "unset";

/// Every value the `orchestration_max_child_effort` operator setting accepts,
/// in ascending-ceiling order after the leading "unset" sentinel.
///
/// This lives in `rsi-common` because it is the shared vocabulary of three
/// separate surfaces that must not drift: the daemon's `UpdateDaemonConfig`
/// validator, the daemon's admission-time read path, and the TUI settings
/// cycle row. The TUI cannot see `rsid` internals, so a copy there would be a
/// silent drift hazard.
pub const ORCHESTRATION_MAX_CHILD_EFFORT_CHOICES: &[&str] = &[
    ORCHESTRATION_MAX_CHILD_EFFORT_UNSET,
    "low",
    "medium",
    "high",
    "xhigh",
    "max",
    "ultra",
];

/// Normalize an operator-supplied orchestration child-effort ceiling to its
/// canonical spelling, or `None` if it names no legal value.
///
/// Whitespace and case are normalized, and the empty string is accepted as a
/// spelling of "unset" so that clearing the field in any surface behaves the
/// way an operator would expect. This is the validator the `UpdateDaemonConfig`
/// write path uses, so a malformed value is rejected at the boundary with a
/// clear error rather than degrading silently later at admission time.
pub fn normalize_orchestration_max_child_effort(raw: &str) -> Option<&'static str> {
    let normalized = raw.trim().to_ascii_lowercase();
    if normalized.is_empty() || normalized == ORCHESTRATION_MAX_CHILD_EFFORT_UNSET {
        return Some(ORCHESTRATION_MAX_CHILD_EFFORT_UNSET);
    }
    ORCHESTRATION_MAX_CHILD_EFFORT_CHOICES
        .iter()
        .find(|choice| **choice == normalized)
        .copied()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetScopeKind {
    Global,
    Provider,
    Project,
    Session,
    Tree,
    Workflow,
    Subsystem,
    Retry,
    ScheduledJob,
    IssueTracker,
    RecursiveGraph,
    Operator,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetScopeRef {
    pub kind: BudgetScopeKind,
    pub scope_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct InvocationOwner {
    pub session_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub workflow_id: Option<Uuid>,
    pub scheduled_job_id: Option<Uuid>,
    pub issue_tracker_id: Option<String>,
    pub issue_identifier: Option<String>,
    pub topology_node_id: Option<String>,
    pub recursive_graph_id: Option<String>,
    pub recursive_task_id: Option<String>,
    pub recursive_attempt_id: Option<String>,
    pub operator: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInvocationUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub embedding_input_count: Option<u64>,
    pub wall_time_ms: Option<u64>,
    pub estimated_cost_usd: Option<f64>,
    pub confidence: ModelUsageConfidence,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelUsageEstimate {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub reasoning_tokens: u64,
    pub embedding_input_count: u64,
    pub wall_time_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelBudgetPolicy {
    pub scope_kind: BudgetScopeKind,
    pub scope_id: Option<String>,
    pub purpose: Option<ModelInvocationPurpose>,
    pub model_tier: Option<ModelTier>,
    pub effort: Option<String>,
    pub ceiling_model_tier: Option<ModelTier>,
    pub ceiling_effort: Option<String>,
    pub max_calls: Option<u64>,
    pub max_total_tokens: Option<u64>,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub max_cache_creation_tokens: Option<u64>,
    pub max_cache_read_tokens: Option<u64>,
    pub max_reasoning_tokens: Option<u64>,
    pub max_embedding_inputs: Option<u64>,
    pub max_wall_time_ms: Option<u64>,
    pub max_concurrency: Option<u32>,
    pub max_retries: Option<u32>,
    pub max_calls_per_window: Option<u64>,
    pub rate_window_seconds: Option<u64>,
    pub alert_threshold_ratio: Option<f64>,
}

// Manual `PartialEq`/`Eq` (not derivable as-is: `alert_threshold_ratio:
// Option<f64>` has no `Eq`, and `f64`'s `PartialEq` is not reflexive for NaN,
// which would make a derived-plus-marker `Eq` an unenforced promise). Needed
// because `LcAction::SubmitBudgetPolicy` embeds this struct and modalkit's
// `ApplicationAction` trait requires `LcAction: Eq`.
//
// Rather than asserting reflexivity as an invariant upheld elsewhere (form
// validation, RPC-boundary checks), `alert_threshold_ratio` is compared by
// bit pattern here so equality is total and reflexive for every possible
// `f64` value, including NaN and signed zero — `Eq`'s contract holds by
// construction, not by trusting that NaN never reaches this type. All other
// fields already have well-behaved `Eq` and compare structurally.
impl PartialEq for ModelBudgetPolicy {
    fn eq(&self, other: &Self) -> bool {
        self.scope_kind == other.scope_kind
            && self.scope_id == other.scope_id
            && self.purpose == other.purpose
            && self.model_tier == other.model_tier
            && self.effort == other.effort
            && self.ceiling_model_tier == other.ceiling_model_tier
            && self.ceiling_effort == other.ceiling_effort
            && self.max_calls == other.max_calls
            && self.max_total_tokens == other.max_total_tokens
            && self.max_input_tokens == other.max_input_tokens
            && self.max_output_tokens == other.max_output_tokens
            && self.max_cache_creation_tokens == other.max_cache_creation_tokens
            && self.max_cache_read_tokens == other.max_cache_read_tokens
            && self.max_reasoning_tokens == other.max_reasoning_tokens
            && self.max_embedding_inputs == other.max_embedding_inputs
            && self.max_wall_time_ms == other.max_wall_time_ms
            && self.max_concurrency == other.max_concurrency
            && self.max_retries == other.max_retries
            && self.max_calls_per_window == other.max_calls_per_window
            && self.rate_window_seconds == other.rate_window_seconds
            && self.alert_threshold_ratio.map(f64::to_bits)
                == other.alert_threshold_ratio.map(f64::to_bits)
    }
}

impl Eq for ModelBudgetPolicy {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelAdmissionHeadroom {
    pub scope: BudgetScopeRef,
    pub remaining_calls: Option<u64>,
    pub remaining_total_tokens: Option<u64>,
    pub remaining_input_tokens: Option<u64>,
    pub remaining_output_tokens: Option<u64>,
    pub remaining_cache_creation_tokens: Option<u64>,
    pub remaining_cache_read_tokens: Option<u64>,
    pub remaining_reasoning_tokens: Option<u64>,
    pub remaining_embedding_inputs: Option<u64>,
    pub remaining_wall_time_ms: Option<u64>,
    pub remaining_concurrency: Option<u32>,
    pub remaining_retries: Option<u32>,
    pub remaining_calls_in_window: Option<u64>,
    pub rate_window_seconds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelAdmissionPreflight {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub backend: Option<String>,
    pub model_tier: ModelTier,
    pub effort: Option<String>,
    pub escalation_reason: Option<String>,
    pub estimate: ModelUsageEstimate,
    pub authorized_limits: Vec<ModelAdmissionHeadroom>,
    pub denial_reason: Option<String>,
    pub degradation_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInvocationRecord {
    pub id: Uuid,
    pub purpose: ModelInvocationPurpose,
    pub kind: ModelInvocationKind,
    pub foreground: InvocationForeground,
    pub paid_risk: PaidRisk,
    pub status: ModelInvocationStatus,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub backend: Option<String>,
    pub model_tier: Option<ModelTier>,
    pub effort: Option<String>,
    pub trigger: String,
    pub owner: InvocationOwner,
    pub owner_scopes: Vec<BudgetScopeRef>,
    pub dedup_key: Option<String>,
    pub request_fingerprint: Option<String>,
    pub parent_invocation_id: Option<Uuid>,
    pub retry_of_invocation_id: Option<Uuid>,
    pub raw_admission_status: String,
    pub raw_status: String,
    pub admission_status: AdmissionStatus,
    pub usage: ModelInvocationUsage,
    pub baseline_usage: ModelInvocationUsage,
    pub error_class: Option<String>,
    pub cancellation_requested_at: Option<String>,
    pub cancellation_reason: Option<String>,
    pub cancellation_mechanism: Option<String>,
    pub authorization_reason: Option<String>,
    pub policy_authorized: bool,
    pub escalation_source: Option<String>,
    pub escalation_reason: Option<String>,
    pub policy_snapshot: Option<serde_json::Value>,
    pub policy_snapshot_status: String,
    pub policy_snapshot_error: Option<String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelBudgetHeadroom {
    pub scope_kind: BudgetScopeKind,
    pub scope_id: Option<String>,
    pub purpose: Option<ModelInvocationPurpose>,
    pub model_tier: Option<ModelTier>,
    pub effort: Option<String>,
    pub source: String,
    pub authorized: bool,
    pub policy_status: String,
    pub remaining_calls: Option<i64>,
    pub remaining_active: Option<i64>,
    pub remaining_total_tokens: Option<i64>,
    pub remaining_input_tokens: Option<i64>,
    pub remaining_output_tokens: Option<i64>,
    pub remaining_embedding_inputs: Option<i64>,
    pub remaining_wall_time_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelBudgetAlert {
    pub invocation_id: Uuid,
    pub scope_kind: BudgetScopeKind,
    pub scope_id: Option<String>,
    pub purpose: Option<ModelInvocationPurpose>,
    pub metric: String,
    pub remaining: i64,
    pub limit: i64,
    pub threshold: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInvocationView {
    pub record: ModelInvocationRecord,
    pub owner_summary: String,
    pub lineage_summary: String,
    pub scope_summary: String,
    pub denial_reason: Option<String>,
    pub stop_mechanism: String,
    pub stop_target: Option<String>,
    pub cancellation_reason: Option<String>,
    pub budget: Vec<ModelBudgetHeadroom>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCircuitStatus {
    pub scope_kind: BudgetScopeKind,
    pub scope_id: Option<String>,
    pub state: String,
    pub reason: String,
    pub error_class: Option<String>,
    pub source: String,
    pub opened_at: Option<String>,
    pub updated_at: String,
    pub reset_at: Option<String>,
    pub cooldown_secs: Option<u64>,
    pub probe_after: Option<String>,
    #[serde(default)]
    pub trip_count: u32,
    #[serde(default)]
    pub transient_failure_count: u32,
    #[serde(default)]
    pub transient_window_started_at: Option<String>,
    #[serde(default)]
    pub probe_invocation_id: Option<Uuid>,
    #[serde(default)]
    pub probe_lease_started_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelControlStatusReport {
    pub mode: ModelControlMode,
    pub mode_updated_at: Option<String>,
    pub restart_required_fields: Vec<String>,
    pub circuit_state: String,
    pub circuit_reason: String,
    pub circuits: Vec<ModelCircuitStatus>,
    pub policies: Vec<ModelBudgetPolicy>,
    pub active_invocations: Vec<ModelInvocationView>,
    pub recent_invocations: Vec<ModelInvocationView>,
    pub recent_denials: Vec<ModelInvocationView>,
    pub recent_budget_alerts: Vec<ModelBudgetAlert>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCancellationSkipped {
    pub invocation_id: Uuid,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelControlPolicyUpdateReport {
    pub previous_mode: ModelControlMode,
    pub current_mode: ModelControlMode,
    pub updated_at: String,
    pub live_applied: bool,
    pub restart_required_fields: Vec<String>,
    pub interrupted_sessions: Vec<Uuid>,
    pub requested_invocations: Vec<Uuid>,
    pub cancelled_invocations: Vec<Uuid>,
    pub skipped_invocations: Vec<Uuid>,
    pub skipped_details: Vec<ModelCancellationSkipped>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelModelInvocationReport {
    pub invocation_id: Uuid,
    pub request_recorded: bool,
    pub already_requested: bool,
    pub cancelled: bool,
    pub mechanism: String,
    pub session_id: Option<Uuid>,
    pub final_status: ModelInvocationStatus,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelInvocationList {
    pub invocations: Vec<ModelInvocationView>,
}

impl Default for ModelInvocationUsage {
    fn default() -> Self {
        Self {
            input_tokens: None,
            output_tokens: None,
            cache_creation_tokens: None,
            cache_read_tokens: None,
            reasoning_tokens: None,
            embedding_input_count: None,
            wall_time_ms: None,
            estimated_cost_usd: None,
            confidence: ModelUsageConfidence::Unavailable,
        }
    }
}

impl Default for ModelUsageEstimate {
    fn default() -> Self {
        Self {
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            reasoning_tokens: 0,
            embedding_input_count: 0,
            wall_time_ms: 0,
        }
    }
}

impl Default for BudgetScopeRef {
    fn default() -> Self {
        Self {
            kind: BudgetScopeKind::Global,
            scope_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ModelInvocationPurpose;

    #[test]
    fn agent_schedule_wake_fresh_has_stable_wire_value() {
        let purpose = ModelInvocationPurpose::AgentScheduleWakeFresh;
        assert_eq!(purpose.as_str(), "agent.schedule_wake.fresh");
        assert_eq!(
            serde_json::to_string(&purpose).expect("serialize purpose"),
            "\"agent.schedule_wake.fresh\""
        );
        assert_eq!(
            serde_json::from_str::<ModelInvocationPurpose>("\"agent.schedule_wake.fresh\"")
                .expect("deserialize purpose"),
            purpose
        );
    }

    #[test]
    fn session_openai_compatible_turn_has_stable_wire_value() {
        let purpose = ModelInvocationPurpose::SessionOpenAiCompatibleTurn;
        assert_eq!(purpose.as_str(), "session.openai_compatible.turn");
        assert_eq!(
            serde_json::to_string(&purpose).expect("serialize purpose"),
            "\"session.openai_compatible.turn\""
        );
        assert_eq!(
            serde_json::from_str::<ModelInvocationPurpose>("\"session.openai_compatible.turn\"")
                .expect("deserialize purpose"),
            purpose
        );
    }
}
