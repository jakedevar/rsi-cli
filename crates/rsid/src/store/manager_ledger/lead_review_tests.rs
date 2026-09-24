#![allow(clippy::unwrap_used)]

use super::*;
use rsi_common::types::SessionProvider;

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn review_policy(f: &Fixture, paused: bool) -> i64 {
    let version = f
        .store
        .get_harness_manager_policy(f.project)
        .unwrap()
        .unwrap()
        .row_version;
    f.store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: f.project,
            expected_scope_version: 1,
            expected_policy_version: version,
            idempotency_key: format!("lead-review-policy-{version}"),
            policy: ManagerPolicyV2 {
                mode: ManagerOperatingModeV2::Execute,
                paused,
                capabilities: vec![
                    ManagerCapabilityV2::WorkPlan,
                    ManagerCapabilityV2::SessionCreate,
                ],
                max_created_sessions: 2,
                allowed_launches: vec![ManagerLaunchChoiceV2 {
                    provider: SessionProvider::Claude,
                    model: "claude-sonnet-5".into(),
                    effort: None,
                }],
                ..Default::default()
            },
        })
        .unwrap();
    version + 1
}

fn source_work(f: &Fixture, key: &str, epic: Uuid, author: Uuid) -> i64 {
    f.store
        .conn
        .execute(
            "UPDATE sessions SET model='gpt-6-sol' WHERE id=?1",
            [author.to_string()],
        )
        .unwrap();
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let policy = f
        .store
        .get_harness_manager_policy(f.project)
        .unwrap()
        .unwrap();
    let mut create = request(
        ManagerUpdateV2::Work {
            key: key.into(),
            expected_row_version: 0,
            epic_id: epic,
            title: format!("{key} work"),
            kind: ManagerWorkKindV2::Product,
            priority: 1,
            weight: 1,
            required_gates: vec![
                ManagerWorkStageV2::Implementation,
                ManagerWorkStageV2::Review,
                ManagerWorkStageV2::Verification,
            ],
        },
        key,
    );
    create.fence.scope_version = config.row_version;
    create.fence.policy_version = policy.row_version;
    f.store
        .manager_v2_commit_update(f.manager, &create, &LedgerObservation::default())
        .unwrap();
    let (row, mut record) = f.store.manager_v2_work(&config, key).unwrap();
    record.source_commit = Some(SHA.into());
    record.source_session_id = Some(author);
    f.store
        .manager_v2_put_record(
            &config,
            "work",
            key,
            Some(epic),
            row.row_version,
            &json!(record),
        )
        .unwrap()
        .row_version
}

fn review(
    key: &str,
    version: i64,
    policy_version: i64,
    model: &str,
) -> AgentManagerUpdateRequestV2 {
    let mut request = request(
        ManagerUpdateV2::RequestReview {
            key: key.into(),
            expected_row_version: version,
            source_commit: SHA.into(),
            query: "Review the sealed source".into(),
            launch: ManagerLaunchChoiceV2 {
                provider: if model.starts_with("claude-") {
                    SessionProvider::Claude
                } else {
                    SessionProvider::Codex
                },
                model: model.into(),
                effort: None,
            },
        },
        &format!("review-{key}-{model}"),
    );
    request.fence.policy_version = policy_version;
    request
}

fn reserve(
    f: &Fixture,
    caller: Uuid,
    request: &AgentManagerUpdateRequestV2,
) -> crate::error::Result<ManagerMutationReceiptV2> {
    f.store.manager_v2_commit_update(
        caller,
        request,
        &LedgerObservation {
            source_commit: Some(SHA.into()),
            ..Default::default()
        },
    )
}

fn lead_review(f: &Fixture, key: &str, policy_version: i64) -> Uuid {
    let version = source_work(f, key, f.epic, f.lead);
    let receipt = reserve(
        f,
        f.lead,
        &review(key, version, policy_version, "claude-sonnet-5"),
    )
    .unwrap();
    let assignment = Uuid::parse_str(&receipt.key).unwrap();
    let requester: String = f.store.conn.query_row(
        "SELECT json_extract(request_json,'$.requester_session_id') FROM manager_review_assignments WHERE assignment_id=?1",
        [assignment.to_string()], |row| row.get(0),
    ).unwrap();
    assert_eq!(requester, f.lead.to_string());
    assignment
}

#[test]
fn current_lead_request_review_reserves_with_itself_as_requester() {
    let f = fixture();
    let policy_version = review_policy(&f, false);
    let assignment = lead_review(&f, "lead-own", policy_version);
    assert!(
        f.store
            .allocate_manager_review_assignment(assignment)
            .unwrap()
    );
    let state: String = f
        .store
        .conn
        .query_row(
            "SELECT state FROM manager_review_assignments WHERE assignment_id=?1",
            [assignment.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "allocating");
}

#[test]
fn lead_request_review_for_another_epics_work_is_out_of_scope() {
    let f = fixture();
    let policy_version = review_policy(&f, false);
    lead_review(&f, "own-first", policy_version);
    let group = f
        .store
        .get_session(f.epic)
        .unwrap()
        .unwrap()
        .parent_id
        .unwrap();
    let mut other_epic = f.store.get_session(f.epic).unwrap().unwrap();
    other_epic.id = Uuid::new_v4();
    other_epic.parent_id = Some(group);
    f.store.insert_session(&other_epic).unwrap();
    let mut other_lead = f.store.get_session(f.lead).unwrap().unwrap();
    other_lead.id = Uuid::new_v4();
    other_lead.parent_id = Some(other_epic.id);
    f.store.insert_session(&other_lead).unwrap();
    f.store
        .conn
        .execute(
            "UPDATE sessions SET lead_session_id=?2 WHERE id=?1",
            params![other_epic.id.to_string(), other_lead.id.to_string()],
        )
        .unwrap();
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    f.store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: vec![],
            project_id: f.project,
            session_id: f.manager,
            epic_ids: Some(vec![f.epic, other_epic.id]),
            expected_row_version: config.row_version,
        })
        .unwrap();
    let policy = f
        .store
        .get_harness_manager_policy(f.project)
        .unwrap()
        .unwrap();
    f.store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: f.project,
            expected_scope_version: 2,
            expected_policy_version: policy.row_version,
            idempotency_key: "two-epics-policy".into(),
            policy: policy.policy,
        })
        .unwrap();
    let mut request = review("other-work", 1, policy_version + 1, "claude-sonnet-5");
    request.fence.scope_version = 2;
    let before = reserve(&f, f.lead, &request).unwrap_err().to_string();
    assert!(before.contains("manager_v2_work_missing"), "{before}");
    let version = source_work(&f, "other-work", other_epic.id, other_lead.id);
    if let ManagerUpdateV2::RequestReview {
        expected_row_version,
        ..
    } = &mut request.change
    {
        *expected_row_version = version;
    }
    let error = reserve(&f, f.lead, &request).unwrap_err().to_string();
    assert!(error.contains("manager_v2_epic_out_of_scope"), "{error}");
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let (_, record) = f.store.manager_v2_work(&config, "other-work").unwrap();
    let direct = f
        .store
        .manager_review_reserve_on(
            f.lead,
            &config,
            &request,
            &record,
            version,
            &LedgerObservation {
                source_commit: Some(SHA.into()),
                ..Default::default()
            },
        )
        .unwrap_err()
        .to_string();
    assert!(direct.contains("manager_v2_epic_out_of_scope"), "{direct}");
    let count: i64 = f
        .store
        .conn
        .query_row(
            "SELECT count(*) FROM manager_review_assignments WHERE work_key='other-work'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn rotated_lead_tip_may_request_and_stale_predecessor_may_not() {
    let f = fixture();
    let policy_version = review_policy(&f, false);
    lead_review(&f, "before-rotation", policy_version);
    let mut tip = f.store.get_session(f.lead).unwrap().unwrap();
    tip.id = Uuid::new_v4();
    tip.continued_from = Some(f.lead);
    f.store.insert_session(&tip).unwrap();
    f.store
        .conn
        .execute(
            "UPDATE sessions SET lead_session_id=?2 WHERE id=?1",
            params![f.epic.to_string(), tip.id.to_string()],
        )
        .unwrap();
    let version = source_work(&f, "after-rotation", f.epic, tip.id);
    let review = review("after-rotation", version, policy_version, "claude-sonnet-5");
    let stale = reserve(&f, f.lead, &review).unwrap_err().to_string();
    assert!(
        stale.contains("manager_v2_current_lead_required")
            || stale.contains("manager_v2_epic_out_of_scope")
            || stale.contains("manager_scope_denied"),
        "{stale}"
    );
    let receipt = reserve(&f, tip.id, &review).unwrap();
    let requester: String = f
        .store
        .conn
        .query_row(
            "SELECT json_extract(request_json,'$.requester_session_id') FROM manager_review_assignments WHERE assignment_id=?1",
            [receipt.key],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(requester, tip.id.to_string());
}

#[test]
fn lead_review_keeps_contributor_and_manager_owned_supersession_guards() {
    let f = fixture();
    let policy_version = review_policy(&f, false);
    lead_review(&f, "guard-own", policy_version);
    let version = source_work(&f, "guard-family", f.epic, f.lead);
    let conflict = reserve(
        &f,
        f.lead,
        &review("guard-family", version, policy_version, "gpt-6-sol"),
    )
    .unwrap_err()
    .to_string();
    assert!(
        conflict.contains("manager_review_family_conflict"),
        "{conflict}"
    );
    let manager_request = review("guard-family", version, policy_version, "claude-sonnet-5");
    reserve(&f, f.manager, &manager_request).unwrap();
    let mut lead_request = manager_request;
    lead_request.idempotency_key = "lead-supersede-manager".into();
    let error = reserve(&f, f.lead, &lead_request).unwrap_err().to_string();
    assert!(error.contains("manager_review_manager_owned"), "{error}");
}

#[test]
fn paused_policy_blocks_a_lead_review_allocation() {
    let f = fixture();
    let policy_version = review_policy(&f, true);
    let assignment = lead_review(&f, "paused-review", policy_version);
    let error = f
        .store
        .allocate_manager_review_assignment(assignment)
        .unwrap_err()
        .to_string();
    assert!(error.contains("manager_v2_policy_paused"), "{error}");
}

#[test]
fn lead_review_does_not_grant_manager_only_updates() {
    let f = fixture();
    let policy_version = review_policy(&f, false);
    lead_review(&f, "restricted", policy_version);
    let changes = [
        ManagerUpdateV2::Work {
            key: "new-work".into(),
            expected_row_version: 0,
            epic_id: f.epic,
            title: "New work".into(),
            kind: ManagerWorkKindV2::Product,
            priority: 1,
            weight: 1,
            required_gates: vec![
                ManagerWorkStageV2::Implementation,
                ManagerWorkStageV2::Review,
                ManagerWorkStageV2::Verification,
            ],
        },
        ManagerUpdateV2::Dependency {
            key: "restricted".into(),
            expected_row_version: 0,
            prerequisite: "restricted".into(),
            require_integrated: false,
            enabled: true,
        },
        ManagerUpdateV2::Ownership {
            key: "restricted".into(),
            expected_row_version: 0,
            domain: "store".into(),
            mode: ManagerOwnershipModeV2::Exclusive,
            files: vec!["crates/rsid/src/store/manager_ledger/mutations.rs".into()],
            active: true,
        },
        ManagerUpdateV2::Migration {
            key: "restricted".into(),
            expected_row_version: 0,
            version: 1,
            baseline_commit: SHA.into(),
            inventory_digest: "digest".into(),
        },
        ManagerUpdateV2::MigrationTransfer {
            key: "restricted".into(),
            expected_row_version: 0,
            version: 1,
        },
        ManagerUpdateV2::MigrationRelease {
            key: "restricted".into(),
            expected_row_version: 0,
            version: 1,
        },
        ManagerUpdateV2::Handoff {
            summary: "handoff".into(),
            next_actions: vec![],
        },
    ];
    for (index, change) in changes.into_iter().enumerate() {
        let mut request = request(change, &format!("manager-only-{index}"));
        request.fence.policy_version = policy_version;
        let error = f
            .store
            .manager_v2_prepare_update(f.lead, &request)
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("manager_v2_manager_required"),
            "change {index}: {error}"
        );
    }
}
