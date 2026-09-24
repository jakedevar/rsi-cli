//! Issue #548: read-only work/ownership projection for manager-created
//! sessions. Every test fails on code without `Store::manager_work_view`.
#![allow(clippy::unwrap_used, clippy::too_many_lines)]
use super::*;

/// Bind `session` as a manager-created entity at the current seat and scope,
/// exactly as `manager_action_bind_entity` records a `create_session`.
fn bind(f: &Fixture, session: Uuid) {
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let operation = f
        .store
        .manager_v2_save_receipt(
            &config,
            Some(f.manager),
            1,
            "action",
            &format!("create-{session}"),
            &json!({"created":session}),
            &json!({"id":session}),
        )
        .unwrap();
    f.store
        .conn
        .execute(
            "INSERT INTO harness_manager_v2_entities(session_id,operation_id,project_id,manager_session_id,scope_version,policy_version,kind,created_at) VALUES(?1,?2,?3,?4,?5,1,'session',?6)",
            params![
                session.to_string(),
                operation.to_string(),
                f.project.to_string(),
                f.manager.to_string(),
                config.row_version,
                now()
            ],
        )
        .unwrap();
}

fn created_worker(f: &Fixture, title: &str) -> Uuid {
    let id = add_worker(f, f.epic, title);
    bind(f, id);
    id
}

fn view(f: &Fixture, caller: Uuid) -> Result<AgentManagerWorkViewResultV1> {
    f.store
        .manager_work_view(caller, &AgentManagerWorkViewRequestV1::default())
}

fn refusal(f: &Fixture, caller: Uuid) -> String {
    view(f, caller).unwrap_err().to_string()
}

fn claim(f: &Fixture, key: &str, files: &[&str]) {
    f.store
        .manager_v2_commit_update(
            f.manager,
            &request(
                ManagerUpdateV2::Ownership {
                    key: key.into(),
                    expected_row_version: 0,
                    domain: "crates/rsid/src/store".into(),
                    mode: ManagerOwnershipModeV2::Exclusive,
                    files: files.iter().map(|f| (*f).to_owned()).collect(),
                    active: true,
                },
                &format!("claim-{key}"),
            ),
            &LedgerObservation::default(),
        )
        .unwrap();
}

/// Rotate `predecessor` into a same-parent successor through the fixture
/// rotation receipt, archiving the predecessor as the finalizer does.
fn rotate(f: &Fixture, predecessor: Uuid) -> Uuid {
    let mut next = f.store.get_session(predecessor).unwrap().unwrap();
    next.id = Uuid::new_v4();
    next.continued_from = Some(predecessor);
    next.rotation_depth += 1;
    next.status = SessionStatus::Running;
    f.store.insert_session(&next).unwrap();
    f.store
        .update_session_status(predecessor, SessionStatus::Archived)
        .unwrap();
    assert!(
        f.store
            .record_harness_manager_rotation(predecessor, next.id)
            .unwrap()
    );
    next.id
}

#[test]
fn work_view_shows_granted_ownership_to_manager_created_worker() {
    let f = fixture();
    let worker = created_worker(&f, "Created worker");
    work(&f, "alpha");
    claim(&f, "alpha", &["crates/rsid/src/store/manager_ledger.rs"]);
    let page = view(&f, worker).unwrap();
    assert_eq!(page.epic_id, f.epic);
    assert_eq!(page.manager_session_id, f.manager);
    assert_eq!(page.scope_version, 1);
    assert_eq!(page.policy_version, 1);
    assert!(!page.paused);
    assert_eq!(page.works.len(), 1);
    let alpha = &page.works[0];
    assert_eq!(alpha.work_key, "alpha");
    assert_eq!(alpha.title, "alpha feature");
    assert_eq!(alpha.kind, ManagerWorkKindV2::Product);
    assert!(!alpha.mine, "no Stage has named a source yet");
    assert_eq!(alpha.stages.len(), STAGES.len());
    assert_eq!(page.ownership.len(), 1);
    let owned = &page.ownership[0];
    assert_eq!(owned.work_key, "alpha");
    assert_eq!(owned.domain, "crates/rsid/src/store");
    assert_eq!(owned.mode, ManagerOwnershipModeV2::Exclusive);
    assert_eq!(
        owned.files,
        vec!["crates/rsid/src/store/manager_ledger.rs".to_owned()]
    );
    assert!(owned.active);

    // A Stage whose evidence names the worker makes the work `mine`.
    let sha = "a".repeat(40);
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let (row, _) = f.store.manager_v2_work(&config, "alpha").unwrap();
    f.store
        .manager_v2_commit_update(
            f.manager,
            &request(
                ManagerUpdateV2::Stage {
                    key: "alpha".into(),
                    expected_row_version: row.row_version,
                    stage: ManagerWorkStageV2::Implementation,
                    state: ManagerStageStateV2::Running,
                    note: "worker implementing".into(),
                    evidence: Some(ManagerEvidenceV2 {
                        source_session_id: worker,
                        source_commit: sha.clone(),
                        artifact_path: "thoughts/shared/notes/alpha.md".into(),
                        artifact_commit: sha.clone(),
                        closure_evidence_id: None,
                    }),
                },
                "alpha-stage",
            ),
            &LedgerObservation {
                source_commit: Some(sha.clone()),
                ..Default::default()
            },
        )
        .unwrap();
    let page = view(&f, worker).unwrap();
    let alpha = &page.works[0];
    assert!(alpha.mine);
    assert_eq!(alpha.source_session_id, Some(worker));
    assert_eq!(alpha.source_commit.as_deref(), Some(sha.as_str()));
    let implementation = alpha
        .stages
        .iter()
        .find(|s| s.stage == ManagerWorkStageV2::Implementation)
        .unwrap();
    assert_eq!(implementation.state, ManagerStageStateV2::Running);

    // Exact key and keyset paging stay inside the caller's Epic.
    work(&f, "beta");
    let first = f
        .store
        .manager_work_view(
            worker,
            &AgentManagerWorkViewRequestV1 {
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(first.works[0].work_key, "alpha");
    assert_eq!(first.next_after_work_key.as_deref(), Some("alpha"));
    let second = f
        .store
        .manager_work_view(
            worker,
            &AgentManagerWorkViewRequestV1 {
                after_work_key: first.next_after_work_key,
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(second.works[0].work_key, "beta");
    assert_eq!(second.next_after_work_key, None);
    assert!(second.ownership.is_empty(), "beta has no claim");
    let exact = f
        .store
        .manager_work_view(
            worker,
            &AgentManagerWorkViewRequestV1 {
                work_key: Some("beta".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(exact.works.len(), 1);
    assert_eq!(exact.works[0].work_key, "beta");
}

#[test]
fn work_view_refuses_unmanaged_sessions_with_exact_codes() {
    let f = fixture();
    let worker = created_worker(&f, "Created worker");
    work(&f, "alpha");
    claim(&f, "alpha", &["src/lib.rs"]);
    // A lead-spawned sibling and the unappointed-by-manager lead.
    let sibling = add_worker(&f, f.epic, "Lead-spawned sibling");
    assert!(refusal(&f, sibling).contains("manager_work_view_not_managed"));
    assert!(refusal(&f, f.lead).contains("manager_work_view_not_managed"));
    // The manager itself created nothing for itself.
    assert!(refusal(&f, f.manager).contains("manager_work_view_not_managed"));
    // Containers are not work-view callers.
    assert!(refusal(&f, f.epic).contains("manager_work_view_unsupported_topology"));
    assert!(refusal(&f, Uuid::new_v4()).contains("manager_work_view_unsupported_topology"));
    // A session of a project without a manager.
    let foreign_project = Uuid::new_v4();
    f.store
        .insert_project(&rsi_common::types::Project {
            id: foreign_project,
            name: "Foreign".into(),
            path: None,
            description: None,
            color: rsi_common::types::Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        })
        .unwrap();
    let mut foreign = f.store.get_session(worker).unwrap().unwrap();
    foreign.id = Uuid::new_v4();
    foreign.project_id = Some(foreign_project);
    foreign.parent_id = None;
    f.store.insert_session(&foreign).unwrap();
    assert!(refusal(&f, foreign.id).contains("manager_not_configured"));
    // Invalid page input is typed before any authority read.
    let invalid = f
        .store
        .manager_work_view(
            worker,
            &AgentManagerWorkViewRequestV1 {
                limit: 33,
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(
        invalid
            .to_string()
            .contains("manager_invalid_work_view_request")
    );
    // The created worker reads until the operator edits the scope.
    assert_eq!(view(&f, worker).unwrap().ownership.len(), 1);
    let mut widened = f.store.get_session(f.epic).unwrap().unwrap();
    widened.id = Uuid::new_v4();
    widened.lead_session_id = None;
    f.store.insert_session(&widened).unwrap();
    f.store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: f.project,
            session_id: f.manager,
            epic_ids: Some(vec![f.epic, widened.id]),
            expected_row_version: 1,
        })
        .unwrap();
    let rescoped = f.store.get_harness_manager(f.project).unwrap().unwrap();
    assert_eq!(rescoped.row_version, 2);
    assert!(rescoped.epic_ids.contains(&f.epic));
    assert!(refusal(&f, worker).contains("manager_work_view_not_managed"));
}

#[test]
fn work_view_reads_current_grant_and_refuses_out_of_scope_epic() {
    let f = fixture();
    let worker = created_worker(&f, "Created worker");
    f.store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: f.project,
            expected_scope_version: 1,
            expected_policy_version: 1,
            idempotency_key: "revoke".into(),
            policy: ManagerPolicyV2 {
                capabilities: vec![ManagerCapabilityV2::WorkPlan],
                ..Default::default()
            },
        })
        .unwrap();
    // The view reports the current grant version, never a stale one.
    assert_eq!(view(&f, worker).unwrap().policy_version, 2);

    // A bound session outside every scoped Epic.
    let f = fixture();
    let mut group = f.store.get_session(f.manager).unwrap().unwrap();
    group.id = Uuid::new_v4();
    group.session_kind = SessionKind::Group;
    f.store.insert_session(&group).unwrap();
    let mut other = group.clone();
    other.id = Uuid::new_v4();
    other.session_kind = SessionKind::Epic;
    other.parent_id = Some(group.id);
    f.store.insert_session(&other).unwrap();
    let stray = add_worker(&f, other.id, "Unscoped worker");
    bind(&f, stray);
    assert!(refusal(&f, stray).contains("manager_v2_epic_out_of_scope"));
}

#[test]
fn work_view_follows_rotation() {
    let f = fixture();
    let worker = created_worker(&f, "Created worker");
    work(&f, "alpha");
    claim(&f, "alpha", &["src/lib.rs"]);
    // The manager seat rotates; the logical anchor keeps the entity.
    let manager_tip = rotate(&f, f.manager);
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    assert_eq!(config.current_session_id, Some(manager_tip));
    assert_eq!(view(&f, worker).unwrap().ownership[0].work_key, "alpha");
    // The worker rotates: the tip reads the root's entity, the predecessor
    // is stale.
    let worker_tip = rotate(&f, worker);
    let page = view(&f, worker_tip).unwrap();
    assert_eq!(page.ownership[0].files, vec!["src/lib.rs".to_owned()]);
    assert!(refusal(&f, worker).contains("manager_work_view_stale_session"));
}

#[test]
fn work_view_reports_pause_without_refusing() {
    let f = fixture();
    let worker = created_worker(&f, "Created worker");
    f.store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: f.project,
            expected_scope_version: 1,
            expected_policy_version: 1,
            idempotency_key: "pause-epic".into(),
            policy: ManagerPolicyV2 {
                capabilities: vec![ManagerCapabilityV2::WorkPlan],
                paused_epic_ids: vec![f.epic],
                ..Default::default()
            },
        })
        .unwrap();
    let page = view(&f, worker).unwrap();
    assert!(page.paused);
    assert_eq!(page.epic_id, f.epic);
}

#[test]
fn work_view_under_busy_lead_returns_relay_delivery_state() {
    let f = fixture();
    let worker = created_worker(&f, "Created worker");
    f.store
        .update_session_status(f.lead, SessionStatus::Running)
        .unwrap();
    let sent = f
        .store
        .manager_send(
            f.manager,
            &AgentManagerSendRequestV1 {
                epic_id: f.epic,
                message: "Please grant the worker src/lib.rs".into(),
                idempotency_key: "relay".into(),
            },
        )
        .unwrap();
    let page = view(&f, worker).unwrap();
    assert!(!page.more_relay);
    assert_eq!(page.relay.len(), 1);
    let queued = &page.relay[0];
    assert_eq!(queued.request_id, sent.message_id);
    assert_eq!(queued.state, "queued");
    assert!(!queued.retrieved);
    assert!(!queued.replied);
    assert_eq!(queued.retrieved_at, None);
    // The relay row carries delivery fields only; bodies stay in the inbox.
    let fields: std::collections::BTreeSet<String> = serde_json::to_value(queued)
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        fields,
        [
            "delivered_at",
            "delivery_issue",
            "queued_at",
            "replied",
            "request_id",
            "retrieved",
            "retrieved_at",
            "settled_at",
            "state",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
    // The busy lead retrieves: the worker observes retrieval, still unanswered.
    f.store
        .manager_inbox(f.lead, &AgentManagerInboxRequestV1::default())
        .unwrap();
    let page = view(&f, worker).unwrap();
    assert_eq!(page.relay[0].state, "retrieved");
    assert!(page.relay[0].retrieved);
    assert!(page.relay[0].queued_at <= page.observed_at);
    // A reply answers the request; it leaves the unanswered relay.
    f.store
        .manager_reply(
            f.lead,
            &AgentManagerReplyRequestV1 {
                request_id: sent.message_id,
                message: "Granted".into(),
                idempotency_key: "relay-reply".into(),
            },
        )
        .unwrap();
    assert!(view(&f, worker).unwrap().relay.is_empty());
}

/// Review 11ed5350: an exact `work_key` read applies the same LIVE predicate
/// as page listings, so integrated work (and its still-active claim) is
/// answered by the typed `manager_work_view_work_not_live` code while live
/// work by exact key keeps returning its full view.
#[test]
fn work_view_exact_key_refuses_integrated_work_and_serves_live_work() {
    let f = fixture();
    let worker = created_worker(&f, "Created worker");
    work(&f, "landed");
    claim(&f, "landed", &["src/landed.rs"]);
    work(&f, "live");
    f.store
        .manager_v2_commit_update(
            f.manager,
            &request(
                ManagerUpdateV2::Ownership {
                    key: "live".into(),
                    expected_row_version: 0,
                    domain: "src/live".into(),
                    mode: ManagerOwnershipModeV2::Exclusive,
                    files: vec!["src/live.rs".into()],
                    active: true,
                },
                "claim-live",
            ),
            &LedgerObservation::default(),
        )
        .unwrap();
    let config = f.store.get_harness_manager(f.project).unwrap().unwrap();
    let (row, mut landed) = f.store.manager_v2_work(&config, "landed").unwrap();
    let source = "a".repeat(40);
    landed.source_commit = Some(source.clone());
    landed.integration = Some(Integration {
        source_commit: source,
        target_commit: "c".repeat(40),
        verification: None,
        integrated_at: now(),
    });
    f.store
        .manager_v2_put_record(
            &config,
            "work",
            "landed",
            Some(f.epic),
            row.row_version,
            &serde_json::to_value(&landed).unwrap(),
        )
        .unwrap();
    let exact = |key: &str| {
        f.store.manager_work_view(
            worker,
            &AgentManagerWorkViewRequestV1 {
                work_key: Some(key.into()),
                ..Default::default()
            },
        )
    };
    let refused = exact("landed").unwrap_err().to_string();
    assert!(
        refused.contains("manager_work_view_work_not_live"),
        "{refused}"
    );
    let live = exact("live").unwrap();
    assert_eq!(live.works.len(), 1);
    assert_eq!(live.works[0].work_key, "live");
    assert!(!live.works[0].integrated);
    assert_eq!(live.ownership.len(), 1);
    assert_eq!(live.ownership[0].work_key, "live");
    assert_eq!(live.ownership[0].files, vec!["src/live.rs".to_owned()]);
    // The default page lists exactly the live work.
    let page = view(&f, worker).unwrap();
    assert_eq!(
        page.works
            .iter()
            .map(|w| w.work_key.as_str())
            .collect::<Vec<_>>(),
        vec!["live"]
    );
}

#[test]
fn work_view_is_side_effect_free() {
    let f = fixture();
    let worker = created_worker(&f, "Created worker");
    work(&f, "alpha");
    claim(&f, "alpha", &["src/lib.rs"]);
    f.store
        .manager_send(
            f.manager,
            &AgentManagerSendRequestV1 {
                epic_id: f.epic,
                message: "Queued relay".into(),
                idempotency_key: "relay".into(),
            },
        )
        .unwrap();
    let tables: Vec<String> = {
        let mut stmt = f
            .store
            .conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='table'
                  AND (name LIKE 'harness_manager%' OR name IN ('scheduled_jobs','sessions'))
                  ORDER BY name",
            )
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert!(tables.contains(&"harness_manager_notices".to_owned()));
    let counts = |store: &Store| -> Vec<i64> {
        tables
            .iter()
            .map(|t| {
                store
                    .conn
                    .query_row(&format!("SELECT count(*) FROM \"{t}\""), [], |r| r.get(0))
                    .unwrap()
            })
            .collect()
    };
    let changes = |store: &Store| -> i64 {
        store
            .conn
            .query_row("SELECT total_changes()", [], |r| r.get(0))
            .unwrap()
    };
    let before = (counts(&f.store), changes(&f.store));
    let page = view(&f, worker).unwrap();
    assert_eq!(page.relay.len(), 1);
    assert_eq!(page.ownership.len(), 1);
    assert_eq!((counts(&f.store), changes(&f.store)), before);
    // Refusals are equally side-effect free.
    let sibling = add_worker(&f, f.epic, "Sibling");
    let before_refusal = (counts(&f.store), changes(&f.store));
    assert!(refusal(&f, sibling).contains("manager_work_view_not_managed"));
    assert_eq!((counts(&f.store), changes(&f.store)), before_refusal);
    assert_ne!(before, before_refusal, "the sibling insert is visible");
    view(&f, worker).unwrap();
    assert_eq!((counts(&f.store), changes(&f.store)), before_refusal);
}
