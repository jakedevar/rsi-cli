//! Fractal manager hierarchy S4 (#1238): reports and escalations route up N
//! levels to the operator; down-mail reaches any descendant seat.
//!
//! One routing primitive, `parent_of(ManagerNodeRefV1)`, decides every hop:
//! - `Area a` -> its parent area, or `Project(p)` under the project root;
//! - `Project p` -> the deepest active portfolio node covering `p`, else the
//!   operator;
//! - `Portfolio n` -> its grant's live parent node, else the operator.
//!
//! Reports and escalations are mail: they never carry authority. A report or
//! escalation that reaches the top becomes an operator queue row, which only
//! the operator-only RPCs below read and settle. A ruling is a manager
//! decision; it never answers a human approval.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The operator-only tier-routing RPCs (never in an agent catalog).
pub const OPERATOR_METHODS: [&str; 3] = [
    "ListOperatorEscalations",
    "RuleOperatorEscalation",
    "AcknowledgeOperatorNotice",
];

/// Largest report or down-mail body (bytes); the v0 global limit.
pub const MANAGER_TIER_MAX_MESSAGE_BYTES: usize = 32 * 1024;
/// Largest operator ruling (bytes); the escalation ruling limit.
pub const MANAGER_TIER_MAX_RULING_BYTES: usize = 8192;
/// Most queued messages one recipient (or one sender) may hold.
pub const MANAGER_TIER_MAX_PENDING: i64 = 64;
/// Most rows `ListOperatorEscalations` returns per list.
pub const OPERATOR_QUEUE_LIMIT: usize = 256;

/// The `target_ref`/`source_ref` of the operator, the top of every chain.
pub const OPERATOR_REF: &str = "operator";

/// The request is malformed.
pub const MANAGER_TIER_INVALID_REQUEST: &str = "manager_tier_invalid_request";
/// The caller holds no manager node seat (portfolio, project or area).
pub const MANAGER_TIER_NOT_NODE_SEAT: &str = "manager_tier_not_node_seat";
/// `AgentSendDown` named the caller itself, an ancestor or a node that is not
/// below the caller.
pub const MANAGER_TARGET_NOT_DESCENDANT: &str = "manager_target_not_descendant";
/// The named node does not exist or is not active.
pub const MANAGER_TIER_TARGET_UNKNOWN: &str = "manager_tier_target_unknown";
/// The target node has no live seat to deliver to.
pub const MANAGER_TIER_TARGET_VACANT: &str = "manager_tier_target_vacant";
/// A reused idempotency key carries a different request.
pub const MANAGER_TIER_IDEMPOTENCY_CONFLICT: &str = "manager_tier_idempotency_conflict";
/// The recipient or the sender already holds the most queued messages.
pub const MANAGER_TIER_MAILBOX_FULL: &str = "manager_tier_mailbox_full";
/// The escalation is held above the project root; only the addressed tier
/// may rule or forward it.
pub const MANAGER_ESCALATION_FORWARDED_ABOVE: &str = "manager_node_escalation_forwarded_above";
/// The operator queue row is not open (already ruled, forwarded or retired).
pub const OPERATOR_ESCALATION_NOT_OPEN: &str = "operator_escalation_not_open";
/// No operator queue row has this id.
pub const OPERATOR_ESCALATION_NOT_FOUND: &str = "operator_escalation_not_found";
/// No operator notice has this id.
pub const OPERATOR_NOTICE_NOT_FOUND: &str = "operator_notice_not_found";

/// A manager node: the unit `parent_of` routes between. Tier names are
/// display labels of portfolio nodes and never appear here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagerNodeRefV1 {
    Portfolio { node_id: Uuid },
    Project { project_id: Uuid },
    Area { node_id: Uuid },
}

impl ManagerNodeRefV1 {
    /// The stored reference text: `portfolio:<id>`, `project:<id>`,
    /// `area:<id>`.
    #[must_use]
    pub fn as_ref_text(&self) -> String {
        match self {
            Self::Portfolio { node_id } => format!("portfolio:{node_id}"),
            Self::Project { project_id } => format!("project:{project_id}"),
            Self::Area { node_id } => format!("area:{node_id}"),
        }
    }

    /// Parse a stored reference (`None` for `operator` or anything else).
    #[must_use]
    pub fn parse_ref_text(text: &str) -> Option<Self> {
        let (kind, id) = text.split_once(':')?;
        let id = Uuid::parse_str(id).ok()?;
        if id.is_nil() || id.to_string() != text[kind.len() + 1..] {
            return None;
        }
        match kind {
            "portfolio" => Some(Self::Portfolio { node_id: id }),
            "project" => Some(Self::Project { project_id: id }),
            "area" => Some(Self::Area { node_id: id }),
            _ => None,
        }
    }

    fn validate(&self) -> Result<(), &'static str> {
        let id = match self {
            Self::Portfolio { node_id } | Self::Area { node_id } => node_id,
            Self::Project { project_id } => project_id,
        };
        if id.is_nil() {
            return Err(MANAGER_TIER_INVALID_REQUEST);
        }
        Ok(())
    }
}

fn valid_key(key: &str) -> bool {
    !key.trim().is_empty() && key.len() <= 128 && !key.contains('\0')
}

fn valid_text(text: &str, max: usize) -> bool {
    !text.trim().is_empty() && text.len() <= max && !text.contains('\0')
}

/// `AgentReportUp {message, idempotency_key}`: durable mail to
/// `parent_of(the caller's node)`; at the top, an operator notice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReportUpRequestV1 {
    pub message: String,
    pub idempotency_key: String,
}

impl AgentReportUpRequestV1 {
    /// # Errors
    /// A stable refusal code.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !valid_text(&self.message, MANAGER_TIER_MAX_MESSAGE_BYTES)
            || !valid_key(&self.idempotency_key)
        {
            return Err(MANAGER_TIER_INVALID_REQUEST);
        }
        Ok(())
    }
}

/// `AgentSendDown {target, message, idempotency_key}`: durable mail to a
/// descendant node's seat inside the caller's coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSendDownRequestV1 {
    pub target: ManagerNodeRefV1,
    pub message: String,
    pub idempotency_key: String,
}

impl AgentSendDownRequestV1 {
    /// # Errors
    /// A stable refusal code.
    pub fn validate(&self) -> Result<(), &'static str> {
        self.target.validate()?;
        if !valid_text(&self.message, MANAGER_TIER_MAX_MESSAGE_BYTES)
            || !valid_key(&self.idempotency_key)
        {
            return Err(MANAGER_TIER_INVALID_REQUEST);
        }
        Ok(())
    }
}

/// Receipt of one tier message. A message to a seat is a durable one-shot
/// resume wake (delivered at the recipient's next idle boundary, waking an
/// idle recipient); a message to the operator is an operator notice row and
/// `target_session_id` is `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerTierMessageReceiptV1 {
    pub message_id: Uuid,
    pub source_ref: String,
    pub target_ref: String,
    pub target_session_id: Option<Uuid>,
    pub deduplicated: bool,
}

/// One hop of an escalation above its project root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagerTierEscalationHopV1 {
    pub hop_id: Uuid,
    /// The originating in-project escalation (`manager_node_escalations.id`).
    pub escalation_id: Uuid,
    pub project_id: Uuid,
    pub subject_id: Uuid,
    pub reason: String,
    /// 1 for the hop that left the project root.
    pub hop: i64,
    pub source_ref: String,
    /// `portfolio:<id>` or `operator`.
    pub target_ref: String,
    pub target_session_id: Option<Uuid>,
    pub target_grant_version: Option<i64>,
    /// `open`, `forwarded`, `ruled` or `retired`.
    pub state: String,
    pub ruling: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One top-of-chain report: a notice for the operator, never authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorNoticeV1 {
    pub message_id: Uuid,
    pub source_ref: String,
    pub source_session_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub body: String,
    /// `queued` until acknowledged, then `delivered`.
    pub state: String,
    pub created_at: DateTime<Utc>,
}

/// Operator-only `ListOperatorEscalations {include_closed}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListOperatorEscalationsRequestV1 {
    /// Also list ruled/retired queue rows and acknowledged notices.
    #[serde(default)]
    pub include_closed: bool,
    /// #1295: page the failed and uncertain tier mail after this cursor
    /// (the previous result's `next_undelivered_after`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undelivered_after: Option<String>,
}

/// `ListOperatorEscalations` result, newest first, each list bounded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListOperatorEscalationsResultV1 {
    pub escalations: Vec<ManagerTierEscalationHopV1>,
    pub notices: Vec<OperatorNoticeV1>,
    /// #1266: tier mail whose delivery failed or is uncertain, any sender and
    /// recipient, newest first, one page (`OPERATOR_QUEUE_LIMIT`) at a time.
    /// Always listed; nothing reopens or replays it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub undelivered: Vec<UndeliveredTierMailV1>,
    /// #1295: set when older undelivered mail follows; pass it back as
    /// `undelivered_after` for the next page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_undelivered_after: Option<String>,
}

/// #1266: one tier message whose delivery `failed` (the continuation that
/// claimed it ended before any provider could see it) or is `uncertain` (the
/// provider may have seen it). Shown to its sender, its recipient and the
/// operator; under the #945 rule it is never replayed automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UndeliveredTierMailV1 {
    pub message_id: Uuid,
    /// `sender` or `recipient` in a seat's inbox; empty in the operator view.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub role: String,
    /// `message`, `report`, `escalation` or `ruling`.
    pub kind: String,
    pub source_ref: String,
    pub target_ref: String,
    pub source_session_id: Option<Uuid>,
    pub target_session_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub body: String,
    /// `failed` or `uncertain`.
    pub state: String,
    pub settle_reason: String,
    pub settled_at: DateTime<Utc>,
}

/// Operator-only `RuleOperatorEscalation {hop_id, ruling, idempotency_key}`:
/// the ruling returns down the recorded hop chain to the source seat. It is a
/// manager decision and never answers a human approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleOperatorEscalationRequestV1 {
    pub hop_id: Uuid,
    pub ruling: String,
    pub idempotency_key: String,
}

impl RuleOperatorEscalationRequestV1 {
    /// # Errors
    /// A stable refusal code.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.hop_id.is_nil()
            || !valid_text(&self.ruling, MANAGER_TIER_MAX_RULING_BYTES)
            || !valid_key(&self.idempotency_key)
        {
            return Err(MANAGER_TIER_INVALID_REQUEST);
        }
        Ok(())
    }
}

/// Operator-only `AcknowledgeOperatorNotice {message_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcknowledgeOperatorNoticeRequestV1 {
    pub message_id: Uuid,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_refs_round_trip_their_stored_text() {
        let id = Uuid::new_v4();
        for node in [
            ManagerNodeRefV1::Portfolio { node_id: id },
            ManagerNodeRefV1::Project { project_id: id },
            ManagerNodeRefV1::Area { node_id: id },
        ] {
            assert_eq!(
                ManagerNodeRefV1::parse_ref_text(&node.as_ref_text()),
                Some(node)
            );
        }
        assert_eq!(ManagerNodeRefV1::parse_ref_text(OPERATOR_REF), None);
        assert_eq!(
            ManagerNodeRefV1::parse_ref_text(&format!("epic:{id}")),
            None
        );
    }

    #[test]
    fn send_down_rejects_forged_fields_and_empty_bodies() {
        let value = serde_json::json!({
            "target": {"kind": "project", "project_id": Uuid::new_v4()},
            "message": "status?",
            "idempotency_key": "k",
            "caller": Uuid::new_v4(),
        });
        assert!(serde_json::from_value::<AgentSendDownRequestV1>(value).is_err());
        let forged_target = serde_json::json!({
            "target": {"kind": "project", "project_id": Uuid::new_v4(), "seat": Uuid::new_v4()},
            "message": "status?",
            "idempotency_key": "k",
        });
        assert!(serde_json::from_value::<AgentSendDownRequestV1>(forged_target).is_err());
        let blank = AgentReportUpRequestV1 {
            message: " ".into(),
            idempotency_key: "k".into(),
        };
        assert_eq!(blank.validate(), Err(MANAGER_TIER_INVALID_REQUEST));
        let nil = AgentSendDownRequestV1 {
            target: ManagerNodeRefV1::Area {
                node_id: Uuid::nil(),
            },
            message: "m".into(),
            idempotency_key: "k".into(),
        };
        assert_eq!(nil.validate(), Err(MANAGER_TIER_INVALID_REQUEST));
    }
}
