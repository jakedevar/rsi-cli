use super::*;
use crate::harness_manager_v2::{ManagerLaunchChoiceV2, ManagerProviderLimitV2};
use crate::types::SessionProvider;
use uuid::Uuid;

fn apply(
    policy: &ManagerPolicyV2,
    preset: ManagerPolicyPreset,
    origin: ManagerPolicyOrigin,
    scope_mode: HarnessManagerScopeModeV1,
    touched: &BTreeSet<ManagerPolicyField>,
) -> ManagerPolicyV2 {
    apply_manager_policy_preset(
        policy,
        preset,
        &ManagerPresetContext {
            origin,
            scope_mode,
            touched,
        },
    )
    .policy
}
fn new_full() -> ManagerPolicyV2 {
    apply(
        &ManagerPolicyV2::default(),
        ManagerPolicyPreset::FullProjectControl,
        ManagerPolicyOrigin::New,
        HarnessManagerScopeModeV1::Project,
        &BTreeSet::new(),
    )
}

#[test]
fn initial_profiles_have_exact_grants_allowances_and_scope_behavior() {
    for scope in [
        HarnessManagerScopeModeV1::Project,
        HarnessManagerScopeModeV1::Selected,
    ] {
        for preset in [
            ManagerPolicyPreset::Observe,
            ManagerPolicyPreset::Execute,
            ManagerPolicyPreset::FullProjectControl,
        ] {
            let p = apply(
                &ManagerPolicyV2::default(),
                preset,
                ManagerPolicyOrigin::New,
                scope,
                &BTreeSet::new(),
            );
            assert_eq!(p.validate(), Ok(()));
            assert_eq!(classify_manager_policy(&p, scope).preset, preset);
            assert_eq!(p.max_active_sessions, 4);
            assert_eq!(
                (p.retry_delay_seconds, p.request_timeout_seconds),
                (60, 900)
            );
            assert_eq!(p.allowed_launches, vec![]);
            assert_eq!(p.provider_limits, vec![]);
            assert_eq!(p.max_spend_usd, None);
            if preset == ManagerPolicyPreset::Observe {
                assert_eq!(p.mode, ManagerOperatingModeV2::Monitor);
                assert_eq!(p.capabilities, vec![]);
                assert_eq!(
                    (
                        p.max_created_containers,
                        p.max_created_sessions,
                        p.max_recovery_attempts
                    ),
                    (0, 0, 0)
                );
            } else {
                assert_eq!(p.mode, ManagerOperatingModeV2::Execute);
                assert_eq!(
                    (
                        p.max_created_containers,
                        p.max_created_sessions,
                        p.max_recovery_attempts
                    ),
                    (8, 32, 3)
                );
                for grant in [
                    ManagerCapabilityV2::SelfSuccession,
                    ManagerCapabilityV2::SessionControl,
                    ManagerCapabilityV2::IssueCoordinate,
                ] {
                    assert!(p.capabilities.contains(&grant));
                }
                assert_eq!(
                    p.capabilities.len(),
                    if preset == ManagerPolicyPreset::Execute {
                        8
                    } else {
                        // K14 (#672): Full also grants OperatorDelegation.
                        10
                    }
                );
                assert_eq!(
                    p.capabilities.contains(&ManagerCapabilityV2::Topology),
                    preset == ManagerPolicyPreset::FullProjectControl
                );
            }
            assert_eq!(
                p.allow_create_groups,
                preset == ManagerPolicyPreset::FullProjectControl
                    && scope == HarnessManagerScopeModeV1::Project
            );
        }
    }
}

#[test]
fn saved_overrides_and_zero_recovery_survive_profile_and_suggestions() {
    let mut saved = new_full();
    saved.max_active_sessions = 12;
    saved.max_created_sessions = 160;
    saved.max_recovery_attempts = 0;
    saved.paused = true;
    saved.paused_epic_ids = vec![Uuid::new_v4()];
    saved.group_ids = vec![Uuid::new_v4()];
    saved.provider_limits = vec![ManagerProviderLimitV2 {
        provider: SessionProvider::Local,
        max_active: 2,
    }];
    saved.max_spend_usd = Some(3.5);
    saved.retry_delay_seconds = 123;
    saved.request_timeout_seconds = 4321;
    saved.allowed_launches = vec![
        ManagerLaunchChoiceV2 {
            provider: SessionProvider::Local,
            model: "retained-model".into(),
            effort: Some("retained-effort".into())
        };
        2
    ];
    let applied = apply(
        &saved,
        ManagerPolicyPreset::FullProjectControl,
        ManagerPolicyOrigin::Saved,
        HarnessManagerScopeModeV1::Project,
        &BTreeSet::new(),
    );
    assert_eq!(applied, saved);
    let classification = classify_manager_policy(&applied, HarnessManagerScopeModeV1::Project);
    assert_eq!(classification.preset, ManagerPolicyPreset::Custom);
    assert_eq!(
        classification.permission_profile,
        Some(ManagerPolicyPreset::FullProjectControl)
    );
    assert_eq!(
        classification.zero_conflicts,
        vec![ManagerAllowanceField::RecoveryAttempts]
    );
    let suggestions =
        suggested_manager_allowances(&applied, ManagerPolicyPreset::FullProjectControl);
    assert_eq!(
        suggestions,
        vec![ManagerAllowanceSuggestion {
            field: ManagerAllowanceField::RecoveryAttempts,
            from: 0,
            to: 3
        }]
    );
    let edit = apply_suggested_manager_allowances(
        &applied,
        ManagerPolicyPreset::FullProjectControl,
        &[ManagerAllowanceField::RecoveryAttempts],
    )
    .unwrap();
    assert_eq!(
        edit.changed_fields,
        vec![ManagerPolicyField::RecoveryAttempts]
    );
    let mut expected = saved;
    expected.max_recovery_attempts = 3;
    assert_eq!(edit.policy, expected);
}

#[test]
fn new_touched_zero_is_explicit_but_untouched_defaults_can_follow_profiles() {
    let original = ManagerPolicyV2::default();
    let touched = BTreeSet::from([ManagerPolicyField::RecoveryAttempts]);
    let full = apply(
        &original,
        ManagerPolicyPreset::FullProjectControl,
        ManagerPolicyOrigin::New,
        HarnessManagerScopeModeV1::Project,
        &touched,
    );
    assert_eq!(
        (full.max_created_sessions, full.max_recovery_attempts),
        (32, 0)
    );
    let observe = apply(
        &full,
        ManagerPolicyPreset::Observe,
        ManagerPolicyOrigin::New,
        HarnessManagerScopeModeV1::Project,
        &touched,
    );
    assert_eq!(
        (
            observe.max_created_containers,
            observe.max_created_sessions,
            observe.max_recovery_attempts
        ),
        (0, 0, 0)
    );
    let execute = apply(
        &observe,
        ManagerPolicyPreset::Execute,
        ManagerPolicyOrigin::New,
        HarnessManagerScopeModeV1::Selected,
        &touched,
    );
    assert_eq!(
        (
            execute.max_created_containers,
            execute.max_created_sessions,
            execute.max_recovery_attempts
        ),
        (8, 32, 0)
    );
    let observed_saved = apply(
        &full,
        ManagerPolicyPreset::Observe,
        ManagerPolicyOrigin::Saved,
        HarnessManagerScopeModeV1::Project,
        &BTreeSet::new(),
    );
    assert_eq!(
        (
            observed_saved.max_created_containers,
            observed_saved.max_created_sessions,
            observed_saved.max_recovery_attempts
        ),
        (8, 32, 0)
    );
}

#[test]
fn selected_full_preserves_explicit_root_grant_and_new_full_does_not_add_it() {
    let saved = new_full();
    let edit = apply_manager_policy_preset(
        &saved,
        ManagerPolicyPreset::FullProjectControl,
        &ManagerPresetContext {
            origin: ManagerPolicyOrigin::Saved,
            scope_mode: HarnessManagerScopeModeV1::Selected,
            touched: &BTreeSet::new(),
        },
    );
    assert_eq!(edit.policy, saved);
    assert_eq!(edit.changed_fields, vec![]);
    let c = classify_manager_policy(&edit.policy, HarnessManagerScopeModeV1::Selected);
    assert_eq!(c.preset, ManagerPolicyPreset::Custom);
    assert_eq!(
        c.permission_profile,
        Some(ManagerPolicyPreset::FullProjectControl)
    );
    assert!(c.root_group_permission_mismatch);
    assert_eq!(c.zero_conflicts, vec![]);
    let new = apply(
        &ManagerPolicyV2::default(),
        ManagerPolicyPreset::FullProjectControl,
        ManagerPolicyOrigin::New,
        HarnessManagerScopeModeV1::Selected,
        &BTreeSet::new(),
    );
    assert_eq!(new.allow_create_groups, false);
    assert_eq!(
        classify_manager_policy(&new, HarnessManagerScopeModeV1::Selected).preset,
        ManagerPolicyPreset::FullProjectControl
    );
    for preset in [ManagerPolicyPreset::Observe, ManagerPolicyPreset::Execute] {
        let changed = apply(
            &saved,
            preset,
            ManagerPolicyOrigin::Saved,
            HarnessManagerScopeModeV1::Selected,
            &BTreeSet::new(),
        );
        assert_eq!(changed.allow_create_groups, false);
        assert_eq!(
            changed
                .capabilities
                .contains(&ManagerCapabilityV2::Topology),
            false
        );
    }
}

#[test]
fn semantic_capability_order_and_legacy_defaults_roundtrip_without_upgrade() {
    let mut saved = new_full();
    saved.capabilities.reverse();
    let bytes = serde_json::to_vec(&saved).unwrap();
    let edited = apply(
        &saved,
        ManagerPolicyPreset::FullProjectControl,
        ManagerPolicyOrigin::Saved,
        HarnessManagerScopeModeV1::Project,
        &BTreeSet::new(),
    );
    assert_eq!(
        classify_manager_policy(&edited, HarnessManagerScopeModeV1::Project).preset,
        ManagerPolicyPreset::FullProjectControl
    );
    assert_eq!(serde_json::to_vec(&edited).unwrap(), bytes);
    let old: ManagerPolicyV2 = serde_json::from_str("{}").unwrap();
    assert_eq!(old, ManagerPolicyV2::default());
    assert_eq!(
        classify_manager_policy(&old, HarnessManagerScopeModeV1::Project).preset,
        ManagerPolicyPreset::Custom
    );
    assert_eq!(
        apply(
            &old,
            ManagerPolicyPreset::Custom,
            ManagerPolicyOrigin::Saved,
            HarnessManagerScopeModeV1::Project,
            &BTreeSet::new()
        ),
        old
    );
    let mut six = saved;
    six.capabilities
        .retain(|c| *c != ManagerCapabilityV2::SelfSuccession);
    let decoded: ManagerPolicyV2 =
        serde_json::from_value(serde_json::to_value(&six).unwrap()).unwrap();
    assert_eq!(decoded, six);
    assert_eq!(
        classify_manager_policy(&decoded, HarnessManagerScopeModeV1::Project).permission_profile,
        None
    );
}

#[test]
fn suggested_allowances_are_atomic_and_refuse_stale_or_duplicate_fields() {
    let mut policy = new_full();
    policy.max_created_sessions = 0;
    policy.max_recovery_attempts = 0;
    for fields in [
        vec![
            ManagerAllowanceField::CreatedSessions,
            ManagerAllowanceField::CreatedContainers,
        ],
        vec![
            ManagerAllowanceField::CreatedSessions,
            ManagerAllowanceField::CreatedSessions,
        ],
    ] {
        let original = policy.clone();
        assert!(
            apply_suggested_manager_allowances(
                &policy,
                ManagerPolicyPreset::FullProjectControl,
                &fields
            )
            .is_err()
        );
        assert_eq!(policy, original);
    }
    policy.max_recovery_attempts = 1;
    assert_eq!(
        apply_suggested_manager_allowances(
            &policy,
            ManagerPolicyPreset::FullProjectControl,
            &[
                ManagerAllowanceField::CreatedSessions,
                ManagerAllowanceField::RecoveryAttempts
            ]
        )
        .unwrap_err(),
        ManagerPresetEditError::NotCurrentlySuggested(ManagerAllowanceField::RecoveryAttempts)
    );
    assert_eq!(policy.max_created_sessions, 0);
    assert_eq!(
        suggested_manager_allowances(&policy, ManagerPolicyPreset::Custom),
        vec![]
    );
    assert_eq!(
        apply_suggested_manager_allowances(&policy, ManagerPolicyPreset::FullProjectControl, &[])
            .unwrap()
            .policy,
        policy
    );
}

#[test]
fn only_advertised_zero_operations_conflict_and_invalid_values_remain_visible() {
    let mut execute = apply(
        &ManagerPolicyV2::default(),
        ManagerPolicyPreset::Execute,
        ManagerPolicyOrigin::New,
        HarnessManagerScopeModeV1::Project,
        &BTreeSet::new(),
    );
    execute.max_created_containers = 0;
    assert_eq!(
        classify_manager_policy(&execute, HarnessManagerScopeModeV1::Project).preset,
        ManagerPolicyPreset::Execute
    );
    execute.max_active_sessions = 0;
    let c = classify_manager_policy(&execute, HarnessManagerScopeModeV1::Project);
    assert_eq!(c.preset, ManagerPolicyPreset::Custom);
    assert_eq!(c.validation_error, Some("manager_v2_invalid_policy"));
    assert_eq!(
        apply(
            &execute,
            ManagerPolicyPreset::Custom,
            ManagerPolicyOrigin::Saved,
            HarnessManagerScopeModeV1::Project,
            &BTreeSet::new()
        ),
        execute
    );
}

#[test]
fn legacy_full_grants_stay_exact_until_the_operator_reapplies_the_preset() {
    // A Full policy saved before SessionControl/IssueCoordinate existed keeps
    // exactly its stored grants: granting the manager is operator-owned.
    let mut legacy = new_full();
    legacy.capabilities.retain(|c| {
        !matches!(
            c,
            ManagerCapabilityV2::SessionControl | ManagerCapabilityV2::IssueCoordinate
        )
    });
    // Full's 10 grants (K14 added OperatorDelegation) minus the two removed.
    assert_eq!(legacy.capabilities.len(), 8);
    assert_eq!(legacy.validate(), Ok(()));
    let classified = classify_manager_policy(&legacy, HarnessManagerScopeModeV1::Project);
    assert_eq!(classified.preset, ManagerPolicyPreset::Custom);

    let reapplied = apply(
        &legacy,
        ManagerPolicyPreset::FullProjectControl,
        ManagerPolicyOrigin::Saved,
        HarnessManagerScopeModeV1::Project,
        &BTreeSet::new(),
    );
    assert!(
        reapplied
            .capabilities
            .contains(&ManagerCapabilityV2::SessionControl)
    );
    assert!(
        reapplied
            .capabilities
            .contains(&ManagerCapabilityV2::IssueCoordinate)
    );
    assert_eq!(
        classify_manager_policy(&reapplied, HarnessManagerScopeModeV1::Project).preset,
        ManagerPolicyPreset::FullProjectControl
    );
}

#[test]
#[allow(clippy::unwrap_used)]
fn full_preset_grants_operator_delegation_but_never_upgrades_a_saved_policy() {
    // K14 (#672): applying Full grants the operator's "full access" delegation.
    let full = new_full();
    assert!(
        full.capabilities
            .contains(&ManagerCapabilityV2::OperatorDelegation)
    );
    assert_eq!(
        classify_manager_policy(&full, HarnessManagerScopeModeV1::Project).preset,
        ManagerPolicyPreset::FullProjectControl
    );
    // A Full policy saved before K14 keeps its exact grants: it is shown as
    // Custom until the operator re-applies Full and saves.
    let mut legacy = full;
    legacy
        .capabilities
        .retain(|c| *c != ManagerCapabilityV2::OperatorDelegation);
    let bytes = serde_json::to_vec(&legacy).unwrap();
    let classification = classify_manager_policy(&legacy, HarnessManagerScopeModeV1::Project);
    assert_eq!(classification.preset, ManagerPolicyPreset::Custom);
    assert_eq!(serde_json::to_vec(&legacy).unwrap(), bytes);
    let reapplied = apply_manager_policy_preset(
        &legacy,
        ManagerPolicyPreset::FullProjectControl,
        &ManagerPresetContext {
            origin: ManagerPolicyOrigin::Saved,
            scope_mode: HarnessManagerScopeModeV1::Project,
            touched: &BTreeSet::new(),
        },
    );
    assert!(
        reapplied
            .changed_fields
            .contains(&ManagerPolicyField::Capabilities)
    );
    assert!(
        reapplied
            .policy
            .capabilities
            .contains(&ManagerCapabilityV2::OperatorDelegation)
    );
}
