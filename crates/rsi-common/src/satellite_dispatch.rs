//! #1017 slice 3: queued delivery from the hub manager to an idle satellite
//! session.
//!
//! `AgentSendSatelliteMessage` is the only agent-facing verb. It queues one
//! message for a session the operator declared in scope for a dispatch-enabled
//! peer. The hub delivers it only while the remote session is idle, over the
//! single `DeliverHubMessage` method the satellite accepts, and never
//! interrupts a running session. Every refusal that could reveal whether a
//! peer or session exists is the same `target_not_authorized` code.

use crate::satellite::SatelliteUuidV1;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Wire method the satellite accepts for hub delivery (its own allowlist).
pub const DELIVER_HUB_MESSAGE_METHOD: &str = "DeliverHubMessage";
/// Wire method the satellite accepts for a hub-initiated deploy (#1017 slice
/// 2). Same allowlisted-hub check as delivery.
pub const REQUEST_HUB_DEPLOY_METHOD: &str = "RequestHubDeploy";
pub const SATELLITE_MESSAGE_MAX_BYTES: usize = 16_384;
pub const SATELLITE_MESSAGE_MAX_KEY_BYTES: usize = 128;
pub const SATELLITE_SENDER_LABEL_MAX_BYTES: usize = 128;
/// Per-owner cap on messages that are still `queued`.
pub const SATELLITE_MESSAGE_MAX_QUEUED: usize = 64;
/// Default lifetime, matching `AgentSendMessage`.
pub const SATELLITE_MESSAGE_DEFAULT_TTL_SECS: i64 = 30 * 60;
pub const SATELLITE_MESSAGE_MAX_TTL_SECS: i64 = 24 * 60 * 60;
pub const SATELLITE_SCOPE_MAX_SESSIONS: usize = 64;
pub const SATELLITE_INBOUND_MAX_ENTRIES: usize = 32;

/// Uniform refusal: unknown peer, unpaired peer, dispatch off, and
/// out-of-scope target all look identical to the caller.
pub const SATELLITE_TARGET_NOT_AUTHORIZED: &str = "target_not_authorized";
/// The satellite refused because the target session is not idle.
pub const SATELLITE_BUSY: &str = "busy";
pub const SATELLITE_MESSAGE_INVALID: &str = "satellite_message_invalid";
pub const SATELLITE_MESSAGE_KEY_INVALID: &str = "satellite_message_key_invalid";
pub const SATELLITE_MESSAGE_EXPIRY_INVALID: &str = "satellite_message_expiry_invalid";
pub const SATELLITE_MESSAGE_QUEUE_FULL: &str = "satellite_message_queue_full";
pub const SATELLITE_MESSAGE_KEY_CONFLICT: &str = "satellite_message_key_conflict";

/// Static, secret-free classes for a satellite whose continuation of the
/// target session failed after the write-ahead record (#1087). The satellite
/// sends exactly one of these in place of the provider error text, and the hub
/// stores it as the message's `safe_error_class`. Provider stderr and paths
/// never cross the link.
pub const SATELLITE_LAUNCH_FAILED_PROVIDER_BINARY_MISSING: &str =
    "satellite_target_launch_failed:provider_binary_missing";
pub const SATELLITE_LAUNCH_FAILED_PROVIDER_SPAWN: &str =
    "satellite_target_launch_failed:provider_spawn_failed";
pub const SATELLITE_LAUNCH_FAILED_CONTINUATION: &str =
    "satellite_target_launch_failed:continuation_failed";
/// Every launch-failure class, for exact matching on the hub.
pub const SATELLITE_LAUNCH_FAILED_CLASSES: [&str; 3] = [
    SATELLITE_LAUNCH_FAILED_PROVIDER_BINARY_MISSING,
    SATELLITE_LAUNCH_FAILED_PROVIDER_SPAWN,
    SATELLITE_LAUNCH_FAILED_CONTINUATION,
];

/// The static launch-failure class named in a satellite's refusal text, if
/// any. The hub stores the returned constant, never the text.
#[must_use]
pub fn satellite_launch_failure_class(message: &str) -> Option<&'static str> {
    SATELLITE_LAUNCH_FAILED_CLASSES
        .into_iter()
        .find(|class| message.contains(class))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSendSatelliteMessageRequestV1 {
    pub peer_id: SatelliteUuidV1,
    pub remote_session_id: SatelliteUuidV1,
    pub message: String,
    pub idempotency_key: String,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

impl AgentSendSatelliteMessageRequestV1 {
    /// # Errors
    /// The stable code of the first invalid field.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.peer_id.0.is_nil() || self.remote_session_id.0.is_nil() {
            return Err(SATELLITE_TARGET_NOT_AUTHORIZED);
        }
        if self.message.trim().is_empty()
            || self.message.len() > SATELLITE_MESSAGE_MAX_BYTES
            || self.message.contains('\0')
        {
            return Err(SATELLITE_MESSAGE_INVALID);
        }
        let key = &self.idempotency_key;
        if key.is_empty() || key.len() > SATELLITE_MESSAGE_MAX_KEY_BYTES || key.contains('\0') {
            return Err(SATELLITE_MESSAGE_KEY_INVALID);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSendSatelliteMessageReceiptV1 {
    pub message_id: Uuid,
    /// `queued`, `sent`, `failed` or `expired`. `queued` is acceptance only:
    /// it proves neither delivery nor that the remote agent read anything.
    pub state: String,
    pub expires_at: DateTime<Utc>,
    /// True when an identical earlier request was replayed.
    pub replayed: bool,
    /// The stable class a settled message carries: `delivery_uncertain` when
    /// the satellite could not say whether an earlier attempt took effect
    /// (never replayed), a refusal class such as `target_not_authorized`, or a
    /// launch-failure class such as
    /// `satellite_target_launch_failed:provider_binary_missing` (#1087).
    /// Replaying the same idempotent request is the typed read of a message's
    /// current state and class.
    /// Absent while `queued`, and for `sent` and `expired`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_error_class: Option<String>,
}

/// Operator-only: replace the declared dispatch scope of one peer. An empty
/// list clears it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatellitePutScopeRequestV1 {
    pub expected_registry_revision: u64,
    pub peer_id: SatelliteUuidV1,
    pub remote_session_ids: Vec<SatelliteUuidV1>,
}

/// Hub to satellite. The satellite trusts nothing here beyond what its own
/// operator-set policy allows.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatelliteDeliverRequestV1 {
    pub wire_version: u16,
    pub message_id: Uuid,
    pub hub_installation_id: SatelliteUuidV1,
    pub sender_label: String,
    pub sender_session_id: Uuid,
    pub remote_session_id: SatelliteUuidV1,
    pub message: String,
}

impl SatelliteDeliverRequestV1 {
    /// # Errors
    /// The stable code of the first invalid field.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.wire_version != crate::satellite::SATELLITE_WIRE_VERSION_V1
            || self.message_id.is_nil()
            || self.hub_installation_id.0.is_nil()
            || self.sender_session_id.is_nil()
            || self.remote_session_id.0.is_nil()
            || self.sender_label.len() > SATELLITE_SENDER_LABEL_MAX_BYTES
            || self.sender_label.chars().any(char::is_control)
        {
            return Err(SATELLITE_MESSAGE_INVALID);
        }
        if self.message.trim().is_empty()
            || self.message.len() > SATELLITE_MESSAGE_MAX_BYTES
            || self.message.contains('\0')
        {
            return Err(SATELLITE_MESSAGE_INVALID);
        }
        Ok(())
    }
}

/// Hub to satellite (#1017 slice 2): run this satellite's own deploy flow
/// (stage, verify, quiet point, exit 75, startup verification) locally.
/// `binaries_dir` is a path on the satellite. The satellite trusts only its own
/// operator-set policy: the hub installation must be allowlisted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatelliteDeployRequestV1 {
    pub wire_version: u16,
    pub hub_installation_id: SatelliteUuidV1,
    pub sender_label: String,
    /// The hub manager session that asked; audit and idempotency namespace.
    pub sender_session_id: Uuid,
    pub sha: String,
    pub binaries_dir: String,
    pub idempotency_key: String,
    #[serde(default)]
    pub max_wait_secs: Option<u32>,
}

impl SatelliteDeployRequestV1 {
    /// # Errors
    /// The stable code of the first invalid field.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.wire_version != crate::satellite::SATELLITE_WIRE_VERSION_V1
            || self.hub_installation_id.0.is_nil()
            || self.sender_session_id.is_nil()
            || self.sender_label.len() > SATELLITE_SENDER_LABEL_MAX_BYTES
            || self.sender_label.chars().any(char::is_control)
        {
            return Err(SATELLITE_MESSAGE_INVALID);
        }
        self.as_local_request().validate()
    }

    /// The local deploy request this wire request stands for. The idempotency
    /// key is namespaced by the hub identity so two hubs (or two hub manager
    /// sessions) cannot collide on one satellite owner row.
    #[must_use]
    pub fn as_local_request(&self) -> crate::agent_deploy::AgentRequestDeployRequestV1 {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        for part in [
            self.hub_installation_id.0.to_string(),
            self.sender_session_id.to_string(),
            self.idempotency_key.clone(),
        ] {
            hasher.update(u64::try_from(part.len()).unwrap_or(u64::MAX).to_be_bytes());
            hasher.update(part.as_bytes());
        }
        let namespaced = if self.idempotency_key.is_empty()
            || self.idempotency_key.len() > crate::agent_deploy::DEPLOY_MAX_KEY_BYTES
        {
            // Keep the original so validate() reports the stable key code.
            self.idempotency_key.clone()
        } else {
            format!("hub-{:x}", hasher.finalize())
        };
        crate::agent_deploy::AgentRequestDeployRequestV1 {
            sha: self.sha.clone(),
            binaries_dir: Some(self.binaries_dir.clone()),
            build: None,
            idempotency_key: namespaced,
            max_wait_secs: self.max_wait_secs,
            peer_id: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SatelliteDeliverOutcomeV1 {
    Delivered,
    /// A replay of a `message_id` the satellite already accepted; no second
    /// delivery happened.
    AlreadyDelivered,
    /// An earlier attempt for this `message_id` was recorded before its
    /// effect and never settled (a crash or store failure in between), so
    /// whether it was delivered is unknown. The satellite did not deliver
    /// again and never will: hub mail is at most once (#1059).
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteDeliverResultV1 {
    pub message_id: Uuid,
    pub outcome: SatelliteDeliverOutcomeV1,
}

/// Satellite-side, operator-set. Empty lists refuse everything.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatelliteInboundPolicyV1 {
    /// Hub installation ids allowed to deliver.
    #[serde(default)]
    pub allowed_hub_installations: Vec<SatelliteUuidV1>,
    /// Scope roots (the satellite's manager seat sessions); their children,
    /// resolved from `parent_id`, are in scope too.
    #[serde(default)]
    pub scope_roots: Vec<SatelliteUuidV1>,
}

impl SatelliteInboundPolicyV1 {
    /// # Errors
    /// A stable code when a list is too long, holds a nil id or repeats one.
    pub fn validate(&self) -> Result<(), &'static str> {
        for list in [&self.allowed_hub_installations, &self.scope_roots] {
            let mut seen = std::collections::HashSet::new();
            if list.len() > SATELLITE_INBOUND_MAX_ENTRIES
                || list.iter().any(|id| id.0.is_nil() || !seen.insert(id.0))
            {
                return Err(SATELLITE_MESSAGE_INVALID);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutSatelliteInboundPolicyRequestV1 {
    pub policy: SatelliteInboundPolicyV1,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> AgentSendSatelliteMessageRequestV1 {
        AgentSendSatelliteMessageRequestV1 {
            peer_id: SatelliteUuidV1(Uuid::new_v4()),
            remote_session_id: SatelliteUuidV1(Uuid::new_v4()),
            message: "hello".into(),
            idempotency_key: "k1".into(),
            expires_at: None,
        }
    }

    #[test]
    fn valid_request_passes_and_bad_fields_get_stable_codes() {
        assert!(request().validate().is_ok());
        let mut empty = request();
        empty.message = "  ".into();
        assert_eq!(empty.validate(), Err(SATELLITE_MESSAGE_INVALID));
        let mut key = request();
        key.idempotency_key = String::new();
        assert_eq!(key.validate(), Err(SATELLITE_MESSAGE_KEY_INVALID));
        let mut nil = request();
        nil.peer_id = SatelliteUuidV1(Uuid::nil());
        assert_eq!(nil.validate(), Err(SATELLITE_TARGET_NOT_AUTHORIZED));
    }

    #[test]
    fn request_rejects_unknown_fields_so_caller_identity_cannot_be_forged() {
        let mut value = serde_json::to_value(request()).unwrap();
        value["owner_session_id"] = serde_json::json!(Uuid::new_v4());
        assert!(serde_json::from_value::<AgentSendSatelliteMessageRequestV1>(value).is_err());
    }

    fn deploy_wire() -> SatelliteDeployRequestV1 {
        SatelliteDeployRequestV1 {
            wire_version: crate::satellite::SATELLITE_WIRE_VERSION_V1,
            hub_installation_id: SatelliteUuidV1(Uuid::new_v4()),
            sender_label: "manager".into(),
            sender_session_id: Uuid::new_v4(),
            sha: "a".repeat(40),
            binaries_dir: "/home/u/.rsi/staging/bin-a".into(),
            idempotency_key: "sat-deploy-1".into(),
            max_wait_secs: None,
        }
    }

    #[test]
    fn deploy_wire_validates_and_namespaces_the_key_by_hub_identity() {
        let wire = deploy_wire();
        assert_eq!(wire.validate(), Ok(()));
        let local = wire.as_local_request();
        assert!(local.idempotency_key.starts_with("hub-"));
        assert_eq!(
            local.idempotency_key,
            wire.as_local_request().idempotency_key
        );
        let mut other_hub = deploy_wire();
        other_hub.idempotency_key = wire.idempotency_key.clone();
        assert_ne!(
            other_hub.as_local_request().idempotency_key,
            local.idempotency_key
        );
        let mut relative = deploy_wire();
        relative.binaries_dir = "bin".into();
        assert_eq!(
            relative.validate(),
            Err(crate::agent_deploy::DEPLOY_DIR_NOT_ALLOWED)
        );
        let mut key = deploy_wire();
        key.idempotency_key = String::new();
        assert_eq!(key.validate(), Err(crate::agent_deploy::DEPLOY_KEY_INVALID));
    }

    #[test]
    fn inbound_policy_defaults_to_refusing_everything() {
        let policy = SatelliteInboundPolicyV1::default();
        assert!(policy.allowed_hub_installations.is_empty());
        assert!(policy.scope_roots.is_empty());
        let mut repeated = SatelliteInboundPolicyV1::default();
        let id = SatelliteUuidV1(Uuid::new_v4());
        repeated.scope_roots = vec![id, id];
        assert!(repeated.validate().is_err());
    }
}
