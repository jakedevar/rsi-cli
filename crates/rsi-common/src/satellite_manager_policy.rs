//! Bounded manager policy pushes over the paired satellite deploy link (#1748).
use crate::harness_manager_v2::{
    HarnessManagerPolicyConfigV2, ManagerLaunchChoiceV2, ManagerPolicyV2, ManagerProviderLimitV2,
};
use crate::satellite::{SATELLITE_WIRE_VERSION_V1, SatelliteUuidV1};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const REQUEST_HUB_MANAGER_POLICY_METHOD: &str = "RequestHubManagerPolicy";
pub const POLICY_INVALID: &str = "satellite_policy_invalid";
pub const POLICY_BOUNDS: &str = "satellite_policy_bounds_exceeded";
pub const POLICY_STALE: &str = "satellite_policy_stale";
pub const POLICY_KEY_CONFLICT: &str = "satellite_policy_key_conflict";
pub const POLICY_UNREACHABLE: &str = "satellite_policy_unreachable";
pub const POLICY_REFUSED: &str = "satellite_policy_refused";

/// Appended to a launch refusal when the resolved launch list is empty and no
/// portfolio parent can supply one (#1941).
pub const NO_LAUNCH_CHOICES_HINT: &str = "no launch choices: a satellite seat has no portfolio fallback; the hub must push a launch list with AgentPushSatelliteManagerPolicy, or the operator saves one";

/// Most manager seats one satellite health report lists.
pub const SATELLITE_MAX_MANAGER_LAUNCH_ROWS: usize = 8;

/// One manager seat's effective launch-choice count, reported on satellite
/// health so the hub sees `launches: none` before the first refused create.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteManagerLaunchesV1 {
    pub project_id: Uuid,
    /// Effective launch choices (own list, or the live ancestor intersection).
    pub launches: u32,
}

/// `none` for zero, otherwise the count; the one rendering hub views share.
#[must_use]
pub fn launches_label(launches: u32) -> String {
    if launches == 0 {
        "none".to_owned()
    } else {
        launches.to_string()
    }
}

/// One-word summary across manager seats: `none` if any seat has no launch
/// choices, otherwise the smallest count; `None` when no seat is reported.
#[must_use]
pub fn launches_summary(rows: &[SatelliteManagerLaunchesV1]) -> Option<String> {
    rows.iter()
        .map(|row| row.launches)
        .min()
        .map(launches_label)
}

/// Only launch choices and session caps can be pushed. Other policy fields
/// remain exactly as saved by the satellite operator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatelliteManagerPolicyPatchV1 {
    pub allowed_launches: Vec<ManagerLaunchChoiceV2>,
    pub max_created_sessions: u16,
    pub max_active_sessions: u16,
    pub provider_limits: Vec<ManagerProviderLimitV2>,
}
impl SatelliteManagerPolicyPatchV1 {
    pub fn apply(&self, policy: &mut ManagerPolicyV2) {
        policy.allowed_launches.clone_from(&self.allowed_launches);
        policy.max_created_sessions = self.max_created_sessions;
        policy.max_active_sessions = self.max_active_sessions;
        policy.provider_limits.clone_from(&self.provider_limits);
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        // An empty delegated list means inheritance; require an explicit list
        // so a push cannot accidentally grant launches through that fallback.
        if self.allowed_launches.is_empty() {
            return Err(POLICY_INVALID);
        }
        let mut policy = ManagerPolicyV2::default();
        self.apply(&mut policy);
        policy.validate().map_err(|_| POLICY_INVALID)
    }
    pub fn within(
        &self,
        launches: &[ManagerLaunchChoiceV2],
        caps: &crate::portfolio_nodes::ManagerResourceCapsV1,
    ) -> bool {
        self.allowed_launches
            .iter()
            .all(|launch| launches.iter().any(|entry| entry.allows(launch)))
            && self.max_created_sessions <= caps.max_created_sessions
            && self.max_active_sessions <= caps.max_active_sessions
            && caps.provider_limits.iter().all(|cap| {
                self.provider_limits.iter().any(|limit| {
                    limit.provider == cap.provider && limit.max_active <= cap.max_active
                })
            })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentPushSatelliteManagerPolicyRequestV1 {
    #[serde(default)]
    pub project_id: Option<Uuid>,
    pub peer_id: SatelliteUuidV1,
    /// Operator-declared remote scope root (stable across seat rotation).
    pub remote_session_id: SatelliteUuidV1,
    pub expected_scope_version: i64,
    pub expected_policy_version: i64,
    pub idempotency_key: String,
    pub policy: SatelliteManagerPolicyPatchV1,
}
impl AgentPushSatelliteManagerPolicyRequestV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.peer_id.0.is_nil()
            || self.remote_session_id.0.is_nil()
            || self.project_id.is_some_and(|id| id.is_nil())
            || self.expected_scope_version <= 0
            || self.expected_policy_version < 0
            || self.idempotency_key.is_empty()
            || self.idempotency_key.len() > 128
            || self.idempotency_key.contains('\0')
        {
            return Err(POLICY_INVALID);
        }
        self.policy.validate()
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SatelliteManagerPolicyRequestV1 {
    pub wire_version: u16,
    pub hub_installation_id: SatelliteUuidV1,
    pub sender_session_id: Uuid,
    pub request: AgentPushSatelliteManagerPolicyRequestV1,
}
impl SatelliteManagerPolicyRequestV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.wire_version != SATELLITE_WIRE_VERSION_V1
            || self.hub_installation_id.0.is_nil()
            || self.sender_session_id.is_nil()
        {
            return Err(POLICY_INVALID);
        }
        self.request.validate()
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SatelliteManagerPolicyReceiptV1 {
    pub hub_installation_id: SatelliteUuidV1,
    pub sender_session_id: Uuid,
    pub config: HarnessManagerPolicyConfigV2,
    pub deduplicated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_summary_says_none_for_an_empty_seat_and_the_smallest_count_otherwise() {
        let row = |launches| SatelliteManagerLaunchesV1 {
            project_id: Uuid::new_v4(),
            launches,
        };
        assert_eq!(launches_summary(&[]), None);
        assert_eq!(launches_summary(&[row(3)]).as_deref(), Some("3"));
        assert_eq!(launches_summary(&[row(3), row(0)]).as_deref(), Some("none"));
        assert!(NO_LAUNCH_CHOICES_HINT.contains("AgentPushSatelliteManagerPolicy"));
    }

    use crate::SessionProvider;
    use crate::harness_manager_v2::ManagerProviderLimitV2;
    use crate::portfolio_nodes::ManagerResourceCapsV1;
    #[test]
    fn explicit_launch_triples_and_every_provider_cap_must_fit_the_grant() {
        let choice = ManagerLaunchChoiceV2 {
            provider: SessionProvider::Claude,
            model: "model".into(),
            effort: Some("high".into()),
        };
        let caps = ManagerResourceCapsV1::from_policy(&ManagerPolicyV2 {
            max_created_sessions: 4,
            max_active_sessions: 3,
            provider_limits: vec![ManagerProviderLimitV2 {
                provider: SessionProvider::Claude,
                max_active: 2,
            }],
            ..Default::default()
        });
        let mut patch = SatelliteManagerPolicyPatchV1 {
            allowed_launches: vec![choice.clone()],
            max_created_sessions: 4,
            max_active_sessions: 3,
            provider_limits: Vec::new(),
        };
        assert!(!patch.within(&[choice.clone()], &caps));
        patch.provider_limits = caps.provider_limits.clone();
        assert!(patch.within(&[choice.clone()], &caps));
        patch.provider_limits[0].max_active = 3;
        assert!(!patch.within(&[choice.clone()], &caps));
        patch.provider_limits[0].max_active = 1;
        patch.allowed_launches[0].effort = None;
        assert!(!patch.within(&[choice.clone()], &caps));
        let mut any = choice.clone();
        any.effort = None;
        assert!(patch.within(&[any.clone()], &caps));
        patch.allowed_launches = vec![choice.clone()];
        assert!(patch.within(&[any], &caps));
        patch.allowed_launches = vec![choice];
        patch.max_active_sessions = 4;
        assert!(!patch.within(&patch.allowed_launches, &caps));
        patch.allowed_launches.clear();
        assert_eq!(patch.validate(), Err(POLICY_INVALID));
    }
}
