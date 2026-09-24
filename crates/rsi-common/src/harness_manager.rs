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
}

impl Default for AgentManagerInboxRequestV1 {
    fn default() -> Self {
        Self {
            after_sequence: 0,
            limit: default_inbox_limit(),
            request_id: None,
        }
    }
}

impl AgentManagerInboxRequestV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.after_sequence < 0
            || self.limit == 0
            || self.limit > HARNESS_MANAGER_MAX_INBOX_PAGE
            || self.request_id.is_some_and(|id| id.is_nil())
        {
            return Err("manager_invalid_inbox_page");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerSendRequestV1 {
    pub epic_id: Uuid,
    pub message: String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentManagerReplyRequestV1 {
    pub request_id: Uuid,
    pub message: String,
    pub idempotency_key: String,
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
    pub next_after_sequence: Option<i64>,
    /// Exact notice subjects settled by this retrieval. Added compatibly: old
    /// clients ignore the fields and missing fields decode as an empty page.
    #[serde(default)]
    pub notices: Vec<HarnessManagerNoticeV1>,
    #[serde(default)]
    pub more_notices: bool,
    /// Durable seat observation for the appointed manager (#669); absent
    /// when the daemon has never observed the seat down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager_seat: Option<ManagerSeatStateV1>,
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
    pub accepted: bool,
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
            manager_seat: Some(seat.clone()),
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
                json!({"project_id": Uuid::new_v4()})
            )
            .is_err()
        );
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
}
