//! Tests for the authority projection. The projection itself lives in
//! `store::agent_authority` (moved down for #1021 S3a: it is a pure `impl
//! Store` block and `store` must not depend on `session`).

pub(crate) use crate::store::agent_authority::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::agent_verbs::tests::test_session;
    use crate::store::Store;
    use rsi_common::agent_control_schema::AgentControlVerbV1 as Verb;
    use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
    use rsi_common::harness_manager_v2::{ConfigureHarnessManagerPolicyRequestV2, ManagerPolicyV2};
    use rsi_common::harness_manager_v2::{
        ManagerActionKindV2 as Action, ManagerCapabilityV2 as Capability, ManagerOperatingModeV2,
    };
    use rsi_common::manager_operator_delegation::{
        DAEMON_SETTINGS_OPERATOR_METHODS, OPERATOR_DELEGATION_METHODS,
        STORAGE_CONTROL_OPERATOR_METHODS,
    };
    use rsi_common::types::Project;
    use rsi_common::types::{SessionKind, SessionStatus};
    use rusqlite::params;
    use std::path::PathBuf;
    use uuid::Uuid;

    /// The authority catalog is every session's entry point, so it must be in
    /// the role-independent baseline; role controls stay role-gated.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn authority_catalog_is_in_every_sessions_baseline() {
        assert!(is_baseline_verb(Verb::GetAuthorityCatalog));
        assert!(is_baseline_verb(Verb::GetStatus));
        for role_verb in [
            Verb::SpawnChild,
            Verb::ManagerControl,
            Verb::SubmitReviewReceipt,
        ] {
            assert!(!is_baseline_verb(role_verb), "{role_verb:?}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn manager_action_grants_cover_new_variants_and_closed_delegation() {
        assert_eq!(
            ACTIONS
                .iter()
                .find(|(action, _)| *action == Action::OperatorCall),
            Some(&(Action::OperatorCall, Capability::OperatorDelegation))
        );
        for action in [
            Action::SettleUncertainAction,
            Action::RetireLeadContinuations,
        ] {
            assert!(ACTIONS.contains(&(action, Capability::LeadControl)));
        }
        assert_eq!(OPERATOR_DELEGATION_METHODS.len(), 4);
        assert_eq!(STORAGE_CONTROL_OPERATOR_METHODS.len(), 2);
        assert_eq!(DAEMON_SETTINGS_OPERATOR_METHODS.len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn manager_update_variants_follow_role_and_grants() {
        let work_only = [Capability::WorkPlan];
        let manager = permitted_updates(&work_only, true, false);
        assert!(manager.contains(&"work"));
        assert!(manager.contains(&"stage"));
        assert!(!manager.contains(&"request"));
        assert!(!manager.contains(&"request_review"));
        assert!(!manager.contains(&"integration"));

        let lead = permitted_updates(&work_only, false, true);
        assert!(lead.contains(&"stage"));
        assert!(lead.contains(&"request"));
        assert!(!lead.contains(&"work"));
        assert!(!lead.contains(&"ownership"));

        let extended = [
            Capability::WorkPlan,
            Capability::SessionCreate,
            Capability::Integration,
        ];
        let lead_extended = permitted_updates(&extended, false, true);
        assert!(lead_extended.contains(&"request_review"));
        assert!(lead_extended.contains(&"integration"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn worker_baseline_and_role_specific_catalog_are_positive() {
        assert!(permitted_verb(Verb::GetStatus, &VerbRights::default()));
        assert!(permitted_verb(
            Verb::SpawnChild,
            &VerbRights {
                lead: true,
                ..Default::default()
            }
        ));
        assert!(permitted_verb(
            Verb::SubmitReviewReceipt,
            &VerbRights {
                reviewer: true,
                ..Default::default()
            }
        ));
        assert!(permitted_verb(
            Verb::ManagerWorkView,
            &VerbRights {
                managed_worker: true,
                ..Default::default()
            }
        ));
        assert!(permitted_verb(
            Verb::ManagerControl,
            &VerbRights {
                control: true,
                ..Default::default()
            }
        ));
        // #1284: a live Issue-bound worker may append to its Issue, and that
        // right grants no lifecycle or archive control.
        let bound_writer = VerbRights {
            issue_bound: true,
            issue_bound_writer: true,
            ..Default::default()
        };
        assert!(permitted_verb(Verb::UpdateIssue, &bound_writer));
        assert!(permitted_verb(Verb::GetIssue, &bound_writer));
        for verb in [
            Verb::UpdateIssueStatus,
            Verb::ArchiveIssue,
            Verb::RestoreIssue,
            Verb::ListIssues,
        ] {
            assert!(!permitted_verb(verb, &bound_writer), "{verb:?}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn managed_worker_with_broken_hierarchy_reports_projection_error() {
        let store = Store::open_in_memory().unwrap();
        let project = Uuid::new_v4();
        let manager_id = Uuid::new_v4();
        let group_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let worker_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        store
            .insert_project(&Project {
                id: project,
                name: "Broken managed hierarchy".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let mut manager = test_session(manager_id, PathBuf::from("/tmp/authority-manager"));
        manager.project_id = Some(project);
        manager.status = SessionStatus::Running;
        store.insert_session(&manager).unwrap();
        let mut group = test_session(group_id, PathBuf::from("/tmp/authority-group"));
        group.project_id = Some(project);
        group.session_kind = SessionKind::Group;
        group.status = SessionStatus::Running;
        store.insert_session(&group).unwrap();
        let mut epic = test_session(epic_id, PathBuf::from("/tmp/authority-epic"));
        epic.project_id = Some(project);
        epic.session_kind = SessionKind::Epic;
        epic.status = SessionStatus::Running;
        epic.parent_id = Some(group_id);
        store.insert_session(&epic).unwrap();
        let mut worker = test_session(worker_id, PathBuf::from("/tmp/authority-worker"));
        worker.project_id = Some(project);
        worker.parent_id = Some(epic_id);
        worker.status = SessionStatus::Running;
        store.insert_session(&worker).unwrap();
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: project,
                session_id: manager_id,
                epic_ids: Some(vec![epic_id]),
                expected_row_version: 0,
            })
            .unwrap();
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: project,
                expected_scope_version: 1,
                expected_policy_version: 0,
                idempotency_key: "managed-worker-policy".into(),
                policy: ManagerPolicyV2 {
                    mode: ManagerOperatingModeV2::Execute,
                    capabilities: vec![Capability::WorkPlan],
                    ..Default::default()
                },
            })
            .unwrap();
        store.conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        store
            .conn
            .execute(
                "INSERT INTO harness_manager_v2_entities
                 (session_id,operation_id,project_id,manager_session_id,
                  scope_version,policy_version,kind,created_at)
                 VALUES (?1,?2,?3,?4,1,1,'session',?5)",
                params![
                    worker_id.to_string(),
                    Uuid::new_v4().to_string(),
                    project.to_string(),
                    manager_id.to_string(),
                    now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                ],
            )
            .unwrap();
        let managed = store.agent_authority_projection(worker_id).unwrap();
        assert!(managed.verbs.contains(&Verb::ManagerWorkView));
        store
            .conn
            .execute(
                "UPDATE sessions SET parent_id=?2 WHERE id=?1",
                params![worker_id.to_string(), group_id.to_string()],
            )
            .unwrap();
        let outside_epic = store.agent_authority_projection(worker_id).unwrap();
        assert!(!outside_epic.verbs.contains(&Verb::ManagerWorkView));
        store
            .conn
            .execute(
                "UPDATE sessions SET parent_id=?1 WHERE id=?1",
                [worker_id.to_string()],
            )
            .unwrap();
        store.conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();

        let error = store.agent_authority_projection(worker_id).unwrap_err();
        assert!(error.to_string().contains("manager_v2_hierarchy_cycle"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn active_reviewer_without_live_custody_reports_projection_error() {
        let store = Store::open_in_memory().unwrap();
        let project = Uuid::new_v4();
        let manager_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let reviewer_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        store
            .insert_project(&Project {
                id: project,
                name: "Review custody failure".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let mut manager = test_session(manager_id, PathBuf::from("/tmp/authority-manager"));
        manager.project_id = Some(project);
        manager.status = SessionStatus::Running;
        store.insert_session(&manager).unwrap();
        let mut epic = test_session(epic_id, PathBuf::from("/tmp/authority-epic"));
        epic.project_id = Some(project);
        epic.session_kind = SessionKind::Epic;
        epic.status = SessionStatus::Running;
        store.insert_session(&epic).unwrap();
        let mut reviewer = test_session(reviewer_id, PathBuf::from("/tmp/authority-reviewer"));
        reviewer.project_id = Some(project);
        reviewer.parent_id = Some(epic_id);
        reviewer.status = SessionStatus::Running;
        store.insert_session(&reviewer).unwrap();

        store.conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        store
            .conn
            .execute(
                "INSERT INTO manager_review_assignments (
                    assignment_id,project_id,epic_id,manager_session_id,scope_version,
                    work_key,spec_revision,author_session_id,source_sha,reviewer_session_id,
                    reviewer_invocation_id,reviewer_custody_id,reviewer_custody_generation,
                    action_operation_id,state,row_version,request_json,request_fingerprint,
                    created_at,updated_at)
                 VALUES (?1,?2,?3,?4,1,'authority',1,?4,?5,?6,?7,?8,1,?9,
                         'active',1,'{}',?10,?11,?11)",
                params![
                    Uuid::new_v4().to_string(),
                    project.to_string(),
                    epic_id.to_string(),
                    manager_id.to_string(),
                    "0".repeat(40),
                    reviewer_id.to_string(),
                    Uuid::new_v4().to_string(),
                    Uuid::new_v4().to_string(),
                    Uuid::new_v4().to_string(),
                    format!("sha256:{}", "0".repeat(64)),
                    now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                ],
            )
            .unwrap();
        store.conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();

        let error = store.agent_authority_projection(reviewer_id).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("live sandbox custody ownership is missing")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn committed_lead_publication_changes_revision_and_role() {
        let store = Store::open_in_memory().unwrap();
        let epic_id = Uuid::new_v4();
        let lead_id = Uuid::new_v4();
        let mut epic = test_session(epic_id, PathBuf::from("/tmp/authority-epic"));
        epic.session_kind = SessionKind::Epic;
        epic.status = SessionStatus::Running;
        epic.project_id = None;
        store.insert_session(&epic).unwrap();
        let mut lead = test_session(lead_id, PathBuf::from("/tmp/authority-lead"));
        lead.status = SessionStatus::Starting;
        lead.parent_id = Some(epic_id);
        lead.project_id = None;
        store.insert_session(&lead).unwrap();

        let pending = store.agent_authority_projection(lead_id).unwrap();
        assert!(pending.pending);
        assert!(!pending.is_lead);
        assert!(pending.verbs.contains(&Verb::GetStatus));

        store
            .conn
            .execute(
                "UPDATE sessions SET lead_session_id=?2,updated_at=?3 WHERE id=?1",
                params![
                    epic_id.to_string(),
                    lead_id.to_string(),
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
        let published = store.agent_authority_projection(lead_id).unwrap();
        assert!(published.is_lead);
        assert!(published.verbs.contains(&Verb::SpawnChild));
        assert!(published.guidance_ids.contains(&"epic_lead"));
        assert_ne!(pending.revision, published.revision);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
    #[test]
    fn live_policy_publication_changes_manager_and_lead_catalogs() {
        let store = Store::open_in_memory().unwrap();
        let project = Uuid::new_v4();
        let manager_id = Uuid::new_v4();
        let group_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let lead_id = Uuid::new_v4();
        let now = chrono::Utc::now();
        store
            .insert_project(&Project {
                id: project,
                name: "Authority projection".into(),
                path: None,
                description: None,
                color: Project::DEFAULT_COLOR.into(),
                context_files: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let mut manager = test_session(manager_id, PathBuf::from("/tmp/authority-manager"));
        manager.project_id = Some(project);
        manager.status = SessionStatus::Running;
        store.insert_session(&manager).unwrap();
        let mut group = test_session(group_id, PathBuf::from("/tmp/authority-group"));
        group.project_id = Some(project);
        group.session_kind = SessionKind::Group;
        group.status = SessionStatus::Running;
        store.insert_session(&group).unwrap();
        let mut epic = test_session(epic_id, PathBuf::from("/tmp/authority-epic"));
        epic.project_id = Some(project);
        epic.session_kind = SessionKind::Epic;
        epic.status = SessionStatus::Running;
        epic.parent_id = Some(group_id);
        epic.lead_session_id = Some(lead_id);
        store.insert_session(&epic).unwrap();
        let mut lead = test_session(lead_id, PathBuf::from("/tmp/authority-lead"));
        lead.project_id = Some(project);
        lead.status = SessionStatus::Running;
        lead.parent_id = Some(epic_id);
        store.insert_session(&lead).unwrap();
        let lead_before_appointment = store.agent_authority_projection(lead_id).unwrap();
        assert!(lead_before_appointment.verbs.contains(&Verb::SpawnChild));
        assert!(
            lead_before_appointment
                .verbs
                .contains(&Verb::TopologyUpsert)
        );
        assert!(lead_before_appointment.verbs.contains(&Verb::TopologyList));
        assert!(!lead_before_appointment.verbs.contains(&Verb::ManagerInbox));
        store
            .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                group_ids: Vec::new(),
                project_id: project,
                session_id: manager_id,
                epic_ids: Some(vec![epic_id]),
                expected_row_version: 0,
            })
            .unwrap();

        let before = store.agent_authority_projection(manager_id).unwrap();
        assert!(before.is_manager);
        assert!(before.verbs.contains(&Verb::ManagerInspect));
        assert!(!before.verbs.contains(&Verb::ManagerControl));

        let capabilities = vec![
            Capability::WorkPlan,
            Capability::IssueCoordinate,
            Capability::LeadControl,
            Capability::OperatorDelegation,
            Capability::Automation,
        ];
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: project,
                expected_scope_version: 1,
                expected_policy_version: 0,
                idempotency_key: "authority-policy".into(),
                policy: ManagerPolicyV2 {
                    mode: ManagerOperatingModeV2::Execute,
                    capabilities,
                    ..Default::default()
                },
            })
            .unwrap();
        let manager_after = store.agent_authority_projection(manager_id).unwrap();
        assert_ne!(before.revision, manager_after.revision);
        assert!(manager_after.verbs.contains(&Verb::ManagerUpdate));
        assert!(manager_after.verbs.contains(&Verb::ListIssues));
        assert!(manager_after.verbs.contains(&Verb::ManagerControl));
        assert!(manager_after.verbs.contains(&Verb::TopologyExecute));
        assert!(manager_after.verbs.contains(&Verb::TopologyGetExecution));
        assert!(
            manager_after
                .control_actions
                .contains(&Action::OperatorCall)
        );
        assert!(
            manager_after
                .control_actions
                .contains(&Action::RetireLeadContinuations)
        );
        assert_eq!(
            manager_after.delegated_operator_methods,
            OPERATOR_DELEGATION_METHODS
        );

        let lead_after = store.agent_authority_projection(lead_id).unwrap();
        assert_ne!(lead_before_appointment.revision, lead_after.revision);
        assert!(lead_after.verbs.contains(&Verb::ManagerInbox));
        assert!(lead_after.verbs.contains(&Verb::ManagerUpdate));
        assert!(!lead_after.verbs.contains(&Verb::ManagerControl));

        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: project,
                expected_scope_version: 1,
                expected_policy_version: 1,
                idempotency_key: "authority-policy-paused".into(),
                policy: ManagerPolicyV2 {
                    mode: ManagerOperatingModeV2::Execute,
                    paused: true,
                    capabilities: vec![
                        Capability::WorkPlan,
                        Capability::IssueCoordinate,
                        Capability::LeadControl,
                        Capability::OperatorDelegation,
                    ],
                    ..Default::default()
                },
            })
            .unwrap();
        let paused = store.agent_authority_projection(manager_id).unwrap();
        assert_ne!(manager_after.revision, paused.revision);
        assert!(paused.verbs.contains(&Verb::ListIssues));
        assert!(paused.verbs.contains(&Verb::ManagerUpdate));
        assert!(!paused.verbs.contains(&Verb::ManagerControl));
        assert!(paused.control_actions.is_empty());
        assert!(paused.delegated_operator_methods.is_empty());

        let reviewer_before_allocation = store.agent_authority_projection(lead_id).unwrap();
        let assignment_id = Uuid::new_v4();
        let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        // Model action allocation precedes invocation/custody publication. The
        // synthetic action id represents that in-flight window only.
        store.conn.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        store
            .conn
            .execute(
                "INSERT INTO manager_review_assignments (
                    assignment_id,project_id,epic_id,manager_session_id,scope_version,
                    work_key,spec_revision,author_session_id,source_sha,reviewer_session_id,
                    action_operation_id,state,row_version,request_json,request_fingerprint,
                    created_at,updated_at)
                 VALUES (?1,?2,?3,?4,1,'authority',1,?4,?5,?6,?7,
                         'allocating',1,'{}',?8,?9,?9)",
                params![
                    assignment_id.to_string(),
                    project.to_string(),
                    epic_id.to_string(),
                    manager_id.to_string(),
                    "0".repeat(40),
                    lead_id.to_string(),
                    Uuid::new_v4().to_string(),
                    format!("sha256:{}", "0".repeat(64)),
                    at,
                ],
            )
            .unwrap();
        store.conn.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        let allocating = store.agent_authority_projection(lead_id).unwrap();
        assert_ne!(reviewer_before_allocation.revision, allocating.revision);
        assert!(allocating.pending);
        assert!(!allocating.is_reviewer);
        assert!(!allocating.verbs.contains(&Verb::SubmitReviewReceipt));

        let next_lead_id = Uuid::new_v4();
        let mut next_lead = lead.clone();
        next_lead.id = next_lead_id;
        store.insert_session(&next_lead).unwrap();
        store
            .conn
            .execute(
                "UPDATE sessions SET lead_session_id=?2,updated_at=?3 WHERE id=?1",
                params![
                    epic_id.to_string(),
                    next_lead_id.to_string(),
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
        let former = store.agent_authority_projection(lead_id).unwrap();
        let successor = store.agent_authority_projection(next_lead_id).unwrap();
        assert_ne!(lead_after.revision, former.revision);
        assert!(!former.verbs.contains(&Verb::SpawnChild));
        assert!(successor.verbs.contains(&Verb::SpawnChild));
        assert!(successor.guidance_ids.contains(&"epic_lead"));
    }
}
