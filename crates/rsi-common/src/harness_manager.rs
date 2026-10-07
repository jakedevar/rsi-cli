//! Project-scoped operator appointment and attributed manager conversations.
//!
//! Agent inputs contain content and routing references, never caller identity.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::types::{PendingQuestion, SessionKind, SessionStatus};

pub const HARNESS_MANAGER_MAX_EPICS: usize = 32;
pub const HARNESS_MANAGER_MAX_GROUPS: usize = 32;
pub const HARNESS_MANAGER_MAX_MESSAGE_BYTES: usize = 8192;
pub const HARNESS_MANAGER_MAX_INBOX_PAGE: u16 = 32;

/// A manager node may address its parent, or the shared ancestor of two
/// currently owned Epic scopes. Caller identity is supplied by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagerNodeEscalationRouteV1 {
    Parent,
    EpicConflict {
        left_epic_id: Uuid,
        right_epic_id: Uuid,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerEscalateRequestV1 {
    pub project_id: Uuid,
    pub subject_id: Uuid,
    pub reason: String,
    pub route: ManagerNodeEscalationRouteV1,
    pub expected_source_authority_epoch: i64,
    pub expected_source_grant_version: i64,
    pub expected_target_authority_epoch: i64,
    pub expected_target_grant_version: i64,
    pub expected_target_session_id: Uuid,
    pub idempotency_key: String,
}

/// Attributed RPC input. The daemon derives the project from the caller's
/// session and supplies it to the transactional store operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerEscalateInputV1 {
    pub subject_id: Uuid,
    pub reason: String,
    pub route: ManagerNodeEscalationRouteV1,
    pub expected_source_authority_epoch: i64,
    pub expected_source_grant_version: i64,
    pub expected_target_authority_epoch: i64,
    pub expected_target_grant_version: i64,
    pub expected_target_session_id: Uuid,
    pub idempotency_key: String,
}

impl AgentManagerEscalateInputV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.clone().into_request(Uuid::from_u128(1)).validate()
    }

    pub fn into_request(self, project_id: Uuid) -> AgentManagerEscalateRequestV1 {
        AgentManagerEscalateRequestV1 {
            project_id,
            subject_id: self.subject_id,
            reason: self.reason,
            route: self.route,
            expected_source_authority_epoch: self.expected_source_authority_epoch,
            expected_source_grant_version: self.expected_source_grant_version,
            expected_target_authority_epoch: self.expected_target_authority_epoch,
            expected_target_grant_version: self.expected_target_grant_version,
            expected_target_session_id: self.expected_target_session_id,
            idempotency_key: self.idempotency_key,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerListEscalationsRequestV1 {}

impl AgentManagerEscalateRequestV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_manager_body(&self.reason, &self.idempotency_key)?;
        if self.project_id.is_nil()
            || self.subject_id.is_nil()
            || self.expected_source_authority_epoch <= 0
            || self.expected_source_grant_version <= 0
            || self.expected_target_authority_epoch <= 0
            || self.expected_target_grant_version <= 0
            || self.expected_target_session_id.is_nil()
            || matches!(self.route, ManagerNodeEscalationRouteV1::EpicConflict { left_epic_id, right_epic_id }
                if left_epic_id.is_nil() || right_epic_id.is_nil() || left_epic_id == right_epic_id)
        {
            return Err("manager_node_invalid_escalation");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerResolveEscalationRequestV1 {
    pub escalation_id: Uuid,
    pub expected_version: i64,
    pub expected_target_authority_epoch: i64,
    pub expected_target_grant_version: i64,
    pub expected_target_session_id: Uuid,
    /// `None` forwards to the addressed node's parent. A ruling is a manager
    /// decision only; it never represents human approval.
    pub ruling: Option<String>,
    pub idempotency_key: String,
}

impl AgentManagerResolveEscalationRequestV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.escalation_id.is_nil()
            || self.expected_version <= 0
            || self.expected_target_authority_epoch <= 0
            || self.expected_target_grant_version <= 0
            || self.expected_target_session_id.is_nil()
            || self.ruling.as_ref().is_some_and(|value| {
                value.trim().is_empty()
                    || value.len() > HARNESS_MANAGER_MAX_MESSAGE_BYTES
                    || value.contains('\0')
            })
            || self.idempotency_key.is_empty()
            || self.idempotency_key.len() > 128
            || self.idempotency_key.contains('\0')
        {
            return Err("manager_node_invalid_escalation_resolution");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerNodeEscalationStateV1 {
    Open,
    Ruled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagerNodeEscalationV1 {
    pub id: Uuid,
    pub project_id: Uuid,
    pub subject_id: Uuid,
    pub source_node_id: Uuid,
    pub target_node_id: Uuid,
    pub reason: String,
    pub source_authority_epoch: i64,
    pub source_grant_version: i64,
    pub target_authority_epoch: i64,
    pub target_grant_version: i64,
    /// The seat lineage tip when this node became the addressed owner.
    pub target_session_id: Uuid,
    pub version: i64,
    pub state: ManagerNodeEscalationStateV1,
    pub ruling: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// #1238: the newest hop of this escalation above its project root (it
    /// is held there while that hop is `open`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub above_project: Option<crate::manager_tier_routing::ManagerTierEscalationHopV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GetHarnessManagerRequestV1 {
    pub project_id: Uuid,
}

/// Operator picker discovery, independent of the TUI's navigation cache.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListHarnessManagerEpicsRequestV1 {
    pub project_id: Uuid,
    #[serde(default)]
    pub after_id: Option<Uuid>,
    pub limit: u16,
}

impl ListHarnessManagerEpicsRequestV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.project_id.is_nil()
            || self.after_id.is_some_and(|id| id.is_nil())
            || !(1..=64).contains(&self.limit)
        {
            return Err("manager_invalid_epic_page");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessManagerEpicCandidateV1 {
    pub id: Uuid,
    pub title: String,
    pub group_title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListHarnessManagerEpicsResultV1 {
    pub epics: Vec<HarnessManagerEpicCandidateV1>,
    pub next_after_id: Option<Uuid>,
}

/// Groups (including empty ones) and their legal Epics share one keyset page.
pub type ListHarnessManagerScopeRequestV1 = ListHarnessManagerEpicsRequestV1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessManagerScopeCandidateV1 {
    pub id: Uuid,
    pub title: String,
    pub kind: SessionKind,
    pub group_id: Option<Uuid>,
    pub group_title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListHarnessManagerScopeResultV1 {
    pub rows: Vec<HarnessManagerScopeCandidateV1>,
    pub next_after_id: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessManagerScopeModeV1 {
    Project,
    #[default]
    Selected,
}

/// Operator-only replacement of the entire scope. Zero is the initial version.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigureHarnessManagerRequestV1 {
    pub project_id: Uuid,
    pub session_id: Uuid,
    /// Omission with no Groups means the whole project. Explicit [] still revokes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epic_ids: Option<Vec<Uuid>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_ids: Vec<Uuid>,
    pub expected_row_version: i64,
}

impl ConfigureHarnessManagerRequestV1 {
    pub fn scope_mode(&self) -> HarnessManagerScopeModeV1 {
        if self.epic_ids.is_none() && self.group_ids.is_empty() {
            HarnessManagerScopeModeV1::Project
        } else {
            HarnessManagerScopeModeV1::Selected
        }
    }

    pub fn is_revocation(&self) -> bool {
        self.epic_ids.as_ref().is_some_and(Vec::is_empty) && self.group_ids.is_empty()
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        let epics = self.epic_ids.as_deref().unwrap_or_default();
        if self.project_id.is_nil()
            || self.session_id.is_nil()
            || epics.iter().any(Uuid::is_nil)
            || epics.len() > HARNESS_MANAGER_MAX_EPICS
            || self.group_ids.iter().any(Uuid::is_nil)
            || self.group_ids.len() > HARNESS_MANAGER_MAX_GROUPS
            || self.expected_row_version < 0
        {
            return Err("manager_invalid_scope");
        }
        let unique: std::collections::HashSet<_> = epics.iter().collect();
        if unique.len() != epics.len() {
            return Err("manager_duplicate_epic");
        }
        let unique: std::collections::HashSet<_> = self.group_ids.iter().collect();
        if unique.len() != self.group_ids.len() {
            return Err("manager_duplicate_group");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessManagerConfigV1 {
    pub project_id: Uuid,
    /// Persisted lineage anchor; resolve the current session before opening it.
    pub manager_session_id: Uuid,
    /// Daemon-resolved current principal. None means lineage needs operator repair.
    #[serde(default)]
    pub current_session_id: Option<Uuid>,
    /// Effective Epic membership, resolved from current project topology.
    pub epic_ids: Vec<Uuid>,
    #[serde(default)]
    pub scope_mode: HarnessManagerScopeModeV1,
    /// None only when reading a response from a legacy daemon.
    #[serde(default)]
    pub selected_epic_ids: Option<Vec<Uuid>>,
    #[serde(default)]
    pub group_ids: Vec<Uuid>,
    pub row_version: i64,
    pub updated_at: DateTime<Utc>,
}

/// What a scope save did to the saved manager policy (#1145). Authority never
/// follows a changed scope by side effect: an identical save leaves the policy
/// untouched, any other save leaves the previous policy as an unconfirmed
/// draft that only an explicit policy save re-grants.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessManagerPolicyOutcomeKindV1 {
    /// No saved policy existed; nothing to carry or revoke.
    #[default]
    NoPolicy,
    /// Identical project, seat and scope: nothing changed, the grant stands.
    Unchanged,
    /// The scope changed: the grant is revoked and no capability was carried.
    /// The previous values stay as the draft the operator must re-save.
    RevokedNeedsConfirmation,
}

/// Additive outcome of a scope save, for the operator surface.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessManagerPolicyOutcomeV1 {
    #[serde(default)]
    pub kind: HarnessManagerPolicyOutcomeKindV1,
    /// Capabilities of the previous saved policy (granted only if `Unchanged`).
    #[serde(default)]
    pub capability_count: usize,
    /// Paused Epics the previous policy holds; they stay paused in the draft,
    /// even across removing and re-adding the Epic.
    #[serde(default)]
    pub paused_epic_ids: Vec<Uuid>,
}

/// `ConfigureHarnessManager` result: the config plus the policy outcome.
/// Flattened, so older clients still read a plain config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigureHarnessManagerResultV1 {
    #[serde(flatten)]
    pub config: HarnessManagerConfigV1,
    #[serde(default)]
    pub policy: HarnessManagerPolicyOutcomeV1,
}

impl HarnessManagerConfigV1 {
    pub fn explicit_epic_ids(&self) -> &[Uuid] {
        self.selected_epic_ids.as_deref().unwrap_or(&self.epic_ids)
    }

    pub fn covers_group(&self, id: Uuid) -> bool {
        self.scope_mode == HarnessManagerScopeModeV1::Project || self.group_ids.contains(&id)
    }

    pub fn has_dynamic_scope(&self) -> bool {
        self.scope_mode == HarnessManagerScopeModeV1::Project || !self.group_ids.is_empty()
    }

    pub fn is_revoked(&self) -> bool {
        !self.has_dynamic_scope() && self.epic_ids.is_empty()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerProgressRequestV1 {
    #[serde(default)]
    pub after_epic_id: Option<Uuid>,
    #[serde(default)]
    pub limit: Option<u16>,
    /// #1235: the target project of a global manager seat acting inside its
    /// operator grant. Omitted means the caller's own project. A target the
    /// daemon checks against the grant, never caller identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
}

impl AgentManagerProgressRequestV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.after_epic_id.is_some_and(|id| id.is_nil())
            || !(1..=64).contains(&self.limit.unwrap_or(32))
        {
            return Err("manager_invalid_progress_page");
        }
        Ok(())
    }
}

/// Keep the advertised object-only contract even for empty request structs,
/// which Serde also accepts as an empty sequence by default.
pub fn decode_manager_request<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
) -> Result<T, &'static str> {
    if !value.is_object() {
        return Err("manager_invalid_request");
    }
    serde_json::from_value(value).map_err(|_| "manager_invalid_request")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessManagerEpicProgressV1 {
    pub epic_id: Uuid,
    pub title: String,
    pub lead_session_id: Option<Uuid>,
    pub lead_title: Option<String>,
    pub status: Option<SessionStatus>,
    pub updated_at: Option<DateTime<Utc>>,
    pub pending_question: Option<PendingQuestion>,
    pub pipeline_artifact: Option<String>,
    /// Bounded latest assistant text, not a verification or acceptance receipt.
    pub last_response: Option<String>,
    pub safe_error_class: Option<String>,
    /// Open manager requests addressed to this Epic (#664 capacity; the
    /// per-Epic cap is `mail_capacity.epic_limit`).
    #[serde(default)]
    pub pending_requests: u32,
}

/// Manager request mail capacity (#664). Counts are open requests in the
/// current scope: unreplied, unsettled, unreleased and not lead-terminal.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessManagerMailCapacityV1 {
    pub project_pending: u32,
    pub project_limit: u32,
    pub epic_limit: u32,
    /// `manager_mail_capacity_75pct` when the project or any Epic is at or
    /// above 75% of its limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessManagerRequestSummaryV1 {
    pub request_id: Uuid,
    pub epic_id: Uuid,
    /// `pending_reply`, `replied`, `scope_revoked`, `lead_changed`,
    /// `readdressed`, `settled`, or `rolled_over` (#656: the standing request
    /// reached its reply cap and continues on `rolled_over_to`).
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readdressed_to: Option<Uuid>,
    /// #656: the next generation of this standing request, when it rolled over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolled_over_to: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManagerProgressResultV1 {
    pub observed_at: DateTime<Utc>,
    pub config: HarnessManagerConfigV1,
    pub rows: Vec<HarnessManagerEpicProgressV1>,
    #[serde(default)]
    pub next_after_epic_id: Option<Uuid>,
    /// Latest 32 requests; full exchanges are keyset-paged through the inbox.
    pub recent_requests: Vec<HarnessManagerRequestSummaryV1>,
    #[serde(default)]
    pub mail_capacity: HarnessManagerMailCapacityV1,
    /// #1235: the daemon's current `{scope_version, policy_version}` fence of
    /// the caller's manager principal in `config.project_id`, ready for the
    /// fenced verbs. Absent when the project has no saved policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fence: Option<crate::harness_manager_v2::ManagerFenceV2>,
}

const fn default_inbox_limit() -> u16 {
    HARNESS_MANAGER_MAX_INBOX_PAGE
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerInboxRequestV1 {
    #[serde(default)]
    pub after_sequence: i64,
    #[serde(default = "default_inbox_limit")]
    pub limit: u16,
    #[serde(default)]
    pub request_id: Option<Uuid>,
    /// Independent cursor for notices; message paging remains unchanged.
    #[serde(default)]
    pub after_notice_sequence: i64,
    #[serde(default)]
    pub notice_kind: Option<String>,
    /// Settle exact notices belonging to this seat, independently of paging.
    #[serde(default)]
    pub settle_notice_ids: Vec<Uuid>,
}

impl Default for AgentManagerInboxRequestV1 {
    fn default() -> Self {
        Self {
            after_sequence: 0,
            limit: default_inbox_limit(),
            request_id: None,
            after_notice_sequence: 0,
            notice_kind: None,
            settle_notice_ids: Vec::new(),
        }
    }
}

impl AgentManagerInboxRequestV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.after_sequence < 0
            || self.after_notice_sequence < 0
            || self.notice_kind.as_deref().is_some_and(|kind| {
                !matches!(
                    kind,
                    "session_state"
                        | "message"
                        | "action_result"
                        | "operator_answer"
                        | "ledger_change"
                )
            })
            || self.settle_notice_ids.len() > usize::from(HARNESS_MANAGER_MAX_INBOX_PAGE)
            || self.settle_notice_ids.iter().any(|id| id.is_nil())
            || self.limit == 0
            || self.limit > HARNESS_MANAGER_MAX_INBOX_PAGE
            || self.request_id.is_some_and(|id| id.is_nil())
        {
            return Err("manager_invalid_inbox_page");
        }
        Ok(())
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde skip_serializing_if passes a reference
const fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerSendRequestV1 {
    pub epic_id: Uuid,
    pub message: String,
    pub idempotency_key: String,
    /// Informational mail is delivered and audited without reserving a reply slot.
    #[serde(default, skip_serializing_if = "is_false")]
    pub informational: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerReplyRequestV1 {
    pub request_id: Uuid,
    pub message: String,
    pub idempotency_key: String,
    /// Keep the request active after this reply instead of settling it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub still_running: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerNotifyRequestV1 {
    pub message: String,
    pub idempotency_key: String,
}

impl AgentManagerNotifyRequestV1 {
    /// Validate the bounded notice body and replay key.
    ///
    /// # Errors
    /// Returns the stable manager validation code for an invalid body or key.
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_manager_body(&self.message, &self.idempotency_key)
    }
}

pub fn validate_manager_message(id: Uuid, message: &str, key: &str) -> Result<(), &'static str> {
    if id.is_nil() {
        return Err("manager_invalid_reference");
    }
    validate_manager_body(message, key)
}

fn validate_manager_body(message: &str, key: &str) -> Result<(), &'static str> {
    if message.trim().is_empty()
        || message.len() > HARNESS_MANAGER_MAX_MESSAGE_BYTES
        || message.contains('\0')
    {
        return Err("manager_invalid_message");
    }
    if key.is_empty() || key.len() > 128 || key.contains('\0') {
        return Err("manager_invalid_idempotency_key");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessManagerMessageReceiptV1 {
    pub message_id: Uuid,
    pub sequence: i64,
    /// For a reply: the request generation that actually stores it (#656).
    pub request_id: Option<Uuid>,
    pub deduplicated: bool,
    /// #656: set on a reply stored on a rollover successor. It is the standing
    /// root request, which is the id the lead names in the normal flow; being
    /// root-relative keeps a replay receipt identical whichever generation the
    /// retry names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolled_over_from: Option<Uuid>,
    /// Durable daemon-owned seat observation (#669). Mail is still durably
    /// queued while the seat is down; this field tells the sender so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager_seat: Option<ManagerSeatStateV1>,
}

/// Daemon-classified condition of the appointed manager seat (#669).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagerSeatConditionV1 {
    /// Positive evidence: the tip produced provider output after the last
    /// down observation or recovery attempt.
    Live,
    /// The tip is `Failed` and automatic recovery is not permitted.
    Down,
    /// The tip is `Failed` (or its resumed turn has not yet produced output)
    /// and a bounded in-place recovery attempt is scheduled or in flight.
    Recovering,
    /// The per-tip recovery budget is spent. Terminal until the operator
    /// resumes, appoints or succeeds the manager.
    Exhausted,
}

impl ManagerSeatConditionV1 {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Down => "down",
            Self::Recovering => "recovering",
            Self::Exhausted => "exhausted",
        }
    }
}

/// Durable `manager_seat` record payload, also returned to leads on inbox
/// and mail receipts. Every field is daemon-observed; none is caller-supplied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerSeatStateV1 {
    pub state: ManagerSeatConditionV1,
    pub tip_session_id: Uuid,
    /// Start of the current observation (down episode or recovery).
    pub since: DateTime<Utc>,
    /// Automatic in-place recovery attempts charged to this tip.
    #[serde(default)]
    pub attempts: u16,
    /// The policy's `max_recovery_attempts` at observation time.
    #[serde(default)]
    pub max_attempts: u16,
    #[serde(default)]
    pub not_before: Option<DateTime<Utc>>,
    pub reason: String,
    #[serde(default)]
    pub next_action: Option<String>,
    #[serde(default)]
    pub last_invocation_id: Option<Uuid>,
    #[serde(default)]
    pub last_error_class: Option<String>,
    /// The tip's persisted `terminal_reason` (e.g. `aborted_streaming`).
    #[serde(default)]
    pub last_terminal_reason: Option<String>,
}

impl ManagerSeatStateV1 {
    /// True when the seat cannot currently act on mail or notices.
    #[must_use]
    pub fn is_down(&self) -> bool {
        self.state != ManagerSeatConditionV1::Live
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessManagerMessageV1 {
    pub message_id: Uuid,
    pub sequence: i64,
    pub request_id: Option<Uuid>,
    pub epic_id: Uuid,
    pub sender_session_id: Uuid,
    pub recipient_session_id: Uuid,
    pub message: String,
    pub created_at: DateTime<Utc>,
    pub replied: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub informational: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readdressed_from: Option<Uuid>,
    /// #656: the standing root request of a rollover successor, set on the
    /// successor and on its replies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standing_root_id: Option<Uuid>,
}

/// One exact durable reason a harness-manager watch requested attention.
///
/// Notices are retained audit evidence. Retrieval settles notification
/// delivery only; it never replies to mail, answers a decision, clears a
/// question, resumes a session, or changes manager authority.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessManagerNoticeV1 {
    pub notice_id: Uuid,
    pub sequence: i64,
    /// The affected Epic when the subject is Epic-scoped. Project-wide
    /// manager action results deliberately carry `None` rather than
    /// fabricating an Epic identity for their transport.
    pub epic_id: Option<Uuid>,
    pub direction: String,
    pub kind: String,
    pub subject_id: String,
    pub subject_version: String,
    pub source_session_id: Option<Uuid>,
    pub recipient_session_id: Uuid,
    pub state: serde_json::Value,
    pub recorded_at: DateTime<Utc>,
    pub queued_at: DateTime<Utc>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub retrieved_at: DateTime<Utc>,
    pub settled_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManagerInboxResultV1 {
    pub messages: Vec<HarnessManagerMessageV1>,
    /// Continuation token: pass it back as `after_sequence`. Set when more
    /// messages remain or `more_notices` is true (notices settle as they are
    /// returned, so the next call reads the next page).
    pub next_after_sequence: Option<i64>,
    /// Exact notice subjects settled by this retrieval. Added compatibly: old
    /// clients ignore the fields and missing fields decode as an empty page.
    #[serde(default)]
    pub notices: Vec<HarnessManagerNoticeV1>,
    #[serde(default)]
    pub more_notices: bool,
    /// Pass back as `after_notice_sequence` with the same notice filters.
    #[serde(default)]
    pub next_after_notice_sequence: Option<i64>,
    /// Authorized IDs explicitly settled by this call (including retries).
    #[serde(default)]
    pub settled_notice_ids: Vec<Uuid>,
    /// Durable seat observation for the appointed manager (#669); absent
    /// when the daemon has never observed the seat down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager_seat: Option<ManagerSeatStateV1>,
    /// #1266: tier mail this seat sent or was sent whose delivery failed or
    /// is uncertain, with the reason. Never replayed automatically.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub undelivered_tier_mail: Vec<crate::manager_tier_routing::UndeliveredTierMailV1>,
    /// #1295: older failed or uncertain tier mail exists beyond this list.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub more_undelivered_tier_mail: bool,
}

pub const MANAGER_WORK_VIEW_MAX_PAGE: u16 = 32;
/// Unanswered manager requests projected per work view (Issue #548).
pub const MANAGER_WORK_VIEW_MAX_RELAY: usize = 8;

const fn default_work_view_limit() -> u16 {
    MANAGER_WORK_VIEW_MAX_PAGE
}

/// Read-only work/ownership projection for a session the current manager
/// created (Issue #548). Caller, Epic, manager and scope are daemon-derived;
/// the request carries only page selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerWorkViewRequestV1 {
    #[serde(default)]
    pub work_key: Option<String>,
    #[serde(default)]
    pub after_work_key: Option<String>,
    #[serde(default = "default_work_view_limit")]
    pub limit: u16,
}

impl Default for AgentManagerWorkViewRequestV1 {
    fn default() -> Self {
        Self {
            work_key: None,
            after_work_key: None,
            limit: default_work_view_limit(),
        }
    }
}

impl AgentManagerWorkViewRequestV1 {
    /// Check page bounds before any authority read.
    ///
    /// # Errors
    ///
    /// `manager_invalid_work_view_request` for a limit outside 1..=32 or an
    /// empty, oversized or NUL-bearing work key.
    pub fn validate(&self) -> Result<(), &'static str> {
        let bad_key = |key: &Option<String>| {
            key.as_ref()
                .is_some_and(|k| k.is_empty() || k.len() > 256 || k.contains('\0'))
        };
        if self.limit == 0
            || self.limit > MANAGER_WORK_VIEW_MAX_PAGE
            || bad_key(&self.work_key)
            || bad_key(&self.after_work_key)
        {
            return Err("manager_invalid_work_view_request");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerWorkViewStageV1 {
    pub stage: crate::harness_manager_v2::ManagerWorkStageV2,
    pub state: crate::harness_manager_v2::ManagerStageStateV2,
    pub updated_at: DateTime<Utc>,
}

/// Current review assignment for the Work's recorded source, without the
/// manager-only review history or reviewer custody details.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerWorkViewReviewV1 {
    pub state: String,
    pub verdict: Option<String>,
    pub blocking_finding_count: i64,
}

/// One live work item of the caller's Epic. Stage notes, evidence paths and
/// acceptance digests stay manager-facing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerWorkViewWorkV1 {
    pub work_key: String,
    pub title: String,
    pub kind: crate::harness_manager_v2::ManagerWorkKindV2,
    pub spec_revision: i64,
    pub row_version: i64,
    /// The recorded source session is the caller or its rotation lineage.
    pub mine: bool,
    pub source_session_id: Option<Uuid>,
    pub source_commit: Option<String>,
    pub stages: Vec<ManagerWorkViewStageV1>,
    /// Kept for existing callers; equivalent to `source_accepted`.
    pub accepted: bool,
    pub source_accepted: bool,
    pub source_acceptance_recorded: bool,
    pub evidence_state: String,
    pub current_review: Option<ManagerWorkViewReviewV1>,
    pub integrated: bool,
}

/// One active manager-granted file ownership claim of a listed work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerWorkViewOwnershipV1 {
    pub key: String,
    pub work_key: String,
    pub domain: String,
    pub mode: crate::harness_manager_v2::ManagerOwnershipModeV2,
    pub files: Vec<String>,
    pub active: bool,
    pub row_version: i64,
    pub updated_at: DateTime<Utc>,
}

/// Delivery state of one unanswered manager request to the caller's Epic lead.
///
/// Message bodies are deliberately omitted. `delivered_at` is provider
/// delivery of the notice; `retrieved_at` is durable inbox retrieval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerWorkViewRelayV1 {
    pub request_id: Uuid,
    pub state: String,
    pub retrieved: bool,
    pub replied: bool,
    pub delivery_issue: Option<String>,
    pub queued_at: DateTime<Utc>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub retrieved_at: Option<DateTime<Utc>>,
    pub settled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentManagerWorkViewResultV1 {
    pub observed_at: DateTime<Utc>,
    pub epic_id: Uuid,
    pub manager_session_id: Uuid,
    pub scope_version: i64,
    pub policy_version: i64,
    /// Operator pause of the manager or this Epic; reported, never refused.
    pub paused: bool,
    pub works: Vec<ManagerWorkViewWorkV1>,
    pub ownership: Vec<ManagerWorkViewOwnershipV1>,
    pub relay: Vec<ManagerWorkViewRelayV1>,
    /// More than `MANAGER_WORK_VIEW_MAX_RELAY` unanswered requests exist.
    pub more_relay: bool,
    pub next_after_work_key: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_current_session_id_decodes_to_none() {
        let config = HarnessManagerConfigV1 {
            project_id: Uuid::new_v4(),
            manager_session_id: Uuid::new_v4(),
            current_session_id: Some(Uuid::new_v4()),
            epic_ids: Vec::new(),
            scope_mode: HarnessManagerScopeModeV1::Selected,
            selected_epic_ids: None,
            group_ids: Vec::new(),
            row_version: 1,
            updated_at: Utc::now(),
        };
        let mut wire = serde_json::to_value(config).unwrap();
        wire.as_object_mut().unwrap().remove("current_session_id");
        let decoded: HarnessManagerConfigV1 = serde_json::from_value(wire).unwrap();
        assert_eq!(decoded.current_session_id, None);
    }
    use serde_json::json;

    #[test]
    fn legacy_inbox_result_decodes_without_notice_fields() {
        let result: AgentManagerInboxResultV1 = serde_json::from_value(json!({
            "messages": [],
            "next_after_sequence": null
        }))
        .unwrap();
        assert!(result.notices.is_empty());
        assert!(!result.more_notices);
        assert_eq!(result.manager_seat, None);
        assert_eq!(result.next_after_notice_sequence, None);
        assert!(result.settled_notice_ids.is_empty());
    }

    #[test]
    fn inbox_notice_controls_are_optional_and_bounded() {
        let legacy: AgentManagerInboxRequestV1 = serde_json::from_value(json!({
            "after_sequence": 7, "limit": 2, "request_id": null
        }))
        .unwrap();
        assert_eq!(legacy.after_sequence, 7);
        assert_eq!(legacy.after_notice_sequence, 0);
        assert_eq!(legacy.notice_kind, None);
        assert!(legacy.settle_notice_ids.is_empty());
        assert!(legacy.validate().is_ok());
        for value in [
            json!({"after_notice_sequence": -1}),
            json!({"notice_kind": "unknown"}),
            json!({"settle_notice_ids": [Uuid::nil()]}),
            json!({"settle_notice_ids": vec![Uuid::new_v4(); 33]}),
        ] {
            let invalid: AgentManagerInboxRequestV1 = serde_json::from_value(value).unwrap();
            assert_eq!(invalid.validate(), Err("manager_invalid_inbox_page"));
        }
        let valid: AgentManagerInboxRequestV1 = serde_json::from_value(json!({
            "after_notice_sequence": 42, "notice_kind": "ledger_change",
            "settle_notice_ids": [Uuid::new_v4()]
        }))
        .unwrap();
        assert!(valid.validate().is_ok());
    }

    #[test]
    fn harness_manager_seat_state_round_trips_on_inbox_and_receipt() {
        let seat = ManagerSeatStateV1 {
            state: ManagerSeatConditionV1::Exhausted,
            tip_session_id: Uuid::new_v4(),
            since: Utc::now(),
            attempts: 2,
            max_attempts: 2,
            not_before: None,
            reason: "manager_seat_recovery_budget_exhausted".into(),
            next_action: Some("resume, appoint or succeed the manager".into()),
            last_invocation_id: Some(Uuid::new_v4()),
            last_error_class: Some("failed".into()),
            last_terminal_reason: Some("aborted_streaming".into()),
        };
        let receipt = HarnessManagerMessageReceiptV1 {
            message_id: Uuid::new_v4(),
            sequence: 7,
            request_id: None,
            deduplicated: false,
            rolled_over_from: None,
            manager_seat: Some(seat.clone()),
        };
        let wire = serde_json::to_value(&receipt).unwrap();
        assert_eq!(wire["manager_seat"]["state"], "exhausted");
        assert_eq!(
            serde_json::from_value::<HarnessManagerMessageReceiptV1>(wire).unwrap(),
            receipt
        );
        let legacy: HarnessManagerMessageReceiptV1 = serde_json::from_value(json!({
            "message_id": Uuid::new_v4(), "sequence": 1, "request_id": null, "deduplicated": true
        }))
        .unwrap();
        assert_eq!(legacy.manager_seat, None);
        let inbox = AgentManagerInboxResultV1 {
            messages: vec![],
            next_after_sequence: None,
            notices: vec![],
            more_notices: false,
            next_after_notice_sequence: None,
            settled_notice_ids: Vec::new(),
            manager_seat: Some(seat.clone()),
            undelivered_tier_mail: vec![],
            more_undelivered_tier_mail: false,
        };
        let decoded: AgentManagerInboxResultV1 =
            serde_json::from_value(serde_json::to_value(&inbox).unwrap()).unwrap();
        assert_eq!(decoded.manager_seat, Some(seat.clone()));
        assert!(seat.is_down());
        assert_eq!(seat.state.label(), "exhausted");
    }

    #[test]
    fn harness_manager_project_default_preserves_explicit_revocation_and_group_selection() {
        let mut value = json!({"project_id": Uuid::new_v4(), "session_id": Uuid::new_v4(), "expected_row_version": 0});
        let default: ConfigureHarnessManagerRequestV1 =
            serde_json::from_value(value.clone()).unwrap();
        assert_eq!(default.scope_mode(), HarnessManagerScopeModeV1::Project);
        assert!(default.validate().is_ok());
        value["epic_ids"] = json!([]);
        let revoke: ConfigureHarnessManagerRequestV1 =
            serde_json::from_value(value.clone()).unwrap();
        assert!(revoke.is_revocation());
        assert_eq!(revoke.scope_mode(), HarnessManagerScopeModeV1::Selected);
        value["group_ids"] = json!([Uuid::new_v4()]);
        let groups: ConfigureHarnessManagerRequestV1 =
            serde_json::from_value(value.clone()).unwrap();
        assert_eq!(groups.scope_mode(), HarnessManagerScopeModeV1::Selected);
        assert!(!groups.is_revocation());
        assert!(groups.validate().is_ok());
        value["group_ids"] = json!([groups.group_ids[0], groups.group_ids[0]]);
        assert_eq!(
            serde_json::from_value::<ConfigureHarnessManagerRequestV1>(value)
                .unwrap()
                .validate(),
            Err("manager_duplicate_group")
        );
    }

    #[test]
    fn harness_manager_agent_inputs_reject_forged_identity() {
        let value = json!({"epic_id": Uuid::new_v4(), "message": "status?",
            "idempotency_key": "one", "sender_session_id": Uuid::new_v4()});
        assert!(serde_json::from_value::<AgentManagerSendRequestV1>(value).is_err());
        assert!(
            serde_json::from_value::<AgentManagerProgressRequestV1>(
                json!({"manager_session_id": Uuid::new_v4()})
            )
            .is_err()
        );
        // #1235: project_id is the global seat's target project, not identity.
        let project = Uuid::new_v4();
        let targeted: AgentManagerProgressRequestV1 =
            serde_json::from_value(json!({"project_id": project})).unwrap();
        assert_eq!(targeted.project_id, Some(project));
        assert!(
            serde_json::from_value::<AgentManagerReplyRequestV1>(
                json!({"request_id": Uuid::new_v4(), "message": "ready",
                "idempotency_key": "reply", "recipient_session_id": Uuid::new_v4()})
            )
            .is_err()
        );
    }

    #[test]
    fn harness_manager_bounds_are_bytes_and_pages_are_bounded() {
        let id = Uuid::new_v4();
        let mut notice = AgentManagerNotifyRequestV1 {
            message: "status".into(),
            idempotency_key: "key".into(),
        };
        assert_eq!(notice.validate(), Ok(()));
        notice.message = " ".into();
        assert_eq!(notice.validate(), Err("manager_invalid_message"));
        notice.message = "status".into();
        notice.idempotency_key = "".into();
        assert_eq!(notice.validate(), Err("manager_invalid_idempotency_key"));
        assert!(validate_manager_message(id, &"é".repeat(4096), "one").is_ok());
        assert!(validate_manager_message(id, &"é".repeat(4097), "one").is_err());
        assert!(validate_manager_message(id, " ", "one").is_err());
        let mut request = AgentManagerInboxRequestV1::default();
        assert!(request.validate().is_ok());
        request.limit = 33;
        assert!(request.validate().is_err());
        let config = ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: Uuid::new_v4(),
            session_id: id,
            epic_ids: Some(vec![id, id]),
            expected_row_version: 0,
        };
        assert_eq!(config.validate(), Err("manager_duplicate_epic"));
    }

    #[test]
    fn scope_save_result_is_a_config_with_an_additive_policy_outcome() {
        let config = HarnessManagerConfigV1 {
            project_id: Uuid::new_v4(),
            manager_session_id: Uuid::new_v4(),
            current_session_id: None,
            epic_ids: Vec::new(),
            scope_mode: HarnessManagerScopeModeV1::Selected,
            selected_epic_ids: None,
            group_ids: Vec::new(),
            row_version: 2,
            updated_at: chrono::Utc::now(),
        };
        // A legacy daemon's bare config reads as "no policy outcome".
        let legacy = serde_json::to_value(&config).unwrap();
        let read: ConfigureHarnessManagerResultV1 = serde_json::from_value(legacy).unwrap();
        assert_eq!(read.config, config);
        assert_eq!(
            read.policy.kind,
            HarnessManagerPolicyOutcomeKindV1::NoPolicy
        );
        // A new result still reads as a plain config for older clients.
        let result = ConfigureHarnessManagerResultV1 {
            config: config.clone(),
            policy: HarnessManagerPolicyOutcomeV1 {
                kind: HarnessManagerPolicyOutcomeKindV1::RevokedNeedsConfirmation,
                capability_count: 2,
                paused_epic_ids: vec![Uuid::new_v4()],
            },
        };
        let wire = serde_json::to_value(&result).unwrap();
        assert_eq!(wire["policy"]["kind"], "revoked_needs_confirmation");
        assert_eq!(
            serde_json::from_value::<HarnessManagerConfigV1>(wire.clone()).unwrap(),
            config
        );
        assert_eq!(
            serde_json::from_value::<ConfigureHarnessManagerResultV1>(wire).unwrap(),
            result
        );
    }
}
