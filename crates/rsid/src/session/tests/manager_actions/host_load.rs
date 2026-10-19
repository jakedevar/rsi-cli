//! #1417: host-load admission. While the host's 1-minute load is above the
//! operator's `host_load_admission_threshold` the daemon holds a manager's
//! `create_session` (queued, never refused), reports why in
//! `AgentManagerGetAction` and `AgentGetDaemonInfo`, and launches it on its own
//! once the load drops, oldest first. Lead recovery and every other action are
//! never held.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening,
    clippy::large_futures
)]

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
use super::*;
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
use crate::host_load::LoadReading;
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
use std::sync::Arc;

/// Make the manager's host load whatever the returned cell holds.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn host_load(p: &Pilot, load: f64) -> Arc<std::sync::Mutex<f64>> {
    let cell = Arc::new(std::sync::Mutex::new(load));
    let shared = Arc::clone(&cell);
    p.manager.host_load().set_source(Arc::new(move || {
        LoadReading::Load1(*shared.lock().unwrap())
    }));
    cell
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn set_threshold(p: &Pilot, threshold: u32) {
    p.manager
        .runtime_config
        .update_field(
            "host_load_admission_threshold",
            &serde_json::json!(threshold),
        )
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
fn create(parent: Uuid, p: &Pilot, query: &str) -> ManagerActionV2 {
    ManagerActionV2::CreateSession {
        parent_id: parent,
        kind: SessionKind::Task,
        query: query.into(),
        launch: p.policy.allowed_launches[0].clone(),
        sandbox_source: None,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn view(p: &Pilot, id: Uuid) -> ManagerActionViewV2 {
    p.manager
        .agent_control()
        .agent_manager_get_action_view(
            p.owner,
            AgentManagerGetActionRequestV2 {
                project_id: None,
                operation_id: id,
            },
        )
        .await
        .unwrap()
}

/// What the reconcile loop does for one claim: leave creates alone while the
/// host is loaded.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn claim_like_the_loop(manager: &SessionManager) -> Option<ManagerActionClaimV2> {
    manager
        .store
        .lock()
        .await
        .claim_manager_action_with_create_admission(
            manager.program_run_boot_id,
            manager.deploy_drain().is_draining(),
            &|key, since, target| manager.admit_manager_create(key, since, target),
        )
        .unwrap()
}

/// The queued creates' target sessions in the order they are released.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn queued_targets(p: &Pilot) -> Vec<Option<Uuid>> {
    p.manager
        .store
        .lock()
        .await
        .queued_manager_create_sessions(16)
        .unwrap()
        .into_iter()
        .map(|(target, _)| target)
        .collect()
}

/// A second project with its own manager seat and Epic in `p`'s store, so
/// held creates of two projects compete for the host.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
struct SecondProject {
    owner: Uuid,
    epic: Uuid,
    policy: ManagerPolicyV2,
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn second_project(p: &Pilot) -> SecondProject {
    let repo = p._dir.path().join("repo-b");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.name", "Lifecycle fixture"]);
    git(&repo, &["config", "user.email", "fixture@example.invalid"]);
    std::fs::write(repo.join("source"), "committed source\n").unwrap();
    git(&repo, &["add", "source"]);
    git(&repo, &["commit", "-qm", "fixture"]);
    let (project, owner, group, epic, lead) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let policy = ManagerPolicyV2 {
        group_ids: vec![group],
        ..p.policy.clone()
    };
    let store = p.manager.store.lock().await;
    store
        .insert_project(&Project {
            id: project,
            name: "Second project".into(),
            path: Some(repo.clone()),
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        })
        .unwrap();
    for (id, kind, parent) in [
        (owner, SessionKind::Standard, None),
        (group, SessionKind::Group, None),
        (epic, SessionKind::Epic, Some(group)),
        (lead, SessionKind::Feature, Some(epic)),
    ] {
        let mut row = bare_session(id);
        row.project_id = Some(project);
        row.working_dir = repo.clone();
        row.session_kind = kind;
        row.parent_id = parent;
        row.provider = SessionProvider::Claude;
        row.model = Some("manager-scripted-provider".into());
        row.claude_session_id = Some(format!("provider-{id}"));
        store.insert_session(&row).unwrap();
        p.manager
            .completed
            .write()
            .await
            .insert(id, CompletedSession::for_test(row));
    }
    store.set_lead_session(epic, Some(lead)).unwrap();
    store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            group_ids: Vec::new(),
            project_id: project,
            session_id: owner,
            epic_ids: Some(vec![epic]),
            expected_row_version: 0,
        })
        .unwrap();
    store
        .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
            project_id: project,
            expected_scope_version: 1,
            expected_policy_version: 0,
            idempotency_key: "grant".into(),
            policy: policy.clone(),
        })
        .unwrap();
    SecondProject {
        owner,
        epic,
        policy,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_loaded_host_holds_a_create_queued_and_it_launches_once_the_load_drops() {
    let p = pilot().await;
    let load = host_load(&p, 62.5);
    let receipt = p
        .admit(
            "held-by-load",
            create(p.epic, &p, "worker for a held Issue"),
        )
        .await;
    let child = receipt.target_session_id.unwrap();

    // Held: the pass leaves the action queued, never refused or blocked.
    p.manager.reconcile_manager_actions_once().await.unwrap();
    assert_eq!(
        p.receipt(receipt.operation_id).await.state,
        ManagerActionStateV2::Queued
    );
    let held = view(&p, receipt.operation_id).await;
    assert_eq!(held.receipt.state, ManagerActionStateV2::Queued);
    let reason = held.held.clone().expect("a held create names its hold");
    assert_eq!(reason.reason, "host_load");
    assert_eq!(reason.load, Some(62.5));
    assert_eq!(reason.threshold, Some(40));
    assert_eq!(reason.recent_admissions, Some(0));
    assert_eq!(reason.deploy_id, None);
    assert_eq!(reason.release_by, None, "a load hold has no release time");
    let wire = serde_json::to_value(&held).unwrap();
    assert_eq!(wire["state"], "queued", "the receipt fields stay top level");
    assert_eq!(wire["held"]["reason"], "host_load");
    assert_eq!(wire["held"]["load"], 62.5);
    assert_eq!(wire["held"]["threshold"], 40);

    // AgentGetDaemonInfo shows the same hold and lists the held create.
    let binary = p._dir.path().join("rsid-binary");
    std::fs::write(&binary, b"rsid").unwrap();
    let service = crate::daemon_info::DaemonInfoService::new(
        p._dir.path().to_path_buf(),
        p._dir.path().to_path_buf(),
        binary,
    );
    let info = p
        .manager
        .agent_control()
        .agent_get_daemon_info_via(p.owner, &service, &p._dir.path().join("satellites"))
        .await
        .unwrap();
    assert!(info.host_load.supported && info.host_load.holding);
    assert_eq!(info.host_load.threshold, 40);
    assert_eq!(info.host_load.load, Some(62.5));
    assert_eq!(info.host_load.held.len(), 1);
    assert_eq!(info.host_load.held[0].kind, "manager_create_session");
    assert_eq!(info.host_load.held[0].session_id, Some(child));
    assert_eq!(info.host_load.held[0].reason, "host_load");

    // The load drops below the threshold: the next pass launches it.
    *load.lock().unwrap() = 12.0;
    assert_eq!(view(&p, receipt.operation_id).await.held, None);
    let process = super::super::super::launch::install_controller_candidate_test_process(child);
    p.manager.reconcile_manager_actions_once().await.unwrap();
    let settled = p.receipt(receipt.operation_id).await;
    assert_eq!(
        settled.state,
        ManagerActionStateV2::Succeeded,
        "{settled:?}"
    );
    assert_eq!(settled.outcome.as_deref(), Some("session_established"));
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    let info = p
        .manager
        .agent_control()
        .agent_get_daemon_info_via(p.owner, &service, &p._dir.path().join("satellites"))
        .await
        .unwrap();
    assert!(!info.host_load.holding);
    assert!(info.host_load.held.is_empty());
    assert_eq!(
        info.host_load.recent_admissions, 1,
        "the launch counts until the load average absorbs it"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_zero_threshold_disables_the_hold() {
    let p = pilot().await;
    host_load(&p, 95.0);
    set_threshold(&p, 0);
    let receipt = p
        .admit("not-held", create(p.epic, &p, "worker on a busy host"))
        .await;
    let child = receipt.target_session_id.unwrap();
    assert_eq!(
        view(&p, receipt.operation_id).await.held,
        None,
        "nothing holds it"
    );
    let process = super::super::super::launch::install_controller_candidate_test_process(child);
    p.manager.reconcile_manager_actions_once().await.unwrap();
    let settled = p.receipt(receipt.operation_id).await;
    assert_eq!(
        settled.state,
        ManagerActionStateV2::Succeeded,
        "{settled:?}"
    );
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn the_threshold_applies_live_to_a_queued_create() {
    let p = pilot().await;
    let load = host_load(&p, 30.0);
    set_threshold(&p, 25);
    let receipt = p
        .admit("live-threshold", create(p.epic, &p, "worker"))
        .await;
    assert!(claim_like_the_loop(&p.manager).await.is_none(), "30 > 25");
    let reason = view(&p, receipt.operation_id).await.held.unwrap();
    assert_eq!((reason.load, reason.threshold), (Some(30.0), Some(25)));
    // Raising the setting releases it without a restart.
    set_threshold(&p, 32);
    assert_eq!(view(&p, receipt.operation_id).await.held, None);
    assert_eq!(
        claim_like_the_loop(&p.manager).await.unwrap().id(),
        receipt.operation_id
    );
    // The admitted create now contributes one to the load comparison.
    *load.lock().unwrap() = 31.0;
    assert!(p.manager.host_load().hold_now().is_none());
}

/// #2562: a hot or full host holds a manager's `create_session` exactly as a
/// busy one does (queued, never refused; `held {reason: thermal|disk_pressure}`)
/// even with the load hold disabled, and the claim admits it once the stubbed
/// temperature drops.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn thermal_and_disk_pressure_hold_a_create_until_the_host_recovers() {
    use crate::host_pressure::PressureHold;
    let p = pilot().await;
    set_threshold(&p, 0);
    let pressure = Arc::new(std::sync::Mutex::new(Some(PressureHold::Thermal(
        rsid_core::thermal_guard::ThermalHold {
            temperature_c: 91.5,
            ceiling_c: 85,
        },
    ))));
    let shared = Arc::clone(&pressure);
    p.manager
        .host_load()
        .set_pressure_source(Arc::new(move || *shared.lock().unwrap()));
    let receipt = p.admit("hot-host", create(p.epic, &p, "worker")).await;

    assert!(
        claim_like_the_loop(&p.manager).await.is_none(),
        "held, not refused"
    );
    assert_eq!(
        p.receipt(receipt.operation_id).await.state,
        ManagerActionStateV2::Queued
    );
    let held = view(&p, receipt.operation_id).await.held.unwrap();
    assert_eq!(held.reason, "thermal");
    assert_eq!(
        p.manager
            .launch_hold_reclaim
            .submissions
            .load(Ordering::SeqCst),
        0
    );
    assert_eq!((held.temperature_c, held.ceiling_c), (Some(91.5), Some(85)));
    assert_eq!(held.load, None);

    *pressure.lock().unwrap() = Some(PressureHold::DiskPressure {
        free_gb: 11.0,
        min_gb: 30,
    });
    let held = view(&p, receipt.operation_id).await.held.unwrap();
    assert_eq!(held.reason, "disk_pressure");
    assert_eq!(
        (held.free_disk_gb, held.min_free_disk_gb),
        (Some(11.0), Some(30))
    );
    assert!(claim_like_the_loop(&p.manager).await.is_none());

    assert_eq!(
        p.manager
            .launch_hold_reclaim
            .submissions
            .load(Ordering::SeqCst),
        1
    );

    *pressure.lock().unwrap() = None;
    assert_eq!(view(&p, receipt.operation_id).await.held, None);
    assert_eq!(
        claim_like_the_loop(&p.manager).await.unwrap().id(),
        receipt.operation_id,
        "the create starts on its own once the host cooled"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn older_topology_work_releases_before_new_manager_creates() {
    let p = pilot().await;
    let load = host_load(&p, 75.0);
    let node = Uuid::new_v4();
    let since = chrono::Utc::now() - chrono::Duration::minutes(1);
    let gate = p.manager.host_load();
    assert!(gate.admit_waiter(node, since, Some(node)).is_some());
    let receipt = p
        .admit("younger-manager-create", create(p.epic, &p, "worker"))
        .await;
    p.manager.reconcile_manager_actions_once().await.unwrap();
    *load.lock().unwrap() = 10.0;
    // Repeated manager passes cannot consume the older topology node's slot.
    for _ in 0..3 {
        p.manager.reconcile_manager_actions_once().await.unwrap();
        assert_eq!(
            p.receipt(receipt.operation_id).await.state,
            ManagerActionStateV2::Queued
        );
        assert_eq!(
            view(&p, receipt.operation_id).await.held.unwrap().reason,
            "host_load"
        );
    }
    assert_eq!(gate.admit_waiter(node, since, Some(node)), None);
    let child = receipt.target_session_id.unwrap();
    let process = super::super::super::launch::install_controller_candidate_test_process(child);
    p.manager.reconcile_manager_actions_once().await.unwrap();
    assert_eq!(
        p.receipt(receipt.operation_id).await.state,
        ManagerActionStateV2::Succeeded
    );
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        gate.status(&[]).recent_admissions,
        2,
        "one admission per launch"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn held_creates_from_several_projects_are_released_oldest_first() {
    let p = pilot().await;
    let b = second_project(&p).await;
    let load = host_load(&p, 70.0);
    let admit_b = |key: &'static str| {
        let control = p.manager.agent_control();
        let request = AgentManagerControlRequestV2 {
            ..AgentManagerControlRequestV2::new(
                ManagerFenceV2 {
                    scope_version: 1,
                    policy_version: 1,
                },
                key.into(),
                ManagerActionV2::CreateSession {
                    parent_id: b.epic,
                    kind: SessionKind::Task,
                    query: format!("second project {key}"),
                    launch: b.policy.allowed_launches[0].clone(),
                    sandbox_source: None,
                },
            )
        };
        let owner = b.owner;
        async move { control.agent_manager_control(owner, request).await.unwrap() }
    };
    let pause = || tokio::time::sleep(std::time::Duration::from_millis(5));

    // Round one: project A asks first, project B second.
    let a1 = p.admit("a-first", create(p.epic, &p, "a first")).await;
    pause().await;
    let b1 = admit_b("b-second").await;
    assert!(claim_like_the_loop(&p.manager).await.is_none(), "all held");
    assert_eq!(
        queued_targets(&p).await,
        [a1.target_session_id, b1.target_session_id],
        "the held list is oldest first"
    );
    *load.lock().unwrap() = 10.0;
    let first = claim_like_the_loop(&p.manager).await.unwrap();
    let second = claim_like_the_loop(&p.manager).await.unwrap();
    assert_eq!(
        [first.id(), second.id()],
        [a1.operation_id, b1.operation_id],
        "the older create goes first across projects"
    );
    for claim in [&first, &second] {
        p.manager
            .store
            .lock()
            .await
            .finish_manager_action(claim, ManagerActionStateV2::Succeeded, "done")
            .unwrap();
    }

    // Round two with the projects swapped: age decides, not the project.
    *load.lock().unwrap() = 70.0;
    let b2 = admit_b("b-first").await;
    pause().await;
    let a2 = p.admit("a-second", create(p.epic, &p, "a second")).await;
    assert!(claim_like_the_loop(&p.manager).await.is_none(), "all held");
    assert_eq!(
        queued_targets(&p).await,
        [b2.target_session_id, a2.target_session_id]
    );
    *load.lock().unwrap() = 10.0;
    let first = claim_like_the_loop(&p.manager).await.unwrap();
    let second = claim_like_the_loop(&p.manager).await.unwrap();
    assert_eq!(
        [first.id(), second.id()],
        [b2.operation_id, a2.operation_id],
        "oldest first whichever project it belongs to"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn lead_recovery_is_not_held_while_a_create_waits_for_the_host() {
    let (p, recovery, child) = queued_last_slot_actions(SessionProvider::Claude, 1).await;
    host_load(&p, 88.0);
    // Without the hold the younger create claims the slot first (see
    // `due_child_creation_claims_last_slot_before_older_intent_resume`); the
    // load holds it, and the older lead recovery is not held.
    let claim = claim_like_the_loop(&p.manager)
        .await
        .expect("lead recovery is admitted under load");
    assert_eq!(claim.id(), recovery.operation_id);
    assert!(
        claim_like_the_loop(&p.manager).await.is_none(),
        "the create stays held"
    );
    assert_eq!(
        p.receipt(child.operation_id).await.state,
        ManagerActionStateV2::Queued
    );
    let held = view(&p, child.operation_id).await.held.unwrap();
    assert_eq!(held.reason, "host_load");
    // The recovery's own receipt carries no host-load hold.
    assert_eq!(view(&p, recovery.operation_id).await.held, None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_deploy_hold_is_reported_before_the_host_load_hold() {
    let (p, _recovery, child) = queued_last_slot_actions(SessionProvider::Claude, 1).await;
    host_load(&p, 88.0);
    let live = hold_deploy_until(&p, chrono::Utc::now() + chrono::Duration::seconds(600));
    let held = view(&p, child.operation_id).await.held.unwrap();
    assert_eq!(held.reason, crate::deploy_drain::DEPLOY_DRAINING);
    assert_eq!(held.deploy_id, Some(live.id));
    assert_eq!(held.load, None);
    // Once the deploy hold ends the host-load hold is what remains.
    p.manager
        .deploy_drain()
        .sync(None, true, chrono::Utc::now());
    let held = view(&p, child.operation_id).await.held.unwrap();
    assert_eq!(held.reason, "host_load");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn recent_launches_count_toward_the_load_so_a_backlog_is_not_released_at_once() {
    let p = pilot().await;
    // 39 of 40: one launch fits, the next waits for the average to absorb it.
    host_load(&p, 38.5);
    let gate = p.manager.host_load();
    assert!(gate.hold_now().is_none());
    gate.note_admitted();
    assert!(gate.hold_now().is_none(), "38.5 + 1 is within 40");
    gate.note_admitted();
    let hold = gate.hold_now().expect("38.5 + 2 is above 40");
    assert_eq!(hold.recent_admissions, 2);
    p.admit("backlog-1", create(p.epic, &p, "backlog one"))
        .await;
    assert!(
        claim_like_the_loop(&p.manager).await.is_none(),
        "the backlog waits while recent launches are unabsorbed"
    );
}

/// #407: disk holds immediately request one serialized, real reclaim pass;
/// retry ticks and another manager's hold share the same debounce. The roster
/// exposes queued launches before they have session rows, within its scope.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn disk_held_creates_request_one_reclaim_and_appear_in_roster() {
    use crate::host_pressure::PressureHold;
    use rsi_common::agent_worker_roster::AgentWorkerRosterRequestV1;

    let p = pilot().await;
    set_threshold(&p, 0);
    p.manager.host_load().set_pressure_source(Arc::new(|| {
        Some(PressureHold::DiskPressure {
            free_gb: 8.0,
            min_gb: 30,
        })
    }));
    let first = p
        .admit("disk-held", create(p.epic, &p, "held worker"))
        .await;
    p.manager.reconcile_manager_actions_once().await.unwrap();
    assert_eq!(
        p.manager
            .launch_hold_reclaim
            .submissions
            .load(Ordering::SeqCst),
        1
    );

    // Another manager's eligible hold must use the same daemon-wide debounce.
    let other = second_project(&p).await;
    let second = p
        .manager
        .agent_control()
        .agent_manager_control(
            other.owner,
            p.request(
                "second-disk-held",
                ManagerActionV2::CreateSession {
                    parent_id: other.epic,
                    kind: SessionKind::Task,
                    query: "another held worker".into(),
                    launch: other.policy.allowed_launches[0].clone(),
                    sandbox_source: None,
                },
            ),
        )
        .await
        .unwrap();
    // Ask about this second hold explicitly: the store may memoize the oldest.
    assert!(!p.manager.admit_manager_create(
        second.operation_id,
        chrono::Utc::now(),
        second.target_session_id
    ));
    p.manager.reconcile_manager_actions_once().await.unwrap();
    assert_eq!(
        p.manager
            .launch_hold_reclaim
            .submissions
            .load(Ordering::SeqCst),
        1
    );

    let roster = p
        .manager
        .agent_control()
        .agent_worker_roster(p.owner, AgentWorkerRosterRequestV1::default())
        .await
        .unwrap();
    assert_eq!(roster.queued_launches.len(), 1);
    assert_eq!(roster.queued_launches[0].operation_id, first.operation_id);
    assert_eq!(
        roster.queued_launches[0].held_reason.as_deref(),
        Some("disk_pressure")
    );
    assert_eq!(
        roster.queued_launches[0].target_session_id,
        first.target_session_id
    );

    // The same queued row is reported without a hold when space recovers.
    p.manager.host_load().set_pressure_source(Arc::new(|| None));
    assert!(p.manager.admit_manager_create(
        first.operation_id,
        chrono::Utc::now() - chrono::Duration::days(1),
        first.target_session_id
    ));
    assert!(p.manager.admit_manager_create(
        second.operation_id,
        chrono::Utc::now() - chrono::Duration::days(1),
        second.target_session_id
    ));
    let roster = p
        .manager
        .agent_control()
        .agent_worker_roster(p.owner, AgentWorkerRosterRequestV1::default())
        .await
        .unwrap();
    assert_eq!(roster.queued_launches[0].held_reason, None);
}

/// #2920: a create the sandbox allocator refuses for free space is held, not
/// blocked as `manager_v2_lifecycle_unconfirmed`. It stays queued with
/// `held {reason: sandbox_free_space, free_disk_gb, min_free_disk_gb}`, no pass
/// retries it while the hold stands, and it launches on its own once the hold
/// ends.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sandbox_free_space_refusal_holds_the_create_and_resumes_on_its_own() {
    use crate::sandbox::SandboxCapacity;
    let p = pilot().await;
    set_threshold(&p, 0);
    p.manager
        .runtime_config
        .sandbox_min_free_gib
        .store(30, Ordering::Relaxed);
    let low = SandboxCapacity {
        source_roots: 0,
        available_bytes: 8 << 30,
        total_bytes: u64::MAX,
    };
    // The allocator measures twice: before and after its one reclaim page.
    p.manager
        .sandbox_allocation_test_capacities
        .lock()
        .unwrap()
        .extend([low, low]);
    let receipt = p
        .admit("disk-full", create(p.epic, &p, "worker on a full disk"))
        .await;
    let child = receipt.target_session_id.unwrap();

    p.manager.reconcile_manager_actions_once().await.unwrap();
    let held = p.receipt(receipt.operation_id).await;
    assert_eq!(
        held.state,
        ManagerActionStateV2::Queued,
        "held, never blocked: {held:?}"
    );
    let reason = view(&p, receipt.operation_id).await.held.unwrap();
    assert_eq!(reason.reason, "sandbox_free_space");
    let roster = p
        .manager
        .agent_control()
        .agent_worker_roster(
            p.owner,
            rsi_common::agent_worker_roster::AgentWorkerRosterRequestV1::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        roster.queued_launches[0].held_reason.as_deref(),
        Some("sandbox_free_space")
    );
    assert_eq!(reason.free_disk_gb, Some(8.0));
    assert_eq!(reason.min_free_disk_gb, Some(30));
    assert_eq!(reason.load, None);
    assert_eq!(
        p.manager
            .launch_hold_reclaim
            .submissions
            .load(Ordering::SeqCst),
        1
    );

    // A later pass leaves it alone while the hold stands.
    p.manager.reconcile_manager_actions_once().await.unwrap();
    assert_eq!(
        p.manager
            .launch_hold_reclaim
            .submissions
            .load(Ordering::SeqCst),
        1
    );
    assert_eq!(
        p.receipt(receipt.operation_id).await.state,
        ManagerActionStateV2::Queued
    );

    // The hold ends (space returned or the retry delay passed): it launches.
    p.manager.host_load().expire_sandbox_free_space_hold();
    assert_eq!(view(&p, receipt.operation_id).await.held, None);
    let process = super::super::super::launch::install_controller_candidate_test_process(child);
    p.manager.reconcile_manager_actions_once().await.unwrap();
    let settled = p.receipt(receipt.operation_id).await;
    assert_eq!(
        settled.state,
        ManagerActionStateV2::Succeeded,
        "{settled:?}"
    );
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
}

/// #2920: a root count at `sandbox_max_source_roots` is not a transient
/// shortage. It still refuses, with its own typed code and no hold, never the
/// `manager_v2_lifecycle_unconfirmed` catch-all.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sandbox_root_cap_refusal_keeps_its_own_typed_code() {
    use crate::sandbox::SandboxCapacity;
    let p = pilot().await;
    set_threshold(&p, 0);
    p.manager
        .runtime_config
        .sandbox_max_source_roots
        .store(1, Ordering::Relaxed);
    let full = SandboxCapacity {
        source_roots: 1,
        available_bytes: u64::MAX,
        total_bytes: u64::MAX,
    };
    p.manager
        .sandbox_allocation_test_capacities
        .lock()
        .unwrap()
        .extend([full, full]);
    let receipt = p
        .admit("root-cap", create(p.epic, &p, "worker at the root cap"))
        .await;

    p.manager.reconcile_manager_actions_once().await.unwrap();
    let settled = p.receipt(receipt.operation_id).await;
    assert_eq!(settled.state, ManagerActionStateV2::Blocked, "{settled:?}");
    assert_eq!(
        settled.outcome.as_deref(),
        Some("manager_v2_sandbox_source_root_limit")
    );
    assert!(
        p.manager.host_load().pressure_now().is_none(),
        "a root-count refusal is no free-space hold"
    );
}
