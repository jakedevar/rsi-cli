//! Durable agent-orchestration coordination contracts.
//!
//! These DTOs are shared by the tokened JSON-RPC surface and the native
//! `rsi_control` tools. Caller identity is deliberately absent from every
//! request: transports bind it server-side.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Maximum number of child rows returned by one progress snapshot.
pub const AGENT_PROGRESS_MAX_COHORT: usize = 256;
/// Maximum UTF-8 byte length of a normalized operator display role.
pub const AGENT_ROLE_MAX_BYTES: usize = 64;

/// Normalize optional operator display metadata without deriving authority.
///
/// Roles preserve caller-provided case, trim outer whitespace, and collapse
/// runs of non-control whitespace. Control characters (including NUL, tabs,
/// and newlines) are rejected before normalization.
pub fn normalize_agent_role(role: Option<&str>) -> Result<Option<String>, &'static str> {
    let Some(role) = role else {
        return Ok(None);
    };
    if role.chars().any(char::is_control) {
        return Err("agent_spawn_role_must_not_contain_control_characters");
    }
    let normalized = role.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return Err("agent_spawn_role_must_not_be_empty");
    }
    if normalized.len() > AGENT_ROLE_MAX_BYTES {
        return Err("agent_spawn_role_must_be_at_most_64_bytes");
    }
    Ok(Some(normalized))
}

/// Strict v1 request for `AgentSpawnChild`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpawnChildRequestV1 {
    pub kind: crate::types::SessionKind,
    /// Optional provider for the child. When omitted, the spawn coordinator
    /// preserves the historical behavior of inheriting the emitter provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<crate::types::SessionProvider>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// Display-ready pipeline function (for example `Researcher`). This is
    /// presentation metadata only; caller and Epic authority stay transport-bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_role: Option<String>,
    pub query: String,
    #[serde(default)]
    pub topology_node: Option<String>,
    #[serde(default)]
    pub iteration: Option<u32>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    pub idempotency_key: String,
}

impl AgentSpawnChildRequestV1 {
    /// Validate transport-independent request bounds.
    ///
    /// # Errors
    ///
    /// Returns a stable safe error class when the idempotency key, query, or
    /// tag collection exceeds the v1 contract bounds.
    pub fn validate(&self) -> Result<(), &'static str> {
        let key = self.idempotency_key.as_bytes();
        if key.is_empty() || key.len() > 128 || key.contains(&0) {
            return Err("agent_spawn_idempotency_key_must_be_1_to_128_bytes_without_nul");
        }
        if self.query.is_empty() || self.query.len() > 256 * 1024 {
            return Err("agent_spawn_query_must_be_1_to_262144_bytes");
        }
        if self.tags.as_ref().is_some_and(|tags| tags.len() > 64) {
            return Err("agent_spawn_tags_must_contain_at_most_64_items");
        }
        normalize_agent_role(self.agent_role.as_deref())?;
        Ok(())
    }

    /// Return the canonical request used for durable JSON and fingerprinting.
    pub fn normalized(mut self) -> Result<Self, &'static str> {
        self.agent_role = normalize_agent_role(self.agent_role.as_deref())?;
        self.validate()?;
        Ok(self)
    }
}

/// Durable state of one reserved agent spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSpawnStateV1 {
    Reserved,
    Queued,
    Launching,
    Launched,
    Failed,
}

impl AgentSpawnStateV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Queued => "queued",
            Self::Launching => "launching",
            Self::Launched => "launched",
            Self::Failed => "failed",
        }
    }
}

/// Stable result returned by every exact `AgentSpawnChild` replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSpawnChildResultV1 {
    pub spawn_request_id: Uuid,
    pub child_session_id: Uuid,
    pub epic_id: Uuid,
    pub kind: crate::types::SessionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_role: Option<String>,
    pub epic_spawn_ordinal: u32,
    pub state: AgentSpawnStateV1,
    pub deduplicated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safe_error_class: Option<String>,
}

/// Strict v1 request for `AgentReserveSuccessor`.
///
/// Every authority-bearing identity is deliberately absent. The daemon binds
/// the predecessor from the authenticated transport and derives the owning
/// Epic, candidate, reservation, and launch identities itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReserveSuccessorRequestV1 {
    pub kind: crate::types::SessionKind,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    pub query: String,
    #[serde(default)]
    pub topology_node: Option<String>,
    #[serde(default)]
    pub iteration: Option<u32>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    pub idempotency_key: String,
}

impl AgentReserveSuccessorRequestV1 {
    /// Validate transport-independent request bounds.
    ///
    /// # Errors
    ///
    /// Returns a stable safe error class when the idempotency key, query, or
    /// tag collection exceeds the v1 contract bounds.
    pub fn validate(&self) -> Result<(), &'static str> {
        let key = self.idempotency_key.as_bytes();
        if key.is_empty() || key.len() > 128 || key.contains(&0) {
            return Err("agent_successor_idempotency_key_must_be_1_to_128_bytes_without_nul");
        }
        if self.query.is_empty() || self.query.len() > 256 * 1024 {
            return Err("agent_successor_query_must_be_1_to_262144_bytes");
        }
        if self.tags.as_ref().is_some_and(|tags| tags.len() > 64) {
            return Err("agent_successor_tags_must_contain_at_most_64_items");
        }
        Ok(())
    }
}

/// Forward-only durable state of one master-baton successor reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSuccessorStateV1 {
    Reserved,
    Launching,
    Committed,
    Failed,
    Uncertain,
}

impl AgentSuccessorStateV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Launching => "launching",
            Self::Committed => "committed",
            Self::Failed => "failed",
            Self::Uncertain => "uncertain",
        }
    }

    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        Some(match value {
            "reserved" => Self::Reserved,
            "launching" => Self::Launching,
            "committed" => Self::Committed,
            "failed" => Self::Failed,
            "uncertain" => Self::Uncertain,
            _ => return None,
        })
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Committed | Self::Failed)
    }

    #[must_use]
    pub const fn may_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Reserved, Self::Launching | Self::Failed)
                | (
                    Self::Launching,
                    Self::Committed | Self::Failed | Self::Uncertain
                )
                | (Self::Uncertain, Self::Committed | Self::Failed)
        )
    }
}

/// Stable receipt returned by every exact `AgentReserveSuccessor` replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentReserveSuccessorResultV1 {
    pub reservation_id: Uuid,
    pub predecessor_session_id: Uuid,
    pub epic_id: Uuid,
    pub candidate_session_id: Uuid,
    pub kind: crate::types::SessionKind,
    pub state: AgentSuccessorStateV1,
    pub state_version: u64,
    pub deduplicated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safe_error_class: Option<String>,
}

/// Optional bounded subdivision for `AgentGetProgress`.
///
/// Omitted/empty `session_ids` means every child in the caller's authorized
/// cohort. The caller identity itself is always transport-bound.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentGetProgressParamsV1 {
    #[serde(default)]
    pub session_ids: Vec<Uuid>,
}

/// Closed progress status, including a pre-session durable reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentProgressStatusV1 {
    Reserved,
    Starting,
    Running,
    WaitingApproval,
    Completed,
    Failed,
    Interrupted,
    Archived,
    Deleted,
}

/// Durable cursor for one reserved child and its current rotation tip.
///
/// `lineage_tip_id`, `event_sequence`, and `custody_generation` are exactly the
/// [`AgentContinuationCursorV1`] tuple, read from the same durable facts inside
/// the same snapshot, so a caller may use them directly through
/// [`AgentContinueChildRequestV1`] without a second round trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProgressCursorV1 {
    pub session_id: Uuid,
    pub lineage_tip_id: Uuid,
    pub event_sequence: i64,
    /// Sandbox custody generation of the rotation tip, when its execution
    /// projection publishes one. Absent for an unsandboxed tip and for one
    /// whose projection carries no generation yet; both are legitimate states
    /// and are distinguished from a present generation by full equality in
    /// [`AgentContinuationCursorV1::satisfies`]. Absence is omitted from the
    /// wire, so a snapshot with no custody is byte-identical to one taken
    /// before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custody_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_at: Option<DateTime<Utc>>,
    pub status: AgentProgressStatusV1,
    pub status_updated_at: DateTime<Utc>,
}

/// Explicit freshness derived from durable status/event times.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProgressFreshnessV1 {
    pub cursor_updated_at: DateTime<Utc>,
    pub staleness_ms: u64,
}

/// Persisted terminal-watch state for the owner/child natural key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentWatchStateV1 {
    Missing,
    Disabled,
    Enabled,
}

/// Message-state counts. Phase 1 creates the storage seam; Phase 2 supplies
/// mutation/delivery behavior without changing this progress envelope.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessageCountsV1 {
    pub queued: u32,
    pub claimed: u32,
    pub injected: u32,
    pub acknowledged: u32,
    pub uncertain: u32,
    pub failed: u32,
    pub expired: u32,
}

/// The current attempt's persisted acknowledgement cursor (P2-01/P2-03).
///
/// This is durable evidence of the exact provider-originated conversation
/// event that acknowledged delivery — never a daemon-created injection event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessageAckCursorV1 {
    pub event_id: i64,
    pub event_session_id: Uuid,
    pub event_sequence: i64,
}

/// Bounded per-target mailbox detail (P2-03).
///
/// Present only when the target has at least one message. Every field is read
/// from the aggregate and its current attempt; message BODIES are never
/// loaded, so progress cost does not grow with payload size.
///
/// This is an ADDITIVE sibling of [`AgentMessageCountsV1`] rather than an
/// extension of it: the counts envelope is accepted Phase 1 surface, and
/// growing it would change an already-shipped DTO. A skipped-when-absent
/// option leaves every existing Phase 1 snapshot byte-identical.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessageQueueSummaryV1 {
    /// Age of the OLDEST still-pending message, in milliseconds. Pending means
    /// exactly the non-terminal states the acceptance caps count:
    /// `queued|claimed|injected|uncertain`. `None` when nothing is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_pending_age_ms: Option<u64>,
    /// State and version of the most recently updated message for this target.
    pub latest_state: AgentMessageStateV1,
    pub latest_state_version: i64,
    pub latest_attempt_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_current_attempt_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_safe_error_class: Option<String>,
    /// The current attempt's acknowledgement cursor, where one is persisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acknowledgement_cursor: Option<AgentMessageAckCursorV1>,
}

/// One UUID-sorted child row in an aggregate progress response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProgressRowV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_request_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_state: Option<AgentSpawnStateV1>,
    /// Safe refusal class for a failed asynchronous spawn. This is the same
    /// class returned by an exact `AgentSpawnChild` replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_safe_error_class: Option<String>,
    /// Immutable source commit recorded when this logical child received its
    /// sandbox custody root. Absent when no durable sandbox base exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    pub cursor: AgentProgressCursorV1,
    pub freshness: AgentProgressFreshnessV1,
    pub watch_state: AgentWatchStateV1,
    pub messages: AgentMessageCountsV1,
    /// Work the lead still needs to do for a terminal child session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obligation: Option<AgentProgressObligationV1>,
    /// P2-03 mailbox detail. Absent when this target has no messages at all,
    /// which keeps every accepted Phase 1 snapshot byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_queue: Option<AgentMessageQueueSummaryV1>,
}

/// Lead-side follow-up required for an unhandled terminal child session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentProgressObligationV1 {
    HarvestFailed,
    HarvestInterrupted,
}

/// Closed status counts for one cohort snapshot.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentProgressStatusCountsV1 {
    pub reserved: u32,
    pub starting: u32,
    pub running: u32,
    pub waiting_approval: u32,
    pub completed: u32,
    pub failed: u32,
    pub interrupted: u32,
    pub archived: u32,
    pub deleted: u32,
}

impl AgentProgressStatusCountsV1 {
    pub const fn record(&mut self, status: AgentProgressStatusV1) {
        let counter = match status {
            AgentProgressStatusV1::Reserved => &mut self.reserved,
            AgentProgressStatusV1::Starting => &mut self.starting,
            AgentProgressStatusV1::Running => &mut self.running,
            AgentProgressStatusV1::WaitingApproval => &mut self.waiting_approval,
            AgentProgressStatusV1::Completed => &mut self.completed,
            AgentProgressStatusV1::Failed => &mut self.failed,
            AgentProgressStatusV1::Interrupted => &mut self.interrupted,
            AgentProgressStatusV1::Archived => &mut self.archived,
            AgentProgressStatusV1::Deleted => &mut self.deleted,
        };
        *counter = counter.saturating_add(1);
    }
}

/// One SQLite-snapshot response for a caller's child cohort.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentGetProgressResultV1 {
    pub observed_at: DateTime<Utc>,
    pub cohort_size: u32,
    pub status_counts: AgentProgressStatusCountsV1,
    pub rows: Vec<AgentProgressRowV1>,
    /// Number of persisted terminal child sessions awaiting lead handling.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unhandled_terminal_children: u32,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
const fn is_zero(value: &u32) -> bool {
    *value == 0
}

/// Stable typed error data returned in JSON-RPC error `data`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentCoordinationErrorV1 {
    pub code: AgentCoordinationErrorCodeV1,
    pub cohort_size: u32,
    pub max_cohort_size: u32,
    pub next_action: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentCoordinationErrorCodeV1 {
    CohortTooLarge,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_coordination_requests_are_strict_and_bounded() {
        for field in [
            "caller_session_id",
            "owner_session_id",
            "parent_id",
            "epic_id",
            "lead_session_id",
            "epic_spawn_ordinal",
            "authority",
            "session_token",
        ] {
            let mut unknown = serde_json::json!({
                "kind": "Task",
                "query": "work",
                "idempotency_key": "k"
            });
            unknown
                .as_object_mut()
                .unwrap()
                .insert(field.into(), serde_json::json!(Uuid::new_v4()));
            assert!(
                serde_json::from_value::<AgentSpawnChildRequestV1>(unknown).is_err(),
                "spawn request accepted server-bound field {field}"
            );
        }

        let request: AgentSpawnChildRequestV1 = serde_json::from_value(serde_json::json!({
            "kind": "Task",
            "provider": "Claude",
            "query": "work",
            "idempotency_key": "k"
        }))
        .unwrap();
        assert_eq!(
            request.provider,
            Some(crate::types::SessionProvider::Claude)
        );
        assert_eq!(request.validate(), Ok(()));

        let legacy_request: AgentSpawnChildRequestV1 = serde_json::from_value(serde_json::json!({
            "kind": "Task",
            "query": "work",
            "idempotency_key": "legacy-k"
        }))
        .unwrap();
        assert_eq!(legacy_request.provider, None);
        assert_eq!(legacy_request.agent_role, None);
        assert!(
            serde_json::to_value(&legacy_request)
                .unwrap()
                .get("provider")
                .is_none(),
            "omitted providers must retain the pre-provider request JSON shape"
        );
        assert!(
            serde_json::to_value(&legacy_request)
                .unwrap()
                .get("agent_role")
                .is_none(),
            "omitted roles must retain the pre-role request JSON shape"
        );
        assert!(
            serde_json::from_value::<AgentGetProgressParamsV1>(
                serde_json::json!({ "caller": Uuid::new_v4() })
            )
            .is_err()
        );
    }

    #[test]
    fn agent_role_normalization_is_canonical_bounded_and_control_free() {
        let request = AgentSpawnChildRequestV1 {
            kind: crate::types::SessionKind::Research,
            provider: None,
            model: None,
            effort: None,
            agent_role: Some("  Principal   Researcher  ".into()),
            query: "work".into(),
            topology_node: None,
            iteration: None,
            tags: None,
            idempotency_key: "role-v1".into(),
        }
        .normalized()
        .unwrap();
        assert_eq!(request.agent_role.as_deref(), Some("Principal Researcher"));
        assert_eq!(normalize_agent_role(None), Ok(None));
        assert!(normalize_agent_role(Some(" \u{2003} ")).is_err());
        assert!(normalize_agent_role(Some("Reviewer\nLead")).is_err());
        assert!(normalize_agent_role(Some(&"x".repeat(AGENT_ROLE_MAX_BYTES + 1))).is_err());
        assert_eq!(
            normalize_agent_role(Some(&"é".repeat(AGENT_ROLE_MAX_BYTES / 2))),
            Ok(Some("é".repeat(AGENT_ROLE_MAX_BYTES / 2)))
        );
    }

    #[test]
    fn agent_coordination_spawn_result_snapshot_is_stable() {
        let result = AgentSpawnChildResultV1 {
            spawn_request_id: Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap(),
            child_session_id: Uuid::parse_str("00000000-0000-4000-8000-000000000002").unwrap(),
            epic_id: Uuid::parse_str("00000000-0000-4000-8000-000000000003").unwrap(),
            kind: crate::types::SessionKind::Task,
            agent_role: Some("Reviewer".into()),
            epic_spawn_ordinal: 7,
            state: AgentSpawnStateV1::Queued,
            deduplicated: false,
            safe_error_class: None,
        };
        assert_eq!(
            serde_json::to_string(&result).unwrap(),
            r#"{"spawn_request_id":"00000000-0000-4000-8000-000000000001","child_session_id":"00000000-0000-4000-8000-000000000002","epic_id":"00000000-0000-4000-8000-000000000003","kind":"Task","agent_role":"Reviewer","epic_spawn_ordinal":7,"state":"queued","deduplicated":false}"#
        );
    }

    #[test]
    fn successor_request_is_strict_bounded_and_identity_free() {
        let request: AgentReserveSuccessorRequestV1 = serde_json::from_value(serde_json::json!({
            "kind": "Task",
            "query": "continue the program",
            "idempotency_key": "turnover-1"
        }))
        .unwrap();
        assert_eq!(request.validate(), Ok(()));

        for field in [
            "caller_session_id",
            "predecessor_session_id",
            "epic_id",
            "owner_session_id",
            "parent_id",
            "lead_session_id",
            "expected_lead_generation",
            "candidate_session_id",
            "reservation_id",
            "model_invocation_id",
            "session_token",
        ] {
            let mut hostile = serde_json::json!({
                "kind": "Task",
                "query": "continue the program",
                "idempotency_key": "turnover-1"
            });
            hostile
                .as_object_mut()
                .unwrap()
                .insert(field.to_string(), serde_json::json!(Uuid::new_v4()));
            assert!(
                serde_json::from_value::<AgentReserveSuccessorRequestV1>(hostile).is_err(),
                "authority field {field} must be rejected"
            );
        }
    }

    #[test]
    fn successor_state_machine_is_forward_only() {
        use AgentSuccessorStateV1 as State;

        assert!(State::Reserved.may_transition_to(State::Launching));
        assert!(State::Reserved.may_transition_to(State::Failed));
        assert!(State::Launching.may_transition_to(State::Committed));
        assert!(State::Launching.may_transition_to(State::Failed));
        assert!(State::Launching.may_transition_to(State::Uncertain));
        assert!(State::Uncertain.may_transition_to(State::Committed));
        assert!(State::Uncertain.may_transition_to(State::Failed));
        assert!(!State::Committed.may_transition_to(State::Launching));
        assert!(!State::Failed.may_transition_to(State::Reserved));
        assert_eq!(State::from_str_exact("uncertain"), Some(State::Uncertain));
        assert_eq!(State::from_str_exact("UNcertain"), None);
    }

    #[test]
    fn agent_progress_base_commit_is_optional_and_serde_compatible() {
        let row = AgentProgressRowV1 {
            spawn_request_id: None,
            spawn_state: None,
            spawn_safe_error_class: None,
            base_commit: Some("known-object-id".into()),
            cursor: AgentProgressCursorV1 {
                session_id: Uuid::nil(),
                lineage_tip_id: Uuid::nil(),
                event_sequence: 0,
                custody_generation: None,
                last_event_at: None,
                status: AgentProgressStatusV1::Reserved,
                status_updated_at: Utc::now(),
            },
            freshness: AgentProgressFreshnessV1 {
                cursor_updated_at: Utc::now(),
                staleness_ms: 0,
            },
            watch_state: AgentWatchStateV1::Missing,
            messages: AgentMessageCountsV1::default(),
            message_queue: None,
            obligation: None,
        };
        let serialized = serde_json::to_value(&row).expect("serialize base-bearing row");
        assert_eq!(serialized["base_commit"], "known-object-id");

        let mut without_base = row.clone();
        without_base.base_commit = None;
        let absent = serde_json::to_value(&without_base).expect("serialize base-less row");
        assert!(absent.get("base_commit").is_none());
        assert!(absent.get("spawn_safe_error_class").is_none());

        for value in [absent.clone(), {
            let mut explicit_null = absent;
            explicit_null["base_commit"] = serde_json::Value::Null;
            explicit_null
        }] {
            assert_eq!(
                serde_json::from_value::<AgentProgressRowV1>(value)
                    .expect("old/null-compatible row")
                    .base_commit,
                None
            );
        }
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn agent_progress_obligation_is_optional_and_round_trips() {
        let mut row = AgentProgressRowV1 {
            spawn_request_id: None,
            spawn_state: None,
            spawn_safe_error_class: None,
            base_commit: None,
            cursor: AgentProgressCursorV1 {
                session_id: Uuid::nil(),
                lineage_tip_id: Uuid::nil(),
                event_sequence: 0,
                custody_generation: None,
                last_event_at: None,
                status: AgentProgressStatusV1::Failed,
                status_updated_at: Utc::now(),
            },
            freshness: AgentProgressFreshnessV1 {
                cursor_updated_at: Utc::now(),
                staleness_ms: 0,
            },
            watch_state: AgentWatchStateV1::Missing,
            messages: AgentMessageCountsV1::default(),
            message_queue: None,
            obligation: Some(AgentProgressObligationV1::HarvestFailed),
        };
        let encoded = serde_json::to_value(&row).expect("serialize obligation row");
        assert_eq!(encoded["obligation"], "harvest_failed");
        assert_eq!(
            serde_json::from_value::<AgentProgressRowV1>(encoded)
                .expect("deserialize obligation row"),
            row
        );

        row.obligation = None;
        let encoded = serde_json::to_value(&row).expect("serialize row without obligation");
        assert!(encoded.get("obligation").is_none());
        assert_eq!(
            serde_json::from_value::<AgentProgressRowV1>(encoded)
                .expect("deserialize old row")
                .obligation,
            None
        );
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn agent_progress_unhandled_count_is_optional_and_round_trips() {
        let result = AgentGetProgressResultV1 {
            observed_at: Utc::now(),
            cohort_size: 0,
            status_counts: AgentProgressStatusCountsV1::default(),
            rows: Vec::new(),
            unhandled_terminal_children: 2,
        };
        let encoded = serde_json::to_value(&result).expect("serialize progress result");
        assert_eq!(encoded["unhandled_terminal_children"], 2);
        assert_eq!(
            serde_json::from_value::<AgentGetProgressResultV1>(encoded)
                .expect("deserialize progress result"),
            result
        );

        let mut empty = result;
        empty.unhandled_terminal_children = 0;
        let encoded = serde_json::to_value(&empty).expect("serialize empty progress result");
        assert!(encoded.get("unhandled_terminal_children").is_none());
        assert_eq!(
            serde_json::from_value::<AgentGetProgressResultV1>(encoded)
                .expect("deserialize pre-field progress result")
                .unhandled_terminal_children,
            0
        );
    }

    /// The progress cursor's custody generation is optional on the wire in
    /// exactly the way the continuation cursor's is: absent when the tip has
    /// no custody, and byte-identical to a pre-field snapshot in that case.
    #[test]
    fn agent_progress_custody_generation_is_optional_and_serde_compatible() {
        let mut cursor = AgentProgressCursorV1 {
            session_id: Uuid::nil(),
            lineage_tip_id: Uuid::nil(),
            event_sequence: 4,
            custody_generation: None,
            last_event_at: None,
            status: AgentProgressStatusV1::Running,
            status_updated_at: Utc::now(),
        };

        let absent = serde_json::to_value(&cursor).expect("serialize custody-less cursor");
        assert!(
            absent.get("custody_generation").is_none(),
            "no custody must stay byte-identical to a pre-field snapshot"
        );

        // Both an omitted field and an explicit null decode to absence, so an
        // older peer's payload still round-trips.
        for value in [absent.clone(), {
            let mut explicit_null = absent;
            explicit_null["custody_generation"] = serde_json::Value::Null;
            explicit_null
        }] {
            assert_eq!(
                serde_json::from_value::<AgentProgressCursorV1>(value)
                    .expect("old/null-compatible cursor")
                    .custody_generation,
                None
            );
        }

        cursor.custody_generation = Some(3);
        let present = serde_json::to_value(&cursor).expect("serialize custody-bearing cursor");
        assert_eq!(present["custody_generation"], 3);
    }

    /// A progress cursor is a continuation cursor: the three staleness-fence components it
    /// publishes satisfy an `AgentContinueChild` request built straight from
    /// them, with no second round trip.
    #[test]
    fn agent_progress_cursor_components_satisfy_a_continue_request() {
        let tip = Uuid::from_u128(0x5eed);
        let cursor = AgentProgressCursorV1 {
            session_id: Uuid::from_u128(0xc0de),
            lineage_tip_id: tip,
            event_sequence: 9,
            custody_generation: Some(2),
            last_event_at: None,
            status: AgentProgressStatusV1::Running,
            status_updated_at: Utc::now(),
        };
        let request = AgentContinueChildRequestV1 {
            target_session_id: cursor.session_id,
            query: "resume the stage".into(),
            expected_tip_session_id: cursor.lineage_tip_id,
            expected_event_sequence: cursor.event_sequence,
            expected_custody_generation: cursor.custody_generation,
            idempotency_key: None,
        };
        request
            .validate()
            .expect("a progress-derived request is in bounds");

        let observed = AgentContinuationCursorV1 {
            tip_session_id: cursor.lineage_tip_id,
            event_sequence: cursor.event_sequence,
            custody_generation: cursor.custody_generation,
        };
        assert!(observed.satisfies(&request));
    }

    /// The published request schema marks `expected_custody_generation`
    /// optional (`default: null`), and the verb description calls it an
    /// optional staleness fence. A caller that omits it -- exactly what a
    /// schema-generated client does -- must therefore still satisfy a cursor
    /// whose tip and sequence match. Requiring it unconditionally refused
    /// every such caller with a false `stale_continuation`.
    #[test]
    fn continue_request_without_custody_generation_satisfies_a_matching_cursor() {
        let tip = Uuid::from_u128(0x5eed);
        let request = AgentContinueChildRequestV1 {
            target_session_id: Uuid::from_u128(0xc0de),
            query: "resume the stage".into(),
            expected_tip_session_id: tip,
            expected_event_sequence: 140,
            expected_custody_generation: None,
            idempotency_key: None,
        };
        let observed = AgentContinuationCursorV1 {
            tip_session_id: tip,
            event_sequence: 140,
            custody_generation: Some(1),
        };
        assert!(
            observed.satisfies(&request),
            "an omitted optional custody fence must not refuse a matching tip+sequence"
        );
    }

    /// The two always-required components still gate staleness on their own: a
    /// moved tip or an advanced sequence is stale even when custody is absent
    /// from the request.
    #[test]
    fn continue_request_without_custody_generation_still_detects_staleness() {
        let tip = Uuid::from_u128(0x5eed);
        let request = AgentContinueChildRequestV1 {
            target_session_id: Uuid::from_u128(0xc0de),
            query: "resume the stage".into(),
            expected_tip_session_id: tip,
            expected_event_sequence: 140,
            expected_custody_generation: None,
            idempotency_key: None,
        };
        let moved_tip = AgentContinuationCursorV1 {
            tip_session_id: Uuid::from_u128(0xbeef),
            event_sequence: 140,
            custody_generation: Some(1),
        };
        assert!(!moved_tip.satisfies(&request), "a moved tip is stale");
        let advanced = AgentContinuationCursorV1 {
            tip_session_id: tip,
            event_sequence: 141,
            custody_generation: Some(1),
        };
        assert!(
            !advanced.satisfies(&request),
            "an advanced sequence is stale"
        );
    }

    /// Supplying the fence keeps its strict meaning: it matches on full
    /// equality INCLUDING absence, so a caller that expected no custody must
    /// not silently continue a target that has since acquired a sandbox, and
    /// vice versa.
    #[test]
    fn explicit_custody_fence_matches_on_full_equality_including_absence() {
        let tip = Uuid::from_u128(0x5eed);
        let base = AgentContinueChildRequestV1 {
            target_session_id: Uuid::from_u128(0xc0de),
            query: "resume the stage".into(),
            expected_tip_session_id: tip,
            expected_event_sequence: 140,
            expected_custody_generation: Some(1),
            idempotency_key: None,
        };

        let custody_acquired = AgentContinuationCursorV1 {
            tip_session_id: tip,
            event_sequence: 140,
            custody_generation: Some(1),
        };
        assert!(custody_acquired.satisfies(&base));

        let custody_changed = AgentContinuationCursorV1 {
            tip_session_id: tip,
            event_sequence: 140,
            custody_generation: Some(2),
        };
        assert!(
            !custody_changed.satisfies(&base),
            "a reallocated custody generation is stale"
        );

        let expected_absent = AgentContinueChildRequestV1 {
            expected_custody_generation: None,
            ..base.clone()
        };
        let acquired = AgentContinuationCursorV1 {
            tip_session_id: tip,
            event_sequence: 140,
            custody_generation: Some(1),
        };
        assert!(
            acquired.satisfies(&expected_absent),
            "omission is permissive by design; only a SUPPLIED fence is strict"
        );
        let still_absent = AgentContinuationCursorV1 {
            tip_session_id: tip,
            event_sequence: 140,
            custody_generation: None,
        };
        assert!(
            !still_absent.satisfies(&base),
            "a supplied fence means the caller expected custody to be present"
        );
    }
}

// ---------------------------------------------------------------------------
// Phase 2 (V81) durable owner-to-child messaging contracts.
//
// Plan: thoughts/shared/plans/2026-08-03-issue-21-harness-acceleration.md
//       Phase 2, items P2-01..P2-07, correction contracts C-P2-01..C-P2-23.
//
// SQLite owns durable intent, CAS, evidence, and acknowledgement. Provider
// dispatch is an external effect and never happens inside a SQLite
// transaction. Nothing here claims exactly-once: an attempt that crossed a
// provider effect threshold without acknowledgement stays visibly uncertain.
// ---------------------------------------------------------------------------

/// Domain separator for every Phase 2 canonical digest (P2-01).
pub const AGENT_MESSAGE_DIGEST_DOMAIN: &str = "agent-message-v1";

/// Accepted acceptance-path bounds, preserved verbatim from Phase 1 (P2-01).
pub const AGENT_MESSAGE_MAX_PAYLOAD_BYTES: usize = 16 * 1024;
/// Maximum idempotency-key length in bytes.
pub const AGENT_MESSAGE_MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;
/// Maximum pending (queued/claimed) rows per logical target root.
pub const AGENT_MESSAGE_MAX_PENDING_PER_TARGET: u32 = 128;
/// Maximum pending (queued/claimed) rows per owner Session.
pub const AGENT_MESSAGE_MAX_PENDING_PER_OWNER: u32 = 512;
/// Frozen all-or-nothing target cap for permanent reserved-child failure
/// settlement (C-P2-18).
pub const AGENT_MESSAGE_SPAWN_SETTLEMENT_MAX_TARGETS: usize = 128;

/// Frozen per-attempt quarantine capacity (C-P2-10).
pub const AGENT_MESSAGE_QUARANTINE_MAX_EVENTS_PER_ATTEMPT: usize = 8;
/// Frozen per-attempt quarantine retained-byte ceiling.
pub const AGENT_MESSAGE_QUARANTINE_MAX_BYTES_PER_ATTEMPT: usize = 64 * 1024;
/// Frozen daemon-wide registered-attempt ceiling.
pub const AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS: usize = 64;
/// Frozen daemon-wide retained-event ceiling.
pub const AGENT_MESSAGE_QUARANTINE_MAX_EVENTS_GLOBAL: usize = 256;
/// Frozen daemon-wide retained-byte ceiling.
pub const AGENT_MESSAGE_QUARANTINE_MAX_BYTES_GLOBAL: usize = 2 * 1024 * 1024;
/// Quarantine timeout measured from an attempt's first quarantined event; the
/// effective deadline is the earlier of this and `claim_expires_at`.
pub const AGENT_MESSAGE_QUARANTINE_TIMEOUT_MS: u64 = 5_000;

/// Frozen AppServer raw-ingress ceilings, counted in raw bytes BEFORE UTF-8
/// decoding or JSON parsing (C-P2-12).
pub const APP_SERVER_UNCLASSIFIED_PREFIX_BYTES: usize = 8 * 1024;
/// Fixed scratch-chunk size used by the incremental frame reader.
pub const APP_SERVER_SCRATCH_CHUNK_BYTES: usize = 8 * 1024;
/// Raw ceiling for a correlated JSON-RPC response frame.
pub const APP_SERVER_MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// Raw ceiling for a provider-originated request frame.
pub const APP_SERVER_MAX_PROVIDER_REQUEST_BYTES: usize = 64 * 1024;
/// Raw ceiling for a notification frame.
pub const APP_SERVER_MAX_NOTIFICATION_BYTES: usize = 256 * 1024;
/// Raw ceiling for an ordinary (non-message-bound) event frame.
pub const APP_SERVER_MAX_ORDINARY_EVENT_BYTES: usize = 1024 * 1024;
/// Absolute per-frame discard budget before the provider is failed.
pub const APP_SERVER_ABSOLUTE_DISCARD_BUDGET_BYTES: usize = 8 * 1024 * 1024;
/// Maximum structural JSON nesting depth tracked by the classifier.
pub const APP_SERVER_MAX_JSON_DEPTH: usize = 64;
/// Maximum raw UTF-8 length of a JSON-RPC string ID.
pub const APP_SERVER_MAX_JSONRPC_STRING_ID_BYTES: usize = 256;

/// Bounded control-worker schedule (C-P2-15).
pub const APP_SERVER_CONTROL_TICK_MS: u64 = 100;
/// Maximum dirty latches processed per control tick.
pub const APP_SERVER_CONTROL_MAX_LATCHES_PER_TICK: usize = 32;
/// Maximum wall-clock milliseconds spent per control tick.
pub const APP_SERVER_CONTROL_MAX_MS_PER_TICK: u64 = 2;

/// Keyset-pagination bounds for every Phase 2 reconciliation scan (C-P2-19).
pub const AGENT_MESSAGE_RECONCILE_MAX_ROWS: usize = 64;
/// Maximum wall-clock milliseconds per reconciliation transaction.
pub const AGENT_MESSAGE_RECONCILE_MAX_MS: u64 = 10;

/// Frozen bounded-text byte ceilings for persisted Phase 2 scalars (C-P2-17).
pub const AGENT_MESSAGE_MAX_CANONICAL_JSONRPC_ID_BYTES: usize = 1_540;
/// Maximum persisted provider turn identifier length.
pub const AGENT_MESSAGE_MAX_PROVIDER_TURN_BYTES: usize = 512;
/// Maximum persisted JSON-RPC method length.
pub const AGENT_MESSAGE_MAX_METHOD_BYTES: usize = 128;
/// Maximum persisted status/enum/phase/authority length.
pub const AGENT_MESSAGE_MAX_ENUM_BYTES: usize = 64;
/// Maximum persisted safe error/error-class length.
pub const AGENT_MESSAGE_MAX_ERROR_CLASS_BYTES: usize = 128;
/// Maximum persisted capability/permit/join/writer/approval identifier length.
pub const AGENT_MESSAGE_MAX_CAPABILITY_ID_BYTES: usize = 36;

/// Closed aggregate message state vocabulary (P2-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageStateV1 {
    Queued,
    Claimed,
    Injected,
    Acknowledged,
    Uncertain,
    Failed,
    Expired,
}

impl AgentMessageStateV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Claimed => "claimed",
            Self::Injected => "injected",
            Self::Acknowledged => "acknowledged",
            Self::Uncertain => "uncertain",
            Self::Failed => "failed",
            Self::Expired => "expired",
        }
    }

    /// Parse the exact persisted spelling.
    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        Some(match value {
            "queued" => Self::Queued,
            "claimed" => Self::Claimed,
            "injected" => Self::Injected,
            "acknowledged" => Self::Acknowledged,
            "uncertain" => Self::Uncertain,
            "failed" => Self::Failed,
            "expired" => Self::Expired,
            _ => return None,
        })
    }

    /// Terminal states are immutable once recorded (P2-06).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Acknowledged | Self::Failed | Self::Expired)
    }

    /// Forward-transition matrix frozen by P2-06. `uncertain→acknowledged` is
    /// additionally restricted at the Store layer to the atomic exact
    /// late-correlation path while the current attempt is still
    /// `correlation_pending`; a sealed attempt cannot take that edge.
    #[must_use]
    pub const fn may_transition_to(self, next: Self) -> bool {
        match (self, next) {
            (Self::Queued, Self::Claimed | Self::Failed | Self::Expired)
            | (
                Self::Claimed,
                Self::Queued
                | Self::Injected
                | Self::Acknowledged
                | Self::Uncertain
                | Self::Failed
                | Self::Expired,
            )
            | (Self::Injected, Self::Acknowledged | Self::Uncertain)
            | (Self::Uncertain, Self::Acknowledged | Self::Failed) => true,
            _ => false,
        }
    }
}

/// Closed provider kinds that can carry a delivery attempt (P2-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryProviderKindV1 {
    Harness,
    CodexAppServer,
    ClaudeCli,
    CodexCli,
    AntigravityCli,
    LocalOpenAiCompatible,
}

impl BoundaryProviderKindV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Harness => "harness",
            Self::CodexAppServer => "codex_app_server",
            Self::ClaudeCli => "claude_cli",
            Self::CodexCli => "codex_cli",
            Self::AntigravityCli => "antigravity_cli",
            Self::LocalOpenAiCompatible => "local_openai_compatible",
        }
    }

    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        Some(match value {
            "harness" => Self::Harness,
            "codex_app_server" => Self::CodexAppServer,
            "claude_cli" => Self::ClaudeCli,
            "codex_cli" => Self::CodexCli,
            "antigravity_cli" => Self::AntigravityCli,
            "local_openai_compatible" => Self::LocalOpenAiCompatible,
            _ => return None,
        })
    }

    /// The frozen provider matrix in P2-05: only CodexAppServer is a genuine
    /// native multi-turn boundary; every other provider delivers exactly one
    /// terminal turn per durable model invocation.
    #[must_use]
    pub const fn capability_kind(self) -> BoundaryCapabilityKindV1 {
        match self {
            Self::CodexAppServer => BoundaryCapabilityKindV1::NativeMultiTurn,
            _ => BoundaryCapabilityKindV1::TerminalOneTurn,
        }
    }

    /// The delivery boundary a live Session's configured provider presents.
    ///
    /// Total and exhaustive by construction: adding a `SessionProvider` variant
    /// fails this match to compile rather than silently defaulting a new
    /// provider onto some existing effect-threshold row of the P2-05 matrix.
    /// That matters because the boundary kind chosen here decides which
    /// `rejected_before_effect` proof the dispatcher is later allowed to accept
    /// — a wrong default would let an unproven rejection requeue a message the
    /// provider may already have acted on.
    #[must_use]
    pub const fn from_session_provider(provider: crate::types::SessionProvider) -> Self {
        use crate::types::SessionProvider;
        match provider {
            SessionProvider::Claude => Self::ClaudeCli,
            SessionProvider::Codex
            | SessionProvider::Pioneer
            | SessionProvider::OpenRouter
            | SessionProvider::Bedrock => Self::CodexCli,
            SessionProvider::Local => Self::LocalOpenAiCompatible,
            SessionProvider::Antigravity => Self::AntigravityCli,
            SessionProvider::CodexAppServer => Self::CodexAppServer,
            SessionProvider::Harness => Self::Harness,
        }
    }
}

/// Closed capability kinds (P2-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryCapabilityKindV1 {
    NativeMultiTurn,
    TerminalOneTurn,
}

impl BoundaryCapabilityKindV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NativeMultiTurn => "native_multi_turn",
            Self::TerminalOneTurn => "terminal_one_turn",
        }
    }

    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        Some(match value {
            "native_multi_turn" => Self::NativeMultiTurn,
            "terminal_one_turn" => Self::TerminalOneTurn,
            _ => return None,
        })
    }

    /// The boundary identity a capability kind persists (P2-02).
    #[must_use]
    pub const fn boundary_kind(self) -> BoundaryKindV1 {
        match self {
            Self::NativeMultiTurn => BoundaryKindV1::NativeTurn,
            Self::TerminalOneTurn => BoundaryKindV1::ModelInvocation,
        }
    }
}

/// Closed boundary identity kinds (P2-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryKindV1 {
    NativeTurn,
    ModelInvocation,
}

impl BoundaryKindV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NativeTurn => "native_turn",
            Self::ModelInvocation => "model_invocation",
        }
    }

    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        Some(match value {
            "native_turn" => Self::NativeTurn,
            "model_invocation" => Self::ModelInvocation,
            _ => return None,
        })
    }
}

/// Closed admission classification (C-P2-05).
///
/// `RejectedBeforeEffect` requires durable, provider-specific proof that no
/// model request or process carrying this message crossed the effect
/// threshold. A generic `Err`, lease expiry, missing output, process death, or
/// timeout is NEVER that proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryClassificationV1 {
    RejectedBeforeEffect,
    AdmittedEffectPossible,
    Unsupported,
}

impl BoundaryClassificationV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RejectedBeforeEffect => "rejected_before_effect",
            Self::AdmittedEffectPossible => "admitted_effect_possible",
            Self::Unsupported => "unsupported",
        }
    }

    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        Some(match value {
            "rejected_before_effect" => Self::RejectedBeforeEffect,
            "admitted_effect_possible" => Self::AdmittedEffectPossible,
            "unsupported" => Self::Unsupported,
            _ => return None,
        })
    }
}

/// Closed effect classification recorded on an attempt (P2-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClassificationV1 {
    ProvedNoEffect,
    EffectPossible,
    EffectAcknowledged,
}

impl EffectClassificationV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProvedNoEffect => "proved_no_effect",
            Self::EffectPossible => "effect_possible",
            Self::EffectAcknowledged => "effect_acknowledged",
        }
    }

    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        Some(match value {
            "proved_no_effect" => Self::ProvedNoEffect,
            "effect_possible" => Self::EffectPossible,
            "effect_acknowledged" => Self::EffectAcknowledged,
            _ => return None,
        })
    }
}

/// Closed durable attempt lifecycle state (P2-02).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStateV1 {
    Claimed,
    /// Durable PRE-DISPATCH marker (H21-P2-R5-001 option (i)).
    ///
    /// Committed in its own short transaction that closes strictly BEFORE the
    /// provider send, and therefore strictly before any external effect is
    /// possible. It exists to split one durably-ambiguous window into two
    /// distinguishable ones for P2-06 crash recovery:
    ///
    /// - `Claimed` + foreign `delivery_boot_id` + no recorded admission
    ///   ⇒ the dead incarnation never reached the send ⇒ **proved no effect**.
    /// - `Dispatching` + foreign `delivery_boot_id` + no recorded admission
    ///   ⇒ the send may have completed and simply never been recorded
    ///   ⇒ **uncertain**, and MUST NOT be requeued.
    ///
    /// Before this state existed both cases left the identical durable triple,
    /// which is why the crash-window requeue licence was withdrawn as unsound.
    /// The guarantee is an ORDERING property of the delivery path, not of this
    /// enum: it holds only because the marker commits before the send and the
    /// send is refused if the marker cannot be made durable.
    Dispatching,
    /// Evidence that the boundary was crossed and an effect is possible.
    ///
    /// There is deliberately **no** state between [`Self::Dispatching`] and
    /// this one. A prior `AdmissionRecorded` variant existed with no writer
    /// anywhere — production or test — and was removed in P2-06a rather than
    /// given one: `record_agent_message_admission` fills the admission
    /// classification and the effect classification in a SINGLE statement, so
    /// there is no durable instant where admission is recorded but effect is
    /// not yet classified. Splitting that statement to manufacture one would
    /// open a new crash window, which is the opposite of what this phase is
    /// for. The variant's real cost was that it read as "there is a state
    /// between dispatching and effect_possible, so recovery must handle it" —
    /// false, and actively misleading to the recovery classifier in
    /// `list_crashed_agent_message_attempts_page_v1`.
    EffectPossible,
    Terminal,
}

impl AttemptStateV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claimed => "claimed",
            Self::Dispatching => "dispatching",
            Self::EffectPossible => "effect_possible",
            Self::Terminal => "terminal",
        }
    }

    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        Some(match value {
            "claimed" => Self::Claimed,
            "dispatching" => Self::Dispatching,
            "effect_possible" => Self::EffectPossible,
            "terminal" => Self::Terminal,
            _ => return None,
        })
    }
}

/// Closed immutable terminal dispositions for one delivery attempt (P2-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptTerminalDispositionV1 {
    ProvedNoEffectRequeue,
    ProvedNoEffectFailed,
    ProvedNoEffectExpired,
    Unsupported,
    Acknowledged,
    SettledFailed,
}

impl AttemptTerminalDispositionV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProvedNoEffectRequeue => "proved_no_effect_requeue",
            Self::ProvedNoEffectFailed => "proved_no_effect_failed",
            Self::ProvedNoEffectExpired => "proved_no_effect_expired",
            Self::Unsupported => "unsupported",
            Self::Acknowledged => "acknowledged",
            Self::SettledFailed => "settled_failed",
        }
    }

    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        Some(match value {
            "proved_no_effect_requeue" => Self::ProvedNoEffectRequeue,
            "proved_no_effect_failed" => Self::ProvedNoEffectFailed,
            "proved_no_effect_expired" => Self::ProvedNoEffectExpired,
            "unsupported" => Self::Unsupported,
            "acknowledged" => Self::Acknowledged,
            "settled_failed" => Self::SettledFailed,
            _ => return None,
        })
    }

    /// A sealed no-effect disposition is the only one that may remain the
    /// aggregate's historical current-attempt pointer while the message is
    /// requeued, queued-expired, or queued-launch-failed (C-P2-09).
    #[must_use]
    pub const fn is_proved_no_effect(self) -> bool {
        matches!(
            self,
            Self::ProvedNoEffectRequeue | Self::ProvedNoEffectFailed | Self::ProvedNoEffectExpired
        )
    }
}

/// Closed AppServer correlation custody (C-P2-13).
///
/// `CorrelationPending` is the ONLY class that permits a single exact late
/// real-turn fill plus atomic acknowledgement. `SealedLiveUncertain` is
/// irreversible and permits only lifecycle settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CorrelationStateV1 {
    NotApplicable,
    CorrelationPending,
    Correlated,
    SealedLiveUncertain,
}

impl CorrelationStateV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotApplicable => "not_applicable",
            Self::CorrelationPending => "correlation_pending",
            Self::Correlated => "correlated",
            Self::SealedLiveUncertain => "sealed_live_uncertain",
        }
    }

    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        Some(match value {
            "not_applicable" => Self::NotApplicable,
            "correlation_pending" => Self::CorrelationPending,
            "correlated" => Self::Correlated,
            "sealed_live_uncertain" => Self::SealedLiveUncertain,
            _ => return None,
        })
    }

    /// Only an unsealed `correlation_pending` attempt may take the
    /// `uncertain→acknowledged` late-correlation edge (P2-06).
    #[must_use]
    pub const fn permits_late_acknowledgement(self) -> bool {
        matches!(self, Self::CorrelationPending)
    }
}

/// Closed authorities that may author an aggregate state transition (P2-02).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionAuthorityKindV1 {
    Acceptance,
    Migration,
    Dispatcher,
    SpawnSettlement,
    ExpiryReconciler,
    AcknowledgementStore,
    OperatorSettlement,
}

impl TransitionAuthorityKindV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Acceptance => "acceptance",
            Self::Migration => "migration",
            Self::Dispatcher => "dispatcher",
            Self::SpawnSettlement => "spawn_settlement",
            Self::ExpiryReconciler => "expiry_reconciler",
            Self::AcknowledgementStore => "acknowledgement_store",
            Self::OperatorSettlement => "operator_settlement",
        }
    }

    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        Some(match value {
            "acceptance" => Self::Acceptance,
            "migration" => Self::Migration,
            "dispatcher" => Self::Dispatcher,
            "spawn_settlement" => Self::SpawnSettlement,
            "expiry_reconciler" => Self::ExpiryReconciler,
            "acknowledgement_store" => Self::AcknowledgementStore,
            "operator_settlement" => Self::OperatorSettlement,
            _ => return None,
        })
    }
}

/// Closed provider-request handler kinds (P2-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderRequestHandlerKindV1 {
    ToolExecution,
    ApprovalPresentation,
}

impl ProviderRequestHandlerKindV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToolExecution => "tool_execution",
            Self::ApprovalPresentation => "approval_presentation",
        }
    }

    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        Some(match value {
            "tool_execution" => Self::ToolExecution,
            "approval_presentation" => Self::ApprovalPresentation,
            _ => return None,
        })
    }
}

/// Closed handler phases. A tool execution and its JSON-RPC result are
/// explicitly TWO effects, never an implicit composite (C-P2-11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderRequestHandlerPhaseV1 {
    EvidenceCommitted,
    HandlerAuthorized,
    HandlerStarted,
    HandlerCompleted,
    PendingDecision,
    DecisionRecorded,
    HandlerUncertain,
}

impl ProviderRequestHandlerPhaseV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EvidenceCommitted => "evidence_committed",
            Self::HandlerAuthorized => "handler_authorized",
            Self::HandlerStarted => "handler_started",
            Self::HandlerCompleted => "handler_completed",
            Self::PendingDecision => "pending_decision",
            Self::DecisionRecorded => "decision_recorded",
            Self::HandlerUncertain => "handler_uncertain",
        }
    }

    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        Some(match value {
            "evidence_committed" => Self::EvidenceCommitted,
            "handler_authorized" => Self::HandlerAuthorized,
            "handler_started" => Self::HandlerStarted,
            "handler_completed" => Self::HandlerCompleted,
            "pending_decision" => Self::PendingDecision,
            "decision_recorded" => Self::DecisionRecorded,
            "handler_uncertain" => Self::HandlerUncertain,
            _ => return None,
        })
    }

    /// Frozen handler progression (P2-02): evidence→authorized→started→
    /// completed, approval-only completed→pending-decision→decision-recorded,
    /// or started→uncertain. A started phase is NEVER reset or retried.
    #[must_use]
    pub const fn may_advance_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::EvidenceCommitted, Self::HandlerAuthorized)
                | (Self::HandlerAuthorized, Self::HandlerStarted)
                | (
                    Self::HandlerStarted,
                    Self::HandlerCompleted | Self::HandlerUncertain
                )
                | (Self::HandlerCompleted, Self::PendingDecision)
                | (Self::PendingDecision, Self::DecisionRecorded)
        )
    }

    /// A live fenced approval awaiting or holding an operator decision. The
    /// fail-closed legacy-branch guard in `answer_question` keys on this
    /// (C-P2-16).
    #[must_use]
    pub const fn is_live_approval_phase(self) -> bool {
        matches!(self, Self::PendingDecision)
    }
}

/// Closed reply phases (P2-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderRequestReplyPhaseV1 {
    NotAuthorized,
    ReplyAuthorized,
    ReplyStarted,
    ReplyCompleted,
    ReplyUncertain,
}

impl ProviderRequestReplyPhaseV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotAuthorized => "not_authorized",
            Self::ReplyAuthorized => "reply_authorized",
            Self::ReplyStarted => "reply_started",
            Self::ReplyCompleted => "reply_completed",
            Self::ReplyUncertain => "reply_uncertain",
        }
    }

    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        Some(match value {
            "not_authorized" => Self::NotAuthorized,
            "reply_authorized" => Self::ReplyAuthorized,
            "reply_started" => Self::ReplyStarted,
            "reply_completed" => Self::ReplyCompleted,
            "reply_uncertain" => Self::ReplyUncertain,
            _ => return None,
        })
    }

    /// Frozen reply progression (P2-02): not-authorized→authorized→started→
    /// completed, or started→uncertain.
    #[must_use]
    pub const fn may_advance_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::NotAuthorized, Self::ReplyAuthorized)
                | (Self::ReplyAuthorized, Self::ReplyStarted)
                | (
                    Self::ReplyStarted,
                    Self::ReplyCompleted | Self::ReplyUncertain
                )
        )
    }
}

/// Closed terminal disposition of one provider-request ledger row (P2-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderRequestDispositionV1 {
    Live,
    Completed,
    ProviderTerminalCancelled,
}

impl ProviderRequestDispositionV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Completed => "completed",
            Self::ProviderTerminalCancelled => "provider_terminal_cancelled",
        }
    }

    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        Some(match value {
            "live" => Self::Live,
            "completed" => Self::Completed,
            "provider_terminal_cancelled" => Self::ProviderTerminalCancelled,
            _ => return None,
        })
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::ProviderTerminalCancelled)
    }
}

/// Closed exact-turn gate states (C-P2-14). Forward only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnGateStateV1 {
    Open,
    Closing,
    Closed,
}

impl TurnGateStateV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closing => "closing",
            Self::Closed => "closed",
        }
    }

    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        Some(match value {
            "open" => Self::Open,
            "closing" => Self::Closing,
            "closed" => Self::Closed,
            _ => return None,
        })
    }

    /// Only an `open` gate admits a new request row, capability mint, or
    /// effect start (C-P2-14).
    #[must_use]
    pub const fn admits_new_effect(self) -> bool {
        matches!(self, Self::Open)
    }
}

/// Closed started-effect permit kinds (C-P2-14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectPermitKindV1 {
    ToolExecution,
    ApprovalPresentation,
    ProviderReply,
}

impl EffectPermitKindV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ToolExecution => "tool_execution",
            Self::ApprovalPresentation => "approval_presentation",
            Self::ProviderReply => "provider_reply",
        }
    }

    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        Some(match value {
            "tool_execution" => Self::ToolExecution,
            "approval_presentation" => Self::ApprovalPresentation,
            "provider_reply" => Self::ProviderReply,
            _ => return None,
        })
    }
}

/// Closed started-effect permit states (C-P2-14).
///
/// A cancellation REQUEST alone never means the effect was undone and never
/// settles a permit; only `CancelConfirmed` carries that immutable evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectPermitStateV1 {
    Issued,
    Completed,
    UncertainUnjoined,
    UncertainJoined,
    CancelConfirmed,
}

impl EffectPermitStateV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Issued => "issued",
            Self::Completed => "completed",
            Self::UncertainUnjoined => "uncertain_unjoined",
            Self::UncertainJoined => "uncertain_joined",
            Self::CancelConfirmed => "cancel_confirmed",
        }
    }

    #[must_use]
    pub fn from_str_exact(value: &str) -> Option<Self> {
        Some(match value {
            "issued" => Self::Issued,
            "completed" => Self::Completed,
            "uncertain_unjoined" => Self::UncertainUnjoined,
            "uncertain_joined" => Self::UncertainJoined,
            "cancel_confirmed" => Self::CancelConfirmed,
            _ => return None,
        })
    }

    /// A gate may only advance `closing→closed` when every issued permit is
    /// resolved. `UncertainUnjoined` deliberately does NOT resolve: it leaves
    /// the gate visibly closing with retained custody (C-P2-14/C-P2-20).
    #[must_use]
    pub const fn is_resolved(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::UncertainJoined | Self::CancelConfirmed
        )
    }
}

/// Immutable per-attempt fence carried through every provider launch/call
/// context (C-P2-07). It binds message, attempt, claim token, daemon boot,
/// delivery Session/generation, and the already admitted model invocation, so
/// no fallback controller can admit a second invocation for the same attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageAttemptFenceV1 {
    pub message_id: Uuid,
    pub attempt_number: u32,
    pub claim_token: Uuid,
    pub delivery_boot_id: Uuid,
    pub delivery_session_id: Uuid,
    pub delivery_session_generation: i64,
    pub delivery_model_invocation_id: Uuid,
}

/// Closed provider-boundary admission result (C-P2-05).
///
/// `native_turn_id` is populated ONLY from a genuine provider response or
/// notification. A thread ID, process ID, transcript token, timestamp, message
/// ID, or `continuation:*` value is never a turn ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryAdmissionV1 {
    pub provider_kind: BoundaryProviderKindV1,
    pub capability_kind: BoundaryCapabilityKindV1,
    pub delivery_session_id: Uuid,
    pub session_generation: i64,
    pub model_invocation_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_turn_id: Option<String>,
    pub classification: BoundaryClassificationV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_error_class: Option<String>,
}

impl BoundaryAdmissionV1 {
    /// Validate the closed coherence rules frozen by C-P2-05 and P2-02.
    ///
    /// # Errors
    ///
    /// Returns a stable safe error class when the admission contradicts the
    /// frozen provider matrix.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.provider_kind.capability_kind() != self.capability_kind {
            return Err("agent_message_boundary_capability_kind_mismatch");
        }
        if self.capability_kind == BoundaryCapabilityKindV1::TerminalOneTurn
            && self.native_turn_id.is_some()
        {
            return Err("agent_message_terminal_provider_must_have_null_native_turn");
        }
        if self.classification != BoundaryClassificationV1::AdmittedEffectPossible
            && self.native_turn_id.is_some()
        {
            return Err("agent_message_native_turn_requires_admitted_effect_possible");
        }
        if let Some(turn) = self.native_turn_id.as_deref() {
            if turn.is_empty() || turn.len() > AGENT_MESSAGE_MAX_PROVIDER_TURN_BYTES {
                return Err("agent_message_native_turn_id_out_of_bounds");
            }
            if turn.starts_with("continuation:") {
                return Err("agent_message_continuation_token_is_not_a_native_turn_id");
            }
        }
        if let Some(class) = self.provider_error_class.as_deref() {
            if class.is_empty() || class.len() > AGENT_MESSAGE_MAX_ERROR_CLASS_BYTES {
                return Err("agent_message_provider_error_class_out_of_bounds");
            }
        }
        if self.session_generation < 0 {
            return Err("agent_message_session_generation_must_be_nonnegative");
        }
        Ok(())
    }

    /// The durable boundary value persisted on the attempt row (P2-02): the
    /// canonical model-invocation UUID for a `model_invocation` boundary, or
    /// the genuine provider turn ID for a `native_turn` boundary — which stays
    /// null until a real response/notification supplies it.
    #[must_use]
    pub fn boundary_value(&self) -> Option<String> {
        match self.capability_kind.boundary_kind() {
            BoundaryKindV1::ModelInvocation => Some(self.model_invocation_id.to_string()),
            BoundaryKindV1::NativeTurn => self.native_turn_id.clone(),
        }
    }
}

/// Strict v1 request for `AgentSendMessage` (P2-03).
///
/// Sender identity and Epic scope are token-resolved server-side; a
/// caller-supplied sender field is a hard deserialization failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSendMessageRequestV1 {
    pub target_session_id: Uuid,
    pub message: String,
    pub idempotency_key: String,
    /// Omit or send null for a deadline 30 minutes after first acceptance.
    /// An explicit RFC3339 deadline is preserved without a policy cap.
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

impl AgentSendMessageRequestV1 {
    /// Validate transport-independent request bounds.
    ///
    /// # Errors
    ///
    /// Returns a stable safe error class when the payload, idempotency key, or
    /// target exceeds the v1 contract bounds.
    pub fn validate(&self) -> Result<(), &'static str> {
        let key = self.idempotency_key.as_bytes();
        if key.is_empty() || key.len() > AGENT_MESSAGE_MAX_IDEMPOTENCY_KEY_BYTES || key.contains(&0)
        {
            return Err("agent_message_idempotency_key_must_be_1_to_128_bytes_without_nul");
        }
        let payload = self.message.as_bytes();
        if payload.is_empty() || payload.len() > AGENT_MESSAGE_MAX_PAYLOAD_BYTES {
            return Err("agent_message_payload_must_be_1_to_16384_bytes");
        }
        if self.target_session_id.is_nil() {
            return Err("agent_message_target_session_id_must_not_be_nil");
        }
        Ok(())
    }
}

/// Deterministic acceptance receipt returned by every exact replay (P2-03).
///
/// `target_session_id` is the IMMUTABLE logical/reserved root. The concrete
/// delivery tip belongs to one attempt and is never a public target.
/// `state=queued` proves acceptance only, not delivery to the provider. The
/// returned deadline is the original persisted deadline, including the
/// 30-minute default when the request omitted `expires_at`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSendMessageResultV1 {
    pub message_id: Uuid,
    pub target_session_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_spawn_request_id: Option<Uuid>,
    pub state: AgentMessageStateV1,
    pub state_version: i64,
    pub deduplicated: bool,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

/// Cursor-bearing durable message-state event, published ONLY after the Store
/// transaction commits (P2-03). Payloads are never exposed through progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessageStateEventV1 {
    pub message_id: Uuid,
    pub owner_session_id: Uuid,
    pub target_session_id: Uuid,
    pub state: AgentMessageStateV1,
    pub state_version: i64,
    pub attempt_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_attempt_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safe_error_class: Option<String>,
    pub observed_at: DateTime<Utc>,
}

/// Typed error codes for the Phase 2 messaging surface (P2-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageErrorCodeV1 {
    TargetNotAuthorized,
    IdempotencyConflict,
    TargetQueueFull,
    OwnerQueueFull,
    PayloadTooLarge,
    TargetUnknown,
    ProviderUnsupported,
    TargetTerminal,
}

impl AgentMessageErrorCodeV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TargetNotAuthorized => "agent_message_target_not_authorized",
            Self::IdempotencyConflict => "agent_message_idempotency_conflict",
            Self::TargetQueueFull => "agent_message_target_queue_full",
            Self::OwnerQueueFull => "agent_message_owner_queue_full",
            Self::PayloadTooLarge => "agent_message_payload_too_large",
            Self::TargetUnknown => "agent_message_target_unknown",
            Self::ProviderUnsupported => "agent_message_provider_unsupported",
            Self::TargetTerminal => "agent_message_target_terminal",
        }
    }
}

/// Stable typed error data returned in JSON-RPC error `data` (P2-01).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMessageErrorV1 {
    pub code: AgentMessageErrorCodeV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    pub next_action: String,
}

// ---------------------------------------------------------------------------
// `AgentContinueChild` (orchestration slice 1)
// ---------------------------------------------------------------------------

/// Maximum continuation prompt accepted by `AgentContinueChild`.
///
/// Deliberately the SPAWN query bound, not the 16 KiB mailbox bound. A
/// continuation carries a full stage prompt exactly like the child's original
/// `query`; clamping it to [`AGENT_MESSAGE_MAX_PAYLOAD_BYTES`] would leave the
/// recovery verb unable to redeliver the very prompt that launched the child.
pub const AGENT_CONTINUE_MAX_QUERY_BYTES: usize = 256 * 1024;

/// Stable typed error classes returned in JSON-RPC error `data` for
/// `AgentContinueChild`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentContinueErrorCodeV1 {
    /// The strict request failed bounded validation before authority was read.
    InvalidRequest,
    /// Caller may not target this session (not a direct child, not a child of
    /// an Epic it leads).
    TargetNotAuthorized,
    /// No `sessions` row for the named target.
    TargetUnknown,
    /// The caller named itself. Continuation is delegation to a child; a
    /// self-continue is an unowned self-injection loop.
    SelfContinuationDenied,
    /// The observed continuation cursor no longer matches the expected one:
    /// the child rotated, produced new events, or changed custody generation
    /// since the caller last observed it.
    StaleContinuation,
    /// The target's provider cannot be continued under its own session id.
    ProviderUnsupported,
    ResumeUnavailableTaskUnresolved,
    IdempotencyConflict,
    RelaunchAbandoned,
    RelaunchInProgress,
    /// Authority and staleness both passed, but the underlying
    /// `continue_session` call failed.
    ContinuationFailed,
}

impl AgentContinueErrorCodeV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "agent_continue_invalid_request",
            Self::TargetNotAuthorized => "agent_continue_target_not_authorized",
            Self::TargetUnknown => "agent_continue_target_unknown",
            Self::SelfContinuationDenied => "agent_continue_self_denied",
            Self::StaleContinuation => "agent_continue_stale_cursor",
            Self::ProviderUnsupported => "agent_continue_provider_unsupported",
            Self::ResumeUnavailableTaskUnresolved => "resume_unavailable_task_unresolved",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::RelaunchAbandoned => "relaunch_abandoned",
            Self::RelaunchInProgress => "relaunch_in_progress",
            Self::ContinuationFailed => "agent_continue_failed",
        }
    }
}

/// The optimistic staleness cursor checked by `AgentContinueChild`.
///
/// `Session` carries no `row_version`, so continuation cannot reuse the Issue
/// verbs' `expected_row_version` discipline. These three components are the
/// durable facts that DO move whenever a child advances, and they are already
/// co-computed by the `AgentGetProgress` snapshot path:
///
/// * `tip_session_id` — the resolved lineage tip. Moves on rotation.
/// * `event_sequence` — `MAX(sequence)` of the tip's conversation events.
///   Moves on every persisted turn, including the one a prior continuation
///   delivered, which is what makes an accidental replay fail closed.
/// * `custody_generation` — the sandbox custody generation, when the target
///   has one. Moves when custody is reallocated under the same session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentContinuationCursorV1 {
    pub tip_session_id: Uuid,
    pub event_sequence: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custody_generation: Option<i64>,
}

impl AgentContinuationCursorV1 {
    /// True when this observed cursor exactly satisfies `request`'s expected
    /// tuple.
    ///
    /// `tip_session_id` and `event_sequence` are always required. They are the
    /// two components that advance when work happens, so they are what makes a
    /// decision stale.
    ///
    /// `custody_generation` is compared only when the caller supplied it.
    /// The field is `Option` and documented as an optional fence, and the
    /// published request schema marks it optional with a `null` default, so a
    /// schema-generated client that omits it must not be refused. Requiring it
    /// unconditionally meant such a client could NEVER continue a sandboxed
    /// target, and the failure surfaced as a false `stale_continuation`
    /// carrying back the very cursor the caller had just sent.
    ///
    /// When the caller DOES supply it, equality still includes absence: a
    /// caller that expected no custody must not silently continue a target
    /// that has since acquired a sandbox, and vice versa.
    #[must_use]
    pub fn satisfies(&self, request: &AgentContinueChildRequestV1) -> bool {
        self.tip_session_id == request.expected_tip_session_id
            && self.event_sequence == request.expected_event_sequence
            && request
                .expected_custody_generation
                .is_none_or(|expected| self.custody_generation == Some(expected))
    }
}

/// Strict v1 request for `AgentContinueChild`.
///
/// Caller identity is token-resolved server-side and is deliberately absent.
/// The optional key identifies a durable fresh-relaunch decision when resume
/// is unavailable. Resume-available continuations retain their cursor contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentContinueChildRequestV1 {
    pub target_session_id: Uuid,
    pub query: String,
    pub expected_tip_session_id: Uuid,
    pub expected_event_sequence: i64,
    #[serde(default)]
    pub expected_custody_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

impl AgentContinueChildRequestV1 {
    /// Validate transport-independent request bounds.
    ///
    /// # Errors
    ///
    /// Returns a stable safe error class when the query or either identity
    /// exceeds the v1 contract bounds.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.target_session_id.is_nil() {
            return Err("agent_continue_target_session_id_must_not_be_nil");
        }
        if self.expected_tip_session_id.is_nil() {
            return Err("agent_continue_expected_tip_session_id_must_not_be_nil");
        }
        let query = self.query.as_bytes();
        if query.is_empty() || query.len() > AGENT_CONTINUE_MAX_QUERY_BYTES {
            return Err("agent_continue_query_must_be_1_to_262144_bytes");
        }
        if self.expected_event_sequence < 0 {
            return Err("agent_continue_expected_event_sequence_must_not_be_negative");
        }
        if self
            .expected_custody_generation
            .is_some_and(|generation| generation <= 0)
        {
            return Err("agent_continue_expected_custody_generation_must_be_positive");
        }
        if self
            .idempotency_key
            .as_ref()
            .is_some_and(|key| key.is_empty() || key.len() > 128 || key.as_bytes().contains(&0))
        {
            return Err("agent_continue_idempotency_key_must_be_1_to_128_bytes_without_nul");
        }
        Ok(())
    }
}

/// Deterministic receipt for an accepted continuation.
///
/// `target_session_id` echoes the IMMUTABLE logical root the caller named;
/// `continued` names the concrete row the daemon actually continued, which is
/// the lineage tip. `observed` is the pre-continuation cursor that admitted the
/// request, not a post-continuation cursor; do not reuse it for another request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentContinueChildResultV1 {
    pub target_session_id: Uuid,
    pub continued_session_id: Uuid,
    pub observed: AgentContinuationCursorV1,
    pub watch_rearmed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relaunch: Option<AgentContinueRelaunchV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentContinueRelaunchV1 {
    pub mode: String,
    pub reason: String,
    pub request_id: Uuid,
    pub task_source_session_id: Uuid,
    pub invocation_id: Uuid,
    pub deduplicated: bool,
    pub recovered: bool,
}

/// Stable typed error data returned in JSON-RPC error `data`.
///
/// `observed` is present on `StaleContinuation` ONLY, and is the version
/// witness the caller must adopt before retrying. It carries no payload,
/// no topology, and no identity beyond the tip the caller is already
/// authorized on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentContinueErrorV1 {
    pub code: AgentContinueErrorCodeV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<AgentContinuationCursorV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<serde_json::Value>,
    pub next_action: String,
}

// ---------------------------------------------------------------------------
// `AgentArchiveChild` (#670 R2 design (b))
// ---------------------------------------------------------------------------

/// Stable typed error classes returned in JSON-RPC error `data` for
/// `AgentArchiveChild`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentArchiveErrorCodeV1 {
    /// The strict request failed bounded validation before authority was read.
    InvalidRequest,
    /// The caller named itself.
    SelfArchiveDenied,
    /// No `sessions` row for the named target.
    TargetUnknown,
    /// The caller is not the current persisted lead of the Epic that owns the
    /// child and its lineage tip.
    TargetNotAuthorized,
    /// The observed cursor no longer matches the expected one.
    StaleArchive,
    TargetNotTerminal,
    TargetNotLeaf,
    /// The target is recorded as a container lead.
    TargetIsLead,
    /// A human, operator or recovery owner still holds the target.
    RecoveryOwnerHeld,
    /// A continuation of the target is live or pending.
    LiveContinuation,
    /// The target is the author of review work that is not yet verdicted.
    ReviewSourceSealed,
    /// Every check passed, but the archive transaction failed.
    ArchiveFailed,
}

impl AgentArchiveErrorCodeV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "agent_archive_invalid_request",
            Self::SelfArchiveDenied => "agent_archive_self_denied",
            Self::TargetUnknown => "agent_archive_target_unknown",
            Self::TargetNotAuthorized => "agent_archive_target_not_authorized",
            Self::StaleArchive => "agent_archive_stale_cursor",
            Self::TargetNotTerminal => "agent_archive_target_not_terminal",
            Self::TargetNotLeaf => "agent_archive_target_not_leaf",
            Self::TargetIsLead => "agent_archive_target_is_lead",
            Self::RecoveryOwnerHeld => "agent_archive_recovery_owner_held",
            Self::LiveContinuation => "agent_archive_live_continuation",
            Self::ReviewSourceSealed => "agent_archive_review_source_sealed",
            Self::ArchiveFailed => "agent_archive_failed",
        }
    }
}

/// Bounded refusal detail for the `recovery_owner_held`, `live_continuation`
/// and `review_source_sealed` classes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentArchiveRefusalDetailV1 {
    RecoveryOwner,
    ProgramEvidenceUnknown,
    RunningSuccessor,
    Active,
    RetryTimer,
    ResumeWake,
    SuccessorReservation,
    FreshRelaunchIntent,
    PendingMail,
    AssignmentOpen,
    VerdictPending,
}

impl AgentArchiveRefusalDetailV1 {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RecoveryOwner => "recovery_owner",
            Self::ProgramEvidenceUnknown => "program_evidence_unknown",
            Self::RunningSuccessor => "running_successor",
            Self::Active => "active",
            Self::RetryTimer => "retry_timer",
            Self::ResumeWake => "resume_wake",
            Self::SuccessorReservation => "successor_reservation",
            Self::FreshRelaunchIntent => "fresh_relaunch_intent",
            Self::PendingMail => "pending_mail",
            Self::AssignmentOpen => "assignment_open",
            Self::VerdictPending => "verdict_pending",
        }
    }
}

/// Strict v1 request for `AgentArchiveChild`.
///
/// Caller identity is token-resolved server-side and is deliberately absent.
/// The cursor fields are the same optimistic fence `AgentContinueChild` uses,
/// so a caller may take them straight from an `AgentGetProgress` snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentArchiveChildRequestV1 {
    pub target_session_id: Uuid,
    pub expected_tip_session_id: Uuid,
    pub expected_event_sequence: i64,
    #[serde(default)]
    pub expected_custody_generation: Option<i64>,
}

impl AgentArchiveChildRequestV1 {
    /// Validate transport-independent request bounds.
    ///
    /// # Errors
    ///
    /// Returns a stable safe error class when an identity or cursor component
    /// is outside the v1 contract bounds.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.target_session_id.is_nil() {
            return Err("agent_archive_target_session_id_must_not_be_nil");
        }
        if self.expected_tip_session_id.is_nil() {
            return Err("agent_archive_expected_tip_session_id_must_not_be_nil");
        }
        if self.expected_event_sequence < 0 {
            return Err("agent_archive_expected_event_sequence_must_not_be_negative");
        }
        if self
            .expected_custody_generation
            .is_some_and(|generation| generation <= 0)
        {
            return Err("agent_archive_expected_custody_generation_must_be_positive");
        }
        Ok(())
    }

    /// True when `observed` exactly satisfies this request's expected cursor,
    /// with the same optional custody-generation rule as
    /// [`AgentContinuationCursorV1::satisfies`].
    #[must_use]
    pub fn admits(&self, observed: &AgentContinuationCursorV1) -> bool {
        observed.tip_session_id == self.expected_tip_session_id
            && observed.event_sequence == self.expected_event_sequence
            && self
                .expected_custody_generation
                .is_none_or(|expected| observed.custody_generation == Some(expected))
    }
}

/// Receipt for an accepted or deduplicated archive.
///
/// `archived_session_ids` lists the lineage rows this call moved to
/// `Archived` (empty on a deduplicated replay). `prior_status` is the tip's
/// status before this call. `lead_generation` is the owning Epic's lead
/// generation read inside the archive transaction. The sandbox is never
/// removed by this verb; `sandbox_retained` reports whether a lineage row
/// still records one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentArchiveChildResultV1 {
    pub target_session_id: Uuid,
    pub archived_session_ids: Vec<Uuid>,
    pub prior_status: crate::types::SessionStatus,
    pub deduplicated: bool,
    pub watches_consumed: u32,
    pub c5_marker_settled: bool,
    pub lead_generation: i64,
    pub sandbox_retained: bool,
}

/// Stable typed error data returned in JSON-RPC error `data`.
///
/// `observed` is present on `StaleArchive` only. `detail` is present only on
/// the classes that carry a bounded refusal detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentArchiveErrorV1 {
    pub code: AgentArchiveErrorCodeV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<AgentArchiveRefusalDetailV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<AgentContinuationCursorV1>,
    pub next_action: String,
}

/// Canonical domain-separated Phase 2 digest over ordered length-prefixed
/// components (P2-01). Length prefixing makes the encoding injective, so no
/// two distinct component vectors can collide by concatenation.
///
/// This reuses the crate's existing dependency-free SHA-256 via
/// [`crate::program_runs::program_run_fingerprint`] rather than pulling `sha2`
/// and `hex` into `rsi-common`, which deliberately carries neither.
#[must_use]
pub fn agent_message_digest(purpose: &str, components: &[&[u8]]) -> String {
    let mut input = Vec::new();
    for component in components {
        input.extend_from_slice(&(component.len() as u64).to_be_bytes());
        input.extend_from_slice(component);
    }
    crate::program_runs::program_run_fingerprint(
        &format!("{AGENT_MESSAGE_DIGEST_DOMAIN}/{purpose}"),
        &input,
    )
}

/// Canonical idempotency digest over exactly `(caller, idempotency key)`.
///
/// The target is deliberately NOT absorbed. P2-03 scopes replay to
/// `(caller,key)` and requires that "same key with changed target, payload, or
/// expiry conflicts" — binding the target here would instead give a changed
/// target a DIFFERENT digest, so it would miss the replay lookup entirely and
/// be accepted as a second message. Target drift is caught where it belongs,
/// in [`agent_message_request_fingerprint`], which is compared only after the
/// digest has already matched.
#[must_use]
pub fn agent_message_idempotency_digest(owner: Uuid, key: &str) -> String {
    agent_message_digest(
        "idempotency",
        &[owner.as_bytes().as_slice(), key.as_bytes()],
    )
}

/// Canonical request fingerprint over every field that must not drift between
/// two sends sharing one idempotency key.
#[must_use]
pub fn agent_message_request_fingerprint(
    owner: Uuid,
    request: &AgentSendMessageRequestV1,
) -> String {
    let expiry = request
        .expires_at
        .map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
        .unwrap_or_default();
    agent_message_digest(
        "request",
        &[
            owner.as_bytes().as_slice(),
            request.target_session_id.as_bytes().as_slice(),
            request.idempotency_key.as_bytes(),
            request.message.as_bytes(),
            expiry.as_bytes(),
        ],
    )
}

/// Canonical payload digest over the exact delivered bytes.
#[must_use]
pub fn agent_message_payload_digest(payload: &str) -> String {
    agent_message_digest("payload", &[payload.as_bytes()])
}

#[cfg(test)]
mod agent_message_tests {
    use super::*;

    fn session(byte: u8) -> Uuid {
        Uuid::from_bytes([byte; 16])
    }

    #[test]
    fn agent_message_request_is_strict_and_bounded() {
        // Caller identity is transport-bound: a caller-supplied sender field
        // must fail deserialization outright.
        assert!(
            serde_json::from_value::<AgentSendMessageRequestV1>(serde_json::json!({
                "target_session_id": session(1),
                "message": "hello",
                "idempotency_key": "k",
                "caller_session_id": session(2)
            }))
            .is_err()
        );

        let request: AgentSendMessageRequestV1 = serde_json::from_value(serde_json::json!({
            "target_session_id": session(1),
            "message": "hello",
            "idempotency_key": "k"
        }))
        .unwrap();
        assert_eq!(request.validate(), Ok(()));

        let oversized = AgentSendMessageRequestV1 {
            message: "x".repeat(AGENT_MESSAGE_MAX_PAYLOAD_BYTES + 1),
            ..request.clone()
        };
        assert_eq!(
            oversized.validate(),
            Err("agent_message_payload_must_be_1_to_16384_bytes")
        );

        let long_key = AgentSendMessageRequestV1 {
            idempotency_key: "k".repeat(AGENT_MESSAGE_MAX_IDEMPOTENCY_KEY_BYTES + 1),
            ..request.clone()
        };
        assert_eq!(
            long_key.validate(),
            Err("agent_message_idempotency_key_must_be_1_to_128_bytes_without_nul")
        );

        // Exactly-at-limit values are accepted.
        assert_eq!(
            AgentSendMessageRequestV1 {
                message: "x".repeat(AGENT_MESSAGE_MAX_PAYLOAD_BYTES),
                idempotency_key: "k".repeat(AGENT_MESSAGE_MAX_IDEMPOTENCY_KEY_BYTES),
                ..request
            }
            .validate(),
            Ok(())
        );
    }

    #[test]
    fn agent_message_state_transition_matrix_is_frozen() {
        use AgentMessageStateV1 as S;

        let legal = [
            (S::Queued, S::Claimed),
            (S::Queued, S::Failed),
            (S::Queued, S::Expired),
            (S::Claimed, S::Queued),
            (S::Claimed, S::Injected),
            (S::Claimed, S::Acknowledged),
            (S::Claimed, S::Uncertain),
            (S::Claimed, S::Failed),
            (S::Claimed, S::Expired),
            (S::Injected, S::Acknowledged),
            (S::Injected, S::Uncertain),
            (S::Uncertain, S::Acknowledged),
            (S::Uncertain, S::Failed),
        ];
        for (from, to) in legal {
            assert!(from.may_transition_to(to), "{from:?}->{to:?} must be legal");
        }

        let all = [
            S::Queued,
            S::Claimed,
            S::Injected,
            S::Acknowledged,
            S::Uncertain,
            S::Failed,
            S::Expired,
        ];
        for from in all {
            for to in all {
                if !legal.contains(&(from, to)) {
                    assert!(
                        !from.may_transition_to(to),
                        "{from:?}->{to:?} must be rejected"
                    );
                }
            }
        }

        // Terminal states are immutable: no outbound edge whatsoever.
        for terminal in [S::Acknowledged, S::Failed, S::Expired] {
            assert!(terminal.is_terminal());
            for to in all {
                assert!(!terminal.may_transition_to(to));
            }
        }

        // Round-trip every spelling.
        for state in all {
            assert_eq!(S::from_str_exact(state.as_str()), Some(state));
        }
        assert_eq!(S::from_str_exact("injecting"), None);
    }

    #[test]
    fn agent_message_boundary_admission_matrix_is_closed() {
        let base = BoundaryAdmissionV1 {
            provider_kind: BoundaryProviderKindV1::CodexAppServer,
            capability_kind: BoundaryCapabilityKindV1::NativeMultiTurn,
            delivery_session_id: session(3),
            session_generation: 4,
            model_invocation_id: session(5),
            native_turn_id: Some("turn_abc".to_string()),
            classification: BoundaryClassificationV1::AdmittedEffectPossible,
            provider_error_class: None,
        };
        assert_eq!(base.validate(), Ok(()));
        assert_eq!(base.boundary_value(), Some("turn_abc".to_string()));

        // Only CodexAppServer is a native multi-turn boundary.
        for provider in [
            BoundaryProviderKindV1::Harness,
            BoundaryProviderKindV1::ClaudeCli,
            BoundaryProviderKindV1::CodexCli,
            BoundaryProviderKindV1::AntigravityCli,
            BoundaryProviderKindV1::LocalOpenAiCompatible,
        ] {
            assert_eq!(
                provider.capability_kind(),
                BoundaryCapabilityKindV1::TerminalOneTurn
            );
            assert_eq!(
                provider.capability_kind().boundary_kind(),
                BoundaryKindV1::ModelInvocation
            );
        }
        assert_eq!(
            BoundaryProviderKindV1::CodexAppServer.capability_kind(),
            BoundaryCapabilityKindV1::NativeMultiTurn
        );

        // Every configurable Session provider maps to exactly one boundary kind.
        // A wrong or defaulted mapping would silently move a provider onto
        // another row of the frozen P2-05 effect-threshold matrix, changing
        // which `rejected_before_effect` proof a dispatcher may accept.
        use crate::types::SessionProvider;
        for (session_provider, expected) in [
            (SessionProvider::Claude, BoundaryProviderKindV1::ClaudeCli),
            (SessionProvider::Codex, BoundaryProviderKindV1::CodexCli),
            (SessionProvider::Pioneer, BoundaryProviderKindV1::CodexCli),
            (SessionProvider::Bedrock, BoundaryProviderKindV1::CodexCli),
            (
                SessionProvider::OpenRouter,
                BoundaryProviderKindV1::CodexCli,
            ),
            (
                SessionProvider::Local,
                BoundaryProviderKindV1::LocalOpenAiCompatible,
            ),
            (
                SessionProvider::Antigravity,
                BoundaryProviderKindV1::AntigravityCli,
            ),
            (
                SessionProvider::CodexAppServer,
                BoundaryProviderKindV1::CodexAppServer,
            ),
            (SessionProvider::Harness, BoundaryProviderKindV1::Harness),
        ] {
            assert_eq!(
                BoundaryProviderKindV1::from_session_provider(session_provider),
                expected,
                "{session_provider:?} must map to exactly one delivery boundary"
            );
        }
        // Only the app-server provider is native multi-turn on the real enum too.
        assert_eq!(
            BoundaryProviderKindV1::from_session_provider(SessionProvider::CodexAppServer)
                .capability_kind(),
            BoundaryCapabilityKindV1::NativeMultiTurn
        );

        // A terminal provider must keep its native turn null and persists the
        // durable model invocation as its boundary value.
        let terminal = BoundaryAdmissionV1 {
            provider_kind: BoundaryProviderKindV1::Harness,
            capability_kind: BoundaryCapabilityKindV1::TerminalOneTurn,
            native_turn_id: None,
            ..base.clone()
        };
        assert_eq!(terminal.validate(), Ok(()));
        assert_eq!(
            terminal.boundary_value(),
            Some(terminal.model_invocation_id.to_string())
        );
        assert_eq!(
            BoundaryAdmissionV1 {
                native_turn_id: Some("turn_x".to_string()),
                ..terminal.clone()
            }
            .validate(),
            Err("agent_message_terminal_provider_must_have_null_native_turn")
        );

        // Capability kind may not contradict the provider matrix.
        assert_eq!(
            BoundaryAdmissionV1 {
                capability_kind: BoundaryCapabilityKindV1::NativeMultiTurn,
                ..terminal
            }
            .validate(),
            Err("agent_message_boundary_capability_kind_mismatch")
        );

        // A continuation token is never a genuine native turn ID.
        assert_eq!(
            BoundaryAdmissionV1 {
                native_turn_id: Some("continuation:abc".to_string()),
                ..base.clone()
            }
            .validate(),
            Err("agent_message_continuation_token_is_not_a_native_turn_id")
        );

        // A native turn may only accompany an admitted-effect-possible result.
        assert_eq!(
            BoundaryAdmissionV1 {
                classification: BoundaryClassificationV1::RejectedBeforeEffect,
                ..base.clone()
            }
            .validate(),
            Err("agent_message_native_turn_requires_admitted_effect_possible")
        );

        // A lost response leaves a native boundary with no durable value.
        assert_eq!(
            BoundaryAdmissionV1 {
                native_turn_id: None,
                ..base
            }
            .boundary_value(),
            None
        );
    }

    #[test]
    fn agent_message_policy_constants_are_frozen() {
        // C-P2-10 quarantine capacity.
        assert_eq!(AGENT_MESSAGE_QUARANTINE_MAX_EVENTS_PER_ATTEMPT, 8);
        assert_eq!(AGENT_MESSAGE_QUARANTINE_MAX_BYTES_PER_ATTEMPT, 65_536);
        assert_eq!(AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS, 64);
        assert_eq!(AGENT_MESSAGE_QUARANTINE_MAX_EVENTS_GLOBAL, 256);
        assert_eq!(AGENT_MESSAGE_QUARANTINE_MAX_BYTES_GLOBAL, 2_097_152);
        assert_eq!(AGENT_MESSAGE_QUARANTINE_TIMEOUT_MS, 5_000);

        // C-P2-12 raw AppServer ingress ceilings.
        assert_eq!(APP_SERVER_UNCLASSIFIED_PREFIX_BYTES, 8_192);
        assert_eq!(APP_SERVER_SCRATCH_CHUNK_BYTES, 8_192);
        assert_eq!(APP_SERVER_MAX_RESPONSE_BYTES, 262_144);
        assert_eq!(APP_SERVER_MAX_PROVIDER_REQUEST_BYTES, 65_536);
        assert_eq!(APP_SERVER_MAX_NOTIFICATION_BYTES, 262_144);
        assert_eq!(APP_SERVER_MAX_ORDINARY_EVENT_BYTES, 1_048_576);
        assert_eq!(APP_SERVER_ABSOLUTE_DISCARD_BUDGET_BYTES, 8_388_608);
        assert_eq!(APP_SERVER_MAX_JSON_DEPTH, 64);
        assert_eq!(APP_SERVER_MAX_JSONRPC_STRING_ID_BYTES, 256);

        // C-P2-15 control-worker bounds.
        assert_eq!(APP_SERVER_CONTROL_TICK_MS, 100);
        assert_eq!(APP_SERVER_CONTROL_MAX_LATCHES_PER_TICK, 32);
        assert_eq!(APP_SERVER_CONTROL_MAX_MS_PER_TICK, 2);

        // C-P2-19 keyset reconciliation bounds.
        assert_eq!(AGENT_MESSAGE_RECONCILE_MAX_ROWS, 64);
        assert_eq!(AGENT_MESSAGE_RECONCILE_MAX_MS, 10);
        assert_eq!(AGENT_MESSAGE_SPAWN_SETTLEMENT_MAX_TARGETS, 128);

        // Accepted Phase 1 acceptance bounds are preserved verbatim.
        assert_eq!(AGENT_MESSAGE_MAX_PAYLOAD_BYTES, 16_384);
        assert_eq!(AGENT_MESSAGE_MAX_IDEMPOTENCY_KEY_BYTES, 128);
        assert_eq!(AGENT_MESSAGE_MAX_PENDING_PER_TARGET, 128);
        assert_eq!(AGENT_MESSAGE_MAX_PENDING_PER_OWNER, 512);

        // C-P2-17 bounded-text ceilings.
        assert_eq!(AGENT_MESSAGE_MAX_CANONICAL_JSONRPC_ID_BYTES, 1_540);
        assert_eq!(AGENT_MESSAGE_MAX_PROVIDER_TURN_BYTES, 512);
        assert_eq!(AGENT_MESSAGE_MAX_METHOD_BYTES, 128);
        assert_eq!(AGENT_MESSAGE_MAX_ENUM_BYTES, 64);
        assert_eq!(AGENT_MESSAGE_MAX_ERROR_CLASS_BYTES, 128);
        assert_eq!(AGENT_MESSAGE_MAX_CAPABILITY_ID_BYTES, 36);

        // The per-attempt quarantine byte ceiling IS the message overlay
        // ceiling inherited by the raw frame reader.
        assert_eq!(
            AGENT_MESSAGE_QUARANTINE_MAX_BYTES_PER_ATTEMPT,
            APP_SERVER_MAX_PROVIDER_REQUEST_BYTES
        );
    }

    #[test]
    fn agent_message_phase_progressions_are_frozen() {
        use ProviderRequestHandlerPhaseV1 as H;
        use ProviderRequestReplyPhaseV1 as R;

        let handler_legal = [
            (H::EvidenceCommitted, H::HandlerAuthorized),
            (H::HandlerAuthorized, H::HandlerStarted),
            (H::HandlerStarted, H::HandlerCompleted),
            (H::HandlerStarted, H::HandlerUncertain),
            (H::HandlerCompleted, H::PendingDecision),
            (H::PendingDecision, H::DecisionRecorded),
        ];
        let handler_all = [
            H::EvidenceCommitted,
            H::HandlerAuthorized,
            H::HandlerStarted,
            H::HandlerCompleted,
            H::PendingDecision,
            H::DecisionRecorded,
            H::HandlerUncertain,
        ];
        for from in handler_all {
            for to in handler_all {
                assert_eq!(
                    from.may_advance_to(to),
                    handler_legal.contains(&(from, to)),
                    "handler {from:?}->{to:?}"
                );
            }
            assert_eq!(H::from_str_exact(from.as_str()), Some(from));
        }
        // A started handler is never reset or retried.
        assert!(!H::HandlerStarted.may_advance_to(H::HandlerAuthorized));
        assert!(!H::HandlerUncertain.may_advance_to(H::HandlerCompleted));

        let reply_legal = [
            (R::NotAuthorized, R::ReplyAuthorized),
            (R::ReplyAuthorized, R::ReplyStarted),
            (R::ReplyStarted, R::ReplyCompleted),
            (R::ReplyStarted, R::ReplyUncertain),
        ];
        let reply_all = [
            R::NotAuthorized,
            R::ReplyAuthorized,
            R::ReplyStarted,
            R::ReplyCompleted,
            R::ReplyUncertain,
        ];
        for from in reply_all {
            for to in reply_all {
                assert_eq!(
                    from.may_advance_to(to),
                    reply_legal.contains(&(from, to)),
                    "reply {from:?}->{to:?}"
                );
            }
            assert_eq!(R::from_str_exact(from.as_str()), Some(from));
        }
        // A started reply never resends.
        assert!(!R::ReplyStarted.may_advance_to(R::ReplyAuthorized));
        assert!(!R::ReplyUncertain.may_advance_to(R::ReplyCompleted));
    }

    #[test]
    fn agent_message_gate_and_permit_vocabulary_is_closed() {
        // Only an open gate admits a new row, capability, or effect start.
        assert!(TurnGateStateV1::Open.admits_new_effect());
        assert!(!TurnGateStateV1::Closing.admits_new_effect());
        assert!(!TurnGateStateV1::Closed.admits_new_effect());

        // An unjoined started effect must NOT resolve a gate: that is exactly
        // the retained-closing custody C-P2-14 requires.
        assert!(!EffectPermitStateV1::Issued.is_resolved());
        assert!(!EffectPermitStateV1::UncertainUnjoined.is_resolved());
        assert!(EffectPermitStateV1::Completed.is_resolved());
        assert!(EffectPermitStateV1::UncertainJoined.is_resolved());
        assert!(EffectPermitStateV1::CancelConfirmed.is_resolved());

        for state in [
            TurnGateStateV1::Open,
            TurnGateStateV1::Closing,
            TurnGateStateV1::Closed,
        ] {
            assert_eq!(TurnGateStateV1::from_str_exact(state.as_str()), Some(state));
        }
        for state in [
            EffectPermitStateV1::Issued,
            EffectPermitStateV1::Completed,
            EffectPermitStateV1::UncertainUnjoined,
            EffectPermitStateV1::UncertainJoined,
            EffectPermitStateV1::CancelConfirmed,
        ] {
            assert_eq!(
                EffectPermitStateV1::from_str_exact(state.as_str()),
                Some(state)
            );
        }
        for kind in [
            EffectPermitKindV1::ToolExecution,
            EffectPermitKindV1::ApprovalPresentation,
            EffectPermitKindV1::ProviderReply,
        ] {
            assert_eq!(
                EffectPermitKindV1::from_str_exact(kind.as_str()),
                Some(kind)
            );
        }
        for disposition in [
            ProviderRequestDispositionV1::Live,
            ProviderRequestDispositionV1::Completed,
            ProviderRequestDispositionV1::ProviderTerminalCancelled,
        ] {
            assert_eq!(
                ProviderRequestDispositionV1::from_str_exact(disposition.as_str()),
                Some(disposition)
            );
        }
        assert!(!ProviderRequestDispositionV1::Live.is_terminal());
        assert!(ProviderRequestDispositionV1::Completed.is_terminal());
        assert!(ProviderRequestDispositionV1::ProviderTerminalCancelled.is_terminal());
    }

    /// Every `AttemptStateV1` variant round-trips through its durable string.
    ///
    /// This exists because `from_str_exact` ends in `_ => return None`, so a
    /// variant added to the enum and to `as_str` but MISSED in `from_str_exact`
    /// compiles silently: rustc sees an exhaustive `as_str` and a total
    /// `from_str_exact`, and SQLite happily stores a string the parser then
    /// refuses. The failure would surface only as a row that will not load at
    /// runtime. Iterating the closed variant list here is what makes the
    /// omission a compile-or-test failure instead of a production one.
    #[test]
    fn agent_message_attempt_state_round_trips_every_variant() {
        let all = [
            AttemptStateV1::Claimed,
            AttemptStateV1::Dispatching,
            AttemptStateV1::EffectPossible,
            AttemptStateV1::Terminal,
        ];
        for state in all {
            assert_eq!(
                AttemptStateV1::from_str_exact(state.as_str()),
                Some(state),
                "{} must parse back to itself",
                state.as_str()
            );
        }

        // The durable spelling of the pre-dispatch marker is load-bearing: the
        // schema CHECK, the transition trigger, and the P2-06 recovery rule all
        // match this exact literal.
        assert_eq!(AttemptStateV1::Dispatching.as_str(), "dispatching");

        // The vocabulary is CLOSED: near-misses must not parse.
        for bogus in ["dispatch", "dispatched", "Dispatching", "", "claiming"] {
            assert_eq!(
                AttemptStateV1::from_str_exact(bogus),
                None,
                "{bogus} must not parse as an attempt state"
            );
        }
    }

    #[test]
    fn agent_message_correlation_permits_exactly_one_late_ack_class() {
        // Only an unsealed correlation_pending attempt may late-acknowledge.
        assert!(CorrelationStateV1::CorrelationPending.permits_late_acknowledgement());
        for state in [
            CorrelationStateV1::NotApplicable,
            CorrelationStateV1::Correlated,
            CorrelationStateV1::SealedLiveUncertain,
        ] {
            assert!(!state.permits_late_acknowledgement());
        }
        for state in [
            CorrelationStateV1::NotApplicable,
            CorrelationStateV1::CorrelationPending,
            CorrelationStateV1::Correlated,
            CorrelationStateV1::SealedLiveUncertain,
        ] {
            assert_eq!(
                CorrelationStateV1::from_str_exact(state.as_str()),
                Some(state)
            );
        }

        // Only a sealed no-effect disposition may remain a historical pointer.
        for disposition in [
            AttemptTerminalDispositionV1::ProvedNoEffectRequeue,
            AttemptTerminalDispositionV1::ProvedNoEffectFailed,
            AttemptTerminalDispositionV1::ProvedNoEffectExpired,
        ] {
            assert!(disposition.is_proved_no_effect());
        }
        for disposition in [
            AttemptTerminalDispositionV1::Unsupported,
            AttemptTerminalDispositionV1::Acknowledged,
            AttemptTerminalDispositionV1::SettledFailed,
        ] {
            assert!(!disposition.is_proved_no_effect());
            assert_eq!(
                AttemptTerminalDispositionV1::from_str_exact(disposition.as_str()),
                Some(disposition)
            );
        }
    }

    #[test]
    fn agent_message_digests_are_domain_separated_and_injective() {
        let owner = session(1);
        let target = session(2);

        let digest = agent_message_idempotency_digest(owner, "k");
        assert!(digest.starts_with("sha256:"));
        assert_eq!(digest.len(), 71);
        assert!(
            digest["sha256:".len()..]
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );

        // Deterministic.
        assert_eq!(digest, agent_message_idempotency_digest(owner, "k"));
        // Caller and key each independently change the digest.
        assert_ne!(digest, agent_message_idempotency_digest(target, "k"));
        assert_ne!(digest, agent_message_idempotency_digest(owner, "k2"));
        // The target does NOT participate: replay is scoped to (caller,key),
        // so a changed target must land on the SAME digest and then be caught
        // as fingerprint drift rather than silently becoming a new message.
        assert_eq!(digest, agent_message_idempotency_digest(owner, "k"));

        // Length prefixing prevents concatenation collisions: ("ab","c") and
        // ("a","bc") must not alias.
        assert_ne!(
            agent_message_digest("t", &[b"ab", b"c"]),
            agent_message_digest("t", &[b"a", b"bc"])
        );
        // Purpose separation is real.
        assert_ne!(
            agent_message_digest("payload", &[b"x"]),
            agent_message_digest("request", &[b"x"])
        );

        // A changed payload or expiry changes the request fingerprint, so an
        // idempotency-key replay with drifted content is a detectable conflict.
        let request = AgentSendMessageRequestV1 {
            target_session_id: target,
            message: "hello".to_string(),
            idempotency_key: "k".to_string(),
            expires_at: None,
        };
        let baseline = agent_message_request_fingerprint(owner, &request);
        assert_eq!(baseline, agent_message_request_fingerprint(owner, &request));
        assert_ne!(
            baseline,
            agent_message_request_fingerprint(
                owner,
                &AgentSendMessageRequestV1 {
                    message: "hello!".to_string(),
                    ..request.clone()
                }
            )
        );
        assert_ne!(
            baseline,
            agent_message_request_fingerprint(
                owner,
                &AgentSendMessageRequestV1 {
                    expires_at: Some(DateTime::<Utc>::from_timestamp_nanos(0)),
                    ..request.clone()
                }
            )
        );
        assert_ne!(baseline, agent_message_payload_digest(&request.message));
    }
}
