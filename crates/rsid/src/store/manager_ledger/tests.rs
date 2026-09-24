use super::*;
use crate::session::agent_verbs::tests::test_session;
use rsi_common::{
    harness_manager::*,
    types::{Project, SessionKind, SessionStatus},
};
use std::path::PathBuf;

#[path = "health_tests.rs"]
mod health_tests;
#[path = "issue_541_tests.rs"]
mod issue_541_tests;
#[path = "lead_review_tests.rs"]
mod lead_review_tests;
mod live_bookkeeping;
mod mail_capacity;
mod work_identity;
mod work_view;

struct Fixture {
    store: Store,
    project: Uuid,
    manager: Uuid,
    epic: Uuid,
    lead: Uuid,
}
fn fixture() -> Fixture {
    fixture_using(Store::open_in_memory().unwrap())
}
fn fixture_using(store: Store) -> Fixture {
    let project = Uuid::new_v4();
    store
        .insert_project(&Project {
            id: project,
            name: "Delivery project".into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .unwrap();
    let mut manager = test_session(Uuid::new_v4(), PathBuf::from("/var/tmp/ham-v2-fd23a414"));
    manager.session_kind = SessionKind::Standard;
    manager.project_id = Some(project);
    manager.status = SessionStatus::Completed;
    store.insert_session(&manager).unwrap();
    let mut group = manager.clone();
    group.id = Uuid::new_v4();
    group.session_kind = SessionKind::Group;
    store.insert_session(&group).unwrap();
    let mut epic = manager.clone();
    epic.id = Uuid::new_v4();
    epic.session_kind = SessionKind::Epic;
    epic.parent_id = Some(group.id);
    store.insert_session(&epic).unwrap();
    let mut lead = manager.clone();
    lead.id = Uuid::new_v4();
    lead.session_kind = SessionKind::Feature;
    lead.parent_id = Some(epic.id);
    store.insert_session(&lead).unwrap();
    store
        .conn
        .execute(
            "UPDATE sessions SET lead_session_id=?2 WHERE id=?1",
            params![epic.id.to_string(), lead.id.to_string()],
        )
        .unwrap();
    store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: project,
            session_id: manager.id,
            epic_ids: Some(vec![epic.id]),
            expected_row_version: 0,
        })
        .unwrap();
    store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: project,
            expected_scope_version: 1,
            expected_policy_version: 0,
            idempotency_key: "policy".into(),
            policy: ManagerPolicyV2 {
                capabilities: vec![
                    ManagerCapabilityV2::WorkPlan,
                    ManagerCapabilityV2::Integration,
                ],
                ..Default::default()
            },
        })
        .unwrap();
    Fixture {
        store,
        project,
        manager: manager.id,
        epic: epic.id,
        lead: lead.id,
    }
}
fn request(change: ManagerUpdateV2, key: &str) -> AgentManagerUpdateRequestV2 {
    AgentManagerUpdateRequestV2 {
        fence: ManagerFenceV2 {
            scope_version: 1,
            policy_version: 1,
        },
        idempotency_key: key.into(),
        change,
    }
}

#[test]
fn overview_pages_include_the_current_root_manager_succession_fence() {
    let f = fixture();
    let observation = f.store.manager_succession_observation(f.manager).unwrap();
    for operator in [false, true] {
        let mut query = AgentManagerInspectRequestV2 {
            limit: 1,
            ..Default::default()
        };
        let mut rows = Vec::new();
        loop {
            let page = if operator {
                f.store.manager_v2_inspect_operator(f.project, &query)
            } else {
                f.store.manager_v2_inspect(f.manager, &query)
            }
            .unwrap();
            assert_eq!(page.rows.len(), 1);
            rows.extend(page.rows);
            query.cursor = page.next_cursor;
            if query.cursor.is_none() {
                assert!(page.complete);
                break;
            }
            assert!(rows.len() < 8, "the bounded overview must finish paging");
        }
        let control = rows
            .iter()
            .find(|row| row["type"] == "manager_control")
            .unwrap();
        assert_eq!(control["logical_manager_session_id"], json!(f.manager));
        assert_eq!(control["current_session_id"], json!(f.manager));
        assert_eq!(control["eligibility"], "eligible");
        assert_eq!(control["expected"], json!(observation.expected));
        assert!(control["expected"]["authority_epoch"].as_i64().unwrap() > 0);
        assert_eq!(control["unresolved_operation_id"], Value::Null);
        assert!(
            rows.iter()
                .any(|row| row["type"] == "lead_control" && row["epic_id"] == json!(f.epic))
        );
        let budget = rows
            .iter()
            .find(|row| row["type"] == "record_budget")
            .unwrap();
        for class in ["coordination", "retrieval", "resource", "lifecycle"] {
            assert_eq!(budget[class]["limit"], 1024);
            assert!(budget[class]["used"].as_i64().unwrap() >= 0);
        }
    }

    // A lead's projected overview remains its own Epic control surface.
    let lead = f
        .store
        .manager_v2_inspect(f.lead, &AgentManagerInspectRequestV2::default())
        .unwrap();
    assert_eq!(
        lead.rows
            .iter()
            .map(|row| row["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["lead_control", "overview", "overview"]
    );
    assert_eq!(lead.rows[0]["epic_id"], json!(f.epic));
    assert!(
        f.store
            .manager_v2_inspect(
                f.lead,
                &AgentManagerInspectRequestV2 {
                    section: ManagerInspectSectionV2::Archive,
                    ..Default::default()
                }
            )
            .is_err()
    );
}

#[test]
#[allow(clippy::unwrap_used)]
fn manager_overview_includes_its_project_metadata() {
    let f = fixture();
    let mut project = f.store.get_project(f.project).unwrap().unwrap();
    project.path = Some(PathBuf::from("/workspace/delivery"));
    project.description = Some("Delivery metadata".into());
    f.store.update_project(&project).unwrap();

    let page = inspect(&f, ManagerInspectSectionV2::Overview);
    let overview = page
        .rows
        .iter()
        .find(|row| row["type"] == "overview")
        .unwrap();
    assert_eq!(
        overview["project"],
        json!({
            "id":f.project,
            "name":"Delivery project",
            "path":"/workspace/delivery",
            "description":"Delivery metadata",
        })
    );
}

#[test]
#[allow(clippy::unwrap_used)]
fn archive_pages_scoped_retired_containers_with_restore_fences() {
    let f = fixture();
    let manager = f.store.get_session(f.manager).unwrap().unwrap();
    let selected_group = f
        .store
        .get_session(f.epic)
        .unwrap()
        .unwrap()
        .parent_id
        .unwrap();

    let mut other_group = manager.clone();
    other_group.id = Uuid::new_v4();
    other_group.session_kind = SessionKind::Group;
    f.store.insert_session(&other_group).unwrap();
    let mut out_of_scope_epic = manager.clone();
    out_of_scope_epic.id = Uuid::new_v4();
    out_of_scope_epic.session_kind = SessionKind::Epic;
    out_of_scope_epic.parent_id = Some(other_group.id);
    f.store.insert_session(&out_of_scope_epic).unwrap();

    let foreign_project = Uuid::new_v4();
    f.store
        .insert_project(&Project {
            id: foreign_project,
            name: "Foreign project".into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .unwrap();
    let mut foreign_group = manager;
    foreign_group.id = Uuid::new_v4();
    foreign_group.project_id = Some(foreign_project);
    foreign_group.session_kind = SessionKind::Group;
    f.store.insert_session(&foreign_group).unwrap();

    f.store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: vec![selected_group],
            project_id: f.project,
            session_id: f.manager,
            epic_ids: None,
            expected_row_version: 1,
        })
        .unwrap();
    grant_topology(&f);
    f.store
        .update_session_status(f.epic, SessionStatus::Archived)
        .unwrap();
    f.store
        .update_session_status(selected_group, SessionStatus::Deleted)
        .unwrap();
    f.store
        .update_session_status(out_of_scope_epic.id, SessionStatus::Archived)
        .unwrap();
    f.store
        .update_session_status(foreign_group.id, SessionStatus::Archived)
        .unwrap();

    let mut query = AgentManagerInspectRequestV2 {
        section: ManagerInspectSectionV2::Archive,
        epic_id: None,
        limit: 1,
        ..Default::default()
    };
    let first = f.store.manager_v2_inspect(f.manager, &query).unwrap();
    assert_eq!(first.rows.len(), 1);
    assert!(!first.complete);
    query.cursor = first.next_cursor.clone();
    let second = f.store.manager_v2_inspect(f.manager, &query).unwrap();
    assert_eq!(second.rows.len(), 1);
    assert!(second.complete);
    assert!(second.next_cursor.is_none());

    let rows = first
        .rows
        .into_iter()
        .chain(second.rows)
        .collect::<Vec<_>>();
    let ids = rows
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(f.epic.to_string().as_str()));
    assert!(ids.contains(selected_group.to_string().as_str()));
    assert!(rows.iter().all(|row| row["type"] == "archive"));
    for row in &rows {
        let id = Uuid::parse_str(row["id"].as_str().unwrap()).unwrap();
        let stored = f.store.get_session(id).unwrap().unwrap();
        assert_eq!(row["expected_updated_at"], json!(stored.updated_at));
        assert!(row["descendant_count"].as_u64().is_some());
    }
    let group = rows
        .iter()
        .find(|row| row["id"] == json!(selected_group))
        .unwrap();
    assert_eq!(group["restorable"], false);
    assert_eq!(group["restore_blocker"], "container_not_empty");
}

#[allow(clippy::unwrap_used)]
fn grant_topology(f: &Fixture) {
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let policy = f
        .store
        .get_harness_manager_policy(f.project)
        .unwrap()
        .unwrap();
    f.store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: f.project,
            expected_scope_version: config.row_version,
            expected_policy_version: policy.row_version,
            idempotency_key: format!("topology-{}", config.row_version),
            policy: ManagerPolicyV2 {
                capabilities: vec![ManagerCapabilityV2::Topology],
                ..Default::default()
            },
        })
        .unwrap();
}

#[test]
#[allow(clippy::unwrap_used)]
fn archive_project_scope_lists_only_its_project_containers() {
    let f = fixture();
    let manager = f.store.get_session(f.manager).unwrap().unwrap();
    let group = manager.parent_id;
    let mut local = manager;
    local.id = Uuid::new_v4();
    local.session_kind = SessionKind::Group;
    local.parent_id = group;
    f.store.insert_session(&local).unwrap();
    let foreign_project = Uuid::new_v4();
    f.store
        .insert_project(&Project {
            id: foreign_project,
            name: "Other".into(),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .unwrap();
    let mut foreign = local.clone();
    foreign.id = Uuid::new_v4();
    foreign.project_id = Some(foreign_project);
    f.store.insert_session(&foreign).unwrap();
    f.store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: vec![],
            project_id: f.project,
            session_id: f.manager,
            epic_ids: None,
            expected_row_version: 1,
        })
        .unwrap();
    f.store
        .update_session_status(local.id, SessionStatus::Archived)
        .unwrap();
    f.store
        .update_session_status(foreign.id, SessionStatus::Archived)
        .unwrap();
    let rows = inspect(&f, ManagerInspectSectionV2::Archive).rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], json!(local.id));
}

#[test]
#[allow(clippy::unwrap_used)]
fn archive_explicit_epic_scope_lists_selected_epic() {
    let f = fixture();
    let manager = f.store.get_session(f.manager).unwrap().unwrap();
    let selected_group = f
        .store
        .get_session(f.epic)
        .unwrap()
        .unwrap()
        .parent_id
        .unwrap();
    let mut outside = manager;
    outside.id = Uuid::new_v4();
    outside.session_kind = SessionKind::Group;
    f.store.insert_session(&outside).unwrap();
    f.store
        .update_session_status(f.epic, SessionStatus::Archived)
        .unwrap();
    f.store
        .update_session_status(selected_group, SessionStatus::Archived)
        .unwrap();
    f.store
        .update_session_status(outside.id, SessionStatus::Archived)
        .unwrap();
    let ids = inspect(&f, ManagerInspectSectionV2::Archive)
        .rows
        .into_iter()
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(ids, std::collections::HashSet::from([f.epic.to_string()]));
}

#[test]
#[allow(clippy::unwrap_used)]
fn archive_entity_scope_expires_on_narrowed_reappointment() {
    let f = fixture();
    let mut group = f.store.get_session(f.manager).unwrap().unwrap();
    group.id = Uuid::new_v4();
    group.session_kind = SessionKind::Group;
    f.store.insert_session(&group).unwrap();
    let mut epic = group.clone();
    epic.id = Uuid::new_v4();
    epic.session_kind = SessionKind::Epic;
    epic.parent_id = Some(group.id);
    f.store.insert_session(&epic).unwrap();
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    for (session, kind) in [(group.id, "Group"), (epic.id, "Epic")] {
        let key = format!("entity-{session}");
        f.store
            .manager_v2_save_receipt(
                &config,
                Some(f.manager),
                1,
                "action",
                &key,
                &json!({"created":session}),
                &json!({"id":session}),
            )
            .unwrap();
        let operation: String = f
            .store
            .conn
            .query_row(
                "SELECT id FROM harness_manager_v2_operations WHERE idempotency_key=?1",
                [&key],
                |row| row.get(0),
            )
            .unwrap();
        f.store.conn.execute(
            "INSERT INTO harness_manager_v2_entities(session_id,operation_id,project_id,manager_session_id,scope_version,policy_version,kind,created_at) VALUES(?1,?2,?3,?4,?5,1,?6,?7)",
            params![session.to_string(), operation, f.project.to_string(), f.manager.to_string(), config.row_version, kind, now()],
        ).unwrap();
    }
    f.store
        .update_session_status(group.id, SessionStatus::Archived)
        .unwrap();
    f.store
        .update_session_status(epic.id, SessionStatus::Archived)
        .unwrap();
    let rows = inspect(&f, ManagerInspectSectionV2::Archive).rows;
    assert!(rows.iter().any(|row| row["id"] == json!(group.id)));
    assert!(rows.iter().any(|row| row["id"] == json!(epic.id)));
    let filtered = f
        .store
        .manager_v2_inspect(
            f.manager,
            &AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Archive,
                epic_id: Some(epic.id),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(filtered.rows.iter().any(|row| row["id"] == json!(epic.id)));

    let selected_group = f
        .store
        .get_session(f.epic)
        .unwrap()
        .unwrap()
        .parent_id
        .unwrap();
    f.store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: vec![selected_group],
            project_id: f.project,
            session_id: f.manager,
            epic_ids: None,
            expected_row_version: 1,
        })
        .unwrap();
    let rows = inspect(&f, ManagerInspectSectionV2::Archive).rows;
    assert!(
        rows.iter()
            .all(|row| row["id"] != json!(group.id) && row["id"] != json!(epic.id))
    );
    assert!(
        f.store
            .manager_v2_inspect(
                f.manager,
                &AgentManagerInspectRequestV2 {
                    section: ManagerInspectSectionV2::Archive,
                    epic_id: Some(epic.id),
                    ..Default::default()
                }
            )
            .is_err()
    );
}

#[test]
#[allow(clippy::unwrap_used)]
fn archive_restorable_fence_admits_restore_container() {
    use super::super::manager_actions::ManagerActionOriginV2;
    let f = fixture();
    let mut group = f.store.get_session(f.manager).unwrap().unwrap();
    group.id = Uuid::new_v4();
    group.session_kind = SessionKind::Group;
    f.store.insert_session(&group).unwrap();
    f.store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: vec![group.id],
            project_id: f.project,
            session_id: f.manager,
            epic_ids: None,
            expected_row_version: 1,
        })
        .unwrap();
    grant_topology(&f);
    f.store
        .update_session_status(group.id, SessionStatus::Archived)
        .unwrap();
    let rows = inspect(&f, ManagerInspectSectionV2::Archive).rows;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row["id"], json!(group.id));
    assert_eq!(row["restorable"], true);
    assert_eq!(row["restore_blocker"], Value::Null);
    let stored = f.store.get_session(group.id).unwrap().unwrap();
    assert_eq!(row["expected_updated_at"], json!(stored.updated_at));
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let policy = f
        .store
        .get_harness_manager_policy(f.project)
        .unwrap()
        .unwrap();
    let receipt = f
        .store
        .enqueue_manager_action(
            ManagerActionOriginV2::Agent { caller: f.manager },
            AgentManagerControlRequestV2 {
                fence: ManagerFenceV2 {
                    scope_version: config.row_version,
                    policy_version: policy.row_version,
                },
                idempotency_key: "restore-from-archive".into(),
                operation: ManagerActionV2::RestoreContainer {
                    container_id: group.id,
                    expected_updated_at: serde_json::from_value(row["expected_updated_at"].clone())
                        .unwrap(),
                },
            },
        )
        .unwrap();
    assert_eq!(receipt.state, ManagerActionStateV2::Queued);
}

#[test]
fn overview_returns_actionable_preflight_for_an_appointed_non_root_manager() {
    let f = fixture();
    let parent = f
        .store
        .get_session(f.epic)
        .unwrap()
        .unwrap()
        .parent_id
        .unwrap();
    f.store
        .conn
        .execute(
            "UPDATE sessions SET parent_id=?2 WHERE id=?1",
            params![f.manager.to_string(), parent.to_string()],
        )
        .unwrap();

    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    assert_eq!(config.current_session_id, Some(f.manager));
    let expected = json!({
        "eligibility":"ineligible",
        "logical_manager_session_id":f.manager,
        "current_session_id":f.manager,
        "denial":{
            "reason":"current_manager_not_parentless_standard",
            "required_action":"appoint_parentless_standard_manager"
        }
    });
    assert_eq!(
        serde_json::to_value(f.store.manager_succession_preflight(&config).unwrap()).unwrap(),
        expected
    );

    for operator in [false, true] {
        let page = if operator {
            f.store
                .manager_v2_inspect_operator(f.project, &AgentManagerInspectRequestV2::default())
        } else {
            f.store
                .manager_v2_inspect(f.manager, &AgentManagerInspectRequestV2::default())
        }
        .unwrap();
        let control = page
            .rows
            .iter()
            .find(|row| row["type"] == "manager_control")
            .unwrap();
        assert_eq!(control["eligibility"], "ineligible");
        assert_eq!(control["logical_manager_session_id"], json!(f.manager));
        assert_eq!(control["current_session_id"], json!(f.manager));
        assert_eq!(control["denial"], expected["denial"]);
    }
}

fn work(f: &Fixture, key: &str) -> ManagerMutationReceiptV2 {
    f.store
        .manager_v2_commit_update(
            f.manager,
            &request(
                ManagerUpdateV2::Work {
                    key: key.into(),
                    expected_row_version: 0,
                    epic_id: f.epic,
                    title: format!("{key} feature"),
                    kind: ManagerWorkKindV2::Product,
                    priority: 1,
                    weight: 2,
                    required_gates: vec![
                        ManagerWorkStageV2::Implementation,
                        ManagerWorkStageV2::Review,
                        ManagerWorkStageV2::Verification,
                    ],
                },
                key,
            ),
            &LedgerObservation::default(),
        )
        .unwrap()
}
fn inspect(f: &Fixture, section: ManagerInspectSectionV2) -> ManagerInspectionV2 {
    f.store
        .manager_v2_inspect(
            f.manager,
            &AgentManagerInspectRequestV2 {
                section,
                ..Default::default()
            },
        )
        .unwrap()
}
#[test]
fn reported_pass_and_completed_session_keep_partial_product_visible_without_acceptance() {
    let f = fixture();
    work(&f, "slice");
    for (i, stage) in [
        ManagerWorkStageV2::Implementation,
        ManagerWorkStageV2::Review,
        ManagerWorkStageV2::Verification,
    ]
    .into_iter()
    .enumerate()
    {
        f.store
            .manager_v2_commit_update(
                f.lead,
                &request(
                    ManagerUpdateV2::Stage {
                        key: "slice".into(),
                        expected_row_version: i as i64 + 1,
                        stage,
                        state: ManagerStageStateV2::Passed,
                        note: "reported progress".into(),
                        evidence: None,
                    },
                    &format!("stage{i}"),
                ),
                &LedgerObservation::default(),
            )
            .unwrap();
    }
    let error = f
        .store
        .manager_v2_commit_update(
            f.manager,
            &request(
                ManagerUpdateV2::Accept {
                    key: "slice".into(),
                    expected_row_version: 4,
                },
                "accept",
            ),
            &LedgerObservation::default(),
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("manager_v2_independent_evidence_required")
    );
    let rows = inspect(&f, ManagerInspectSectionV2::Work).rows;
    assert_eq!(rows[0]["title"], "slice feature");
    assert_eq!(rows[0]["source_accepted"], false);
    assert_eq!(rows[0]["integrated"], false);
    assert_eq!(rows[0]["stages"][1]["state"], "passed");
    assert_eq!(
        inspect(&f, ManagerInspectSectionV2::Decisions).rows.len(),
        0
    );
    let overview = inspect(&f, ManagerInspectSectionV2::Overview);
    let product = overview
        .rows
        .iter()
        .find(|r| r["kind"] == "product")
        .unwrap();
    assert_eq!(product["denominator"], 1);
    assert_eq!(product["weight_denominator"], 2);
    assert_eq!(product["accepted"], 0);
}
#[test]
fn dependencies_reject_cycles_and_compute_ready_from_current_records() {
    let f = fixture();
    work(&f, "a");
    work(&f, "b");
    f.store
        .manager_v2_commit_update(
            f.manager,
            &request(
                ManagerUpdateV2::Dependency {
                    key: "b".into(),
                    expected_row_version: 0,
                    prerequisite: "a".into(),
                    require_integrated: true,
                    enabled: true,
                },
                "dep",
            ),
            &LedgerObservation::default(),
        )
        .unwrap();
    let err = f
        .store
        .manager_v2_commit_update(
            f.manager,
            &request(
                ManagerUpdateV2::Dependency {
                    key: "a".into(),
                    expected_row_version: 0,
                    prerequisite: "b".into(),
                    require_integrated: false,
                    enabled: true,
                },
                "cycle",
            ),
            &LedgerObservation::default(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("cycle"));
    let rows = inspect(&f, ManagerInspectSectionV2::Work).rows;
    assert_eq!(
        rows.iter().find(|r| r["key"] == "b").unwrap()["blockers"],
        json!(["a"])
    );
}
#[test]
fn semantic_domains_conflict_while_shared_files_remain_concurrent() {
    let f = fixture();
    work(&f, "a");
    work(&f, "b");
    let claim = |key: &str, domain: &str| ManagerUpdateV2::Ownership {
        key: key.into(),
        expected_row_version: 0,
        domain: domain.into(),
        mode: ManagerOwnershipModeV2::Exclusive,
        files: vec!["shared.rs".into()],
        active: true,
    };
    f.store
        .manager_v2_commit_update(
            f.manager,
            &request(claim("a", "navigation"), "claim1"),
            &LedgerObservation::default(),
        )
        .unwrap();
    f.store
        .manager_v2_commit_update(
            f.manager,
            &request(claim("b", "storage"), "claim2"),
            &LedgerObservation::default(),
        )
        .unwrap();
    assert!(
        f.store
            .manager_v2_commit_update(
                f.manager,
                &request(claim("b", "navigation"), "claim3"),
                &LedgerObservation::default()
            )
            .unwrap_err()
            .to_string()
            .contains("domain_conflict")
    );
}
#[test]
fn live_scope_and_target_are_checked_before_replay() {
    let f = fixture();
    let original = work(&f, "a");
    let req = request(
        ManagerUpdateV2::Stage {
            key: "a".into(),
            expected_row_version: original.row_version,
            stage: ManagerWorkStageV2::Implementation,
            state: ManagerStageStateV2::Passed,
            note: "reported".into(),
            evidence: None,
        },
        "retry",
    );
    let first = f
        .store
        .manager_v2_commit_update(f.manager, &req, &LedgerObservation::default())
        .unwrap();
    let replay = f
        .store
        .manager_v2_commit_update(f.manager, &req, &LedgerObservation::default())
        .unwrap();
    assert_eq!(first.event_sequence, replay.event_sequence);
    assert!(replay.deduplicated);
    f.store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: f.project,
            session_id: f.manager,
            epic_ids: Some(vec![]),
            expected_row_version: 1,
        })
        .unwrap();
    assert!(
        f.store
            .manager_v2_commit_update(f.manager, &req, &LedgerObservation::default())
            .is_err()
    );
}
#[test]
fn inbox_retrieval_tracks_only_returned_ids_and_lead_transitions_are_distinct() {
    let f = fixture();
    let first = f
        .store
        .manager_send(
            f.manager,
            &AgentManagerSendRequestV1 {
                epic_id: f.epic,
                message: "First request".into(),
                idempotency_key: "msg1".into(),
            },
        )
        .unwrap();
    let second = f
        .store
        .manager_send(
            f.manager,
            &AgentManagerSendRequestV1 {
                epic_id: f.epic,
                message: "Second request".into(),
                idempotency_key: "msg2".into(),
            },
        )
        .unwrap();
    let page = f
        .store
        .manager_inbox(
            f.lead,
            &AgentManagerInboxRequestV1 {
                after_sequence: 0,
                limit: 1,
                request_id: None,
            },
        )
        .unwrap();
    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.messages[0].message_id, first.message_id);
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    assert!(
        f.store
            .manager_v2_record(
                &config,
                "retrieval",
                &format!("{}:{}", first.message_id, f.lead)
            )
            .unwrap()
            .is_some()
    );
    assert!(
        f.store
            .manager_v2_record(
                &config,
                "retrieval",
                &format!("{}:{}", second.message_id, f.lead)
            )
            .unwrap()
            .is_none()
    );
    let accept = request(
        ManagerUpdateV2::Request {
            request_id: first.message_id,
            expected_row_version: 0,
            state: ManagerRequestStateV2::Accepted,
            message: "Accepted".into(),
            work_key: None,
        },
        "accepted",
    );
    assert!(
        f.store
            .manager_v2_commit_update(f.manager, &accept, &LedgerObservation::default())
            .is_err()
    );
    f.store
        .manager_v2_commit_update(f.lead, &accept, &LedgerObservation::default())
        .unwrap();
    let rows = inspect(&f, ManagerInspectSectionV2::Requests).rows;
    let row = rows
        .iter()
        .find(|r| r["request_id"] == first.message_id.to_string())
        .unwrap();
    assert_eq!(row["state"], "accepted");
    assert_eq!(row["replied"], false);
    let transition = |state, version| ManagerUpdateV2::Request {
        request_id: first.message_id,
        expected_row_version: version,
        state,
        message: "Explicit lead status".into(),
        work_key: None,
    };
    f.store
        .manager_v2_commit_update(
            f.lead,
            &request(
                transition(ManagerRequestStateV2::Running, 1),
                "request-running",
            ),
            &LedgerObservation::default(),
        )
        .unwrap();
    let completion = request(
        transition(ManagerRequestStateV2::Completed, 2),
        "request-complete",
    );
    assert!(
        f.store
            .manager_v2_commit_update(f.lead, &completion, &LedgerObservation::default())
            .is_err()
    );
    f.store
        .manager_reply(
            f.lead,
            &AgentManagerReplyRequestV1 {
                request_id: first.message_id,
                message: "Status report delivered; implementation still pending".into(),
                idempotency_key: "status-reply".into(),
            },
        )
        .unwrap();
    assert_eq!(
        inspect(&f, ManagerInspectSectionV2::Requests)
            .rows
            .iter()
            .find(|r| r["request_id"] == first.message_id.to_string())
            .unwrap()["state"],
        "running"
    );
    f.store
        .manager_v2_commit_update(f.lead, &completion, &LedgerObservation::default())
        .unwrap();
    let rows = inspect(&f, ManagerInspectSectionV2::Requests).rows;
    let row = rows
        .iter()
        .find(|r| r["request_id"] == first.message_id.to_string())
        .unwrap();
    assert_eq!(row["state"], "completed");
    assert_eq!(
        row["execution"]["execution_evidence"]["kind"],
        "attributed_reply"
    );
    assert_eq!(row["reply_retrieved"], false);
    f.store
        .manager_inbox(f.manager, &AgentManagerInboxRequestV1::default())
        .unwrap();
    assert_eq!(
        inspect(&f, ManagerInspectSectionV2::Requests)
            .rows
            .iter()
            .find(|r| r["request_id"] == first.message_id.to_string())
            .unwrap()["reply_retrieved"],
        true
    );
}

#[test]
fn settled_bookkeeping_history_does_not_exhaust_coordination_or_new_receipts() {
    let f = fixture();
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let stamp = now();
    let tx = f.store.conn.unchecked_transaction().unwrap();
    for n in 0..=MANAGER_V2_MAX_RECORDS {
        for kind in ["retrieval", "resource_spend", "lifecycle_context"] {
            tx.execute(
                "INSERT INTO harness_manager_v2_records(project_id,manager_session_id,scope_version,kind,record_key,row_version,payload_json,archived,created_at,updated_at)
                 VALUES(?1,?2,?3,?4,?5,1,'{}',1,?6,?6)",
                params![config.project_id.to_string(),config.manager_session_id.to_string(),
                    config.row_version,kind,format!("settled:{kind}:{n}"),stamp],
            )
            .unwrap();
        }
    }
    tx.commit().unwrap();
    let sent = f
        .store
        .manager_send(
            f.manager,
            &AgentManagerSendRequestV1 {
                epic_id: f.epic,
                message: "Budget receipt".into(),
                idempotency_key: "budget-receipt".into(),
            },
        )
        .unwrap();
    let request = AgentManagerInboxRequestV1 {
        after_sequence: 0,
        limit: 1,
        request_id: Some(sent.message_id),
    };
    let first = f.store.manager_inbox(f.lead, &request).unwrap();
    assert_eq!(first.messages[0].message_id, sent.message_id);
    let key = format!("{}:{}", sent.message_id, f.lead);
    let receipt = f
        .store
        .manager_v2_record(&config, "retrieval", &key)
        .unwrap()
        .unwrap();
    assert!(receipt.archived);
    let event_count: i64 = f.store.conn.query_row(
        "SELECT count(*) FROM harness_manager_v2_events WHERE project_id=?1 AND kind='retrieval' AND record_key=?2",
        params![f.project.to_string(),key], |r| r.get(0),
    ).unwrap();
    f.store.manager_inbox(f.lead, &request).unwrap();
    let replay_count: i64 = f.store.conn.query_row(
        "SELECT count(*) FROM harness_manager_v2_events WHERE project_id=?1 AND kind='retrieval' AND record_key=?2",
        params![f.project.to_string(),key], |r| r.get(0),
    ).unwrap();
    assert_eq!(event_count, 1);
    assert_eq!(replay_count, event_count);
    f.store
        .manager_v2_put_record(&config, "work", "budget-work", Some(f.epic), 0, &json!({}))
        .unwrap();
    f.store
        .manager_v2_put_record(
            &config,
            "resource_launch_origin",
            &Uuid::new_v4().to_string(),
            Some(f.epic),
            0,
            &json!({}),
        )
        .unwrap();
}

#[test]
fn live_bookkeeping_classes_refuse_their_own_typed_limits() {
    for (kind, code) in [
        ("retrieval", "manager_v2_retrieval_limit"),
        ("resource_launch_origin", "manager_v2_resource_record_limit"),
        ("lifecycle_context", "manager_v2_lifecycle_limit"),
    ] {
        let f = fixture();
        let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
        let stamp = now();
        let tx = f.store.conn.unchecked_transaction().unwrap();
        for n in 0..=MANAGER_V2_MAX_RECORDS {
            tx.execute(
                "INSERT INTO harness_manager_v2_records(project_id,manager_session_id,scope_version,kind,record_key,row_version,payload_json,created_at,updated_at)
                 VALUES(?1,?2,?3,?4,?5,1,'{}',?6,?6)",
                params![config.project_id.to_string(),config.manager_session_id.to_string(),
                    config.row_version,kind,format!("live:{n}"),stamp],
            ).unwrap();
        }
        tx.commit().unwrap();
        let error = f
            .store
            .manager_v2_put_record(&config, kind, "overflow", None, 0, &json!({}))
            .unwrap_err();
        assert!(error.to_string().contains(code), "{error}");
        let read_error = f
            .store
            .manager_v2_records_of_kind(&config, kind)
            .unwrap_err();
        assert!(read_error.to_string().contains(code), "{read_error}");
    }
}
#[test]
fn descendant_pages_preserve_parent_and_worker_names() {
    let f = fixture();
    for name in ["Planner", "Implementer", "Reviewer"] {
        let mut child = test_session(Uuid::new_v4(), PathBuf::from("/var/tmp/ham-v2-fd23a414"));
        child.parent_id = Some(f.epic);
        child.session_kind = SessionKind::Task;
        child.project_id = Some(f.project);
        child.title = Some(name.into());
        f.store.insert_session(&child).unwrap();
    }
    let mut query = AgentManagerInspectRequestV2 {
        section: ManagerInspectSectionV2::Workers,
        limit: 1,
        ..Default::default()
    };
    let mut names = Vec::new();
    loop {
        let page = f.store.manager_v2_inspect(f.manager, &query).unwrap();
        names.extend(
            page.rows
                .iter()
                .filter_map(|r| r["title"].as_str().map(str::to_owned)),
        );
        if page.next_cursor.is_none() {
            assert!(page.complete);
            break;
        }
        query.cursor = page.next_cursor;
    }
    for name in ["Planner", "Implementer", "Reviewer"] {
        assert!(names.iter().any(|n| n == name));
    }
    assert_eq!(names.len(), 4);
}
#[test]
fn operator_retains_bounded_state_when_manager_is_unavailable() {
    let f = fixture();
    work(&f, "retained");
    f.store
        .conn
        .execute(
            "UPDATE sessions SET status='Deleted' WHERE id=?1",
            [f.manager.to_string()],
        )
        .unwrap();
    let result = f
        .store
        .manager_v2_inspect_operator(
            f.project,
            &AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Work,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(result.rows[0]["title"], "retained feature");
    assert!(
        f.store
            .manager_v2_inspect(f.manager, &AgentManagerInspectRequestV2::default())
            .is_err()
    );
}

#[test]
fn migration_reservations_refuse_released_versions_and_duplicate_claims() {
    let f = fixture();
    let released = crate::store::LATEST_SCHEMA_VERSION as u32;
    let available = released + 1;
    work(&f, "a");
    work(&f, "b");
    let baseline = "a".repeat(40);
    let digest = format!("sha256:{}", "b".repeat(64));
    let observed = LedgerObservation {
        migration: Some((baseline.clone(), digest.clone())),
        ..Default::default()
    };
    let change = |key: &str, version| ManagerUpdateV2::Migration {
        key: key.into(),
        expected_row_version: 0,
        version,
        baseline_commit: baseline.clone(),
        inventory_digest: digest.clone(),
    };
    assert!(
        f.store
            .manager_v2_commit_update(
                f.manager,
                &request(change("a", released), "released"),
                &observed
            )
            .unwrap_err()
            .to_string()
            .contains("migration_baseline_or_released")
    );
    f.store
        .manager_v2_commit_update(
            f.manager,
            &request(change("a", available), "reserve"),
            &observed,
        )
        .unwrap();
    assert!(
        f.store
            .manager_v2_commit_update(
                f.manager,
                &request(change("b", available), "duplicate"),
                &observed
            )
            .unwrap_err()
            .to_string()
            .contains("migration_conflict")
    );
    let rows = inspect(&f, ManagerInspectSectionV2::Work).rows;
    let a = rows.iter().find(|r| r["key"] == "a").unwrap();
    assert_eq!(a["released_schema_head"], released);
    assert_eq!(a["migration_reservations"][0]["version"], available);
    assert_eq!(a["migration_reservations"][0]["row_version"], 1);
}

#[test]
fn migration_reservation_transfer_and_release_are_fenced() {
    let f = fixture();
    let version = crate::store::LATEST_SCHEMA_VERSION as u32 + 1;
    work(&f, "a");
    work(&f, "b");
    let baseline = "a".repeat(40);
    let digest = format!("sha256:{}", "b".repeat(64));
    let observed = LedgerObservation {
        migration: Some((baseline.clone(), digest.clone())),
        ..Default::default()
    };
    let reserve = |key: &str, expected_row_version| ManagerUpdateV2::Migration {
        key: key.into(),
        expected_row_version,
        version,
        baseline_commit: baseline.clone(),
        inventory_digest: digest.clone(),
    };
    f.store
        .manager_v2_commit_update(f.manager, &request(reserve("a", 0), "reserve-a"), &observed)
        .unwrap();
    let transfer = |expected_row_version| ManagerUpdateV2::MigrationTransfer {
        key: "b".into(),
        expected_row_version,
        version,
    };
    assert!(
        f.store
            .manager_v2_commit_update(
                f.manager,
                &request(transfer(0), "stale-transfer"),
                &LedgerObservation::default()
            )
            .unwrap_err()
            .to_string()
            .contains("record_changed")
    );
    f.store
        .manager_v2_commit_update(
            f.manager,
            &request(transfer(1), "transfer"),
            &LedgerObservation::default(),
        )
        .unwrap();
    let rows = inspect(&f, ManagerInspectSectionV2::Work).rows;
    let a = rows.iter().find(|r| r["key"] == "a").unwrap();
    let b = rows.iter().find(|r| r["key"] == "b").unwrap();
    assert!(a["migration_reservations"].as_array().unwrap().is_empty());
    assert_eq!(b["migration_reservations"][0]["row_version"], 2);
    assert_eq!(b["migration_reservations"][0]["active"], true);
    assert_eq!(b["migration_reservations"][0]["consumed"], false);
    f.store
        .manager_v2_commit_update(
            f.manager,
            &request(
                ManagerUpdateV2::MigrationRelease {
                    key: "b".into(),
                    expected_row_version: 2,
                    version,
                },
                "release",
            ),
            &LedgerObservation::default(),
        )
        .unwrap();
    let rows = inspect(&f, ManagerInspectSectionV2::Work).rows;
    let b = rows.iter().find(|r| r["key"] == "b").unwrap();
    assert_eq!(b["migration_reservations"][0]["active"], false);
    f.store
        .manager_v2_commit_update(
            f.manager,
            &request(reserve("a", 3), "reserve-again"),
            &observed,
        )
        .unwrap();
    assert!(
        f.store
            .manager_v2_commit_update(
                f.manager,
                &request(reserve("b", 4), "live-conflict"),
                &observed
            )
            .unwrap_err()
            .to_string()
            .contains("migration_conflict")
    );
}

#[test]
fn consumed_migration_reservation_remains_visible_and_cannot_move() {
    let f = fixture();
    work(&f, "a");
    work(&f, "b");
    let version = crate::store::LATEST_SCHEMA_VERSION as u32;
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    f.store
        .manager_v2_put_record(
            &config,
            "migration",
            &version.to_string(),
            Some(f.epic),
            0,
            &serde_json::json!({"work_key":"a","version":version,
                "baseline_commit":"a".repeat(40),"inventory_digest":"digest"}),
        )
        .unwrap();
    let rows = inspect(&f, ManagerInspectSectionV2::Work).rows;
    let a = rows.iter().find(|r| r["key"] == "a").unwrap();
    assert_eq!(a["migration_reservations"][0]["consumed"], true);
    assert!(
        f.store
            .manager_v2_commit_update(
                f.manager,
                &request(
                    ManagerUpdateV2::MigrationTransfer {
                        key: "b".into(),
                        expected_row_version: 1,
                        version
                    },
                    "consumed-transfer"
                ),
                &LedgerObservation::default()
            )
            .unwrap_err()
            .to_string()
            .contains("migration_baseline_or_released")
    );
}

#[test]
fn a_plan_revision_preserves_partial_source_and_reopens_acceptance() {
    let f = fixture();
    work(&f, "partial");
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let (r, mut w) = f.store.manager_v2_work(&config, "partial").unwrap();
    w.source_commit = Some("a".repeat(40));
    w.source_session_id = Some(f.lead);
    w.stages[1].state = ManagerStageStateV2::Passed;
    w.acceptance = Some(Acceptance {
        source_commit: "a".repeat(40),
        spec_revision: 1,
        evidence_digest: "receipt".into(),
        method: "operator_exact_gate".into(),
        accepted_at: now(),
    });
    f.store
        .manager_v2_put_record(
            &config,
            "work",
            "partial",
            Some(f.epic),
            r.row_version,
            &json!(w),
        )
        .unwrap();
    f.store
        .manager_v2_commit_update(
            f.manager,
            &request(
                ManagerUpdateV2::Work {
                    key: "partial".into(),
                    expected_row_version: 2,
                    epic_id: f.epic,
                    title: "Revised feature".into(),
                    kind: ManagerWorkKindV2::Product,
                    priority: 2,
                    weight: 3,
                    required_gates: w.required_gates,
                },
                "revise",
            ),
            &LedgerObservation::default(),
        )
        .unwrap();
    let row = &inspect(&f, ManagerInspectSectionV2::Work).rows[0];
    assert_eq!(row["title"], "Revised feature");
    assert_eq!(row["source_commit"], "a".repeat(40));
    assert_eq!(row["stages"][1]["state"], "partial");
    assert_eq!(row["source_accepted"], false);
    assert_eq!(row["spec_revision"], 2);
    assert_eq!(row["evidence_state"], "unknown");
}

#[test]
fn agent_decisions_cannot_replace_exact_operator_acceptance_gates() {
    let f = fixture();
    work(&f, "a");
    for prefix in ["accept:", "question:", "approval:"] {
        let change = ManagerUpdateV2::Decision {
            key: format!("{prefix}a"),
            expected_row_version: 0,
            epic_id: f.epic,
            question: "Replace acceptance?".into(),
            request_id: None,
            work_key: Some("a".into()),
        };
        assert!(
            f.store
                .manager_v2_commit_update(
                    f.lead,
                    &request(change, "invalid-gate"),
                    &LedgerObservation::default()
                )
                .unwrap_err()
                .to_string()
                .contains("reserved_decision_key")
        );
    }
    f.store
        .manager_v2_commit_update(
            f.lead,
            &request(
                ManagerUpdateV2::Decision {
                    key: "design".into(),
                    expected_row_version: 0,
                    epic_id: f.epic,
                    question: "Keep both visible identities?".into(),
                    request_id: None,
                    work_key: Some("a".into()),
                },
                "question",
            ),
            &LedgerObservation::default(),
        )
        .unwrap();
    let row = &inspect(&f, ManagerInspectSectionV2::Decisions).rows[0];
    assert_eq!(row["question"], "Keep both visible identities?");
    assert_eq!(row["status"], "pending");
    assert!(
        row["target_digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
}

#[test]
fn descendant_live_cursor_tolerates_noise_and_refreshes_later_insertions() {
    let f = fixture();
    let reserved = Uuid::new_v4();
    let spawn = rsi_common::agent_coordination::AgentSpawnChildRequestV1 {
        kind: SessionKind::Task,
        agent_role: None,
        provider: None,
        model: None,
        effort: None,
        query: "Pending implementation worker".into(),
        topology_node: None,
        iteration: None,
        tags: None,
        idempotency_key: "pending".into(),
    };
    f.store
        .reserve_agent_spawn_request(
            f.lead,
            &format!("sha256:{}", "a".repeat(64)),
            &format!("sha256:{}", "b".repeat(64)),
            &spawn,
            f.epic,
            Uuid::new_v4(),
            reserved,
        )
        .unwrap();
    let query = AgentManagerInspectRequestV2 {
        section: ManagerInspectSectionV2::Workers,
        limit: 1,
        ..Default::default()
    };
    let page = f.store.manager_v2_inspect(f.manager, &query).unwrap();
    assert!(page.next_cursor.is_some());
    f.store
        .update_session_status(f.lead, SessionStatus::Interrupted)
        .unwrap();
    work(&f, "ledger-noise");
    // Event/status noise does not prevent paging the same structural cohort.
    f.store
        .manager_v2_inspect(
            f.manager,
            &AgentManagerInspectRequestV2 {
                cursor: page.next_cursor.clone(),
                ..query.clone()
            },
        )
        .unwrap();
    let mut child = test_session(Uuid::new_v4(), PathBuf::from("/var/tmp/ham-v2-fd23a414"));
    child.project_id = Some(f.project);
    child.parent_id = Some(f.epic);
    child.session_kind = SessionKind::Task;
    child.title = Some("New structural member".into());
    f.store.insert_session(&child).unwrap();
    let continued = AgentManagerInspectRequestV2 {
        cursor: page.next_cursor,
        ..query
    };
    let continued = f.store.manager_v2_inspect(f.manager, &continued).unwrap();
    assert_eq!(continued.rows[0]["session_id"], reserved.to_string());
    assert!(continued.complete);
    let rows = inspect(&f, ManagerInspectSectionV2::Workers).rows;
    let pending = rows
        .iter()
        .find(|r| r["session_id"] == reserved.to_string())
        .unwrap();
    assert_eq!(pending["title"], "Pending implementation worker");
    assert_eq!(pending["spawn_state"], "reserved");
    assert_eq!(
        rows.iter()
            .find(|r| r["session_id"] == child.id.to_string())
            .unwrap()["title"],
        "New structural member"
    );
}

#[test]
fn a_legal_epic_over_1024_workers_returns_every_identity_and_operator_topology() {
    let f = fixture();
    let lead = f.store.get_session(f.lead).unwrap().unwrap();
    let mut expected =
        std::collections::BTreeMap::from([(f.lead.to_string(), lead.title.unwrap_or(lead.query))]);
    let tx = Transaction::new_unchecked(&f.store.conn, TransactionBehavior::Immediate).unwrap();
    for index in 0..GRAPH_BUDGET + 1 {
        let mut child = test_session(Uuid::new_v4(), PathBuf::from("/var/tmp/ham-v2-fd23a414"));
        child.project_id = Some(f.project);
        child.parent_id = Some(f.epic);
        child.session_kind = SessionKind::Task;
        child.title = Some(format!("Worker {index}"));
        expected.insert(child.id.to_string(), child.title.clone().unwrap());
        f.store.insert_session(&child).unwrap();
    }
    tx.commit().unwrap();
    for section in [
        ManagerInspectSectionV2::Workers,
        ManagerInspectSectionV2::Topology,
    ] {
        for epic in [Some(f.epic), None] {
            let rows = collect_legal_pages(&f, section, epic);
            let mut seen = std::collections::BTreeMap::new();
            for row in &rows {
                let id = row["key"].as_str().unwrap();
                if expected.contains_key(id) {
                    assert!(
                        seen.insert(id.to_owned(), row["title"].as_str().unwrap().to_owned())
                            .is_none()
                    );
                }
            }
            assert_eq!(seen, expected);
            if section == ManagerInspectSectionV2::Topology {
                let root = rows.iter().find(|r| r["id"] == f.epic.to_string()).unwrap();
                assert_eq!(
                    root["expected"],
                    json!(f.store.manager_action_lead_fence(f.epic).unwrap())
                );
                assert_eq!(root["expected_updated_at"], root["updated_at"]);
                let group = f
                    .store
                    .get_session(f.epic)
                    .unwrap()
                    .unwrap()
                    .parent_id
                    .unwrap();
                assert_eq!(
                    rows.iter().find(|r| r["id"] == group.to_string()).unwrap()["kind"],
                    "Group"
                );
            }
        }
    }
}

fn collect_legal_pages(
    f: &Fixture,
    section: ManagerInspectSectionV2,
    epic: Option<Uuid>,
) -> Vec<Value> {
    let mut query = AgentManagerInspectRequestV2 {
        section,
        epic_id: epic,
        limit: 64,
        ..Default::default()
    };
    let mut rows = Vec::new();
    for _ in 0..100 {
        let page = if section == ManagerInspectSectionV2::Topology {
            f.store
                .manager_v2_inspect_operator(f.project, &query)
                .unwrap()
        } else {
            f.store.manager_v2_inspect(f.manager, &query).unwrap()
        };
        assert!(page.rows.len() <= 64);
        rows.extend(page.rows);
        let Some(next) = page.next_cursor else {
            assert!(page.complete);
            return rows;
        };
        assert!(next.len() <= 512);
        assert!(!page.complete);
        query.cursor = Some(next);
    }
    panic!("paging failed to terminate");
}

fn add_worker(f: &Fixture, epic: Uuid, title: &str) -> Uuid {
    let mut s = f.store.get_session(f.lead).unwrap().unwrap();
    s.id = Uuid::new_v4();
    s.parent_id = Some(epic);
    s.session_kind = SessionKind::Task;
    s.title = Some(title.into());
    f.store.insert_session(&s).unwrap();
    s.id
}

#[test]
fn live_keyset_reparenting_never_repeats_rows_and_logical_removal_retains_identity() {
    let f = fixture();
    let mut second = f.store.get_session(f.epic).unwrap().unwrap();
    second.id = Uuid::new_v4();
    second.lead_session_id = None;
    f.store.insert_session(&second).unwrap();
    let mut outside = second.clone();
    outside.id = Uuid::new_v4();
    f.store.insert_session(&outside).unwrap();
    f.store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: f.project,
            session_id: f.manager,
            epic_ids: Some(vec![f.epic, second.id]),
            expected_row_version: 1,
        })
        .unwrap();
    let moved = add_worker(&f, f.epic, "Moved within selected Epics");
    let removed = add_worker(&f, f.epic, "Retained deleted worker");
    let outgoing = add_worker(&f, f.epic, "Moved to outside Epic");
    let incoming = add_worker(&f, outside.id, "Moved into selected Epic");
    let mut query = AgentManagerInspectRequestV2 {
        section: ManagerInspectSectionV2::Workers,
        limit: 1,
        ..Default::default()
    };
    let first = f.store.manager_v2_inspect(f.manager, &query).unwrap();
    assert_eq!(first.rows[0]["session_id"], f.lead.to_string());
    query.cursor = first.next_cursor;
    for (id, parent) in [
        (f.lead, second.id),
        (moved, second.id),
        (outgoing, outside.id),
        (incoming, second.id),
    ] {
        f.store
            .conn
            .execute(
                "UPDATE sessions SET parent_id=?2 WHERE id=?1",
                params![id.to_string(), parent.to_string()],
            )
            .unwrap();
    }
    f.store
        .update_session_status(removed, SessionStatus::Deleted)
        .unwrap();
    let late = add_worker(&f, second.id, "Later insertion requires refresh");
    let mut seen = std::collections::BTreeSet::from([f.lead.to_string()]);
    loop {
        let page = f.store.manager_v2_inspect(f.manager, &query).unwrap();
        for row in &page.rows {
            assert!(seen.insert(row["session_id"].as_str().unwrap().to_owned()));
            if row["session_id"] == removed.to_string() {
                assert_eq!(row["title"], "Retained deleted worker");
                assert_eq!(row["status"], "Deleted");
            }
            if row["session_id"] == moved.to_string() || row["session_id"] == incoming.to_string() {
                assert_eq!(row["epic_id"], second.id.to_string());
            }
        }
        let Some(next) = page.next_cursor else {
            assert!(page.complete);
            break;
        };
        query.cursor = Some(next);
    }
    assert_eq!(
        seen,
        [f.lead, moved, removed, incoming]
            .map(|id| id.to_string())
            .into_iter()
            .collect()
    );
    let refreshed = collect_legal_pages(&f, ManagerInspectSectionV2::Workers, None);
    assert_eq!(
        refreshed
            .iter()
            .find(|r| r["session_id"] == late.to_string())
            .unwrap()["title"],
        "Later insertion requires refresh"
    );
}

#[test]
fn leaf_cursor_rechecks_scope_anchor_policy_and_container_structure() {
    for change in ["scope", "anchor", "section", "epic", "policy", "container"] {
        let f = fixture();
        add_worker(&f, f.epic, "Another worker for pagination");
        let query = AgentManagerInspectRequestV2 {
            section: ManagerInspectSectionV2::Workers,
            limit: 1,
            ..Default::default()
        };
        let first = f.store.manager_v2_inspect(f.manager, &query).unwrap();
        let mut cursor: Value = serde_json::from_str(first.next_cursor.as_ref().unwrap()).unwrap();
        match change {
            "scope" => {
                // A real scope edit (selected -> project mode, still covering
                // f.epic): identical re-saves are no-ops since #450.
                f.store
                    .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                        group_ids: Vec::new(),
                        project_id: f.project,
                        session_id: f.manager,
                        epic_ids: None,
                        expected_row_version: 1,
                    })
                    .unwrap();
            }
            "anchor" => cursor["anchor"] = json!(Uuid::new_v4()),
            "section" => cursor["section"] = json!(ManagerInspectSectionV2::Topology),
            "epic" => cursor["epic"] = json!(Uuid::new_v4()),
            "policy" => {
                f.store
                    .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                        project_id: f.project,
                        expected_scope_version: 1,
                        expected_policy_version: 1,
                        idempotency_key: "edited-policy".into(),
                        policy: ManagerPolicyV2::default(),
                    })
                    .unwrap();
            }
            "container" => {
                let parent = f
                    .store
                    .get_session(f.epic)
                    .unwrap()
                    .unwrap()
                    .parent_id
                    .unwrap();
                let mut group = f.store.get_session(parent).unwrap().unwrap();
                group.id = Uuid::new_v4();
                f.store.insert_session(&group).unwrap();
                f.store
                    .conn
                    .execute(
                        "UPDATE sessions SET parent_id=?2 WHERE id=?1",
                        params![f.epic.to_string(), group.id.to_string()],
                    )
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            f.store
                .manager_v2_inspect(
                    f.manager,
                    &AgentManagerInspectRequestV2 {
                        cursor: Some(serde_json::to_string(&cursor).unwrap()),
                        ..query
                    }
                )
                .unwrap_err()
                .to_string()
                .contains("cursor_changed"),
            "{change}"
        );
    }
}

#[test]
fn pending_reservations_page_past_1024_and_survive_rotation_and_publication() {
    let f = fixture();
    let mut expected = std::collections::BTreeSet::from([f.lead.to_string()]);
    let mut last = Uuid::nil();
    for index in 0..GRAPH_BUDGET + 1 {
        let id = Uuid::new_v4();
        expected.insert(id.to_string());
        last = id;
        f.store
            .reserve_agent_spawn_request(
                f.lead,
                &format!("sha256:{index:064x}"),
                &format!("sha256:{index:064x}"),
                &rsi_common::agent_coordination::AgentSpawnChildRequestV1 {
                    kind: SessionKind::Task,
                    agent_role: None,
                    provider: None,
                    model: None,
                    effort: None,
                    query: format!("Reserved identity {index}"),
                    topology_node: None,
                    iteration: None,
                    tags: None,
                    idempotency_key: format!("reserved-{index}"),
                },
                f.epic,
                Uuid::new_v4(),
                id,
            )
            .unwrap();
    }
    let mut query = AgentManagerInspectRequestV2 {
        section: ManagerInspectSectionV2::Workers,
        limit: 64,
        ..Default::default()
    };
    let first = f.store.manager_v2_inspect(f.manager, &query).unwrap();
    query.cursor = first.next_cursor;
    let mut successor = f.store.get_session(f.lead).unwrap().unwrap();
    successor.id = Uuid::new_v4();
    successor.continued_from = Some(f.lead);
    successor.rotation_depth += 1;
    successor.title = Some("Current rotated lead".into());
    f.store.insert_session(&successor).unwrap();
    f.store
        .update_session_status(f.lead, SessionStatus::Archived)
        .unwrap();
    f.store
        .record_harness_manager_rotation(f.lead, successor.id)
        .unwrap();
    f.store
        .set_lead_session(f.epic, Some(successor.id))
        .unwrap();
    let mut published = successor.clone();
    published.id = last;
    published.continued_from = None;
    published.session_kind = SessionKind::Task;
    published.title = Some("Published reserved identity".into());
    f.store.insert_session(&published).unwrap();
    let mut seen: std::collections::BTreeSet<_> = first
        .rows
        .iter()
        .map(|r| r["session_id"].as_str().unwrap().to_owned())
        .collect();
    let mut finished = false;
    for _ in 0..40 {
        let page = f.store.manager_v2_inspect(f.manager, &query).unwrap();
        assert!(page.rows.len() <= 64);
        for row in &page.rows {
            assert!(seen.insert(row["session_id"].as_str().unwrap().to_owned()));
            if row["session_id"] == last.to_string() {
                assert_eq!(row["title"], "Published reserved identity");
                assert_eq!(row["status"], "Completed");
            }
        }
        let Some(next) = page.next_cursor else {
            assert!(page.complete);
            finished = true;
            break;
        };
        query.cursor = Some(next);
    }
    assert!(finished);
    assert_eq!(seen, expected);
    let refreshed = collect_legal_pages(&f, ManagerInspectSectionV2::Workers, None);
    let rotated = refreshed
        .iter()
        .find(|r| r["session_id"] == successor.id.to_string())
        .unwrap();
    assert_eq!(rotated["title"], "Current rotated lead");
    assert_eq!(rotated["continued_from"], f.lead.to_string());
}

#[test]
fn sparse_reservation_pages_advance_without_widening_epic_scope() {
    let f = fixture();
    let mut outside = f.store.get_session(f.epic).unwrap().unwrap();
    outside.id = Uuid::new_v4();
    outside.lead_session_id = None;
    f.store.insert_session(&outside).unwrap();
    let owner = add_worker(&f, outside.id, "Outside lead");
    f.store.set_lead_session(outside.id, Some(owner)).unwrap();
    let scoped = Uuid::new_v4();
    for index in 0..261 {
        let local = index == 260;
        f.store
            .reserve_agent_spawn_request(
                if local { f.lead } else { owner },
                &format!("sha256:{index:064x}"),
                &format!("sha256:{index:064x}"),
                &rsi_common::agent_coordination::AgentSpawnChildRequestV1 {
                    kind: SessionKind::Task,
                    agent_role: None,
                    provider: None,
                    model: None,
                    effort: None,
                    query: format!("Reserved page item {index}"),
                    topology_node: None,
                    iteration: None,
                    tags: None,
                    idempotency_key: format!("reservation-{index}"),
                },
                if local { f.epic } else { outside.id },
                Uuid::new_v4(),
                if local { scoped } else { Uuid::new_v4() },
            )
            .unwrap();
    }
    let mut query = AgentManagerInspectRequestV2 {
        section: ManagerInspectSectionV2::Workers,
        ..Default::default()
    };
    let first = f.store.manager_v2_inspect(f.lead, &query).unwrap();
    assert_eq!(first.rows.len(), 1);
    assert_eq!(first.rows[0]["session_id"], f.lead.to_string());
    query.cursor = first.next_cursor;
    let second = f.store.manager_v2_inspect(f.lead, &query).unwrap();
    assert!(second.rows.is_empty());
    assert!(!second.complete);
    assert!(second.next_cursor.is_some());
    assert_ne!(second.next_cursor, query.cursor);
    query.cursor = second.next_cursor;
    let third = f.store.manager_v2_inspect(f.lead, &query).unwrap();
    assert!(third.complete);
    assert_eq!(third.rows.len(), 1);
    assert_eq!(third.rows[0]["session_id"], scoped.to_string());
    assert_eq!(third.rows[0]["epic_id"], f.epic.to_string());
    assert_eq!(third.rows[0]["title"], "Reserved page item 260");
}

#[test]
fn page_queries_seek_existing_indexes_instead_of_sorting_whole_epics() {
    let f = fixture();
    for (sql, params, index) in [
        (
            "EXPLAIN QUERY PLAN SELECT rowid,id FROM sessions WHERE parent_id=?1 AND rowid>?2 AND rowid<=?3 ORDER BY rowid LIMIT ?4",
            vec![f.epic.to_string(), "0".into(), "999999".into(), "64".into()],
            "idx_sessions_parent_id",
        ),
        (
            "EXPLAIN QUERY PLAN SELECT rowid,session_id FROM harness_manager_v2_entities WHERE project_id=?1 AND manager_session_id=?2 AND scope_version=?3 AND kind=?4 AND rowid>?5 AND rowid<=?6 ORDER BY rowid LIMIT 128",
            vec![
                f.project.to_string(),
                f.manager.to_string(),
                "1".into(),
                "Group".into(),
                "0".into(),
                "999999".into(),
            ],
            "harness_manager_v2_entity_scope",
        ),
    ] {
        let mut statement = f.store.conn.prepare(sql).unwrap();
        let plan = statement
            .query_map(rusqlite::params_from_iter(params), |r| {
                r.get::<_, String>(3)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(plan.len(), 1, "{plan:?}");
        assert!(
            plan[0].contains(index) && plan[0].contains("rowid>?") && plan[0].contains("rowid<?"),
            "{plan:?}"
        );
    }
}

#[test]
fn forged_leaf_cursor_cannot_enter_manager_owned_container_pages() {
    let f = fixture();
    add_worker(&f, f.epic, "Page continuation worker");
    for (section, phase) in [
        (ManagerInspectSectionV2::Workers, 0),
        (ManagerInspectSectionV2::Workers, 5),
        (ManagerInspectSectionV2::Topology, 5),
    ] {
        let query = AgentManagerInspectRequestV2 {
            section,
            limit: 1,
            ..Default::default()
        };
        let page = f.store.manager_v2_inspect(f.lead, &query).unwrap();
        let mut cursor: Value = serde_json::from_str(page.next_cursor.as_ref().unwrap()).unwrap();
        let mut position: Value = serde_json::from_str(cursor["after"].as_str().unwrap()).unwrap();
        position["p"] = json!(phase);
        cursor["after"] = json!(serde_json::to_string(&position).unwrap());
        assert!(
            f.store
                .manager_v2_inspect(
                    f.lead,
                    &AgentManagerInspectRequestV2 {
                        cursor: Some(serde_json::to_string(&cursor).unwrap()),
                        ..query
                    }
                )
                .unwrap_err()
                .to_string()
                .contains("invalid_cursor")
        );
    }
}

#[test]
fn operator_request_audit_survives_unavailable_manager_lineage() {
    let f = fixture();
    let sent = f
        .store
        .manager_send(
            f.manager,
            &AgentManagerSendRequestV1 {
                epic_id: f.epic,
                message: "Retained request audit".into(),
                idempotency_key: "audit".into(),
            },
        )
        .unwrap();
    f.store
        .update_session_status(f.manager, SessionStatus::Deleted)
        .unwrap();
    let result = f
        .store
        .manager_v2_inspect_operator(
            f.project,
            &AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Requests,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(result.rows[0]["request_id"], sent.message_id.to_string());
    assert_eq!(result.rows[0]["message"], "Retained request audit");
    assert_eq!(result.rows[0]["recipient_session_id"], f.lead.to_string());
    assert_eq!(result.rows[0]["delivery_issue"], "lead_changed");
}

#[test]
fn operator_topology_pages_every_owned_container_and_keeps_retired_titles() {
    let f = fixture();
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let mut expected = std::collections::BTreeMap::new();
    for index in 0..325 {
        let mut group = f.store.get_session(f.manager).unwrap().unwrap();
        group.id = Uuid::new_v4();
        group.session_kind = SessionKind::Group;
        group.title = Some(format!("Owned Group {index}"));
        group.status = if index % 2 == 0 {
            SessionStatus::Archived
        } else {
            SessionStatus::Completed
        };
        f.store.insert_session(&group).unwrap();
        let key = format!("creation-{index}");
        // Seed committed creation receipts to test read consumers, not action
        // execution. Both the receipt and its provenance keep real foreign keys.
        f.store
            .manager_v2_save_receipt(
                &config,
                Some(f.manager),
                1,
                "action",
                &key,
                &json!({"created":group.id}),
                &json!({"id":group.id}),
            )
            .unwrap();
        let op: String = f
            .store
            .conn
            .query_row(
                "SELECT id FROM harness_manager_v2_operations WHERE idempotency_key=?1",
                [key],
                |r| r.get(0),
            )
            .unwrap();
        f.store.conn.execute("INSERT INTO harness_manager_v2_entities(session_id,operation_id,project_id,manager_session_id,scope_version,policy_version,kind,created_at) VALUES(?1,?2,?3,?4,1,1,'Group',?5)",params![group.id.to_string(),op,f.project.to_string(),f.manager.to_string(),now()]).unwrap();
        expected.insert(
            group.id.to_string(),
            (group.title.unwrap(), json!(group.status)),
        );
    }
    let rows = collect_legal_pages(&f, ManagerInspectSectionV2::Topology, None);
    let mut seen = std::collections::BTreeMap::new();
    for row in rows {
        let id = row["id"].as_str().unwrap();
        if expected.contains_key(id) {
            assert_eq!(row["kind"], "Group");
            assert_eq!(row["expected_updated_at"], row["updated_at"]);
            assert!(
                seen.insert(
                    id.to_owned(),
                    (
                        row["title"].as_str().unwrap().to_owned(),
                        row["status"].clone()
                    )
                )
                .is_none()
            );
        }
    }
    assert_eq!(seen, expected);
}

#[test]
fn unsupported_nested_hierarchy_retains_parent_identity_and_reports_unknown_coverage() {
    let f = fixture();
    let mut child = f.store.get_session(f.lead).unwrap().unwrap();
    child.id = Uuid::new_v4();
    child.title = Some("Unsupported nested worker".into());
    child.parent_id = Some(f.lead);
    f.store.insert_session(&child).unwrap();
    let page = inspect(&f, ManagerInspectSectionV2::Workers);
    assert!(!page.complete);
    let lead = page
        .rows
        .iter()
        .find(|r| r["session_id"] == f.lead.to_string())
        .unwrap();
    assert_eq!(
        lead["logical_title"],
        f.store
            .get_session(f.lead)
            .unwrap()
            .unwrap()
            .title
            .unwrap_or_else(|| f.store.get_session(f.lead).unwrap().unwrap().query)
    );
    assert_eq!(lead["hierarchy_issue"], "unsupported_hierarchy");
    assert_eq!(
        page.rows.iter().find(|r| r["type"] == "coverage").unwrap()["state"],
        "unknown"
    );
}

#[test]
fn dependency_readiness_and_handoff_are_rehydrated_after_reopen() {
    let dir = tempfile::Builder::new()
        .prefix("reopen-")
        .tempdir_in("/var/tmp/ham-v2-fd23a414")
        .unwrap();
    let path = dir.path().join("store.db");
    let f = fixture_using(Store::open(&path).unwrap());
    work(&f, "a");
    work(&f, "b");
    f.store
        .manager_v2_commit_update(
            f.manager,
            &request(
                ManagerUpdateV2::Dependency {
                    key: "b".into(),
                    expected_row_version: 0,
                    prerequisite: "a".into(),
                    require_integrated: false,
                    enabled: true,
                },
                "dep",
            ),
            &LedgerObservation::default(),
        )
        .unwrap();
    f.store
        .manager_v2_commit_update(
            f.manager,
            &request(
                ManagerUpdateV2::Handoff {
                    summary: "Continue the product after the prerequisite".into(),
                    next_actions: vec!["Review source a".into()],
                },
                "handoff",
            ),
            &LedgerObservation::default(),
        )
        .unwrap();
    let manager = f.manager;
    drop(f);
    let store = Store::open(&path).unwrap();
    let rows = store
        .manager_v2_inspect(
            manager,
            &AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Work,
                ..Default::default()
            },
        )
        .unwrap()
        .rows;
    let b = rows.iter().find(|r| r["key"] == "b").unwrap();
    assert_eq!(b["ready"], false);
    assert_eq!(b["blockers"], json!(["a"]));
    let overview = store
        .manager_v2_inspect(manager, &AgentManagerInspectRequestV2::default())
        .unwrap();
    assert_eq!(
        overview
            .rows
            .iter()
            .find(|r| r["type"] == "handoff")
            .unwrap()["summary"],
        "Continue the product after the prerequisite"
    );
}

#[test]
fn request_cursor_detects_new_mail_and_lead_action_pages_include_its_work_updates() {
    let f = fixture();
    for index in 0..2 {
        f.store
            .manager_send(
                f.manager,
                &AgentManagerSendRequestV1 {
                    epic_id: f.epic,
                    message: format!("Request {index}"),
                    idempotency_key: format!("mail{index}"),
                },
            )
            .unwrap();
    }
    let query = AgentManagerInspectRequestV2 {
        section: ManagerInspectSectionV2::Requests,
        limit: 1,
        ..Default::default()
    };
    let first = f.store.manager_v2_inspect(f.manager, &query).unwrap();
    f.store
        .manager_send(
            f.manager,
            &AgentManagerSendRequestV1 {
                epic_id: f.epic,
                message: "Another request".into(),
                idempotency_key: "new-mail".into(),
            },
        )
        .unwrap();
    assert!(
        f.store
            .manager_v2_inspect(
                f.manager,
                &AgentManagerInspectRequestV2 {
                    cursor: first.next_cursor,
                    ..query
                }
            )
            .unwrap_err()
            .to_string()
            .contains("cursor_changed")
    );
    work(&f, "a");
    f.store
        .manager_v2_commit_update(
            f.lead,
            &request(
                ManagerUpdateV2::Stage {
                    key: "a".into(),
                    expected_row_version: 1,
                    stage: ManagerWorkStageV2::Implementation,
                    state: ManagerStageStateV2::Running,
                    note: "Implementing product a".into(),
                    evidence: None,
                },
                "stage",
            ),
            &LedgerObservation::default(),
        )
        .unwrap();
    let actions = f
        .store
        .manager_v2_inspect(
            f.lead,
            &AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Actions,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        actions
            .rows
            .iter()
            .any(|r| r["operation"]["update"] == "stage" && r["operation"]["key"] == "a")
    );
}

#[test]
fn an_explicit_stage_blocker_owns_unfinished_work_until_it_is_cleared() {
    let f = fixture();
    work(&f, "blocked");
    f.store
        .manager_v2_commit_update(
            f.lead,
            &request(
                ManagerUpdateV2::Stage {
                    key: "blocked".into(),
                    expected_row_version: 1,
                    stage: ManagerWorkStageV2::Verification,
                    state: ManagerStageStateV2::Blocked,
                    note: "Waiting for exact operator decision".into(),
                    evidence: None,
                },
                "stage-blocked",
            ),
            &LedgerObservation::default(),
        )
        .unwrap();
    let row = &inspect(&f, ManagerInspectSectionV2::Work).rows[0];
    assert_eq!(row["ready"], false);
    assert_eq!(row["blockers"], json!(["stage:verification"]));
    assert_eq!(
        row["stages"][3]["note"],
        "Waiting for exact operator decision"
    );
}

#[test]
fn harness_manager_empty_group_scope_is_visible_and_topology_requires_policy() {
    let f = fixture();
    let mut group = f.store.get_session(f.epic).unwrap().unwrap();
    group.id = Uuid::new_v4();
    group.parent_id = None;
    group.session_kind = SessionKind::Group;
    group.lead_session_id = None;
    group.title = Some("Future delivery group".into());
    f.store.insert_session(&group).unwrap();
    let config = f
        .store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: f.project,
            session_id: f.manager,
            epic_ids: None,
            group_ids: vec![group.id],
            expected_row_version: 1,
        })
        .unwrap();
    assert!(!config.is_revoked());
    let result = f
        .store
        .manager_v2_inspect(
            f.manager,
            &AgentManagerInspectRequestV2 {
                section: ManagerInspectSectionV2::Topology,
                ..Default::default()
            },
        )
        .unwrap();
    let row = result
        .rows
        .iter()
        .find(|row| row["id"] == json!(group.id))
        .unwrap();
    assert_eq!(row["title"], "Future delivery group");
    assert_eq!(row["selected_group"], true);
    assert_eq!(row["topology_granted"], false);
    let fence = ManagerFenceV2 {
        scope_version: 2,
        policy_version: 1,
    };
    assert!(
        f.store
            .manager_v2_authorize(f.manager, &fence, Some(ManagerCapabilityV2::Topology))
            .is_err()
    );
    f.store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: f.project,
            expected_scope_version: 2,
            expected_policy_version: 1,
            idempotency_key: "group-topology".into(),
            policy: ManagerPolicyV2 {
                capabilities: vec![ManagerCapabilityV2::Topology],
                ..Default::default()
            },
        })
        .unwrap();
    let authority = f
        .store
        .manager_v2_authorize(
            f.manager,
            &ManagerFenceV2 {
                scope_version: 2,
                policy_version: 2,
            },
            Some(ManagerCapabilityV2::Topology),
        )
        .unwrap();
    assert_eq!(
        f.store
            .manager_v2_require_container(&authority, group.id, false)
            .unwrap()
            .id,
        group.id
    );
    let original_group = f
        .store
        .get_session(f.epic)
        .unwrap()
        .unwrap()
        .parent_id
        .unwrap();
    assert!(
        f.store
            .manager_v2_require_container(&authority, original_group, false)
            .is_err()
    );
    let mut epic = f.store.get_session(f.epic).unwrap().unwrap();
    epic.id = Uuid::new_v4();
    epic.parent_id = Some(group.id);
    epic.lead_session_id = None;
    f.store.insert_session(&epic).unwrap();
    let refreshed = f
        .store
        .manager_v2_authorize(
            f.manager,
            &ManagerFenceV2 {
                scope_version: 2,
                policy_version: 2,
            },
            Some(ManagerCapabilityV2::Topology),
        )
        .unwrap();
    assert_eq!(
        f.store
            .manager_v2_require_epic(&refreshed, epic.id)
            .unwrap()
            .id,
        epic.id
    );
}
