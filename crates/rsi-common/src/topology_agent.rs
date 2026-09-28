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
    /// `next_cursor` of the previous page (a topology name).
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologyListResultV1 {
    pub topologies: Vec<AgentTopologySummaryV1>,
    /// Present only with `include_executions`: active and blocked
    /// executions in the caller's scope, newest first, at most 32.
    #[serde(default)]
    pub executions: Vec<AgentTopologyExecutionSummaryV1>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTopologyGetExecutionResultV1 {
    pub execution: AgentTopologyExecutionSummaryV1,
    pub nodes: Vec<TopologyAttemptSummary>,
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
            topology_id: Uuid::new_v4(),
            expected_digest: format!("sha256:{}", "a".repeat(64)),
            epic_id: Uuid::new_v4(),
            inputs: serde_json::Value::Null,
            base_commit: None,
            idempotency_key: "run-1".into(),
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
