//! Operator-only remote answers to producer-bound pending decisions.
use crate::remote_read::{Text, WireUuid};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteAnswerOriginV1 {
    pub kind: RemoteAnswerOriginKindV1,
    pub client_node: Text<128>,
    pub gateway_epoch: WireUuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAnswerOriginKindV1 {
    Remote,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerPendingDecisionV1 {
    pub project_id: WireUuid,
    pub session_id: WireUuid,
    pub decision_id: Text<160>,
    pub expected_target_digest: Text<128>,
    pub answer: Text<2048>,
    pub idempotency_key: WireUuid,
    pub origin: RemoteAnswerOriginV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAnswerStateV1 {
    Queued,
    Running,
    Succeeded,
    Refused,
    Failed,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteAnswerReceiptV1 {
    pub receipt_key: WireUuid,
    pub state: RemoteAnswerStateV1,
    pub outcome: Option<Value>,
}
