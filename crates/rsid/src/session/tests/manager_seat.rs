//! Issue #669 runtime pins: the real coordinator resumes a Failed appointed
//! manager in place, exactly once, through the scripted provider seam.
use super::*;
use crate::session::launch;
use rsi_common::harness_manager::{ManagerSeatConditionV1, ManagerSeatStateV1};
use std::time::Duration;

async fn fail_manager(p: &Pilot) {
    p.manager
        .store
        .lock()
        .await
        .update_session_status(p.owner, SessionStatus::Failed)
        .unwrap();
    if let Some(cached) = p.manager.completed.write().await.get_mut(&p.owner) {
        cached.session.status = SessionStatus::Failed;
    }
    p.manager
        .runtime_config
        .retry_enabled
        .store(true, Ordering::SeqCst);
}

async fn seat_ops(p: &Pilot) -> Vec<(String, String)> {
    let store = p.manager.store.lock().await;
    let mut stmt = store
        .conn
        .prepare(
            "SELECT idempotency_key,state FROM harness_manager_v2_operations
             WHERE project_id=?1 AND kind='seat_recovery' ORDER BY created_at",
        )
        .unwrap();
    stmt.query_map([p.project.to_string()], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap()
}

async fn seat_state(p: &Pilot) -> ManagerSeatStateV1 {
    let store = p.manager.store.lock().await;
    let config = store.get_harness_manager(p.project).unwrap().unwrap();
    store.manager_seat_state(&config).unwrap().unwrap()
}

async fn session_count(p: &Pilot) -> i64 {
    p.manager
        .store
        .lock()
        .await
        .conn
        .query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))
        .unwrap()
}

/// First pass observes the seat down and schedules attempt 1 after the
/// fixture's one-second backoff; the caller runs the due pass.
async fn schedule_first_attempt(p: &Pilot) {
    p.manager.reconcile_harness_managers_once().await.unwrap();
    let state = seat_state(p).await;
    assert_eq!(state.state, ManagerSeatConditionV1::Recovering);
    let wait = (state.not_before.unwrap() - chrono::Utc::now())
        .to_std()
        .unwrap_or_default();
    assert!(wait < Duration::from_secs(5));
    tokio::time::sleep(wait + Duration::from_millis(20)).await;
}

fn seat_messages(
    events: &mut tokio::sync::broadcast::Receiver<std::sync::Arc<DaemonEvent>>,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let DaemonEvent::SystemMessage { level, message } = event.as_ref()
            && message.starts_with("[manager-seat]")
        {
            out.push((level.clone(), message.clone()));
        }
    }
    out
}

async fn release(p: &Pilot) {
    crate::session::lifecycle::interrupt_active_in_maps(&p.manager.active, p.owner)
        .await
        .unwrap();
    launch::drop_controller_candidate_test_stream(p.owner);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seat_failed_tip_resumes_once_in_place() {
    let p = pilot().await;
    let mut events = p.manager.event_bus.subscribe();
    let before = p
        .manager
        .store
        .lock()
        .await
        .get_session(p.owner)
        .unwrap()
        .unwrap();
    let sessions = session_count(&p).await;
    fail_manager(&p).await;
    schedule_first_attempt(&p).await;
    let resumed = launch::install_controller_candidate_test_process(p.owner);
    p.manager.reconcile_harness_managers_once().await.unwrap();
    assert_eq!(resumed.productive_start_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        seat_ops(&p).await,
        vec![(format!("seat:{}:1", p.owner), "succeeded".to_string())]
    );
    // Same row, same working tree, same sandbox; no new session or tip.
    assert_eq!(session_count(&p).await, sessions);
    let after = p
        .manager
        .store
        .lock()
        .await
        .get_session(p.owner)
        .unwrap()
        .unwrap();
    assert_eq!(
        (after.working_dir.clone(), after.sandbox_root.clone()),
        (before.working_dir.clone(), before.sandbox_root.clone())
    );
    assert!(p.manager.active.read().await.contains_key(&p.owner));
    let config = p
        .manager
        .store
        .lock()
        .await
        .get_harness_manager(p.project)
        .unwrap()
        .unwrap();
    assert_eq!(config.current_session_id, Some(p.owner));
    let state = seat_state(&p).await;
    assert_eq!(state.state, ManagerSeatConditionV1::Recovering);
    assert_eq!(state.reason, "manager_seat_recovery_in_flight");
    assert_eq!(state.attempts, 1);
    let messages = seat_messages(&mut events);
    assert!(
        messages
            .iter()
            .any(|(level, message)| level == "error" && message.contains("scheduled")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|(level, message)| level == "error" && message.contains("resuming Failed manager")),
        "{messages:?}"
    );
    release(&p).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seat_recovery_single_flight_under_concurrent_passes() {
    let p = pilot().await;
    fail_manager(&p).await;
    schedule_first_attempt(&p).await;
    let resumed = launch::install_controller_candidate_test_process(p.owner);
    let (first, second) = tokio::join!(
        p.manager.reconcile_harness_managers_once(),
        p.manager.reconcile_harness_managers_once()
    );
    first.unwrap();
    second.unwrap();
    assert_eq!(resumed.productive_start_count.load(Ordering::SeqCst), 1);
    // A scheduler wake racing the recovered tip is refused as busy.
    let wake = crate::issue_tracker::poller::SessionLauncher::resume_scheduled(
        &p.manager,
        p.owner,
        "scheduled wake".into(),
    )
    .await
    .unwrap_err();
    assert!(wake.to_string().contains("is still active"), "{wake}");
    for _ in 0..3 {
        p.manager.reconcile_harness_managers_once().await.unwrap();
    }
    assert_eq!(resumed.productive_start_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        seat_ops(&p).await,
        vec![(format!("seat:{}:1", p.owner), "succeeded".to_string())]
    );
    release(&p).await;
}

async fn seat_outcomes(p: &Pilot) -> Vec<(String, String, String)> {
    let store = p.manager.store.lock().await;
    let mut stmt = store
        .conn
        .prepare(
            "SELECT idempotency_key,state,json_extract(outcome_json,'$.outcome')
             FROM harness_manager_v2_operations
             WHERE project_id=?1 AND kind='seat_recovery' ORDER BY created_at",
        )
        .unwrap();
    stmt.query_map([p.project.to_string()], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })
    .unwrap()
    .collect::<std::result::Result<_, _>>()
    .unwrap()
}

/// Claim attempt 1 through the store pass without executing it, so a test
/// can change state between the claim and the continuation boundary.
async fn claim_only(p: &Pilot) -> crate::store::manager_intent::manager_seat::ManagerSeatClaimV1 {
    let store = p.manager.store.lock().await;
    let t0 = chrono::Utc::now();
    store
        .reconcile_manager_seat(
            p.project,
            |_| false,
            true,
            p.manager.program_run_boot_id,
            t0,
        )
        .unwrap();
    store
        .reconcile_manager_seat(
            p.project,
            |_| false,
            true,
            p.manager.program_run_boot_id,
            t0 + chrono::Duration::seconds(2),
        )
        .unwrap()
        .claim
        .unwrap()
}

/// Round 2 (appserver_new_row): the coordinator never claims a Failed
/// `CodexAppServer` seat, whose continuation would allocate a new row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seat_codex_appserver_tip_is_never_resumed_in_place() {
    let p = pilot().await;
    p.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET provider='CodexAppServer' WHERE id=?1",
            [p.owner.to_string()],
        )
        .unwrap();
    if let Some(cached) = p.manager.completed.write().await.get_mut(&p.owner) {
        cached.session.provider = SessionProvider::CodexAppServer;
    }
    let sessions = session_count(&p).await;
    fail_manager(&p).await;
    for _ in 0..2 {
        p.manager.reconcile_harness_managers_once().await.unwrap();
        tokio::time::sleep(Duration::from_millis(1100)).await;
    }
    p.manager.reconcile_harness_managers_once().await.unwrap();
    assert_eq!(session_count(&p).await, sessions);
    let state = seat_state(&p).await;
    assert_eq!(state.state, ManagerSeatConditionV1::Down);
    assert_eq!(state.reason, "manager_seat_recovery_unavailable");
    assert_eq!(state.tip_session_id, p.owner);
    assert_eq!(seat_outcomes(&p).await, Vec::new());
    let config = p
        .manager
        .store
        .lock()
        .await
        .get_harness_manager(p.project)
        .unwrap()
        .unwrap();
    assert_eq!(config.current_session_id, Some(p.owner));
}

/// Round 2 (stale_claim_effect): an operator pause between claim and effect
/// is honoured at the continuation boundary; no provider launches.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seat_pause_after_claim_is_refused_at_continuation_boundary() {
    let p = pilot().await;
    fail_manager(&p).await;
    let claim = claim_only(&p).await;
    {
        let store = p.manager.store.lock().await;
        let grant = store
            .get_harness_manager_policy(p.project)
            .unwrap()
            .unwrap();
        let mut policy = grant.policy;
        policy.paused = true;
        store
            .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                project_id: p.project,
                expected_scope_version: 1,
                expected_policy_version: grant.row_version,
                idempotency_key: "seat-pause-after-claim".into(),
                policy,
            })
            .unwrap();
    }
    let process = launch::install_controller_candidate_test_process(p.owner);
    p.manager
        .execute_manager_seat_claim(p.project, claim)
        .await
        .unwrap();
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
    assert_eq!(
        seat_outcomes(&p).await,
        vec![(
            format!("seat:{}:1", p.owner),
            "blocked".to_string(),
            "manager_v2_policy_paused".to_string()
        )]
    );
    let row = p
        .manager
        .store
        .lock()
        .await
        .get_session(p.owner)
        .unwrap()
        .unwrap();
    assert_eq!(row.status, SessionStatus::Failed);
    launch::take_controller_candidate_test_process(p.owner);
}

/// Round 2 (stale_claim_effect): a rotation between claim and effect is a
/// typed refusal; the claimed tip is not resumed and the successor is never
/// launched by seat recovery.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seat_rotation_after_claim_never_launches_successor() {
    let p = pilot().await;
    fail_manager(&p).await;
    let claim = claim_only(&p).await;
    let successor = Uuid::new_v4();
    {
        let store = p.manager.store.lock().await;
        let mut session = bare_session(successor);
        session.project_id = Some(p.project);
        session.working_dir = p.repo.clone();
        session.session_kind = SessionKind::Standard;
        session.continued_from = Some(p.owner);
        session.rotation_depth = 1;
        session.provider = SessionProvider::Claude;
        session.model = Some("manager-scripted-provider".into());
        session.claude_session_id = Some(format!("provider-{successor}"));
        session.status = SessionStatus::Failed;
        store.insert_session(&session).unwrap();
        store
            .update_session_status(p.owner, SessionStatus::Archived)
            .unwrap();
        store
            .record_harness_manager_rotation(p.owner, successor)
            .unwrap();
    }
    let old_tip = launch::install_controller_candidate_test_process(p.owner);
    let new_tip = launch::install_controller_candidate_test_process(successor);
    p.manager
        .execute_manager_seat_claim(p.project, claim)
        .await
        .unwrap();
    assert_eq!(old_tip.productive_start_count.load(Ordering::SeqCst), 0);
    assert_eq!(new_tip.productive_start_count.load(Ordering::SeqCst), 0);
    assert_eq!(
        seat_outcomes(&p).await,
        vec![(
            format!("seat:{}:1", p.owner),
            "blocked".to_string(),
            "manager_seat_tip_changed".to_string()
        )]
    );
    let config = p
        .manager
        .store
        .lock()
        .await
        .get_harness_manager(p.project)
        .unwrap()
        .unwrap();
    assert_eq!(config.current_session_id, Some(successor));
    launch::take_controller_candidate_test_process(p.owner);
    launch::take_controller_candidate_test_process(successor);
}

#[derive(Clone, Copy, Debug)]
enum LateMutation {
    Pause,
    Revoke,
    Question,
    Rotate,
}

/// Round 3 (stale_claim_effect_late_mutation): the claim passes the early
/// gate, the continuation reaches the pre-launch pause after preparation,
/// admission and reaping, then state changes. The final fence refuses with a
/// typed outcome and nothing launches.
async fn late_mutation_is_refused_before_launch(kind: LateMutation, code: &str) {
    let p = pilot().await;
    fail_manager(&p).await;
    let claim = claim_only(&p).await;
    let successor = Uuid::new_v4();
    let old_tip = launch::install_controller_candidate_test_process(p.owner);
    let new_tip = launch::install_controller_candidate_test_process(successor);
    let (reached, resume) = launch::install_controller_candidate_test_pause(
        p.owner,
        launch::ControllerCandidateTestPhase::ManagerSeatBeforeFinalGate,
    );
    let mutate = async {
        reached.await.unwrap();
        let store = p.manager.store.lock().await;
        match kind {
            LateMutation::Pause => {
                let grant = store
                    .get_harness_manager_policy(p.project)
                    .unwrap()
                    .unwrap();
                let mut policy = grant.policy;
                policy.paused = true;
                store
                    .configure_harness_manager_policy(&ConfigureHarnessManagerPolicyRequestV2 {
                        project_id: p.project,
                        expected_scope_version: 1,
                        expected_policy_version: grant.row_version,
                        idempotency_key: "late-pause".into(),
                        policy,
                    })
                    .unwrap();
            }
            LateMutation::Revoke => {
                store
                    .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
                        group_ids: Vec::new(),
                        project_id: p.project,
                        session_id: p.owner,
                        epic_ids: Some(Vec::new()),
                        expected_row_version: 1,
                    })
                    .unwrap();
            }
            LateMutation::Question => {
                crate::store::manager_resources::tests::raise_question(&store, p.owner, 900);
            }
            LateMutation::Rotate => {
                let mut session = bare_session(successor);
                session.project_id = Some(p.project);
                session.working_dir = p.repo.clone();
                session.session_kind = SessionKind::Standard;
                session.continued_from = Some(p.owner);
                session.rotation_depth = 1;
                session.provider = SessionProvider::Claude;
                session.model = Some("manager-scripted-provider".into());
                session.claude_session_id = Some(format!("provider-{successor}"));
                session.status = SessionStatus::Failed;
                store.insert_session(&session).unwrap();
                store
                    .update_session_status(p.owner, SessionStatus::Archived)
                    .unwrap();
                store
                    .record_harness_manager_rotation(p.owner, successor)
                    .unwrap();
            }
        }
        drop(store);
        resume.send(()).unwrap();
    };
    let (executed, ()) = tokio::join!(
        p.manager.execute_manager_seat_claim(p.project, claim),
        mutate
    );
    executed.unwrap();
    assert_eq!(
        old_tip.productive_start_count.load(Ordering::SeqCst),
        0,
        "{kind:?}"
    );
    assert_eq!(
        new_tip.productive_start_count.load(Ordering::SeqCst),
        0,
        "{kind:?}"
    );
    assert_eq!(
        seat_outcomes(&p).await,
        vec![(
            format!("seat:{}:1", p.owner),
            "blocked".to_string(),
            code.to_string()
        )],
        "{kind:?}"
    );
    let row = p
        .manager
        .store
        .lock()
        .await
        .get_session(p.owner)
        .unwrap()
        .unwrap();
    // The tip row keeps exactly the state the mutation left it in: seat
    // recovery neither launched nor re-statused it.
    let expected = match kind {
        LateMutation::Rotate => SessionStatus::Archived,
        LateMutation::Question => SessionStatus::WaitingApproval,
        LateMutation::Pause | LateMutation::Revoke => SessionStatus::Failed,
    };
    assert_eq!(row.status, expected, "{kind:?}");
    assert_eq!(
        row.claude_session_id,
        Some(format!("provider-{}", p.owner)),
        "{kind:?}"
    );
    assert!(p.manager.completed.read().await.contains_key(&p.owner));
    launch::take_controller_candidate_test_process(p.owner);
    launch::take_controller_candidate_test_process(successor);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seat_pause_after_early_gate_is_refused_before_launch() {
    late_mutation_is_refused_before_launch(LateMutation::Pause, "manager_v2_policy_paused").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seat_revocation_after_early_gate_is_refused_before_launch() {
    late_mutation_is_refused_before_launch(LateMutation::Revoke, "manager_seat_scope_changed")
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seat_question_after_early_gate_is_refused_before_launch() {
    late_mutation_is_refused_before_launch(
        LateMutation::Question,
        "manager_v2_human_or_recovery_owner",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seat_rotation_after_early_gate_is_refused_before_launch() {
    late_mutation_is_refused_before_launch(LateMutation::Rotate, "manager_seat_tip_changed").await;
}
