//! Operator-only Remote read of exact answerable decision occurrences.
//! Manager inbox items echo their row version and policy fence. Published
//! pending questions and native approvals outside that inbox echo a producer
//! occurrence and daemon digest, independently of manager configuration.
//! This optional read stays outside the six V1 observation methods and the
//! agent catalogs.

use crate::remote_read::{DecimalI64, Text, WireUuid};
use serde::{Deserialize, Serialize};

/// Strict operator-local request. Unknown and positional fields are refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteGetDecisionTargetsV1 {
    pub project_id: WireUuid,
    pub session_id: WireUuid,
}

/// Whether the project has a configured manager at all. Absence is not an
/// error: it is an empty, unambiguous answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionTargetsManagerV1 {
    Configured,
    NotConfigured,
}

/// The exact decision occurrence kind, taken from the decision key prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionTargetKindV1 {
    Question,
    Approval,
}

/// The manager fence the answer request must echo. Present only while the
/// project's current policy grant is live and aligned with the scope version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionTargetFenceV1 {
    pub policy_version: DecimalI64,
    pub scope_version: DecimalI64,
}

/// One bounded, answerable decision occurrence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionTargetItemV1 {
    pub decision_key: Text<160>,
    pub row_version: DecimalI64,
    pub target_digest: Text<128>,
    pub kind: DecisionTargetKindV1,
    pub title: Text<512>,
    pub detail: Text<2048, false>,
    pub options: Vec<Text<128>>,
}

/// Producer-published occurrence outside the manager inbox. The digest is
/// computed by the daemon; clients must echo both identities exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingDecisionTargetItemV1 {
    pub decision_id: Text<160>,
    pub target_digest: Text<128>,
    pub kind: DecisionTargetKindV1,
    pub title: Text<512>,
    pub detail: Text<2048, false>,
    pub options: Vec<Text<128>>,
    pub receipt: Option<crate::remote_pending_decisions::RemoteAnswerReceiptV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteGetDecisionTargetsResponseV1 {
    pub manager: DecisionTargetsManagerV1,
    pub fence: Option<DecisionTargetFenceV1>,
    pub items: Vec<DecisionTargetItemV1>,
    #[serde(default)]
    pub pending_items: Vec<PendingDecisionTargetItemV1>,
    pub truncated: bool,
}

/// Maximum answerable items one response may carry. More rows set `truncated`.
pub const MAX_DECISION_TARGETS: usize = 16;
/// Maximum option labels per item.
pub const MAX_DECISION_TARGET_OPTIONS: usize = 8;
