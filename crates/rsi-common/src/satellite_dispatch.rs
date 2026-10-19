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
/// Prefix that marks a project entry in operator scope text (`project:<uuid>`);
/// a bare uuid is a fixed session id (#1928).
pub const SATELLITE_SCOPE_PROJECT_PREFIX: &str = "project:";

/// Uniform refusal: unknown peer, unpaired peer, dispatch off, and
/// out-of-scope target all look identical to the caller.
pub const SATELLITE_TARGET_NOT_AUTHORIZED: &str = "target_not_authorized";
/// The satellite refused because the target session is neither idle nor live
/// (a live, running target is queued for its next tool or turn boundary
/// instead, #2566).
pub const SATELLITE_BUSY: &str = "busy";
/// `waiting_for` on a still-queued message (#2566): the last delivery attempt
/// found the remote target unable to take it (interrupted, or an older
/// satellite that only delivers at idle). Reported only while the target is
/// not terminal; a terminal target is `remote_terminal` (#2744).
pub const SATELLITE_WAITING_REMOTE_BUSY: &str = "remote_busy";
/// `waiting_for`: the last delivery attempt could not reach the paired peer.
pub const SATELLITE_WAITING_PEER_UNREACHABLE: &str = "peer_unreachable";
/// `waiting_for` (#2744): the remote target session is terminal (failed,
/// archived or deleted), so it can never take a turn until someone repairs it.
/// The receipt and the notice carry `remote_status` and `remote_stop_reason`.
pub const SATELLITE_WAITING_REMOTE_TERMINAL: &str = "remote_terminal";
/// Separator inside the satellite's typed `remote_terminal` refusal and the
/// hub's stored wait reason (`remote_terminal|<status>|<stop reason>`). A stop
/// reason such as `provider_error:codex_usage_limit` holds `:`, so it is not
/// the separator, and `|` is removed from the reason.
const REMOTE_TERMINAL_SEP: char = '|';
/// Longest stop reason carried across the link.
const REMOTE_STOP_REASON_MAX_BYTES: usize = 64;

/// The typed refusal a satellite answers with when the target is terminal
/// (#2744): `remote_terminal|<status>|<stop reason>`. `status` is the target's
/// session status (`failed`, `archived`, `deleted`). The stop reason is
/// reduced to a short token (ASCII letters, digits and `_ - . :` only, 64
/// bytes), so provider text, paths and ids never cross the link; `none` when
/// the session recorded no reason.
#[must_use]
pub fn remote_terminal_refusal(status: &str, stop_reason: Option<&str>) -> String {
    let status: String = status
        .chars()
        .filter(|c| c.is_ascii_lowercase() || *c == '_')
        .collect();
    let reason: String = stop_reason
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':') {
                c
            } else {
                '_'
            }
        })
        .take(REMOTE_STOP_REASON_MAX_BYTES)
        .collect();
    let reason = if reason.is_empty() {
        "none".into()
    } else {
        reason
    };
    format!(
        "{SATELLITE_WAITING_REMOTE_TERMINAL}{REMOTE_TERMINAL_SEP}{status}{REMOTE_TERMINAL_SEP}{reason}"
    )
}

/// The `remote_terminal|<status>|<reason>` token inside a refusal message or a
/// stored wait reason, whole, or `None` when there is none.
#[must_use]
pub fn find_remote_terminal(text: &str) -> Option<&str> {
    let prefix = format!("{SATELLITE_WAITING_REMOTE_TERMINAL}{REMOTE_TERMINAL_SEP}");
    let start = text.find(&prefix)?;
    let rest = &text[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
        .unwrap_or(rest.len());
    let token = &rest[..end];
    let mut parts = token.splitn(3, REMOTE_TERMINAL_SEP);
    let (_, Some(status), Some(reason)) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    (!status.is_empty() && !reason.is_empty()).then_some(token)
}

/// A stored wait reason split into its class and, for `remote_terminal`, the
/// target's status and stop reason: `(waiting_for, remote_status,
/// remote_stop_reason)`.
#[must_use]
pub fn split_waiting_for(stored: &str) -> (String, Option<String>, Option<String>) {
    let mut parts = stored.splitn(3, REMOTE_TERMINAL_SEP);
    match (parts.next(), parts.next(), parts.next()) {
        (Some(class), Some(status), Some(reason)) if class == SATELLITE_WAITING_REMOTE_TERMINAL => {
            (
                class.to_string(),
                Some(status.to_string()),
                Some(reason.to_string()),
            )
        }
        _ => (stored.to_string(), None, None),
    }
}
/// A message still `queued` this long after acceptance notifies its sender
/// once (#2566, extends #2456, which covers failed and expired messages).
pub const SATELLITE_MESSAGE_STALL_NOTICE_SECS: i64 = 10 * 60;
pub const SATELLITE_MESSAGE_INVALID: &str = "satellite_message_invalid";
pub const SATELLITE_MESSAGE_KEY_INVALID: &str = "satellite_message_key_invalid";
pub const SATELLITE_MESSAGE_EXPIRY_INVALID: &str = "satellite_message_expiry_invalid";
pub const SATELLITE_MESSAGE_QUEUE_FULL: &str = "satellite_message_queue_full";
pub const SATELLITE_MESSAGE_KEY_CONFLICT: &str = "satellite_message_key_conflict";
/// Most hub Issues one message may expand (`issue_refs`).
pub const SATELLITE_ISSUE_REFS_MAX: usize = 8;
/// `issue_refs` is malformed: a number below 1, a duplicate or too many.
pub const SATELLITE_ISSUE_REF_INVALID: &str = "satellite_issue_ref_invalid";
/// A referenced Issue is not one the sender may read: missing, another
/// project, or no Issue authority. One code for all three, so a refusal never
/// says whether an Issue exists.
pub const SATELLITE_ISSUE_REF_UNAVAILABLE: &str = "satellite_issue_ref_unavailable";
/// The message plus the expanded Issues exceed `SATELLITE_MESSAGE_MAX_BYTES`.
/// Nothing is truncated: send fewer Issues per message.
pub const SATELLITE_ISSUE_REFS_TOO_LARGE: &str = "satellite_issue_refs_too_large";

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
/// The target's sandbox custody refused the continuation (#2956): the session
/// has no live custody root it owns, or another owner holds it. Naming it lets
/// the hub's `satellite_message_failure` notice say why, not "continuation
/// failed". The specific `sandbox_custody:<code>` stays on the satellite.
pub const SATELLITE_LAUNCH_FAILED_SANDBOX_CUSTODY: &str =
    "satellite_target_launch_failed:sandbox_custody";
/// Every launch-failure class, for exact matching on the hub.
pub const SATELLITE_LAUNCH_FAILED_CLASSES: [&str; 4] = [
    SATELLITE_LAUNCH_FAILED_PROVIDER_BINARY_MISSING,
    SATELLITE_LAUNCH_FAILED_PROVIDER_SPAWN,
    SATELLITE_LAUNCH_FAILED_CONTINUATION,
    SATELLITE_LAUNCH_FAILED_SANDBOX_CUSTODY,
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
    /// The declared scope address: a remote session id the operator declared,
    /// or a declared project id (#1928), which the satellite resolves to that
    /// project's current appointed manager at delivery time.
    pub remote_session_id: SatelliteUuidV1,
    /// #1928: a declared project that authorizes the target when
    /// `remote_session_id` is a session id the hub cannot verify (a descendant
    /// of the project's current seat). The hub checks only that the peer
    /// declares this project; the satellite resolves the project's seat live
    /// and refuses a target that is not that seat or in its tree. Absent: the
    /// target itself must be a declared address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_project_id: Option<SatelliteUuidV1>,
    pub message: String,
    pub idempotency_key: String,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    /// Hub Issue display numbers the hub expands, whole, into the delivered
    /// message (read-only; no Issue write path). The Issues come from the
    /// sender's own project and only what the sender's `AgentGetIssue` could
    /// read; nothing here names a project. The expansion happens once, at
    /// acceptance and after the peer and target are authorized, and the
    /// message plus every Issue must fit `SATELLITE_MESSAGE_MAX_BYTES` or the
    /// request is refused (never truncated).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issue_refs: Vec<i64>,
}

impl AgentSendSatelliteMessageRequestV1 {
    /// # Errors
    /// The stable code of the first invalid field.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.peer_id.0.is_nil()
            || self.remote_session_id.0.is_nil()
            || self.scope_project_id.is_some_and(|id| id.0.is_nil())
        {
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
        self.validate_issue_refs()
    }

    fn validate_issue_refs(&self) -> Result<(), &'static str> {
        let refs = &self.issue_refs;
        let mut seen = std::collections::HashSet::new();
        if refs.len() > SATELLITE_ISSUE_REFS_MAX
            || refs
                .iter()
                .any(|number| *number < 1 || !seen.insert(*number))
        {
            return Err(SATELLITE_ISSUE_REF_INVALID);
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
    /// Delivery attempts the hub has made so far (0 on a fresh acceptance).
    #[serde(default)]
    pub attempt_count: u32,
    /// Why a message that is not delivered has not been yet: `remote_busy`
    /// (the target is interrupted or cannot take it now), `remote_terminal`
    /// (the target is failed, archived or deleted, #2744) or `peer_unreachable`. Present once an
    /// attempt found a reason, while `queued` and on an `expired` message (what
    /// it was waiting on); absent on a fresh acceptance and once `sent` or
    /// `failed`. Replaying the same idempotent request reads it with the
    /// attempt count (#2566).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    /// With `waiting_for: "remote_terminal"` (#2744): the target session's
    /// status (`failed`, `archived` or `deleted`) and its stop reason (`none`
    /// when it recorded none). The target can take no turn until repaired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_stop_reason: Option<String>,
}

/// Operator-only: replace the declared dispatch scope of one peer. Empty lists
/// clear it. `remote_project_ids` (#1928) name satellite projects whose current
/// appointed manager is the target; the satellite resolves the seat itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatellitePutScopeRequestV1 {
    pub expected_registry_revision: u64,
    pub peer_id: SatelliteUuidV1,
    pub remote_session_ids: Vec<SatelliteUuidV1>,
    #[serde(default)]
    pub remote_project_ids: Vec<SatelliteUuidV1>,
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
    /// #1928: the declared project that authorized the target at the hub; the
    /// satellite re-resolves it live (see `AgentSendSatelliteMessageRequestV1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_project_id: Option<SatelliteUuidV1>,
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
            || self.scope_project_id.is_some_and(|id| id.0.is_nil())
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
    /// #2587: interrupt a worker still mid-turn once the satellite's own drain
    /// hold is over (#1461). Sent only to a satellite advertising
    /// `deploy_interrupt`; an older satellite would refuse the unknown field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupt_workers: Option<bool>,
    /// Withdraw the matching waiting deploy already stored on this satellite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel: Option<bool>,
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
            project_id: None,
            sha: self.sha.clone(),
            binaries_dir: Some(self.binaries_dir.clone()),
            build: None,
            idempotency_key: namespaced,
            max_wait_secs: self.max_wait_secs,
            peer_id: None,
            cancel: self.cancel,
            interrupt_workers: self.interrupt_workers,
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
    /// #272: set when the hub addressed a manager seat another session has
    /// replaced and the satellite delivered to the current appointed seat.
    /// Absent from older satellites and from undirected deliveries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirected: Option<SatelliteDeliverRedirectV1>,
}

/// #272: the original and actual recipient of a redirected hub message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteDeliverRedirectV1 {
    /// The session id the hub addressed.
    pub addressed_session_id: Uuid,
    /// The replaced seat that address resolved to.
    pub displaced_session_id: Uuid,
    /// The project's current appointed seat, which received the message.
    pub delivered_session_id: Uuid,
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
    /// Scope projects (#1928): each names a project of THIS host whose current
    /// appointed manager seat, and its descendants, are in scope. The seat is
    /// resolved live (rotation and re-appointment follow it), so the operator
    /// declares it once. An unappointed or revoked project resolves to nothing.
    #[serde(default)]
    pub scope_projects: Vec<SatelliteUuidV1>,
}

impl SatelliteInboundPolicyV1 {
    /// # Errors
    /// A stable code when a list is too long, holds a nil id or repeats one.
    pub fn validate(&self) -> Result<(), &'static str> {
        for list in [
            &self.allowed_hub_installations,
            &self.scope_roots,
            &self.scope_projects,
        ] {
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

/// The kind of one declared scope entry (#1928).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SatelliteScopeKindV1 {
    /// A fixed session id (its rotation lineage tip is the seat).
    Session,
    /// A project: its current appointed manager is the seat.
    Project,
}

/// What a declared scope entry resolves to right now, on the host that owns
/// the sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SatelliteScopeStateV1 {
    /// Resolves to a live seat session.
    Live,
    /// The project has no appointed manager.
    Unappointed,
    /// The appointment, its scope or its policy is revoked.
    Revoked,
    /// The declared session (or its rotation lineage) has no live seat:
    /// unknown, archived, deleted, branched or too long.
    NoLiveSeat,
}

impl SatelliteScopeStateV1 {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Unappointed => "unappointed",
            Self::Revoked => "revoked",
            Self::NoLiveSeat => "no live seat",
        }
    }
}

/// One declared entry of this host's inbound scope and its live resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteScopeEntryStatusV1 {
    pub kind: SatelliteScopeKindV1,
    pub id: SatelliteUuidV1,
    pub state: SatelliteScopeStateV1,
    /// The resolved seat session when `state` is `live`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seat_session_id: Option<SatelliteUuidV1>,
}

/// Operator-only read (`GetSatelliteScopeStatus`): this host's declared inbound
/// scope with each entry resolved against the live sessions (#1928).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteScopeStatusV1 {
    pub inbound: Vec<SatelliteScopeEntryStatusV1>,
}

/// Parse operator scope text (comma-separated): a bare uuid is a fixed session
/// id, `project:<uuid>` is a project (#1928). Returns `(sessions, projects)`.
///
/// # Errors
/// The first entry that is not a canonical uuid.
pub fn parse_satellite_scope_text(
    text: &str,
) -> Result<(Vec<SatelliteUuidV1>, Vec<SatelliteUuidV1>), String> {
    let mut sessions = Vec::new();
    let mut projects = Vec::new();
    for part in text
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let (list, id_text, what) = match part.strip_prefix(SATELLITE_SCOPE_PROJECT_PREFIX) {
            Some(rest) => (&mut projects, rest.trim(), "project"),
            None => (&mut sessions, part, "session"),
        };
        let id = serde_json::from_value::<SatelliteUuidV1>(serde_json::Value::String(
            id_text.to_string(),
        ))
        .map_err(|error| format!("Invalid scope {what} ID `{id_text}`: {error}"))?;
        list.push(id);
    }
    Ok((sessions, projects))
}

/// The inverse of [`parse_satellite_scope_text`], for the edit form.
#[must_use]
pub fn format_satellite_scope_text(
    sessions: &[SatelliteUuidV1],
    projects: &[SatelliteUuidV1],
) -> String {
    sessions
        .iter()
        .map(|id| id.0.to_string())
        .chain(
            projects
                .iter()
                .map(|id| format!("{SATELLITE_SCOPE_PROJECT_PREFIX}{}", id.0)),
        )
        .collect::<Vec<_>>()
        .join(",")
}

// ---------------------------------------------------------------------------
// #1103: satellite -> hub typed reports. Informational only: a report carries
// no authority, and the hub verifies every SHA it acts on. The hub PULLS them
// over the existing hub-initiated link (`FetchHubReports`), so the satellite
// never dials the hub and the sender identity is the authenticated peer.
// ---------------------------------------------------------------------------

/// Wire method the satellite accepts to hand queued reports to an allowlisted
/// hub (its own one-method allowlist on the hub side).
pub const FETCH_HUB_REPORTS_METHOD: &str = "FetchHubReports";
/// Bound on one report's text (a short ENQUEUE / DEPLOY-READY / RESULT line).
pub const SATELLITE_REPORT_TEXT_MAX_BYTES: usize = 1_024;
/// Reports a satellite keeps un-acknowledged; further reports are refused.
pub const SATELLITE_REPORT_OUTBOX_MAX: usize = 32;
/// Reports handed over (and recorded) per fetch.
pub const SATELLITE_REPORT_MAX_PER_FETCH: usize = 8;
/// Acknowledged report ids the hub returns per fetch.
pub const SATELLITE_REPORT_MAX_ACKS: usize = 64;
/// Per-peer rate limit, enforced on the hub when it records a report.
pub const SATELLITE_REPORT_RATE_LIMIT: i64 = 12;
pub const SATELLITE_REPORT_RATE_WINDOW_SECS: i64 = 300;
/// Cumulative cap on retained report notices per peer. Notices are immutable
/// and never deleted, so the hub stops recording new reports from a peer at
/// this count and keeps them on the satellite (already-seen ids still dedupe).
pub const SATELLITE_REPORT_RETAINED_MAX_PER_PEER: i64 = 2048;
/// Uniform refusal: not the declared seat (or its current tip), no allowlisted
/// hub, or not the appointed manager. Static text, nothing about the satellite.
pub const SATELLITE_REPORT_NOT_AUTHORIZED: &str = "satellite_report_not_authorized";
pub const SATELLITE_REPORT_INVALID: &str = "satellite_report_invalid";
pub const SATELLITE_REPORT_QUEUE_FULL: &str = "satellite_report_queue_full";
/// Typed refusal of a new report while the hub is demonstrably not pulling:
/// the oldest queued report is past [`SATELLITE_REPORT_UNDELIVERED_AFTER_SECS`]
/// and no hub pull has been seen for as long. The manager keeps the report
/// (for example in its own outbox note) instead of trusting a `queued` receipt.
pub const SATELLITE_REPORT_HUB_UNREACHABLE: &str = "satellite_report_hub_unreachable";
/// A report nothing has happened to for this long is `undelivered`. The hub
/// pulls every 15 seconds, so this is twelve missed pulls.
pub const SATELLITE_REPORT_UNDELIVERED_AFTER_SECS: i64 = 180;
/// Settled reports whose outcome the satellite remembers for the status read.
pub const SATELLITE_REPORT_STATUS_SETTLED_MAX: usize = 32;
/// Bound on a hub verdict reason (a static snake_case class).
pub const SATELLITE_REPORT_REASON_MAX_BYTES: usize = 64;
/// `undelivered` reasons the satellite itself assigns.
pub const SATELLITE_REPORT_REASON_HUB_NOT_PULLING: &str = "hub_not_pulling";
pub const SATELLITE_REPORT_REASON_HUB_NOT_ACKNOWLEDGING: &str = "hub_not_acknowledging";
/// The satellite restarted with the report still unacknowledged (#2453): its
/// outbox is process-local, so whether the hub ever recorded it is unknown.
pub const SATELLITE_REPORT_REASON_SATELLITE_RESTARTED: &str = "satellite_restarted";

/// The report line types. The text is free-form but bounded and untrusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SatelliteReportKindV1 {
    Enqueue,
    DeployReady,
    Result,
}

impl SatelliteReportKindV1 {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Enqueue => "ENQUEUE",
            Self::DeployReady => "DEPLOY-READY",
            Self::Result => "RESULT",
        }
    }
}

/// Shared by the satellite (on enqueue) and the hub (on record, fail closed).
///
/// # Errors
/// `satellite_report_invalid` for empty, oversized or control-character text.
pub fn validate_satellite_report_text(text: &str) -> Result<(), &'static str> {
    if text.trim().is_empty()
        || text.len() > SATELLITE_REPORT_TEXT_MAX_BYTES
        || text.chars().any(|c| c.is_control() && c != '\n')
    {
        return Err(SATELLITE_REPORT_INVALID);
    }
    Ok(())
}

/// Agent verb on the satellite: the appointed manager queues one report.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReportToHubRequestV1 {
    pub kind: SatelliteReportKindV1,
    pub text: String,
}

impl AgentReportToHubRequestV1 {
    /// # Errors
    /// The stable code of the invalid text.
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_satellite_report_text(&self.text)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentReportToHubReceiptV1 {
    pub report_id: Uuid,
    /// Always `queued`: acceptance, not delivery. The hub pulls reports and
    /// delivery is at most once (an unsent report is lost on a restart).
    /// Read what became of it in `AgentGetDaemonInfo` `hub_reports`
    /// (`delivered`, `held`, `refused` or the typed failure `undelivered`).
    pub state: String,
    /// Advisory only, never a refusal (#276): `sha_not_on_origin: <sha>` for
    /// each 40-hex SHA the report text names that no origin ref the
    /// satellite can see contains, so the hub could not fetch it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Prefix of the `AgentReportToHub` warning for an unpublished SHA (#276).
pub const SATELLITE_REPORT_SHA_NOT_ON_ORIGIN: &str = "sha_not_on_origin";

/// The most 40-hex SHAs one report is checked for (#276); a longer list is
/// checked in order and the rest is ignored, which bounds the git work.
pub const SATELLITE_REPORT_SHA_CHECK_MAX: usize = 8;

/// The distinct lowercase 40-hex SHAs named in a report text, in order of
/// first appearance, at most [`SATELLITE_REPORT_SHA_CHECK_MAX`]. A run of hex
/// digits longer or shorter than 40 (a hash of another size, a short SHA) is
/// not a SHA here.
#[must_use]
pub fn report_text_full_shas(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for token in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        if found.len() >= SATELLITE_REPORT_SHA_CHECK_MAX {
            break;
        }
        let is_sha = token.len() == 40
            && token
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if is_sha && !found.iter().any(|seen| seen == token) {
            found.push(token.to_string());
        }
    }
    found
}

/// Where one report is on its way to the hub manager, as the satellite knows
/// it. Readable by the reporting seat in `AgentGetDaemonInfo` `hub_reports`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SatelliteReportStateV1 {
    /// Accepted; the hub has not pulled it yet.
    Queued,
    /// The hub pulled it and has not acknowledged it yet.
    Fetched,
    /// The hub pulled it and cannot record it yet (`reason`); the satellite
    /// keeps it and the hub retries on every pull.
    Held,
    /// The hub recorded it as a notice for the project's current manager.
    Delivered,
    /// The hub refused it for good (`reason`); it will not be retried.
    Refused,
    /// Typed failure: nothing happened to it for
    /// [`SATELLITE_REPORT_UNDELIVERED_AFTER_SECS`] (`reason` names why).
    Undelivered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteReportStatusEntryV1 {
    pub report_id: Uuid,
    pub kind: SatelliteReportKindV1,
    pub state: SatelliteReportStateV1,
    /// The typed reason for `held`, `refused` and `undelivered`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// RFC3339 with nanoseconds.
    pub queued_at: String,
    pub updated_at: String,
}

/// Pending reports per kind (#135).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteReportKindCountsV1 {
    pub result: usize,
    pub enqueue: usize,
    pub deploy_ready: usize,
}

/// What the outbox holds for the hub right now (#135): the queue depth against
/// its bound, the oldest pending report and the per-kind counts. The same
/// numbers ride the `satellite_report_queue_full` refusal.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteReportBacklogV1 {
    /// Reports the hub has not acknowledged.
    pub depth: usize,
    /// [`SATELLITE_REPORT_OUTBOX_MAX`]: at this depth a new report is refused.
    pub max: usize,
    /// When the oldest pending report was queued, RFC3339 with nanoseconds;
    /// absent when nothing is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_queued_at: Option<String>,
    /// Seconds the oldest pending report has waited (never negative).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_age_secs: Option<i64>,
    pub by_kind: SatelliteReportKindCountsV1,
}

/// The satellite's view of its hub reports (newest first). Process-local like
/// the outbox: a satellite restart forgets it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteReportStatusV1 {
    /// When an allowlisted hub last pulled, RFC3339 with nanoseconds; absent
    /// until the first pull since this daemon started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_hub_pull_at: Option<String>,
    /// Whether a new report would be accepted: false once the hub has not
    /// pulled for [`SATELLITE_REPORT_UNDELIVERED_AFTER_SECS`] with a report
    /// waiting (`AgentReportToHub` then refuses `satellite_report_hub_unreachable`).
    pub hub_pulling: bool,
    /// Depth, oldest pending report and per-kind counts (#135).
    #[serde(default)]
    pub backlog: SatelliteReportBacklogV1,
    #[serde(default)]
    pub reports: Vec<SatelliteReportStatusEntryV1>,
}

/// Why the hub did not acknowledge a report it pulled, sent on the next pull
/// to a satellite that advertises `report_outcomes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SatelliteReportVerdictV1 {
    /// Not recorded yet; the satellite keeps it and the hub retries.
    Held,
    /// Refused for good. The report is also in `acked`: the satellite drops it.
    Refused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatelliteReportOutcomeV1 {
    pub report_id: Uuid,
    pub verdict: SatelliteReportVerdictV1,
    /// A static snake_case class (for example `manager_unavailable`).
    pub reason: String,
}

impl SatelliteReportOutcomeV1 {
    /// # Errors
    /// `satellite_report_invalid` for a nil id or a reason that is not a short
    /// snake_case class.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.report_id.is_nil()
            || self.reason.is_empty()
            || self.reason.len() > SATELLITE_REPORT_REASON_MAX_BYTES
            || !self
                .reason
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(SATELLITE_REPORT_INVALID);
        }
        Ok(())
    }
}

/// Hub to satellite. `acked` names reports the hub already recorded.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatelliteFetchReportsRequestV1 {
    pub wire_version: u16,
    pub hub_installation_id: SatelliteUuidV1,
    #[serde(default)]
    pub acked: Vec<Uuid>,
    /// What became of the pulled reports that were held or refused. The hub
    /// sends it only to a satellite whose identity advertises
    /// `report_outcomes`: an older satellite would refuse the unknown field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outcomes: Vec<SatelliteReportOutcomeV1>,
}

impl SatelliteFetchReportsRequestV1 {
    /// # Errors
    /// `satellite_report_invalid` for a wrong version, nil id, too many acks
    /// or outcomes, or an invalid outcome.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.wire_version != crate::satellite::SATELLITE_WIRE_VERSION_V1
            || self.hub_installation_id.0.is_nil()
            || self.acked.len() > SATELLITE_REPORT_MAX_ACKS
            || self.outcomes.len() > SATELLITE_REPORT_MAX_ACKS
        {
            return Err(SATELLITE_REPORT_INVALID);
        }
        self.outcomes
            .iter()
            .try_for_each(SatelliteReportOutcomeV1::validate)
    }
}

/// One report as it crosses the link: no session ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatelliteReportV1 {
    pub report_id: Uuid,
    pub kind: SatelliteReportKindV1,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatelliteFetchReportsReplyV1 {
    pub reports: Vec<SatelliteReportV1>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> AgentSendSatelliteMessageRequestV1 {
        AgentSendSatelliteMessageRequestV1 {
            peer_id: SatelliteUuidV1(Uuid::new_v4()),
            remote_session_id: SatelliteUuidV1(Uuid::new_v4()),
            scope_project_id: None,
            message: "hello".into(),
            idempotency_key: "k1".into(),
            expires_at: None,
            issue_refs: Vec::new(),
        }
    }

    #[test]
    fn remote_terminal_refusal_round_trips_and_is_bounded() {
        let wire = remote_terminal_refusal("failed", Some("provider_error:codex_usage_limit"));
        assert_eq!(
            wire,
            "remote_terminal|failed|provider_error:codex_usage_limit"
        );
        // As carried by the generic RPC error text.
        let message = format!("Policy denied: {wire}");
        let token = find_remote_terminal(&message).unwrap();
        assert_eq!(
            split_waiting_for(token),
            (
                "remote_terminal".to_string(),
                Some("failed".to_string()),
                Some("provider_error:codex_usage_limit".to_string())
            )
        );
        // Free text, separators and length never cross the link.
        let wild = remote_terminal_refusal("archived", Some(&format!("a b|c/{}", "x".repeat(200))));
        let token = find_remote_terminal(&wild).unwrap();
        let (_, status, reason) = split_waiting_for(token);
        assert_eq!(status.as_deref(), Some("archived"));
        let reason = reason.unwrap();
        assert!(
            reason.starts_with("a_b_c_x") && reason.len() == 64,
            "{reason}"
        );
        assert_eq!(
            remote_terminal_refusal("deleted", None),
            "remote_terminal|deleted|none"
        );
        // Other waits and refusals are not terminal.
        assert!(find_remote_terminal("Policy denied: busy").is_none());
        assert_eq!(
            split_waiting_for("remote_busy"),
            ("remote_busy".to_string(), None, None)
        );
    }

    #[test]
    fn report_text_names_only_distinct_full_lowercase_shas() {
        let a = "a".repeat(40);
        let b = "0123456789abcdef0123456789abcdef01234567";
        let text = format!(
            "ENQUEUE {a} then {b}, again {a}; short abc1234; long {}; upper {}",
            "c".repeat(41),
            "D".repeat(40)
        );
        assert_eq!(report_text_full_shas(&text), vec![a.clone(), b.to_string()]);
        let many: String = (0..20).map(|i| format!("{i:040x} ")).collect();
        assert_eq!(
            report_text_full_shas(&many).len(),
            SATELLITE_REPORT_SHA_CHECK_MAX
        );
        let receipt = AgentReportToHubReceiptV1 {
            report_id: Uuid::new_v4(),
            state: "queued".into(),
            warnings: Vec::new(),
        };
        let value = serde_json::to_value(&receipt).unwrap();
        assert!(value.get("warnings").is_none());
    }

    #[test]
    fn issue_refs_are_optional_on_the_wire_and_bounded() {
        let plain = serde_json::to_value(request()).unwrap();
        assert!(plain.get("issue_refs").is_none());
        let mut with_refs = request();
        with_refs.issue_refs = vec![2359, 189];
        assert!(with_refs.validate().is_ok());
        let value = serde_json::to_value(&with_refs).unwrap();
        assert_eq!(value["issue_refs"], serde_json::json!([2359, 189]));
        let back: AgentSendSatelliteMessageRequestV1 = serde_json::from_value(value).unwrap();
        assert_eq!(back.issue_refs, vec![2359, 189]);
        for bad in [vec![0], vec![-3], vec![5, 5]] {
            let mut invalid = request();
            invalid.issue_refs = bad;
            assert_eq!(invalid.validate(), Err(SATELLITE_ISSUE_REF_INVALID));
        }
        let mut many = request();
        many.issue_refs = (1..=i64::try_from(SATELLITE_ISSUE_REFS_MAX).unwrap()).collect();
        assert!(many.validate().is_ok());
        many.issue_refs.push(1000);
        assert_eq!(many.validate(), Err(SATELLITE_ISSUE_REF_INVALID));
    }

    #[test]
    fn scope_project_id_is_optional_on_the_wire_and_never_nil() {
        // Absent: the field is not serialized, so an old request is unchanged.
        let plain = serde_json::to_value(request()).unwrap();
        assert!(plain.get("scope_project_id").is_none());
        assert!(serde_json::from_value::<AgentSendSatelliteMessageRequestV1>(plain).is_ok());
        let mut scoped = request();
        let project = Uuid::new_v4();
        scoped.scope_project_id = Some(SatelliteUuidV1(project));
        assert!(scoped.validate().is_ok());
        let value = serde_json::to_value(&scoped).unwrap();
        assert_eq!(value["scope_project_id"], serde_json::json!(project));
        let back: AgentSendSatelliteMessageRequestV1 = serde_json::from_value(value).unwrap();
        assert_eq!(back.scope_project_id, Some(SatelliteUuidV1(project)));
        scoped.scope_project_id = Some(SatelliteUuidV1(Uuid::nil()));
        assert_eq!(scoped.validate(), Err(SATELLITE_TARGET_NOT_AUTHORIZED));
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
            interrupt_workers: None,
            cancel: None,
        }
    }

    #[test]
    fn interrupt_workers_rides_the_deploy_wire_only_when_set() {
        let wire = deploy_wire();
        let value = serde_json::to_value(&wire).unwrap();
        assert!(
            value
                .as_object()
                .unwrap()
                .get("interrupt_workers")
                .is_none()
        );
        assert_eq!(wire.as_local_request().interrupt_workers, None);
        let mut set = deploy_wire();
        set.interrupt_workers = Some(true);
        let value = serde_json::to_value(&set).unwrap();
        assert_eq!(value["interrupt_workers"], true);
        let back: SatelliteDeployRequestV1 = serde_json::from_value(value).unwrap();
        assert!(back.as_local_request().interrupts_workers());
        assert_eq!(back.validate(), Ok(()));
    }

    #[test]
    fn non_cancel_deploy_wire_omits_cancel_and_old_shape_parses() {
        let wire = deploy_wire();
        let value = serde_json::to_value(&wire).unwrap();
        assert!(value.as_object().unwrap().get("cancel").is_none());
        // Pre-#1859 shape: no `cancel` key.
        let back: SatelliteDeployRequestV1 = serde_json::from_value(value).unwrap();
        assert_eq!(back.cancel, None);
        let mut cancel = deploy_wire();
        cancel.cancel = Some(true);
        let value = serde_json::to_value(&cancel).unwrap();
        assert_eq!(value["cancel"], serde_json::json!(true));
        let back: SatelliteDeployRequestV1 = serde_json::from_value(value).unwrap();
        assert_eq!(back.cancel, Some(true));
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

    #[test]
    fn scope_text_separates_sessions_from_projects_and_round_trips() {
        let session = Uuid::new_v4();
        let project = Uuid::new_v4();
        let text = format!(" {session} , project:{project},");
        let (sessions, projects) = parse_satellite_scope_text(&text).unwrap();
        assert_eq!(sessions, vec![SatelliteUuidV1(session)]);
        assert_eq!(projects, vec![SatelliteUuidV1(project)]);
        assert_eq!(
            format_satellite_scope_text(&sessions, &projects),
            format!("{session},project:{project}")
        );
        assert!(parse_satellite_scope_text("project:not-a-uuid").is_err());
        assert!(parse_satellite_scope_text("").unwrap().0.is_empty());
        // A repeated project is refused like a repeated session.
        let mut policy = SatelliteInboundPolicyV1::default();
        policy.scope_projects = vec![SatelliteUuidV1(project), SatelliteUuidV1(project)];
        assert!(policy.validate().is_err());
    }

    fn fetch_request() -> SatelliteFetchReportsRequestV1 {
        SatelliteFetchReportsRequestV1 {
            wire_version: crate::satellite::SATELLITE_WIRE_VERSION_V1,
            hub_installation_id: SatelliteUuidV1(Uuid::new_v4()),
            acked: Vec::new(),
            outcomes: Vec::new(),
        }
    }

    #[test]
    fn a_fetch_without_outcomes_keeps_the_old_wire_shape() {
        let request = fetch_request();
        let value = serde_json::to_value(&request).unwrap();
        assert!(value.as_object().unwrap().get("outcomes").is_none());
        // An older hub's request (no `outcomes` key) still parses and validates.
        let back: SatelliteFetchReportsRequestV1 = serde_json::from_value(value).unwrap();
        assert!(back.outcomes.is_empty());
        back.validate().unwrap();
    }

    #[test]
    fn outcomes_are_bounded_typed_snake_case_classes() {
        let outcome = |reason: &str| SatelliteReportOutcomeV1 {
            report_id: Uuid::new_v4(),
            verdict: SatelliteReportVerdictV1::Held,
            reason: reason.into(),
        };
        let mut request = fetch_request();
        request.outcomes = vec![outcome("manager_unavailable"), outcome("rate_limited")];
        request.validate().unwrap();
        let wire = serde_json::to_value(&request).unwrap();
        assert_eq!(wire["outcomes"][0]["verdict"], "held");
        let long = "x".repeat(SATELLITE_REPORT_REASON_MAX_BYTES + 1);
        for bad in ["", "Has Space", "UPPER", "semi;colon", long.as_str()] {
            request.outcomes = vec![outcome(bad)];
            assert_eq!(request.validate(), Err(SATELLITE_REPORT_INVALID), "{bad}");
        }
        let mut nil = outcome("manager_unavailable");
        nil.report_id = Uuid::nil();
        request.outcomes = vec![nil];
        assert_eq!(request.validate(), Err(SATELLITE_REPORT_INVALID));
        request.outcomes = (0..=SATELLITE_REPORT_MAX_ACKS)
            .map(|_| outcome("rate_limited"))
            .collect();
        assert_eq!(request.validate(), Err(SATELLITE_REPORT_INVALID));
    }

    #[test]
    fn report_states_use_snake_case_names() {
        let wire = serde_json::to_value(SatelliteReportStatusEntryV1 {
            report_id: Uuid::new_v4(),
            kind: SatelliteReportKindV1::DeployReady,
            state: SatelliteReportStateV1::Undelivered,
            reason: Some(SATELLITE_REPORT_REASON_HUB_NOT_PULLING.into()),
            queued_at: "2026-10-13T19:00:00.000000000Z".into(),
            updated_at: "2026-10-13T19:00:00.000000000Z".into(),
        })
        .unwrap();
        assert_eq!(wire["state"], "undelivered");
        assert_eq!(wire["kind"], "deploy_ready");
        assert_eq!(wire["reason"], "hub_not_pulling");
    }
}
