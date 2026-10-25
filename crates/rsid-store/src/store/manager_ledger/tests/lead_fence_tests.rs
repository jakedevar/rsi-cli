//! #4011: the lead fence `pause_lead` needs is readable from the manager's
//! own inspect rows, and a stale fence names the mismatched field.

use super::*;
use crate::store::manager_actions::ManagerActionOriginV2;

fn grant_lead_control(f: &Fixture) {
    f.store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: f.project,
            expected_scope_version: 1,
            expected_policy_version: 1,
            idempotency_key: "policy-lead-control".into(),
            policy: ManagerPolicyV2 {
                mode: ManagerOperatingModeV2::Execute,
                capabilities: vec![ManagerCapabilityV2::LeadControl],
                ..Default::default()
            },
        })
        .unwrap();
}

fn pause(f: &Fixture, expected: ManagerLeadFenceV2, key: &str) -> Result<()> {
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let policy = f
        .store
        .get_harness_manager_policy(f.project)
        .unwrap()
        .unwrap();
    f.store
        .enqueue_manager_action(
            ManagerActionOriginV2::Agent { caller: f.manager },
            AgentManagerControlRequestV2::new(
                ManagerFenceV2 {
                    scope_version: config.row_version,
                    policy_version: policy.row_version,
                },
                key.into(),
                ManagerActionV2::PauseLead {
                    epic_id: f.epic,
                    expected,
                    reason: "fence read from inspect".into(),
                },
            ),
        )
        .map(|_| ())
}

fn inspected_fence(f: &Fixture, section: ManagerInspectSectionV2) -> ManagerLeadFenceV2 {
    let page = f
        .store
        .manager_v2_inspect(
            f.manager,
            &AgentManagerInspectRequestV2 {
                section,
                epic_id: Some(f.epic),
                ..Default::default()
            },
        )
        .unwrap();
    let row = page
        .rows
        .iter()
        .find(|row| row["type"] == "lead_control" && row["epic_id"] == json!(f.epic))
        .unwrap();
    assert_eq!(row["fence_state"], "current");
    serde_json::from_value(row["expected"].clone()).unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_pause_lead_built_only_from_the_inspect_row_succeeds() {
    let f = fixture();
    grant_lead_control(&f);
    let fence = inspected_fence(&f, ManagerInspectSectionV2::Overview);
    assert_eq!(fence.lead_session_id, Some(f.lead));
    pause(&f, fence, "pause-from-inspect").unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
#[test]
fn a_stale_lead_fence_names_the_mismatched_field_with_both_values() {
    let f = fixture();
    grant_lead_control(&f);
    let mut fence = inspected_fence(&f, ManagerInspectSectionV2::Overview);
    let current = fence.event_sequence;
    fence.event_sequence = current + 7;
    let message = pause(&f, fence, "pause-stale").unwrap_err().to_string();
    assert!(message.contains("manager_v2_lead_changed"), "{message}");
    assert!(
        message.contains(&format!(
            "event_sequence expected {}, current {current}",
            current + 7
        )),
        "{message}"
    );
    assert!(!message.contains("lead_generation expected"), "{message}");
}
