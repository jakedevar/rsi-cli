//! Request and result contracts for the scoped agent topology verbs (#633,
//! plan §5.1).
//!
//! Every request is strict (`deny_unknown_fields`) and names no caller,
//! Epic-lead, manager or session identity: the daemon binds the caller from
//! the transport token and derives every authority fact.
//!
//! `AgentTopologyResolveAttempt` reuses the operator
//! [`crate::rpc::ResolveTopologyAttemptParams`] schema unchanged (plan §3.4).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::rpc::TopologyAttemptSummary;
use crate::types::{TopologyDefinition, WorkflowExecutionStatus};

/// Largest page any topology list returns.
pub const AGENT_TOPOLOGY_LIST_MAX: u32 = 32;
/// Largest event page `AgentTopologyGetExecution` returns.
pub const AGENT_TOPOLOGY_EVENTS_MAX: u32 = 64;
/// Longest topology name an agent may author.
pub const AGENT_TOPOLOGY_NAME_MAX: usize = 128;
/// Longest idempotency key.
pub const AGENT_TOPOLOGY_KEY_MAX: usize = 128;

/// Ownership scope of an agent-authored topology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTopologyScopeV1 {
    /// Owned by one Epic; its lead (or the in-scope manager) authors it.
    Epic,
    /// Owned by the current appointed manager of the project.
    Manager,
}

impl AgentTopologyScopeV1 {
    /// Stored `topologies.owner_kind` value.
    #[must_use]
    pub const fn owner_kind(self) -> &'static str {
        match self {
            Self::Epic => "epic",
            Self::Manager => "manager",
        }
    }
}

fn valid_key(key: &str) -> bool {
    !key.trim().is_empty() && key.len() <= AGENT_TOPOLOGY_KEY_MAX && !key.contains('\0')
}

fn valid_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn valid_commit(commit: &str) -> bool {
    commit.len() == 40 && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// `AgentTopologyUpsert`: create or revise one scoped topology.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTopologyUpsertRequestV1 {
    pub name: String,
    pub definition: TopologyDefinition,
    pub scope: AgentTopologyScopeV1,
    /// Required for a manager `epic` upsert; a lead's Epic is daemon-derived
    /// and, when supplied, must equal it.
    #[serde(default)]
    pub epic_id: Option<Uuid>,
    /// CAS fence for revising an existing topology; omitted to create.
    #[serde(default)]
    pub expected_revision: Option<i64>,
    /// Validate and report diagnostics without writing.
    #[serde(default)]
    pub validate_only: bool,
    pub idempotency_key: String,
    /// #1235: the target project of a global manager seat acting inside its
    /// operator grant. Omitted means the caller's own project. A target the
    /// daemon checks against the grant, never caller identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
}

impl AgentTopologyUpsertRequestV1 {
    /// Transport-independent shape checks.
    ///
    /// # Errors
    /// A stable code naming the malformed field.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.name.trim().is_empty()
            || self.name.len() > AGENT_TOPOLOGY_NAME_MAX
            || self.name.contains('\0')
        {
            return Err("topology_invalid_name");
        }
        if !valid_key(&self.idempotency_key) {
            return Err("topology_invalid_idempotency_key");
        }
        if self.expected_revision.is_some_and(|revision| revision < 1) {
            return Err("topology_invalid_expected_revision");
        }
        if self.epic_id.is_some_and(|id| id.is_nil()) {
            return Err("topology_invalid_epic_id");
        }
        Ok(())
    }
}

/// `AgentTopologyUpsert` result. `diagnostics` is empty exactly when the
/// definition validates; a `validate_only` request never writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologyUpsertResultV1 {
    #[serde(default)]
    pub topology_id: Option<Uuid>,
    #[serde(default)]
    pub revision: Option<i64>,
    pub definition_digest: String,
    #[serde(default)]
    pub diagnostics: Vec<String>,
    #[serde(default)]
    pub deduplicated: bool,
}

/// `AgentTopologyList`: a scoped page of topologies.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTopologyListRequestV1 {
    #[serde(default)]
    pub scope: Option<AgentTopologyScopeV1>,
    #[serde(default)]
    pub epic_id: Option<Uuid>,
    #[serde(default)]
    pub include_executions: bool,
    /// #2264: also list settled executions (`succeeded`, `failed`,
    /// `cancelled`), so a manager can read a topology's run history. Implies
    /// `include_executions`. The default is live-only (accepted, running,
    /// cancelling, blocked).
    #[serde(default)]
    pub include_settled: bool,
    /// `next_cursor` of the previous page (a topology name).
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    /// #1235: the target project of a global manager seat acting inside its
    /// operator grant. Omitted means the caller's own project. A target the
    /// daemon checks against the grant, never caller identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
}

impl AgentTopologyListRequestV1 {
    /// # Errors
    /// A stable code naming the malformed field.
    pub fn validated_limit(&self) -> Result<u32, &'static str> {
        if self.epic_id.is_some_and(|id| id.is_nil()) {
            return Err("topology_invalid_epic_id");
        }
        if self
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.len() > AGENT_TOPOLOGY_NAME_MAX || cursor.contains('\0'))
        {
            return Err("topology_invalid_cursor");
        }
        match self.limit {
            None => Ok(AGENT_TOPOLOGY_LIST_MAX),
            Some(limit) if (1..=AGENT_TOPOLOGY_LIST_MAX).contains(&limit) => Ok(limit),
            Some(_) => Err("topology_invalid_limit"),
        }
    }
}

/// One topology visible to the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologySummaryV1 {
    pub topology_id: Uuid,
    pub name: String,
    /// `operator`, `manager` or `epic`.
    pub owner_kind: String,
    #[serde(default)]
    pub epic_id: Option<Uuid>,
    pub revision: i64,
    pub definition_digest: String,
    pub shared: bool,
    pub updated_at: String,
    /// #2660: set when the name is a daemon-shipped starter and the stored
    /// definition is not the shipped one (edited, or an older text the daemon
    /// does not upgrade). Absent when the row follows the shipped definition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starter_drift: Option<StarterDriftV1>,
}

/// How a stored starter differs from what the daemon ships (#2660).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StarterDriftV1 {
    /// Canonical digest of the definition this daemon build ships under the
    /// row's name. The row's own `definition_digest` is the stored side.
    pub shipped_digest: String,
}

/// `AgentTopologyGet`: one visible topology with its full definition (#2414).
/// Exactly one of `topology_id` and `name` selects it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTopologyGetRequestV1 {
    #[serde(default)]
    pub topology_id: Option<Uuid>,
    #[serde(default)]
    pub name: Option<String>,
    /// #1235: the target project of a global manager seat acting inside its
    /// operator grant. Omitted means the caller's own project. A target the
    /// daemon checks against the grant, never caller identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
}

impl AgentTopologyGetRequestV1 {
    /// # Errors
    /// A stable code naming the malformed field.
    pub fn validate(&self) -> Result<(), &'static str> {
        match (&self.topology_id, &self.name) {
            (Some(id), None) if !id.is_nil() => Ok(()),
            (None, Some(name))
                if !name.trim().is_empty()
                    && name.len() <= AGENT_TOPOLOGY_NAME_MAX
                    && !name.contains('\0') =>
            {
                Ok(())
            }
            (Some(_), Some(_)) => Err("topology_get_selector_ambiguous"),
            (None, None) => Err("topology_get_selector_missing"),
            _ => Err("topology_invalid_name"),
        }
    }
}

/// `AgentTopologyGet` result: the summary fields plus the stored definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologyGetResultV1 {
    pub topology: AgentTopologySummaryV1,
    /// The stored definition as JSON. `topology.definition_digest` is the
    /// digest of the stored text (not of this value's re-serialization) and is
    /// what fences `AgentTopologyExecute`. Fork it by passing it, with a new
    /// name, to `AgentTopologyUpsert`; revise it by passing the edited
    /// definition with `expected_revision = topology.revision`.
    pub definition: serde_json::Value,
    /// The session that last wrote the topology (a retired seat session
    /// is normal; revising is keyed to scope, not to this session).
    #[serde(default)]
    pub owner_session_id: Option<Uuid>,
    #[serde(default)]
    pub project_id: Option<Uuid>,
    pub created_at: String,
}

/// One durable execution visible to the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologyExecutionSummaryV1 {
    pub execution_id: Uuid,
    #[serde(default)]
    pub topology_id: Option<Uuid>,
    #[serde(default)]
    pub topology_revision: Option<i64>,
    pub topology_name: String,
    #[serde(default)]
    pub epic_id: Option<Uuid>,
    /// `accepted`, `running`, `cancelling`, `succeeded`, `failed`,
    /// `cancelled` or `blocked`.
    pub status: String,
    pub row_version: i64,
    /// `operator`, `manager`, `epic_lead` or `schedule`.
    pub requested_by_kind: String,
    pub base_commit: String,
    #[serde(default)]
    pub blocked_reason: Option<serde_json::Value>,
    #[serde(default)]
    pub error: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub finished_at: Option<String>,
    /// #2559: what the run did, so a settled execution is readable without
    /// further reads. Present on `AgentTopologyGetExecution` for a settled
    /// (`succeeded`, `failed`, `cancelled` or `blocked`) execution only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<AgentTopologyOutcomeV1>,
}

/// #2559: the bounded digest of a settled execution: the facts a manager
/// otherwise reads the node list, the Issue and the Issue list for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologyOutcomeV1 {
    /// The `inputs.issue` and its status after the settle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<AgentTopologyOutcomeIssueV1>,
    /// One per land node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lands: Vec<AgentTopologyOutcomeLandV1>,
    /// One per review node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviews: Vec<AgentTopologyOutcomeReviewV1>,
    /// Issues the run's node sessions filed (other than `inputs.issue`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub followups: Vec<AgentTopologyOutcomeFollowupV1>,
    /// One per node whose latest attempt failed or is blocked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<AgentTopologyOutcomeFailureV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologyOutcomeIssueV1 {
    pub display_number: i64,
    #[serde(default)]
    pub title: String,
    /// `Open`, `InProgress`, `Closed` or `Cancelled`; absent when unreadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologyOutcomeLandV1 {
    pub node_id: String,
    /// The queue entry state (`published`, `refused`, ...) or the attempt
    /// status when no output was recorded.
    pub state: String,
    /// The SHA the landing published on `rolling`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landed_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<Uuid>,
    /// Queue wall time: enqueued to finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_wall_secs: Option<i64>,
    /// The code of a refused Issue close (#2578): the landing published but the
    /// execution's owner could not close the input Issue, which stays open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue_close_refused: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologyOutcomeReviewV1 {
    pub node_id: String,
    /// `approved`, `changes_requested`, `rounds_exhausted` or `unsettled`.
    pub verdict: String,
    /// The settling attempt's number within its round (2 = one earlier
    /// attempt gave no usable verdict).
    pub attempts: u32,
    /// 1-based review round (loop iteration) of the settling attempt.
    pub round: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologyOutcomeFollowupV1 {
    pub display_number: i64,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologyOutcomeFailureV1 {
    pub node_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    pub attempt_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologyListResultV1 {
    pub topologies: Vec<AgentTopologySummaryV1>,
    /// Present only with `include_executions` or `include_settled`: live
    /// (active and blocked) executions in the caller's scope first, then, with
    /// `include_settled`, settled ones, each group newest first, at most 32 in
    /// all.
    #[serde(default)]
    pub executions: Vec<AgentTopologyExecutionSummaryV1>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

/// The execution input key that carries the on-call seat (#1641 S3a). The
/// executor strips it from node inputs; callers may not supply it.
pub const ON_CALL_INPUT_KEY: &str = "_rsi_on_call";

/// `AgentTopologyExecute`: run one visible topology under an Epic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTopologyExecuteRequestV1 {
    pub topology_id: Uuid,
    /// The `definition_digest` the caller last observed (list or upsert).
    pub expected_digest: String,
    pub epic_id: Uuid,
    #[serde(default)]
    pub inputs: serde_json::Value,
    /// Full 40-hex base commit; defaults to the repository's rolling tip.
    #[serde(default)]
    pub base_commit: Option<String>,
    pub idempotency_key: String,
    /// #1235: the target project of a global manager seat acting inside its
    /// operator grant. Omitted means the caller's own project. A target the
    /// daemon checks against the grant, never caller identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    /// #1641 S3a: the on-call manager seat that answers this execution's node
    /// questions. Omitted means the project's manager (else the covering
    /// portfolio seat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_call: Option<crate::types::TopologyOnCallSeat>,
    /// #2427: the live execution that holds `inputs.issue`, to interrupt and
    /// replace. Without it a live holder refuses `topology_issue_already_live`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes_execution: Option<Uuid>,
    /// Who is woken when the execution settles (hub #2967), like
    /// `AgentSubmitJob.wake`. `owner` (the default) arms one
    /// `topology_execution_settled` resume wake for the requester, as
    /// `AgentEnqueueLandingSource` does for its outcome (#2405); `none`
    /// arms nothing. A replay of an accepted request never arms a second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wake: Option<crate::agent_jobs::JobWake>,
}

impl AgentTopologyExecuteRequestV1 {
    /// # Errors
    /// A stable code naming the malformed field.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.topology_id.is_nil() {
            return Err("topology_invalid_topology_id");
        }
        if self.epic_id.is_nil() {
            return Err("topology_invalid_epic_id");
        }
        if !valid_digest(&self.expected_digest) {
            return Err("topology_invalid_expected_digest");
        }
        if self
            .base_commit
            .as_deref()
            .is_some_and(|commit| !valid_commit(commit))
        {
            return Err("topology_invalid_base_commit");
        }
        if !self.inputs.is_null() && !self.inputs.is_object() {
            return Err("topology_invalid_inputs");
        }
        // The seat travels in the execution's input under a reserved key.
        if self
            .inputs
            .as_object()
            .is_some_and(|inputs| inputs.contains_key(ON_CALL_INPUT_KEY))
        {
            return Err("topology_invalid_inputs");
        }
        if matches!(
            self.on_call,
            Some(crate::types::TopologyOnCallSeat::Portfolio { node_id }) if node_id.is_nil()
        ) {
            return Err("topology_invalid_on_call");
        }
        if !valid_key(&self.idempotency_key) {
            return Err("topology_invalid_idempotency_key");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologyExecuteResultV1 {
    pub execution_id: Uuid,
    pub accepted_at: DateTime<Utc>,
    pub base_commit: String,
    /// True when the idempotency key replayed an earlier acceptance.
    pub deduplicated: bool,
    /// True when this call armed the requester's `topology_execution_settled`
    /// wake (hub #2967). False on a replay, on `wake:"none"`, and when arming
    /// was refused (then arm it yourself with `AgentScheduleWake`).
    #[serde(default)]
    pub settle_wake_armed: bool,
    /// The stable refusal code (for example `wake_when_cap_reached`) when the
    /// daemon tried to arm the settle wake and was refused (#3019): the
    /// execution is accepted and unwatched, so arm your own wake or poll it.
    /// Absent when the wake was armed, replayed, covered or `wake:"none"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settle_wake_refusal: Option<String>,
}

/// `AgentTopologyGetExecution`: one execution with its attempts and a page
/// of its audit events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTopologyGetExecutionRequestV1 {
    pub execution_id: Uuid,
    #[serde(default)]
    pub after_sequence: Option<u64>,
    #[serde(default)]
    pub limit: Option<u32>,
    /// #1235: the target project of a global manager seat acting inside its
    /// operator grant. Omitted means the caller's own project. A target the
    /// daemon checks against the grant, never caller identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
}

impl AgentTopologyGetExecutionRequestV1 {
    /// # Errors
    /// A stable code naming the malformed field.
    pub fn validated_limit(&self) -> Result<u32, &'static str> {
        if self.execution_id.is_nil() {
            return Err("topology_invalid_execution_id");
        }
        match self.limit {
            None => Ok(AGENT_TOPOLOGY_EVENTS_MAX),
            Some(limit) if (1..=AGENT_TOPOLOGY_EVENTS_MAX).contains(&limit) => Ok(limit),
            Some(_) => Err("topology_invalid_limit"),
        }
    }
}

/// One audit event. Payload details (including other callers' idempotency
/// keys) are not projected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologyEventV1 {
    pub sequence: u64,
    pub kind: String,
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub attempt_id: Option<Uuid>,
    pub actor_kind: String,
    #[serde(default)]
    pub actor_session_id: Option<Uuid>,
    pub created_at: String,
}

/// Most receipts of each kind one execution view carries.
pub const REVIEW_RECEIPTS_MAX: usize = 32;

/// A failure an open known-failure record already names (#3020).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologyKnownFailureV1 {
    pub test: String,
    pub issue: u64,
}

/// The author's `scoped_test` receipt the daemon verified for one node attempt
/// (#3020): the job, the exact commit it verified and its verdict. Keyed to
/// `head`; a reviewer compares it with the commit it reviews.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologyScopedTestReceiptV1 {
    pub attempt_id: Uuid,
    pub node_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_id: Option<Uuid>,
    /// The commit the job's receipt verified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// `precise` or `targeted_subset`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default)]
    pub failing_tests: Vec<String>,
    /// Baseline reds: failures an open known-failure record names.
    #[serde(default)]
    pub known_failures: Vec<AgentTopologyKnownFailureV1>,
    #[serde(default)]
    pub new_failures: Vec<String>,
    /// Why the daemon did not accept the receipt, when it did not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unverified_reason: Option<String>,
}

/// The merge-queue gate's receipt for a land node's published entry (#3020).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologyGateReceiptV1 {
    /// The land attempt that enqueued the entry.
    pub attempt_id: Uuid,
    pub entry_id: Uuid,
    /// The source commit the entry landed.
    pub commit: String,
    pub state: String,
    /// The filters the gate ran.
    #[serde(default)]
    pub test_filters: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_glob: Option<String>,
}

/// The test evidence of one execution, for its assigned reviewer only (#3020).
/// Each list is bounded by [`REVIEW_RECEIPTS_MAX`], newest attempts kept.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologyReviewReceiptsV1 {
    #[serde(default)]
    pub scoped_test: Vec<AgentTopologyScopedTestReceiptV1>,
    #[serde(default)]
    pub gate: Vec<AgentTopologyGateReceiptV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologyGetExecutionResultV1 {
    pub execution: AgentTopologyExecutionSummaryV1,
    pub nodes: Vec<TopologyAttemptSummary>,
    /// The author's `scoped_test` and the merge-queue gate receipts (#3020).
    /// Present only for the assigned reviewer of this execution; a manager or
    /// lead reading the execution gets none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipts: Option<AgentTopologyReviewReceiptsV1>,
    pub events: Vec<AgentTopologyEventV1>,
    /// Pass as `after_sequence` to continue; equals the last returned
    /// event's sequence (or the request's cursor when none were returned).
    pub next_sequence: u64,
}

/// `AgentTopologyInterrupt`: request cancellation under a row-version CAS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentTopologyInterruptRequestV1 {
    pub execution_id: Uuid,
    pub expected_row_version: i64,
    pub idempotency_key: String,
    /// #1235: the target project of a global manager seat acting inside its
    /// operator grant. Omitted means the caller's own project. A target the
    /// daemon checks against the grant, never caller identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
}

impl AgentTopologyInterruptRequestV1 {
    /// # Errors
    /// A stable code naming the malformed field.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.execution_id.is_nil() {
            return Err("topology_invalid_execution_id");
        }
        if self.expected_row_version < 1 {
            return Err("topology_invalid_expected_row_version");
        }
        if !valid_key(&self.idempotency_key) {
            return Err("topology_invalid_idempotency_key");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologyInterruptResultV1 {
    pub execution_id: Uuid,
    pub status: WorkflowExecutionStatus,
    pub interrupt_requested_at: DateTime<Utc>,
    pub deduplicated: bool,
}

/// Transport-independent checks for `AgentTopologyResolveAttempt`.
///
/// Reuses [`crate::rpc::ResolveTopologyAttemptParams`] and mirrors the daemon:
/// `confirm_preserved_commit` (full 40-hex) is required iff `discard`.
///
/// # Errors
/// A stable code naming the malformed field.
pub fn validate_resolve_attempt(
    params: &crate::rpc::ResolveTopologyAttemptParams,
) -> Result<(), &'static str> {
    if params.execution_id.is_nil() || params.attempt_id.is_nil() {
        return Err("topology_invalid_attempt");
    }
    if params.expected_row_version < 1 {
        return Err("topology_invalid_expected_row_version");
    }
    if !valid_key(&params.idempotency_key) {
        return Err("topology_invalid_idempotency_key");
    }
    let discard = params.action == crate::rpc::TopologyAttemptAction::Discard;
    match params.confirm_preserved_commit.as_deref() {
        Some(commit) if discard && valid_commit(commit) => Ok(()),
        None if !discard => Ok(()),
        _ => Err("topology_invalid_confirm_preserved_commit"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_reject_caller_identity_and_malformed_fences() {
        let spoof = serde_json::json!({
            "execution_id": Uuid::new_v4(),
            "expected_row_version": 1,
            "idempotency_key": "k",
            "caller_session_id": Uuid::new_v4(),
        });
        assert!(serde_json::from_value::<AgentTopologyInterruptRequestV1>(spoof).is_err());
        let execute = AgentTopologyExecuteRequestV1 {
            project_id: None,
            topology_id: Uuid::new_v4(),
            expected_digest: format!("sha256:{}", "a".repeat(64)),
            epic_id: Uuid::new_v4(),
            inputs: serde_json::Value::Null,
            base_commit: None,
            idempotency_key: "run-1".into(),
            on_call: None,
            supersedes_execution: None,
            wake: None,
        };
        assert_eq!(execute.validate(), Ok(()));
        let mut bad = execute.clone();
        bad.expected_digest = "sha256:ABC".into();
        assert_eq!(bad.validate(), Err("topology_invalid_expected_digest"));
        let mut bad = execute;
        bad.base_commit = Some("abc".into());
        assert_eq!(bad.validate(), Err("topology_invalid_base_commit"));
        let list = AgentTopologyListRequestV1 {
            limit: Some(33),
            ..AgentTopologyListRequestV1::default()
        };
        assert_eq!(list.validated_limit(), Err("topology_invalid_limit"));
        assert_eq!(
            AgentTopologyListRequestV1::default().validated_limit(),
            Ok(AGENT_TOPOLOGY_LIST_MAX)
        );
    }
}
