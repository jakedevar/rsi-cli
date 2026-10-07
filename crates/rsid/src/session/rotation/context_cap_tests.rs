//! #1005: an idle Epic lead past its coordinator context cap is rotated once
//! by the daemon; the successor holds the lead and starts from the
//! daemon-written handoff, which validates strict and names the open work.
#![allow(
    clippy::expect_used,
    clippy::significant_drop_tightening,
    clippy::large_futures
)]

use super::super::context_cap::{
    CapAction, CapCrossing, CapState, cap_rotation, evaluate_cap_crossing, plan_cap_actions,
    process_boot_id,
};
use super::super::launch::{
    drop_controller_candidate_test_stream, install_controller_candidate_test_process,
};
use super::tests::{
    reserve_and_bind_live_successor_for_test, rotation_manager_on,
    rotation_manager_with_context_rotation, seat_holders, seated_live_parent, test_session,
};
use super::*;
use rsi_common::daemon_handoff::{CoordinatorSeatV1, parse_strict};
use rsi_common::types::{Project, SessionKind};
use std::time::Duration;

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capped_idle_lead_rotates_once_to_a_successor_holding_the_lead() -> anyhow::Result<()> {
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    manager
        .runtime_config
        .context_rotation_enabled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    set_cap(&manager);
    let project = Project {
        id: Uuid::new_v4(),
        name: "Context cap".into(),
        path: None,
        description: None,
        color: Project::DEFAULT_COLOR.into(),
        context_files: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let mut lead = test_session(Uuid::new_v4(), SessionStatus::Completed);
    lead.working_dir = dir.path().to_path_buf();
    lead.project_id = Some(project.id);
    lead.query = "Lead the Recovery Epic".into();
    let mut epic = test_session(Uuid::new_v4(), SessionStatus::Completed);
    epic.working_dir = dir.path().to_path_buf();
    epic.project_id = Some(project.id);
    epic.session_kind = SessionKind::Epic;
    epic.lead_session_id = Some(lead.id);
    let mut child = test_session(Uuid::new_v4(), SessionStatus::Running);
    child.working_dir = dir.path().to_path_buf();
    child.project_id = Some(project.id);
    child.parent_id = Some(epic.id);
    {
        let mut store = manager.store.lock().await;
        store.insert_project(&project)?;
        store.insert_session(&lead)?;
        store.insert_session(&epic)?;
        store.insert_session(&child)?;
        store.publish_startup_ordinary(lead.id)?;
        store.conn.execute(
            "INSERT INTO issues(id,project_id,display_number,title,status,created_at,updated_at)
             VALUES(?1,?2,9100005,'Daemon-written handoffs','InProgress',?3,?3)",
            rusqlite::params![
                Uuid::new_v4().to_string(),
                project.id.to_string(),
                "2026-10-02T12:00:00.000000000Z"
            ],
        )?;
        assert_eq!(
            evaluate_cap_crossing(
                &store,
                &lead,
                210_000,
                Some(200_000),
                None,
                chrono::Utc::now()
            )?,
            CapCrossing::Recorded(true)
        );
    }
    manager
        .completed
        .write()
        .await
        .insert(lead.id, CompletedSession::for_test(lead.clone()));
    let successor = Uuid::new_v4();
    install_rotation_child_id_for_test(lead.id, successor);
    install_controller_candidate_test_process(successor);

    assert_eq!(manager.rotate_capped_coordinators().await?, 1);
    // The decider runs in a background task and publishes the successor (the
    // Epic lead moves in that commit): the only thing to wait for.
    wait_until("the successor to hold the lead", || async {
        epic_lead(&manager, epic.id).await == Some(successor)
    })
    .await;

    let row = manager
        .store
        .lock()
        .await
        .get_session(successor)?
        .expect("successor row");
    assert_eq!(row.continued_from, Some(lead.id));
    assert!(row.query.contains("source=\"context-cap-handoff\""));
    let start = row.query.find("---\ndate:").expect("handoff document");
    let end = row
        .query
        .rfind("\n</rsid-daemon-message>")
        .expect("envelope end");
    let handoff = parse_strict(&row.query[start..end])
        .map_err(|error| anyhow::anyhow!("handoff does not validate strict: {error}"))?;
    assert_eq!(handoff.predecessor_session_id, lead.id);
    assert_eq!(
        handoff.seat,
        CoordinatorSeatV1::EpicLead { epic_id: epic.id }
    );
    assert_eq!(handoff.original_task, "Lead the Recovery Epic");
    assert_eq!(
        handoff.issues_in_progress[0].title,
        "Daemon-written handoffs"
    );
    assert_eq!(handoff.live_children[0].session_id, child.id);

    // The publication is durable, so the next pass settles the rotation on the
    // successor and starts nothing.
    assert_eq!(
        run_cap_passes(&manager, process_boot_id(), lead.id, 2).await,
        CapState::Rotated
    );
    assert_eq!(
        cap_rotation(&*manager.store.lock().await, lead.id)?
            .expect("record")
            .successor,
        Some(successor)
    );
    assert_eq!(manager.rotate_capped_coordinators().await?, 0);
    drop_controller_candidate_test_stream(successor);
    Ok(())
}

/// The operator's cap setting; every test sets it, none relies on a default.
fn set_cap(manager: &SessionManager) {
    manager
        .runtime_config
        .update_field(
            "coordinator_context_cap_tokens",
            &serde_json::json!(200_000),
        )
        .expect("cap setting");
}

struct CapFixture {
    lead: Session,
    epic: Session,
}

/// An idle Epic lead past the cap with one running child: its crossing is
/// recorded (`Due`) and it sits in the completed map.
async fn lead_cap_fixture(manager: &SessionManager, dir: &tempfile::TempDir) -> CapFixture {
    manager
        .runtime_config
        .context_rotation_enabled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    set_cap(manager);
    let project = Project {
        id: Uuid::new_v4(),
        name: "Context cap".into(),
        path: None,
        description: None,
        color: Project::DEFAULT_COLOR.into(),
        context_files: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let mut lead = test_session(Uuid::new_v4(), SessionStatus::Completed);
    lead.working_dir = dir.path().to_path_buf();
    lead.project_id = Some(project.id);
    lead.query = "Lead the Recovery Epic".into();
    let mut epic = test_session(Uuid::new_v4(), SessionStatus::Completed);
    epic.working_dir = dir.path().to_path_buf();
    epic.project_id = Some(project.id);
    epic.session_kind = SessionKind::Epic;
    epic.lead_session_id = Some(lead.id);
    let mut child = test_session(Uuid::new_v4(), SessionStatus::Running);
    child.working_dir = dir.path().to_path_buf();
    child.project_id = Some(project.id);
    child.parent_id = Some(epic.id);
    {
        let mut store = manager.store.lock().await;
        store.insert_project(&project).expect("project");
        store.insert_session(&lead).expect("lead");
        store.insert_session(&epic).expect("epic");
        store.insert_session(&child).expect("child");
        store.publish_startup_ordinary(lead.id).expect("custody");
        assert_eq!(
            evaluate_cap_crossing(
                &store,
                &lead,
                210_000,
                Some(200_000),
                None,
                chrono::Utc::now()
            )
            .expect("crossing"),
            CapCrossing::Recorded(true)
        );
    }
    manager
        .completed
        .write()
        .await
        .insert(lead.id, CompletedSession::for_test(lead.clone()));
    CapFixture { lead, epic }
}

/// Drive the cap pass `passes` times as `boot` and return the request's
/// state. Recovery and publication are synchronous and durable before the
/// pass returns, so no wall-clock wait is involved.
async fn run_cap_passes(
    manager: &SessionManager,
    boot: &str,
    seat: Uuid,
    passes: usize,
) -> CapState {
    for _ in 0..passes {
        manager
            .rotate_capped_coordinators_for_boot(boot)
            .await
            .expect("pass");
    }
    cap_rotation(&*manager.store.lock().await, seat)
        .expect("record")
        .expect("present")
        .state
}

/// The pass's planning half, as `boot` (the idle snapshot is "everything").
async fn plan_now(manager: &SessionManager, boot: &str) -> Vec<CapAction> {
    let store = manager.store.lock().await;
    plan_cap_actions(
        &store,
        chrono::Utc::now(),
        boot,
        |_| true,
        |_| Some(200_000),
    )
    .expect("plan")
}

async fn wait_until<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    // A failure ceiling only: every wait here is for a background task that
    // finishes in milliseconds, so a slow machine must not fail the test.
    tokio::time::timeout(Duration::from_secs(60), async {
        while !check().await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn successors_of(store: &crate::store::Store, predecessor: Uuid) -> i64 {
    store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE continued_from=?1",
            [predecessor.to_string()],
            |row| row.get(0),
        )
        .expect("successor count")
}

fn events_of(store: &crate::store::Store, session: Uuid, like: &str) -> i64 {
    store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM rotation_events WHERE session_id=?1 AND event_type LIKE ?2",
            rusqlite::params![session.to_string(), like],
            |row| row.get(0),
        )
        .expect("event count")
}

async fn epic_lead(manager: &SessionManager, epic: Uuid) -> Option<Uuid> {
    manager
        .store
        .lock()
        .await
        .get_session(epic)
        .ok()
        .flatten()
        .and_then(|epic| epic.lead_session_id)
}

/// #1142 F1: the pass observed the lead idle; a continuation started before
/// the dispatch. The automatic trigger must defer: it never takes the
/// running/manual-trigger branch, so the new turn is not interrupted.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cap_dispatch_defers_a_seat_that_resumed_instead_of_interrupting_it() -> anyhow::Result<()>
{
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    let fx = lead_cap_fixture(&manager, &dir).await;
    let actions = plan_now(&manager, "boot-a").await;
    assert!(matches!(actions.as_slice(), [CapAction::Rotate { .. }]));

    // A continuation starts after the idle snapshot, before the dispatch.
    manager.completed.write().await.remove(&fx.lead.id);
    let mut running = fx.lead.clone();
    running.status = SessionStatus::Running;
    manager
        .active
        .write()
        .await
        .insert(fx.lead.id, TrackedSession::new_for_test(running));

    assert_eq!(manager.dispatch_cap_actions(actions).await?, 0);
    {
        let active = manager.active.read().await;
        let tracked = active.get(&fx.lead.id).expect("the turn is still running");
        assert!(
            !tracked.rotation.is_rotating(),
            "no rotation was forced on it"
        );
        assert!(tracked.rotation.rotation_id().is_none());
        assert_eq!(tracked.session.status, SessionStatus::Running);
    }
    {
        let store = manager.store.lock().await;
        let record = cap_rotation(&store, fx.lead.id)?.expect("record");
        assert_eq!(record.state, CapState::Due);
        assert_eq!(record.reason.as_deref(), Some("seat_resumed"));
        assert_eq!(record.attempts, 0, "a deferral is not a failed attempt");
        assert_eq!(
            events_of(&store, fx.lead.id, "%"),
            0,
            "no rotation was started"
        );
        assert_eq!(successors_of(&store, fx.lead.id), 0);
    }

    // The turn ends; the next idle boundary rotates the seat, once.
    manager.active.write().await.remove(&fx.lead.id);
    manager
        .completed
        .write()
        .await
        .insert(fx.lead.id, CompletedSession::for_test(fx.lead.clone()));
    let successor = Uuid::new_v4();
    install_rotation_child_id_for_test(fx.lead.id, successor);
    install_controller_candidate_test_process(successor);
    assert_eq!(manager.rotate_capped_coordinators().await?, 1);
    wait_until("the successor to hold the lead", || async {
        epic_lead(&manager, fx.epic.id).await == Some(successor)
    })
    .await;
    assert_eq!(successors_of(&*manager.store.lock().await, fx.lead.id), 1);
    drop_controller_candidate_test_stream(successor);
    Ok(())
}

/// #1142 F1: a continuation that started AND finished between the idle
/// snapshot and the decider leaves the seat in the completed map with a new
/// row version. The decider, under the predecessor's spawn guard, refuses to
/// rotate that incarnation: deferred, nothing removed, nothing refused.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn decider_defers_a_seat_whose_idle_incarnation_changed() -> anyhow::Result<()> {
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    let fx = lead_cap_fixture(&manager, &dir).await;
    let actions = plan_now(&manager, "boot-a").await;
    assert!(matches!(actions.as_slice(), [CapAction::Rotate { .. }]));

    // The continuation's first durable write: the seat's row version moved.
    manager.store.lock().await.conn.execute(
        "UPDATE sessions SET updated_at=?2 WHERE id=?1",
        rusqlite::params![
            fx.lead.id.to_string(),
            (chrono::Utc::now() + chrono::Duration::seconds(5))
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
        ],
    )?;
    assert_eq!(manager.dispatch_cap_actions(actions).await?, 1);
    wait_until("the decider to defer the request", || async {
        cap_rotation(&*manager.store.lock().await, fx.lead.id)
            .ok()
            .flatten()
            .is_some_and(|record| record.state == CapState::Due)
    })
    .await;
    {
        let store = manager.store.lock().await;
        let record = cap_rotation(&store, fx.lead.id)?.expect("record");
        assert_eq!(record.reason.as_deref(), Some("seat_changed"));
        assert_eq!(successors_of(&store, fx.lead.id), 0);
        // The deferral closes the request's intent and records no other refusal.
        assert_eq!(events_of(&store, fx.lead.id, "refused:cap_deferred"), 1);
        assert_eq!(events_of(&store, fx.lead.id, "refused:%"), 1);
    }
    assert!(
        manager.completed.read().await.contains_key(&fx.lead.id),
        "the idle seat was left where it was"
    );

    // Planned afresh against the new incarnation, it rotates.
    let successor = Uuid::new_v4();
    install_rotation_child_id_for_test(fx.lead.id, successor);
    install_controller_candidate_test_process(successor);
    assert_eq!(manager.rotate_capped_coordinators().await?, 1);
    wait_until("the successor to hold the lead", || async {
        epic_lead(&manager, fx.epic.id).await == Some(successor)
    })
    .await;
    assert_eq!(successors_of(&*manager.store.lock().await, fx.lead.id), 1);
    drop_controller_candidate_test_stream(successor);
    Ok(())
}

/// #1142 F2: the daemon died after the handoff and `Rotating` marker were
/// written and before the dispatch. The next boot dispatches the request once
/// under its identity: one successor, never a second.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_before_dispatch_is_reconciled_by_the_next_boot_with_one_successor()
-> anyhow::Result<()> {
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    let fx = lead_cap_fixture(&manager, &dir).await;
    let planned = plan_now(&manager, "boot-dead").await;
    let [CapAction::Rotate { rotation_id, .. }] = planned.as_slice() else {
        panic!("one rotation expected: {planned:?}");
    };
    let rotation_id = rotation_id.clone();
    drop(planned); // the process died here

    let successor = Uuid::new_v4();
    install_rotation_child_id_for_test(fx.lead.id, successor);
    install_controller_candidate_test_process(successor);
    assert_eq!(
        manager
            .rotate_capped_coordinators_for_boot("boot-alive")
            .await?,
        1
    );
    wait_until("the successor to hold the lead", || async {
        epic_lead(&manager, fx.epic.id).await == Some(successor)
    })
    .await;
    wait_until("the rotation to settle", || async {
        manager
            .rotate_capped_coordinators_for_boot("boot-alive")
            .await
            .expect("pass");
        cap_rotation(&*manager.store.lock().await, fx.lead.id)
            .ok()
            .flatten()
            .is_some_and(|record| record.state == CapState::Rotated)
    })
    .await;
    let store = manager.store.lock().await;
    assert_eq!(
        successors_of(&store, fx.lead.id),
        1,
        "never a second successor"
    );
    let record = cap_rotation(&store, fx.lead.id)?.expect("record");
    assert_eq!(record.rotation_id.as_deref(), Some(rotation_id.as_str()));
    assert_eq!(record.successor, Some(successor));
    let published: i64 = store.conn.query_row(
        "SELECT COUNT(*) FROM rotation_events WHERE session_id=?1 AND rotation_id=?2 AND event_type='completed'",
        rusqlite::params![fx.lead.id.to_string(), rotation_id],
        |row| row.get(0),
    )?;
    assert_eq!(
        published, 1,
        "the request's own identity published the successor"
    );
    drop(store);
    drop_controller_candidate_test_stream(successor);
    Ok(())
}

/// #1142 F2: the daemon died after the trigger was recorded and before any
/// successor was reserved. The next boot replays the same request: its
/// trigger is not logged twice and exactly one successor results.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_before_reservation_is_replayed_once_under_the_same_identity() -> anyhow::Result<()> {
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    let fx = lead_cap_fixture(&manager, &dir).await;
    let planned = plan_now(&manager, "boot-dead").await;
    let [CapAction::Rotate { rotation_id, .. }] = planned.as_slice() else {
        panic!("one rotation expected: {planned:?}");
    };
    manager.store.lock().await.insert_rotation_event(
        fx.lead.id,
        rotation_id,
        "completed",
        "cap_triggered",
        None,
    )?;
    let rotation_id = rotation_id.clone();
    drop(planned);

    let successor = Uuid::new_v4();
    install_rotation_child_id_for_test(fx.lead.id, successor);
    install_controller_candidate_test_process(successor);
    assert_eq!(
        manager
            .rotate_capped_coordinators_for_boot("boot-alive")
            .await?,
        1
    );
    wait_until("the successor to hold the lead", || async {
        epic_lead(&manager, fx.epic.id).await == Some(successor)
    })
    .await;
    let store = manager.store.lock().await;
    assert_eq!(successors_of(&store, fx.lead.id), 1);
    let triggers: i64 = store.conn.query_row(
        "SELECT COUNT(*) FROM rotation_events WHERE session_id=?1 AND rotation_id=?2 AND event_type='cap_triggered'",
        rusqlite::params![fx.lead.id.to_string(), rotation_id],
        |row| row.get(0),
    )?;
    assert_eq!(triggers, 1, "the replay does not log a second trigger");
    drop(store);
    drop_controller_candidate_test_stream(successor);
    Ok(())
}

/// The global manager seat past its cap: grant configured, crossing recorded
/// (`Due`), the seat idle in the completed map.
async fn global_cap_fixture(
    manager: &SessionManager,
    dir: &tempfile::TempDir,
) -> (Session, rsi_common::global_manager::GlobalManagerGrantV1) {
    use rsi_common::global_manager::ConfigureGlobalManagerRequestV1;
    use rsi_common::harness_manager_v2::{
        ManagerCapabilityV2, ManagerLaunchChoiceV2, ManagerOperatingModeV2, ManagerPolicyV2,
    };
    use rsi_common::types::SessionProvider;
    manager
        .runtime_config
        .context_rotation_enabled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    set_cap(manager);
    let project = Project {
        id: Uuid::new_v4(),
        name: "Global cap".into(),
        path: None,
        description: None,
        color: Project::DEFAULT_COLOR.into(),
        context_files: None,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    let mut seat = test_session(Uuid::new_v4(), SessionStatus::Completed);
    seat.working_dir = dir.path().to_path_buf();
    seat.project_id = Some(project.id);
    seat.query = "Be the global manager".into();
    let grant = {
        let mut store = manager.store.lock().await;
        store.insert_project(&project).expect("project");
        store.insert_session(&seat).expect("seat");
        store.publish_startup_ordinary(seat.id).expect("custody");
        let grant = store
            .configure_global_manager(
                &ConfigureGlobalManagerRequestV1 {
                    session_id: seat.id,
                    project_ids: vec![project.id],
                    allowed_launches: vec![ManagerLaunchChoiceV2 {
                        provider: SessionProvider::Claude,
                        model: "claude-opus-5-5".into(),
                        effort: Some("high".into()),
                    }],
                    project_policy: ManagerPolicyV2 {
                        mode: ManagerOperatingModeV2::Execute,
                        capabilities: vec![ManagerCapabilityV2::WorkPlan],
                        ..ManagerPolicyV2::default()
                    },
                    expected_grant_version: 0,
                    idempotency_key: "grant-1142-e2e".into(),
                },
                "operator:test",
            )
            .expect("grant");
        assert_eq!(
            evaluate_cap_crossing(
                &store,
                &seat,
                210_000,
                Some(200_000),
                None,
                chrono::Utc::now()
            )
            .expect("crossing"),
            CapCrossing::Recorded(true)
        );
        grant
    };
    manager
        .completed
        .write()
        .await
        .insert(seat.id, CompletedSession::for_test(seat.clone()));
    (seat, grant)
}

async fn assert_grant_moved(
    manager: &SessionManager,
    successor: Uuid,
    original: &rsi_common::global_manager::GlobalManagerGrantV1,
) {
    let moved = manager
        .store
        .lock()
        .await
        .active_global_grant()
        .expect("grant read")
        .expect("active grant");
    assert_eq!(moved.seat_session_id, successor);
    assert_eq!(moved.grant_version, original.grant_version + 1);
    assert_eq!(moved.project_ids, original.project_ids);
    assert_eq!(moved.allowed_launches, original.allowed_launches);
    assert_eq!(moved.project_policy, original.project_policy);
    assert_eq!(moved.operator_origin, original.operator_origin);
}

/// #1142 F3: the global manager grant moves in the successor's publication
/// transaction. It is on the successor as soon as the successor is
/// published, with no later cap pass needed; the pass only records `Rotated`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn global_grant_moves_with_the_successors_publication() -> anyhow::Result<()> {
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    let (seat, grant) = global_cap_fixture(&manager, &dir).await;
    let successor = Uuid::new_v4();
    install_rotation_child_id_for_test(seat.id, successor);
    install_controller_candidate_test_process(successor);

    assert_eq!(manager.rotate_capped_coordinators().await?, 1);
    // No further cap pass runs: the grant is on the successor from the
    // publication commit itself.
    wait_until("the grant to move with the publication", || async {
        manager
            .store
            .lock()
            .await
            .active_global_grant()
            .ok()
            .flatten()
            .is_some_and(|grant| grant.seat_session_id == successor)
    })
    .await;
    assert_grant_moved(&manager, successor, &grant).await;
    // The next pass records the settlement and moves nothing further.
    wait_until("the rotation to settle", || async {
        manager.rotate_capped_coordinators().await.expect("pass");
        cap_rotation(&*manager.store.lock().await, seat.id)
            .ok()
            .flatten()
            .is_some_and(|record| record.state == CapState::Rotated)
    })
    .await;
    assert_grant_moved(&manager, successor, &grant).await;
    drop_controller_candidate_test_stream(successor);
    Ok(())
}

/// #1142 R2/R3: the daemon died after the global seat's successor finished its
/// first turn and before publication. The next boot publishes that exact
/// candidate; the grant moves with that publication, once, in its scope.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_before_publication_moves_the_global_grant_with_the_recovered_publication()
-> anyhow::Result<()> {
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    let (seat, grant) = global_cap_fixture(&manager, &dir).await;
    let planned = plan_now(&manager, "boot-dead").await;
    let [CapAction::Rotate { rotation_id, .. }] = planned.as_slice() else {
        panic!("one rotation expected: {planned:?}");
    };
    let fx = CapFixture {
        lead: seat.clone(),
        epic: seat.clone(),
    };
    let candidate =
        reserve_successor(&manager, &fx, rotation_id, &dir, SessionStatus::Completed).await;
    drop(planned);
    drop(manager); // the daemon dies

    let restarted = restarted_under(&dir);
    restarted.restore_sessions().await?;
    // Until the cap pass recovers the candidate, the grant is on the seat.
    assert_eq!(
        restarted
            .store
            .lock()
            .await
            .active_global_grant()?
            .expect("grant")
            .seat_session_id,
        seat.id
    );
    assert_eq!(
        run_cap_passes(&restarted, "boot-alive", seat.id, 3).await,
        CapState::Rotated,
        "the publication is durable, so the next pass records the settlement"
    );
    assert_grant_moved(&restarted, candidate.id, &grant).await;
    let store = restarted.store.lock().await;
    assert_eq!(successors_of(&store, seat.id), 1);
    assert_eq!(events_of(&store, seat.id, "completed"), 1);
    Ok(())
}

/// Release the dispatch, wait for the decider to put the request back to due
/// with `reason`, and prove it started nothing.
async fn assert_deferred(manager: &SessionManager, lead: Uuid, reason: &str) {
    wait_until("the decider to defer the request", || async {
        cap_rotation(&*manager.store.lock().await, lead)
            .ok()
            .flatten()
            .is_some_and(|record| record.state == CapState::Due)
    })
    .await;
    let store = manager.store.lock().await;
    let record = cap_rotation(&store, lead)
        .expect("record")
        .expect("present");
    assert_eq!(record.reason.as_deref(), Some(reason));
    assert_eq!(successors_of(&store, lead), 0, "no successor was allocated");
    // #1149: the deferral closes the request's intent with its own terminal
    // event, and records no other refusal.
    assert_eq!(events_of(&store, lead, "refused:cap_deferred"), 1);
    assert_eq!(events_of(&store, lead, "refused:%"), 1);
    assert!(
        manager.completed.read().await.contains_key(&lead),
        "the idle seat was left where it was"
    );
}

async fn rotates_once_released(manager: &SessionManager, fx: &CapFixture) {
    let successor = Uuid::new_v4();
    install_rotation_child_id_for_test(fx.lead.id, successor);
    install_controller_candidate_test_process(successor);
    assert_eq!(manager.rotate_capped_coordinators().await.expect("pass"), 1);
    wait_until("the successor to hold the lead", || async {
        epic_lead(manager, fx.epic.id).await == Some(successor)
    })
    .await;
    assert_eq!(successors_of(&*manager.store.lock().await, fx.lead.id), 1);
    drop_controller_candidate_test_stream(successor);
}

/// #1142 R1: the operator pauses the seat (soft or hard) after the pass
/// planned its rotation. The decider, under the predecessor's spawn guard,
/// defers: nothing is reserved, the pause is untouched, and the intent is
/// retained until the operator lifts it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn operator_pause_after_planning_defers_the_cap_rotation() -> anyhow::Result<()> {
    use crate::store::manager_actions::OperatorPause;
    for pause in [OperatorPause::Soft, OperatorPause::Hard] {
        let (manager, dir) = rotation_manager_with_context_rotation(true);
        let fx = lead_cap_fixture(&manager, &dir).await;
        let actions = plan_now(&manager, "boot-a").await;
        assert!(matches!(actions.as_slice(), [CapAction::Rotate { .. }]));
        manager
            .store
            .lock()
            .await
            .set_operator_pause(fx.lead.id, pause)?;
        assert_eq!(manager.dispatch_cap_actions(actions).await?, 1);
        assert_deferred(&manager, fx.lead.id, "operator_paused").await;
        assert_eq!(
            manager.store.lock().await.get_operator_pause(fx.lead.id)?,
            pause,
            "the cap never clears the operator's pause"
        );
        // While paused the seat is not planned again.
        assert_eq!(manager.rotate_capped_coordinators().await?, 0);
        assert_eq!(
            cap_rotation(&*manager.store.lock().await, fx.lead.id)?
                .expect("record")
                .state,
            CapState::Due
        );
        manager
            .store
            .lock()
            .await
            .set_operator_pause(fx.lead.id, OperatorPause::None)?;
        rotates_once_released(&manager, &fx).await;
    }
    Ok(())
}

/// #1142 R1: the operator disables rotation of the seat (the real
/// `toggle_rotation_disabled`) after planning. The decider defers without
/// re-enabling it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rotation_disabled_after_planning_defers_the_cap_rotation() -> anyhow::Result<()> {
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    let fx = lead_cap_fixture(&manager, &dir).await;
    let actions = plan_now(&manager, "boot-a").await;
    assert!(matches!(actions.as_slice(), [CapAction::Rotate { .. }]));
    assert!(
        manager
            .toggle_rotation_disabled(fx.lead.id)
            .await?
            .is_some()
    );
    assert_eq!(manager.dispatch_cap_actions(actions).await?, 1);
    assert_deferred(&manager, fx.lead.id, "rotation_disabled").await;
    assert!(
        manager
            .store
            .lock()
            .await
            .get_session(fx.lead.id)?
            .expect("lead row")
            .rotation_disabled_at
            .is_some(),
        "the cap never re-enables rotation"
    );
    assert_eq!(manager.rotate_capped_coordinators().await?, 0);
    // The operator enables rotation again: the seat rotates once.
    assert!(
        manager
            .toggle_rotation_disabled(fx.lead.id)
            .await?
            .is_none()
    );
    rotates_once_released(&manager, &fx).await;
    Ok(())
}

/// #1142 R1: the operator paused the seat and disabled its rotation while the
/// daemon was down. The replayed request (next boot) is deferred, not run.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_and_disable_between_boots_defer_the_replayed_request() -> anyhow::Result<()> {
    use crate::store::manager_actions::OperatorPause;
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    let fx = lead_cap_fixture(&manager, &dir).await;
    drop(plan_now(&manager, "boot-dead").await); // the process died after planning
    manager
        .store
        .lock()
        .await
        .set_operator_pause(fx.lead.id, OperatorPause::Soft)?;
    assert!(
        manager
            .toggle_rotation_disabled(fx.lead.id)
            .await?
            .is_some()
    );
    assert_eq!(
        manager
            .rotate_capped_coordinators_for_boot("boot-alive")
            .await?,
        1
    );
    assert_deferred(&manager, fx.lead.id, "rotation_disabled").await;
    // Re-enabled but still paused: deferred for the pause.
    assert!(
        manager
            .toggle_rotation_disabled(fx.lead.id)
            .await?
            .is_none()
    );
    assert_eq!(manager.rotate_capped_coordinators().await?, 0);
    manager
        .store
        .lock()
        .await
        .set_operator_pause(fx.lead.id, OperatorPause::None)?;
    rotates_once_released(&manager, &fx).await;
    Ok(())
}

/// A reserved successor row the way `insert_reserved_rotation_session...`
/// leaves it: `continued_from` the seat, with the reservation marker under
/// the request's identity.
async fn reserve_successor(
    manager: &SessionManager,
    fx: &CapFixture,
    rotation_id: &str,
    dir: &tempfile::TempDir,
    status: SessionStatus,
) -> Session {
    let mut child = test_session(Uuid::new_v4(), status);
    child.working_dir = dir.path().to_path_buf();
    child.project_id = fx.lead.project_id;
    child.parent_id = fx.lead.parent_id;
    child.continued_from = Some(fx.lead.id);
    child.rotation_depth = 1;
    child.created_at = chrono::Utc::now() + chrono::Duration::seconds(1);
    let store = manager.store.lock().await;
    store.insert_session(&child).expect("reserved successor");
    store
        .insert_rotation_event(
            fx.lead.id,
            rotation_id,
            "reserved",
            "successor_reserved",
            Some(&serde_json::json!({ "successor_id": child.id }).to_string()),
        )
        .expect("reservation marker");
    child
}

/// #1142 R3: the daemon died after the successor was reserved (row plus
/// marker) and before it was bound or published. Startup (`restore_sessions`)
/// fails the reserved child. The next cap pass recovers that exact candidate
/// (refusing it as never live) and never allocates a second successor under
/// the same identity.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_after_reservation_restores_the_exact_candidate_without_a_second_successor()
-> anyhow::Result<()> {
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    let fx = lead_cap_fixture(&manager, &dir).await;
    let planned = plan_now(&manager, "boot-dead").await;
    let [CapAction::Rotate { rotation_id, .. }] = planned.as_slice() else {
        panic!("one rotation expected: {planned:?}");
    };
    let rotation_id = rotation_id.clone();
    manager.store.lock().await.insert_rotation_event(
        fx.lead.id,
        &rotation_id,
        "completed",
        "cap_triggered",
        None,
    )?;
    let reserved =
        reserve_successor(&manager, &fx, &rotation_id, &dir, SessionStatus::Starting).await;
    drop(planned);
    drop(manager); // the daemon dies

    let restarted = restarted_under(&dir);
    restarted.restore_sessions().await?;
    assert_eq!(
        restarted
            .store
            .lock()
            .await
            .get_session(reserved.id)?
            .expect("reserved child")
            .status,
        SessionStatus::Failed,
        "startup fails the child that never launched"
    );
    // Pass 1 recovers the candidate and refuses it (recovery returns with the
    // refusal durable); pass 2 sees that terminal event and settles the
    // request. Further passes change nothing and never allocate a new child.
    // No wall-clock wait: the passes are the only driver.
    for _ in 0..4 {
        restarted
            .rotate_capped_coordinators_for_boot("boot-alive")
            .await?;
    }
    let store = restarted.store.lock().await;
    assert_eq!(
        cap_rotation(&store, fx.lead.id)?.expect("record").state,
        CapState::Failed,
        "the refused request is settled by the second pass"
    );
    assert_eq!(
        successors_of(&store, fx.lead.id),
        1,
        "never a second successor"
    );
    let record = cap_rotation(&store, fx.lead.id)?.expect("record");
    assert_eq!(record.reason.as_deref(), Some("refused:successor_not_live"));
    assert_eq!(record.rotation_id.as_deref(), Some(rotation_id.as_str()));
    assert_eq!(
        events_of(&store, fx.lead.id, "refused:successor_not_live"),
        1
    );
    assert_eq!(events_of(&store, fx.lead.id, "cap_triggered"), 1);
    assert_eq!(store.find_published_rotation_successor(fx.lead.id)?, None);
    Ok(())
}

/// #1142 R3: the daemon died after the successor finished its first turn
/// (custody bound, provider done) and before publication. The next cap pass
/// publishes that exact candidate once, and no second successor exists.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_before_publication_publishes_the_exact_candidate_once() -> anyhow::Result<()> {
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    let fx = lead_cap_fixture(&manager, &dir).await;
    let planned = plan_now(&manager, "boot-dead").await;
    let [CapAction::Rotate { rotation_id, .. }] = planned.as_slice() else {
        panic!("one rotation expected: {planned:?}");
    };
    let rotation_id = rotation_id.clone();
    let candidate =
        reserve_successor(&manager, &fx, &rotation_id, &dir, SessionStatus::Completed).await;
    drop(planned);
    drop(manager); // the daemon dies

    let restarted = restarted_under(&dir);
    restarted.restore_sessions().await?;
    assert_eq!(
        run_cap_passes(&restarted, "boot-alive", fx.lead.id, 3).await,
        CapState::Rotated,
        "the publication is durable, so the next pass records the settlement"
    );
    let store = restarted.store.lock().await;
    assert_eq!(
        successors_of(&store, fx.lead.id),
        1,
        "never a second successor"
    );
    assert_eq!(
        store.find_published_rotation_successor(fx.lead.id)?,
        Some(candidate.id)
    );
    assert_eq!(
        events_of(&store, fx.lead.id, "completed"),
        1,
        "published exactly once"
    );
    assert_eq!(
        cap_rotation(&store, fx.lead.id)?.expect("record").successor,
        Some(candidate.id)
    );
    Ok(())
}

/// A newer row of the seat that a different rotation reserved, as a manual
/// rotation of the idle predecessor leaves it.
async fn foreign_successor(
    manager: &SessionManager,
    fx: &CapFixture,
    rotation_id: &str,
    dir: &tempfile::TempDir,
    status: SessionStatus,
) -> Session {
    let mut child = reserve_successor(manager, fx, rotation_id, dir, status).await;
    child.created_at = chrono::Utc::now() + chrono::Duration::seconds(30);
    manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET created_at=?1 WHERE id=?2",
            rusqlite::params![
                child
                    .created_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                child.id.to_string()
            ],
        )
        .expect("age the foreign successor");
    child
}

/// #1153: the request R reserved A; the restart failed A; a manual rotation
/// of the same idle seat then reserved B (newer, `Completed`) under another
/// rotation id. R acts on A alone: it refuses A as never live, never
/// publishes B, and never moves the global grant to B.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_acts_on_the_reserved_successor_never_a_newer_foreign_row() -> anyhow::Result<()> {
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    let (seat, grant) = global_cap_fixture(&manager, &dir).await;
    let planned = plan_now(&manager, "boot-dead").await;
    let [CapAction::Rotate { rotation_id, .. }] = planned.as_slice() else {
        panic!("one rotation expected: {planned:?}");
    };
    let rotation_id = rotation_id.clone();
    let fx = CapFixture {
        lead: seat.clone(),
        epic: seat.clone(),
    };
    let reserved =
        reserve_successor(&manager, &fx, &rotation_id, &dir, SessionStatus::Failed).await;
    let foreign = foreign_successor(
        &manager,
        &fx,
        "manual-rotation",
        &dir,
        SessionStatus::Completed,
    )
    .await;
    drop(planned);

    let recovered = plan_now(&manager, "boot-alive").await;
    assert_eq!(
        recovered,
        vec![CapAction::Recover {
            session_id: seat.id,
            rotation_id: rotation_id.clone(),
            successor: reserved.id,
            status: SessionStatus::Failed,
        }],
        "the request recovers the successor it reserved"
    );
    wait_until("the request to settle as refused", || async {
        manager
            .rotate_capped_coordinators_for_boot("boot-alive")
            .await
            .expect("pass");
        cap_rotation(&*manager.store.lock().await, seat.id)
            .ok()
            .flatten()
            .is_some_and(|record| record.state == CapState::Failed)
    })
    .await;
    let store = manager.store.lock().await;
    let record = cap_rotation(&store, seat.id)?.expect("record");
    assert_eq!(record.reason.as_deref(), Some("refused:successor_not_live"));
    assert_eq!(record.successor, None);
    assert_eq!(
        events_of(&store, seat.id, "completed"),
        0,
        "no completed witness is written for the foreign row"
    );
    assert_eq!(
        store.published_rotation_successor_of(seat.id, Some(&rotation_id))?,
        None
    );
    assert_eq!(
        store.get_session(foreign.id)?.expect("foreign").status,
        SessionStatus::Completed,
        "the other rotation's successor is untouched"
    );
    let moved = store.active_global_grant()?.expect("grant");
    assert_eq!(moved.seat_session_id, seat.id, "the grant never moved");
    assert_eq!(moved.grant_version, grant.grant_version);
    Ok(())
}

/// #1153: a request that never reserved (crash before reservation) does not
/// adopt a newer row another rotation reserved: it is replayed under its own
/// identity, and the foreign row stays unpublished.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn markerless_request_does_not_adopt_a_foreign_reservation() -> anyhow::Result<()> {
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    let fx = lead_cap_fixture(&manager, &dir).await;
    let planned = plan_now(&manager, "boot-dead").await;
    let [CapAction::Rotate { rotation_id, .. }] = planned.as_slice() else {
        panic!("one rotation expected: {planned:?}");
    };
    let rotation_id = rotation_id.clone();
    let foreign = foreign_successor(
        &manager,
        &fx,
        "manual-rotation",
        &dir,
        SessionStatus::Completed,
    )
    .await;
    drop(planned);

    let replanned = plan_now(&manager, "boot-alive").await;
    let [
        CapAction::Rotate {
            rotation_id: replayed,
            ..
        },
    ] = replanned.as_slice()
    else {
        panic!("the request replays instead of adopting a foreign row: {replanned:?}");
    };
    assert_eq!(*replayed, rotation_id);
    let store = manager.store.lock().await;
    assert_eq!(events_of(&store, fx.lead.id, "completed"), 0);
    assert_eq!(
        store.get_session(foreign.id)?.expect("foreign").status,
        SessionStatus::Completed
    );
    Ok(())
}

/// #1153: two unclaimed rows and no marker: the request cannot tell which one
/// it started, so it fails closed (settled `Failed`, the operator told once)
/// and recovers neither.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ambiguous_legacy_candidates_fail_closed() -> anyhow::Result<()> {
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    let fx = lead_cap_fixture(&manager, &dir).await;
    let planned = plan_now(&manager, "boot-dead").await;
    let [CapAction::Rotate { .. }] = planned.as_slice() else {
        panic!("one rotation expected: {planned:?}");
    };
    drop(planned);
    for age in [1_i64, 2] {
        let mut row = test_session(Uuid::new_v4(), SessionStatus::Completed);
        row.working_dir = dir.path().to_path_buf();
        row.continued_from = Some(fx.lead.id);
        row.rotation_depth = 1;
        row.created_at = chrono::Utc::now() + chrono::Duration::seconds(age);
        manager.store.lock().await.insert_session(&row)?;
    }
    let actions = plan_now(&manager, "boot-alive").await;
    assert!(
        matches!(actions.as_slice(), [CapAction::Escalate { session_id, .. }] if *session_id == fx.lead.id),
        "{actions:?}"
    );
    let store = manager.store.lock().await;
    let record = cap_rotation(&store, fx.lead.id)?.expect("record");
    assert_eq!(record.state, CapState::Failed);
    assert_eq!(record.reason.as_deref(), Some("successor_ambiguous"));
    assert_eq!(events_of(&store, fx.lead.id, "completed"), 0);
    Ok(())
}

/// #1149: the cap pass's dispatch makes the request a durable open intent
/// (one `entered` row under the request's identity) before its decider
/// reserves a successor, and the rotation still completes.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cap_dispatch_records_the_rotation_intent_before_the_decider_reserves() -> anyhow::Result<()>
{
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    let fx = lead_cap_fixture(&manager, &dir).await;
    rotates_once_released(&manager, &fx).await;
    let store = manager.store.lock().await;
    let rotation_id = cap_rotation(&store, fx.lead.id)?
        .and_then(|record| record.rotation_id)
        .expect("request identity");
    let intents: Vec<(String, String)> = store
        .conn
        .prepare(
            "SELECT phase, json_extract(metadata,'$.trigger') FROM rotation_events
             WHERE session_id=?1 AND rotation_id=?2 AND event_type='entered'",
        )?
        .query_map(
            rusqlite::params![fx.lead.id.to_string(), rotation_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?
        .collect::<std::result::Result<_, _>>()?;
    assert_eq!(
        intents,
        vec![("completed_trigger".to_string(), "cap_triggered".to_string())]
    );
    Ok(())
}

/// #1149: the daemon died after the cap trigger was logged and before any
/// successor was reserved. The trigger's intent is owned by restart recovery,
/// which launches the one decider; the cap pass waits for that rotation's
/// terminal event instead of dispatching a second one. One successor, one
/// publication, the global grant moves with it, and the request settles
/// `Rotated`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_after_cap_trigger_before_reservation_is_owned_by_restart_recovery()
-> anyhow::Result<()> {
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    let (seat, grant) = global_cap_fixture(&manager, &dir).await;
    let planned = plan_now(&manager, "boot-dead").await;
    let [CapAction::Rotate { rotation_id, .. }] = planned.as_slice() else {
        panic!("one rotation expected: {planned:?}");
    };
    let rotation_id = rotation_id.clone();
    {
        let store = manager.store.lock().await;
        assert!(store.record_completed_trigger_intent(seat.id, &rotation_id, "cap_triggered")?);
        store.insert_rotation_event(seat.id, &rotation_id, "completed", "cap_triggered", None)?;
    }
    drop(planned);
    drop(manager); // the daemon dies

    let restarted = restarted_under(&dir);
    let successor = Uuid::new_v4();
    install_rotation_child_id_for_test(seat.id, successor);
    install_controller_candidate_test_process(successor);
    restarted.restore_sessions().await?;
    wait_until(
        "the recovered rotation to publish the successor",
        || async {
            restarted
                .store
                .lock()
                .await
                .active_global_grant()
                .ok()
                .flatten()
                .is_some_and(|grant| grant.seat_session_id == successor)
        },
    )
    .await;
    drop_controller_candidate_test_stream(successor);
    assert_eq!(
        run_cap_passes(&restarted, "boot-alive", seat.id, 3).await,
        CapState::Rotated,
        "the publication is durable, so the next pass records the settlement"
    );
    assert_grant_moved(&restarted, successor, &grant).await;
    let store = restarted.store.lock().await;
    assert_eq!(
        successors_of(&store, seat.id),
        1,
        "never a second successor"
    );
    assert_eq!(
        events_of(&store, seat.id, "completed"),
        1,
        "published exactly once"
    );
    assert_eq!(events_of(&store, seat.id, "recovery_claimed"), 1);
    assert_eq!(
        cap_rotation(&store, seat.id)?.expect("record").successor,
        Some(successor)
    );
    Ok(())
}

/// #1149: the daemon died after the cap request's successor was reserved and
/// finished its first turn, before publication, with the request's intent on
/// record. Restart recovery publishes that exact successor once (the grant
/// moves with it); the cap pass only records the settlement.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_after_cap_reservation_with_an_intent_is_published_by_restart_recovery()
-> anyhow::Result<()> {
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    let (seat, grant) = global_cap_fixture(&manager, &dir).await;
    let planned = plan_now(&manager, "boot-dead").await;
    let [CapAction::Rotate { rotation_id, .. }] = planned.as_slice() else {
        panic!("one rotation expected: {planned:?}");
    };
    let rotation_id = rotation_id.clone();
    {
        let store = manager.store.lock().await;
        assert!(store.record_completed_trigger_intent(seat.id, &rotation_id, "cap_triggered")?);
        store.insert_rotation_event(seat.id, &rotation_id, "completed", "cap_triggered", None)?;
    }
    let fx = CapFixture {
        lead: seat.clone(),
        epic: seat.clone(),
    };
    let candidate =
        reserve_successor(&manager, &fx, &rotation_id, &dir, SessionStatus::Completed).await;
    drop(planned);
    drop(manager); // the daemon dies

    let restarted = restarted_under(&dir);
    restarted.restore_sessions().await?;
    assert_grant_moved(&restarted, candidate.id, &grant).await;
    assert_eq!(
        run_cap_passes(&restarted, "boot-alive", seat.id, 3).await,
        CapState::Rotated,
        "the publication is durable, so the next pass records the settlement"
    );
    let store = restarted.store.lock().await;
    assert_eq!(successors_of(&store, seat.id), 1);
    assert_eq!(events_of(&store, seat.id, "completed"), 1);
    assert_eq!(
        cap_rotation(&store, seat.id)?.expect("record").successor,
        Some(candidate.id)
    );
    Ok(())
}

/// #1149: a request whose intent restart recovery has claimed is not replayed
/// or recovered by the cap pass (recovery owns its effect); an unclaimed
/// request left by an earlier boot is still replayed.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cap_pass_leaves_a_claimed_intent_to_restart_recovery() -> anyhow::Result<()> {
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    let fx = lead_cap_fixture(&manager, &dir).await;
    let planned = plan_now(&manager, "boot-dead").await;
    let [CapAction::Rotate { rotation_id, .. }] = planned.as_slice() else {
        panic!("one rotation expected: {planned:?}");
    };
    let rotation_id = rotation_id.clone();
    drop(planned);
    {
        let store = manager.store.lock().await;
        store.record_completed_trigger_intent(fx.lead.id, &rotation_id, "cap_triggered")?;
    }
    let unclaimed = plan_now(&manager, "boot-alive").await;
    assert!(
        matches!(unclaimed.as_slice(), [CapAction::Rotate { .. }]),
        "an unclaimed request is replayed: {unclaimed:?}"
    );
    // Planning marked the replay dispatched by this boot; model the next boot.
    {
        let store = manager.store.lock().await;
        let intent = store
            .latest_open_rotation_intent(fx.lead.id)?
            .expect("open intent");
        store.claim_open_rotation_intent_for_recovery(fx.lead.id, &intent)?;
    }
    let claimed = plan_now(&manager, "boot-after").await;
    assert!(
        claimed.is_empty(),
        "recovery owns a claimed intent: {claimed:?}"
    );
    Ok(())
}

/// #1149: a request that stays `Rotating` for hours without publishing or
/// refusing is escalated to the operator once, and stays `Rotating`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_rotating_request_escalates_once() -> anyhow::Result<()> {
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    let fx = lead_cap_fixture(&manager, &dir).await;
    let planned = plan_now(&manager, "boot-live").await;
    assert!(matches!(planned.as_slice(), [CapAction::Rotate { .. }]));
    let later = chrono::Utc::now() + chrono::Duration::hours(3);
    let plan_later = |boot: &'static str| {
        let manager = &manager;
        async move {
            let store = manager.store.lock().await;
            plan_cap_actions(&store, later, boot, |_| true, |_| Some(200_000)).expect("plan")
        }
    };
    let stale = plan_later("boot-live").await;
    assert!(
        matches!(stale.as_slice(), [CapAction::Escalate { session_id, .. }] if *session_id == fx.lead.id),
        "{stale:?}"
    );
    assert!(
        plan_later("boot-live").await.is_empty(),
        "the operator is told once"
    );
    let store = manager.store.lock().await;
    let record = cap_rotation(&store, fx.lead.id)?.expect("record");
    assert_eq!(record.state, CapState::Rotating);
    assert_eq!(record.reason.as_deref(), Some("rotating_stale"));
    Ok(())
}

/// The operator policies the cap must never bypass.
#[derive(Clone, Copy, Debug)]
enum Policy {
    SoftPause,
    HardPause,
    RotationDisabled,
    CapOff,
}

const POLICIES: [Policy; 4] = [
    Policy::SoftPause,
    Policy::HardPause,
    Policy::RotationDisabled,
    Policy::CapOff,
];

fn deferral_reason(policy: Policy) -> &'static str {
    match policy {
        Policy::SoftPause | Policy::HardPause => "operator_paused",
        Policy::RotationDisabled => "rotation_disabled",
        Policy::CapOff => "cap_disabled",
    }
}

/// Put `policy` in place. The pauses and rotation-disable are durable; the cap
/// setting is runtime state, so a restarted manager applies it again.
async fn apply_policy(manager: &SessionManager, seat: Uuid, policy: Policy) {
    use crate::store::manager_actions::OperatorPause;
    match policy {
        Policy::SoftPause => {
            manager
                .store
                .lock()
                .await
                .set_operator_pause(seat, OperatorPause::Soft)
                .expect("soft pause");
        }
        Policy::HardPause => {
            manager
                .store
                .lock()
                .await
                .set_operator_pause(seat, OperatorPause::Hard)
                .expect("hard pause");
        }
        Policy::RotationDisabled => {
            assert!(
                manager
                    .toggle_rotation_disabled(seat)
                    .await
                    .expect("toggle")
                    .is_some()
            );
        }
        Policy::CapOff => {
            manager
                .runtime_config
                .update_field("coordinator_context_cap_tokens", &serde_json::json!(0))
                .expect("cap off");
        }
    }
}

/// A restarted daemon over the same data directory. Context rotation is
/// enabled on its runtime config explicitly: the default comes from the
/// process environment (`RSI_CONTEXT_ROTATION_ENABLED`), which differs between
/// a developer shell and the merge-queue gate, and a disabled cap pass does
/// nothing at all.
fn restarted_under(dir: &tempfile::TempDir) -> SessionManager {
    let restarted = rotation_manager_on(dir.path(), true);
    restarted
        .runtime_config
        .context_rotation_enabled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    set_cap(&restarted);
    restarted
}

/// Nothing was started for the seat, and no intent is left to recover.
async fn assert_nothing_started(manager: &SessionManager, seat: Uuid, policy: Policy) {
    let store = manager.store.lock().await;
    assert_eq!(successors_of(&store, seat), 0, "{policy:?}: no successor");
    assert_eq!(events_of(&store, seat, "completed"), 0, "{policy:?}");
    assert!(
        store
            .open_completed_trigger_rotation_sessions()
            .expect("open intents")
            .is_empty(),
        "{policy:?}: no open intent is left to recover"
    );
}

/// #1149 R1: a cap request deferred by the decider (pause, rotation-disable or
/// cap off) closes its intent with the record. A real restart with the policy
/// still in place recovers nothing and starts nothing; neither does the cap
/// pass under the policy.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deferred_cap_request_is_not_recovered_by_a_restart() -> anyhow::Result<()> {
    for policy in POLICIES {
        let (manager, dir) = rotation_manager_with_context_rotation(true);
        let (seat, _grant) = global_cap_fixture(&manager, &dir).await;
        let planned = plan_now(&manager, "boot-a").await;
        assert!(matches!(planned.as_slice(), [CapAction::Rotate { .. }]));
        apply_policy(&manager, seat.id, policy).await;
        assert_eq!(manager.dispatch_cap_actions(planned).await?, 1);
        assert_deferred(&manager, seat.id, deferral_reason(policy)).await;
        {
            let store = manager.store.lock().await;
            assert_eq!(events_of(&store, seat.id, "entered"), 1);
            assert!(store.latest_open_rotation_intent(seat.id)?.is_none());
        }
        assert_nothing_started(&manager, seat.id, policy).await;
        drop(manager); // the daemon dies

        let restarted = restarted_under(&dir);
        if matches!(policy, Policy::CapOff) {
            apply_policy(&restarted, seat.id, policy).await;
        }
        restarted.restore_sessions().await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_nothing_started(&restarted, seat.id, policy).await;
        restarted
            .rotate_capped_coordinators_for_boot("boot-b")
            .await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_nothing_started(&restarted, seat.id, policy).await;
    }
    Ok(())
}

/// #1149 R1: the daemon died after the cap trigger was logged, with the
/// operator's policy set meanwhile. The first restart's recovery runs the
/// decider, which defers under the policy and closes the intent atomically
/// with the record; a second restart then recovers nothing, so the policy is
/// never bypassed.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recovery_deferral_survives_a_second_restart_under_the_policy() -> anyhow::Result<()> {
    for policy in POLICIES {
        let (manager, dir) = rotation_manager_with_context_rotation(true);
        let (seat, _grant) = global_cap_fixture(&manager, &dir).await;
        let planned = plan_now(&manager, "boot-dead").await;
        let [CapAction::Rotate { rotation_id, .. }] = planned.as_slice() else {
            panic!("one rotation expected: {planned:?}");
        };
        let rotation_id = rotation_id.clone();
        {
            let store = manager.store.lock().await;
            assert!(store.record_completed_trigger_intent(
                seat.id,
                &rotation_id,
                "cap_triggered"
            )?);
            store.insert_rotation_event(
                seat.id,
                &rotation_id,
                "completed",
                "cap_triggered",
                None,
            )?;
        }
        apply_policy(&manager, seat.id, policy).await;
        drop(planned);
        drop(manager); // the daemon dies

        let restarted = restarted_under(&dir);
        if matches!(policy, Policy::CapOff) {
            apply_policy(&restarted, seat.id, policy).await;
        }
        restarted.restore_sessions().await?;
        wait_until("recovery's decider to defer the request", || async {
            cap_rotation(&*restarted.store.lock().await, seat.id)
                .ok()
                .flatten()
                .is_some_and(|record| record.state == CapState::Due)
        })
        .await;
        assert_nothing_started(&restarted, seat.id, policy).await;
        assert_eq!(
            events_of(
                &*restarted.store.lock().await,
                seat.id,
                "refused:cap_deferred"
            ),
            1
        );
        drop(restarted); // and dies again

        let again = restarted_under(&dir);
        if matches!(policy, Policy::CapOff) {
            apply_policy(&again, seat.id, policy).await;
        }
        again.restore_sessions().await?;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_nothing_started(&again, seat.id, policy).await;
    }
    Ok(())
}

/// #1149 R1: an open cap-triggered intent whose request is no longer the
/// seat's current one is superseded, never a manual rotation: recovery closes
/// it without running a decider, whatever the seat's policy.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn superseded_cap_intent_fails_closed_in_recovery() -> anyhow::Result<()> {
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    let (seat, _grant) = global_cap_fixture(&manager, &dir).await;
    let planned = plan_now(&manager, "boot-dead").await;
    assert!(matches!(planned.as_slice(), [CapAction::Rotate { .. }]));
    {
        // An orphaned automatic intent: it names an identity the cap record
        // no longer holds.
        let store = manager.store.lock().await;
        assert!(store.record_completed_trigger_intent(
            seat.id,
            "orphaned-cap-id",
            "cap_triggered"
        )?);
    }
    drop(planned);
    drop(manager);

    let restarted = restarted_under(&dir);
    restarted.restore_sessions().await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let store = restarted.store.lock().await;
    assert_eq!(successors_of(&store, seat.id), 0);
    assert_eq!(events_of(&store, seat.id, "refused:cap_superseded"), 1);
    assert!(store.open_completed_trigger_rotation_sessions()?.is_empty());
    Ok(())
}

/// #1155: another rotation's refusal of the same seat (a manual rotation)
/// never settles a cap request that is claimed or pending. The request stays
/// rotating and owned until its own terminal event.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn foreign_refusal_never_settles_a_claimed_cap_request() -> anyhow::Result<()> {
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    let fx = lead_cap_fixture(&manager, &dir).await;
    let planned = plan_now(&manager, "boot-dead").await;
    let [CapAction::Rotate { rotation_id, .. }] = planned.as_slice() else {
        panic!("one rotation expected: {planned:?}");
    };
    let rotation_id = rotation_id.clone();
    drop(planned);
    {
        let store = manager.store.lock().await;
        store.record_completed_trigger_intent(fx.lead.id, &rotation_id, "cap_triggered")?;
        let intent = store
            .latest_open_rotation_intent(fx.lead.id)?
            .expect("open intent");
        store.claim_open_rotation_intent_for_recovery(fx.lead.id, &intent)?;
        store.insert_rotation_event(
            fx.lead.id,
            "a-manual-rotation",
            "completed",
            "refused:rate_limited",
            None,
        )?;
    }
    let actions = plan_now(&manager, "boot-after").await;
    assert!(
        actions.is_empty(),
        "the request stays owned by recovery: {actions:?}"
    );
    {
        let store = manager.store.lock().await;
        let record = cap_rotation(&store, fx.lead.id)?.expect("record");
        assert_eq!(record.state, CapState::Rotating);
        assert_eq!(record.rotation_id.as_deref(), Some(rotation_id.as_str()));
        // Its own refusal is still the one that settles it.
        store.insert_rotation_event(
            fx.lead.id,
            &rotation_id,
            "completed",
            "refused:rate_limited",
            None,
        )?;
    }
    let settled = plan_now(&manager, "boot-after").await;
    assert!(
        matches!(settled.as_slice(), [CapAction::Escalate { .. }]),
        "{settled:?}"
    );
    assert_eq!(
        cap_rotation(&*manager.store.lock().await, fx.lead.id)?
            .expect("record")
            .state,
        CapState::Failed
    );
    Ok(())
}

/// #1158: a coordinator cap rotation blocked on its failed, never-started
/// successor (it holds the seat's sandbox custody) is finished by the
/// operator's Continue: the exact reserved successor starts in a new thread,
/// is published in this boot (Epic lead and global grant move to it), and the
/// next cap passes settle the request on it without allocating another.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_cap_rotation_settles_after_the_operator_continues_its_successor()
-> anyhow::Result<()> {
    let (manager, dir) = rotation_manager_with_context_rotation(true);
    manager
        .runtime_config
        .context_rotation_enabled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    set_cap(&manager);
    let seat = seated_live_parent(&manager, dir.path()).await?;
    let parent = seat.fixture.parent.clone();
    {
        let store = manager.store.lock().await;
        assert_eq!(
            evaluate_cap_crossing(
                &store,
                &parent,
                210_000,
                Some(200_000),
                None,
                chrono::Utc::now()
            )?,
            CapCrossing::Recorded(true)
        );
    }
    manager
        .completed
        .write()
        .await
        .insert(parent.id, CompletedSession::for_test(parent.clone()));
    let planned = plan_now(&manager, "boot-dead").await;
    let [CapAction::Rotate { rotation_id, .. }] = planned.as_slice() else {
        panic!("one rotation expected: {planned:?}");
    };
    let rotation_id = rotation_id.clone();
    drop(planned);
    {
        let store = manager.store.lock().await;
        assert!(store.record_completed_trigger_intent(parent.id, &rotation_id, "cap_triggered")?);
        store.insert_rotation_event(parent.id, &rotation_id, "completed", "cap_triggered", None)?;
    }
    let failed =
        reserve_and_bind_live_successor_for_test(&manager, &seat.fixture, &rotation_id).await?;
    manager.store.lock().await.conn.execute(
        "UPDATE sessions SET claude_session_id=NULL WHERE id=?1",
        [failed.id.to_string()],
    )?;
    drop(manager); // the daemon dies after the bind, before the successor starts

    let restarted = restarted_under(&dir);
    restarted.restore_sessions().await?;
    assert_eq!(
        run_cap_passes(&restarted, "boot-alive", parent.id, 2).await,
        CapState::Rotating,
        "the cap pass leaves the blocked request to its recovery"
    );
    assert_eq!(
        seat_holders(&*restarted.store.lock().await, seat.epic),
        (Some(parent.id), Some(parent.id)),
        "nothing moves while the successor is blocked"
    );

    let _scripted = install_controller_candidate_test_process(failed.id);
    restarted
        .continue_session_operator(failed.id, "start the blocked successor".into())
        .await?;
    drop_controller_candidate_test_stream(failed.id);
    assert_eq!(
        seat_holders(&*restarted.store.lock().await, seat.epic),
        (Some(failed.id), Some(failed.id)),
        "published in this boot"
    );
    assert_eq!(
        run_cap_passes(&restarted, "boot-alive", parent.id, 3).await,
        CapState::Rotated
    );
    let store = restarted.store.lock().await;
    assert_eq!(
        cap_rotation(&store, parent.id)?.expect("record").successor,
        Some(failed.id)
    );
    assert_eq!(successors_of(&store, parent.id), 1);
    assert_eq!(events_of(&store, parent.id, "completed"), 1);
    Ok(())
}
