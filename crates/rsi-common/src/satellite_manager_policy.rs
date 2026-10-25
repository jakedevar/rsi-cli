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

/// Most manager seats in the legacy satellite launch summary.
pub const SATELLITE_MAX_MANAGER_LAUNCH_ROWS: usize = 8;

/// One manager seat's effective launch-choice count, reported on satellite
/// health so the hub sees `launches: none` before the first refused create.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteManagerLaunchesV1 {
    pub project_id: Uuid,
    /// Effective launch choices (own list, or the live ancestor intersection).
    pub launches: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager_session_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_version: Option<i64>,
    /// Zero means no saved policy; absent means unknown on an older satellite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_version: Option<i64>,
    /// Saved allowed_launches count, before any ancestor intersection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_launches: Option<u32>,
}

/// Safe facts returned with a stale policy push. Optional for mixed-version
/// links; the hub parses only these typed fields, never arbitrary remote text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SatelliteManagerPolicyStaleV1 {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_scope_version: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_policy_version: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_launches: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager_session_id: Option<Uuid>,
}

impl SatelliteManagerPolicyStaleV1 {
    #[must_use]
    pub fn rpc_error(&self) -> crate::rpc::RpcError {
        let shown = |value: Option<String>| value.unwrap_or_else(|| "unknown".into());
        let mut data = serde_json::to_value(self).expect("stale policy facts serialize");
        data["code"] = serde_json::json!(POLICY_STALE);
        crate::rpc::RpcError {
            code: crate::rpc::INTERNAL_ERROR,
            message: format!(
                "Policy denied (actual_scope_version={} actual_policy_version={} allowed_launches={} manager_session_id={}): {POLICY_STALE}",
                shown(self.actual_scope_version.map(|v| v.to_string())),
                shown(self.actual_policy_version.map(|v| v.to_string())),
                shown(self.allowed_launches.map(|v| v.to_string())),
                shown(self.manager_session_id.map(|v| v.to_string())),
            ),
            data: Some(data),
        }
    }
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
    #[serde(deserialize_with = "crate::strict_input::deserialize")]
    pub allowed_launches: Vec<ManagerLaunchChoiceV2>,
    /// Sessions the seat may create per `created_sessions_window_hours`
    /// (#3416).
    pub max_created_sessions: u16,
    /// The rolling window of `max_created_sessions`, in hours; omitted means
    /// the default (24).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_sessions_window_hours: Option<u16>,
    pub max_active_sessions: u16,
    #[serde(deserialize_with = "crate::strict_input::deserialize")]
    pub provider_limits: Vec<ManagerProviderLimitV2>,
}
impl SatelliteManagerPolicyPatchV1 {
    /// The window this patch applies (the default when omitted).
    #[must_use]
    pub fn window_hours(&self) -> u16 {
        self.created_sessions_window_hours
            .unwrap_or(crate::harness_manager_v2::DEFAULT_CREATED_SESSIONS_WINDOW_HOURS)
    }
    pub fn apply(&self, policy: &mut ManagerPolicyV2) {
        policy.allowed_launches.clone_from(&self.allowed_launches);
        policy.max_created_sessions = self.max_created_sessions;
        policy.created_sessions_window_hours = self.window_hours();
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
            && caps.admits_created_rate(self.max_created_sessions, self.window_hours())
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
    fn old_manager_health_and_stale_error_shapes_round_trip() {
        let project = Uuid::new_v4();
        let old_row = serde_json::json!({"project_id": project, "launches": 0});
        let row: SatelliteManagerLaunchesV1 = serde_json::from_value(old_row.clone()).unwrap();
        assert_eq!(row.manager_session_id, None);
        assert_eq!(row.scope_version, None);
        assert_eq!(row.policy_version, None);
        assert_eq!(row.allowed_launches, None);
        assert_eq!(serde_json::to_value(&row).unwrap(), old_row);

        // An old hub ignores additive fields and still reads the original count.
        #[derive(Deserialize)]
        struct OldRow {
            project_id: Uuid,
            launches: u32,
        }
        let current = SatelliteManagerLaunchesV1 {
            manager_session_id: Some(Uuid::new_v4()),
            scope_version: Some(3),
            policy_version: Some(7),
            allowed_launches: Some(0),
            ..row
        };
        let wire = serde_json::to_value(&current).unwrap();
        let old: OldRow = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!((old.project_id, old.launches), (project, 0));
        assert_eq!(
            serde_json::from_value::<SatelliteManagerLaunchesV1>(wire).unwrap(),
            current
        );

        let legacy: crate::rpc::RpcError = serde_json::from_value(serde_json::json!({
            "code": crate::rpc::INTERNAL_ERROR,
            "message": format!("Policy denied: {POLICY_STALE}")
        }))
        .unwrap();
        assert_eq!(legacy.data, None);
        let facts: SatelliteManagerPolicyStaleV1 =
            serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(serde_json::to_value(facts).unwrap(), serde_json::json!({}));
        let error = SatelliteManagerPolicyStaleV1 {
            actual_scope_version: Some(3),
            actual_policy_version: Some(7),
            allowed_launches: Some(0),
            manager_session_id: current.manager_session_id,
        }
        .rpc_error();
        // Old hubs classify the exact code after the final prefix separator.
        assert_eq!(error.message.rsplit_once(": ").unwrap().1, POLICY_STALE);
        let round_trip: crate::rpc::RpcError =
            serde_json::from_value(serde_json::to_value(&error).unwrap()).unwrap();
        assert_eq!(round_trip.data, error.data);

        // The new list lives beside the legacy summary, whose old eight-row
        // validation limit must still admit a new satellite's health payload.
        let legacy_health = serde_json::json!({
            "daemon_version": "1.2.3", "uptime_seconds": 1,
            "sessions_running": 0, "sessions_waiting_approval": 0,
            "manager_launches": [old_row],
        });
        let mut health: crate::satellite::SatelliteHealthV1 =
            serde_json::from_value(legacy_health).unwrap();
        assert_eq!(health.manager_policies, None);
        let legacy_wire = serde_json::to_value(&health).unwrap();
        assert_eq!(legacy_wire["manager_policies"], serde_json::Value::Null);
        health.manager_policies = Some(vec![
                current;
                crate::satellite_dispatch::SATELLITE_INBOUND_MAX_ENTRIES
            ]);
        health.validate().unwrap();
        #[derive(Deserialize)]
        struct OldHealth {
            manager_launches: Vec<OldRow>,
        }
        let wire = serde_json::to_value(&health).unwrap();
        let old: OldHealth = serde_json::from_value(wire.clone()).unwrap();
        assert!(old.manager_launches.len() <= 8);
        let received: crate::satellite::SatelliteHealthV1 = serde_json::from_value(wire).unwrap();
        assert_eq!(received, health);
    }

    #[test]
    fn launch_summary_says_none_for_an_empty_seat_and_the_smallest_count_otherwise() {
        let row = |launches| SatelliteManagerLaunchesV1 {
            project_id: Uuid::new_v4(),
            launches,
            ..Default::default()
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
            created_sessions_window_hours: None,
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

    /// #3416: the hub grant's number is per window, so a push compares as a
    /// rate: it cannot exceed the grant's sessions per hour, however the
    /// window is stated, and a push without a window means the default.
    #[test]
    fn a_pushed_created_sessions_allowance_cannot_exceed_the_grants_per_window_rate() {
        let choice = ManagerLaunchChoiceV2 {
            provider: SessionProvider::Claude,
            model: "model".into(),
            effort: Some("high".into()),
        };
        let caps = ManagerResourceCapsV1::from_policy(&ManagerPolicyV2 {
            max_created_sessions: 240,
            created_sessions_window_hours: 24,
            max_active_sessions: 3,
            provider_limits: vec![ManagerProviderLimitV2 {
                provider: SessionProvider::Claude,
                max_active: 2,
            }],
            ..Default::default()
        });
        let mut patch = SatelliteManagerPolicyPatchV1 {
            allowed_launches: vec![choice.clone()],
            max_created_sessions: 240,
            created_sessions_window_hours: None,
            max_active_sessions: 3,
            provider_limits: caps.provider_limits.clone(),
        };
        assert!(patch.within(&[choice.clone()], &caps));
        patch.max_created_sessions = 241;
        assert!(!patch.within(&[choice.clone()], &caps));
        // Half the window carries at most half the number.
        patch.created_sessions_window_hours = Some(12);
        patch.max_created_sessions = 120;
        assert!(patch.within(&[choice.clone()], &caps));
        patch.max_created_sessions = 121;
        assert!(!patch.within(&[choice.clone()], &caps));
        // A longer window than the grant's may carry proportionally more.
        patch.created_sessions_window_hours = Some(48);
        patch.max_created_sessions = 480;
        assert!(patch.within(&[choice], &caps));
        let mut policy = ManagerPolicyV2::default();
        patch.apply(&mut policy);
        assert_eq!(policy.created_sessions_window_hours, 48);
        assert_eq!(policy.max_created_sessions, 480);
        patch.created_sessions_window_hours = None;
        patch.apply(&mut policy);
        assert_eq!(policy.created_sessions_window_hours, 24);
        patch.created_sessions_window_hours = Some(0);
        assert_eq!(patch.validate(), Err(POLICY_INVALID));
    }
}
