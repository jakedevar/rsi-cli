//! #2750: move a manager seat whose provider is down to another provider in
//! one supported step.
//!
//! A session's provider is fixed at creation, and `succeed_manager` must be
//! called by the seat itself, which cannot take a turn once its provider is
//! gone. `ReseatHarnessManager` (operator-only) and `AgentReseatSatelliteManager`
//! (the hub manager, for a paired satellite) launch a successor on the new
//! provider with `continued_from` set to the failed seat, re-appoint it with the
//! project's saved scope and policy, and record the reason. A scope that names
//! the failed seat (the hub's peer scope, the satellite's inbound roots)
//! resolves to the successor through that lineage.
use crate::SessionProvider;
use crate::satellite::{SATELLITE_WIRE_VERSION_V1, SatelliteUuidV1};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Longest reason a reseat records.
pub const RESEAT_REASON_MAX_BYTES: usize = 512;

/// The project has no stored manager appointment.
pub const RESEAT_NOT_CONFIGURED: &str = "manager_reseat_not_configured";
/// The appointment is revoked; a reseat never revives it.
pub const RESEAT_REVOKED: &str = "manager_reseat_revoked";
/// The appointment has no live seat to replace.
pub const RESEAT_NO_SEAT: &str = "manager_reseat_no_seat";
/// The seat is running, starting or waiting on an approval: it is not down.
pub const RESEAT_SEAT_LIVE: &str = "manager_seat_live";
/// The seat is stopped but not `Failed`; the caller must assert the provider
/// is unavailable (`provider_unavailable`) to replace it.
pub const RESEAT_SEAT_NOT_FAILED: &str = "manager_seat_not_failed";
/// Only a parentless Standard session is a replaceable project seat.
pub const RESEAT_ROOT_REQUIRED: &str = "manager_reseat_root_required";
/// The request is malformed.
pub const RESEAT_INVALID: &str = "manager_reseat_invalid";

/// The hub-side refusal and wire codes of the satellite path.
pub const REQUEST_HUB_MANAGER_RESEAT_METHOD: &str = "RequestHubManagerReseat";
pub const SATELLITE_RESEAT_INVALID: &str = "satellite_reseat_invalid";
pub const SATELLITE_RESEAT_UNREACHABLE: &str = "satellite_reseat_unreachable";
/// The request was sent but no usable reply came back (timeout, lost
/// connection, malformed reply): the satellite may have reseated (#3291).
pub const SATELLITE_RESEAT_UNCERTAIN: &str = "satellite_reseat_uncertain";
pub const SATELLITE_RESEAT_REFUSED: &str = "satellite_reseat_refused";
/// The requested provider/model/effort is outside the hub's effective launch
/// grant for the project; a reseat never widens it.
pub const SATELLITE_RESEAT_BOUNDS: &str = "satellite_reseat_bounds_exceeded";

/// `ReseatHarnessManager` (operator-only): replace the project's failed
/// manager seat with a new session on `provider`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReseatHarnessManagerParams {
    pub project_id: Uuid,
    /// The provider, and optionally the model and effort, of the successor.
    pub provider: SessionProvider,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    /// Why the seat is replaced; recorded on both sessions.
    pub reason: String,
    /// The caller asserts the seat's provider is unavailable. Required to
    /// replace a seat that is stopped but not `Failed` (`Completed` or
    /// `Interrupted`); a `Failed` seat needs no assertion.
    #[serde(default)]
    pub provider_unavailable: bool,
}

impl ReseatHarnessManagerParams {
    /// The request with its free-form fields trimmed, or the stable refusal.
    ///
    /// # Errors
    /// `manager_reseat_invalid` for a nil project, an empty or oversized
    /// reason, or an empty model or effort.
    pub fn normalized(mut self) -> Result<Self, &'static str> {
        self.reason = self.reason.trim().to_owned();
        let blank = |value: &Option<String>| value.as_deref().is_some_and(|v| v.trim().is_empty());
        if self.project_id.is_nil()
            || self.reason.is_empty()
            || self.reason.len() > RESEAT_REASON_MAX_BYTES
            || self.reason.contains('\0')
            || blank(&self.model)
            || blank(&self.effort)
        {
            return Err(RESEAT_INVALID);
        }
        self.model = self.model.map(|m| m.trim().to_owned());
        self.effort = self.effort.map(|e| e.trim().to_owned());
        Ok(self)
    }
}

/// What a reseat did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReseatHarnessManagerResult {
    pub project_id: Uuid,
    /// The failed seat that was replaced.
    pub predecessor_session_id: Uuid,
    pub predecessor_provider: SessionProvider,
    /// The successor, now the appointed seat (`continued_from` the predecessor).
    pub successor_session_id: Uuid,
    pub provider: SessionProvider,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    pub reason: String,
    /// The appointment's scope version after the reseat.
    pub scope_version: i64,
    /// The re-granted policy's version; `None` when the project had no policy.
    #[serde(default)]
    pub policy_version: Option<i64>,
}

/// `AgentReseatSatelliteManager` (the hub manager, Deploy authority): ask a
/// paired satellite to reseat the manager its declared address resolves to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReseatSatelliteManagerRequestV1 {
    /// The hub project whose health the caller owns.
    #[serde(default)]
    pub project_id: Option<Uuid>,
    pub peer_id: SatelliteUuidV1,
    /// The operator-declared remote scope root (the seat, or its project); the
    /// satellite resolves it to the live seat.
    pub remote_session_id: SatelliteUuidV1,
    pub provider: SessionProvider,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<String>,
    pub reason: String,
    #[serde(default)]
    pub provider_unavailable: bool,
}

impl AgentReseatSatelliteManagerRequestV1 {
    /// # Errors
    /// `satellite_reseat_invalid`.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.peer_id.0.is_nil()
            || self.remote_session_id.0.is_nil()
            || self.project_id.is_some_and(|id| id.is_nil())
        {
            return Err(SATELLITE_RESEAT_INVALID);
        }
        self.reseat_params(Uuid::from_u128(1))
            .normalized()
            .map(|_| ())
            .map_err(|_| SATELLITE_RESEAT_INVALID)
    }

    /// The satellite-local request once the declared address resolved to
    /// `project_id`.
    #[must_use]
    pub fn reseat_params(&self, project_id: Uuid) -> ReseatHarnessManagerParams {
        ReseatHarnessManagerParams {
            project_id,
            provider: self.provider,
            model: self.model.clone(),
            effort: self.effort.clone(),
            reason: self.reason.clone(),
            provider_unavailable: self.provider_unavailable,
        }
    }
}

/// The wire form the hub sends over the paired link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatelliteManagerReseatRequestV1 {
    pub wire_version: u16,
    pub hub_installation_id: SatelliteUuidV1,
    pub sender_session_id: Uuid,
    pub request: AgentReseatSatelliteManagerRequestV1,
}

impl SatelliteManagerReseatRequestV1 {
    /// # Errors
    /// `satellite_reseat_invalid`.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.wire_version != SATELLITE_WIRE_VERSION_V1
            || self.hub_installation_id.0.is_nil()
            || self.sender_session_id.is_nil()
        {
            return Err(SATELLITE_RESEAT_INVALID);
        }
        self.request.validate()
    }
}

/// The satellite's answer, relayed to the hub caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteManagerReseatReceiptV1 {
    pub hub_installation_id: SatelliteUuidV1,
    pub sender_session_id: Uuid,
    pub result: ReseatHarnessManagerResult,
}

/// The first prompt of a reseat successor: who it replaces, why, and where to
/// orient. Everything the failed seat knew is in the durable store, not in the
/// session, so the brief names the reads rather than restating them.
#[must_use]
pub fn successor_brief(
    predecessor: Uuid,
    predecessor_provider: SessionProvider,
    reason: &str,
) -> String {
    format!(
        "You are the project manager seat for this project, reseated onto a new provider \
         because the previous seat ({predecessor}, provider {predecessor_provider:?}) became \
         unavailable. Reason recorded by the operator: {reason}\n\n\
         The appointment, its scope and its policy are already yours, and mail and wakes of the \
         previous seat were handed to you. Start with AgentGetAuthorityCatalog, then read the \
         manager inbox and progress to see what was in flight. Follow the catalog's current \
         role playbook file reference or its binary-shipped role guidance fallback. \
         The previous seat will not resume."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successor_uses_the_catalog_playbook_reference_or_guidance_fallback() {
        let predecessor = Uuid::from_u128(1);
        let brief = successor_brief(predecessor, SessionProvider::Codex, "provider unavailable");
        assert!(brief.contains(&predecessor.to_string()));
        assert!(brief.contains("Start with AgentGetAuthorityCatalog"));
        assert!(brief.contains("Follow the catalog's current role playbook file reference or its binary-shipped role guidance fallback"));
    }

    fn params() -> ReseatHarnessManagerParams {
        ReseatHarnessManagerParams {
            project_id: Uuid::new_v4(),
            provider: SessionProvider::Claude,
            model: Some(" sonnet ".into()),
            effort: None,
            reason: "  codex login expired  ".into(),
            provider_unavailable: false,
        }
    }

    #[test]
    fn normalized_trims_and_refuses_malformed_requests() {
        let ok = params().normalized().expect("valid");
        assert_eq!(ok.reason, "codex login expired");
        assert_eq!(ok.model.as_deref(), Some("sonnet"));
        for bad in [
            ReseatHarnessManagerParams {
                project_id: Uuid::nil(),
                ..params()
            },
            ReseatHarnessManagerParams {
                reason: "   ".into(),
                ..params()
            },
            ReseatHarnessManagerParams {
                reason: "x".repeat(RESEAT_REASON_MAX_BYTES + 1),
                ..params()
            },
            ReseatHarnessManagerParams {
                model: Some(" ".into()),
                ..params()
            },
        ] {
            assert_eq!(bad.normalized(), Err(RESEAT_INVALID));
        }
    }

    #[test]
    fn the_hub_request_validates_and_maps_to_the_local_request() {
        let request = AgentReseatSatelliteManagerRequestV1 {
            project_id: None,
            peer_id: SatelliteUuidV1(Uuid::new_v4()),
            remote_session_id: SatelliteUuidV1(Uuid::new_v4()),
            provider: SessionProvider::Claude,
            model: None,
            effort: Some("high".into()),
            reason: "provider outage".into(),
            provider_unavailable: true,
        };
        request.validate().expect("valid");
        let project = Uuid::new_v4();
        let local = request.reseat_params(project);
        assert_eq!(local.project_id, project);
        assert!(local.provider_unavailable);
        let mut bad = request;
        bad.reason.clear();
        assert_eq!(bad.validate(), Err(SATELLITE_RESEAT_INVALID));
    }
}
