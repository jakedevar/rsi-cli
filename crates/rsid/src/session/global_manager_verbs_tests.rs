//! Global manager v0 verb tests (#872 Slice B): they assert the stated intent
//! (scope, seat, appointment, mail, report-up and watch authority).

use super::*;
use crate::session::agent_verbs::tests::{control_handle_with_store, test_session};
use rsi_common::agent_control_schema::AgentControlVerbV1 as Verb;
use rsi_common::global_manager::{
    ConfigureGlobalManagerRequestV1, GLOBAL_LAUNCH_NOT_ALLOWED, GLOBAL_MANAGER_NOT_SEAT,
    GLOBAL_PROJECT_NOT_IN_GRANT, GLOBAL_REPORT_NOT_AUTHORIZED, GlobalManagerGrantV1,
};
use rsi_common::harness_manager_v2::{
    ManagerCapabilityV2, ManagerLaunchChoiceV2, ManagerOperatingModeV2, ManagerPolicyV2,
};
use rsi_common::types::{Project, SessionKind, SessionProvider, SessionStatus, WakeMode};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

type SharedStore = Arc<tokio::sync::Mutex<Store>>;

struct Portfolio {
    a: Uuid,
    b: Uuid,
    c: Uuid,
    seat: Uuid,
    pm_a: Uuid,
    pm_c: Uuid,
    worker: Uuid,
    grant: GlobalManagerGrantV1,
}

fn launch() -> ManagerLaunchChoiceV2 {
    ManagerLaunchChoiceV2 {
        provider: SessionProvider::Claude,
        model: "claude-opus-5-5".into(),
        effort: Some("high".into()),
    }
}

fn execute_policy() -> ManagerPolicyV2 {
    ManagerPolicyV2 {
        mode: ManagerOperatingModeV2::Execute,
        capabilities: vec![ManagerCapabilityV2::WorkPlan],
        ..ManagerPolicyV2::default()
    }
}

fn add_project(store: &Store, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    let now = chrono::Utc::now();
    store
        .insert_project(&Project {
            id,
            name: format!("{name} {id}"),
            path: None,
            description: None,
            color: Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        })
        .unwrap();
    id
}

fn add_session(store: &Store, id: Uuid, project: Option<Uuid>) -> Uuid {
    let mut row = test_session(id, PathBuf::from("/tmp/global-manager"));
    row.project_id = project;
    row.session_kind = SessionKind::Standard;
    row.status = SessionStatus::Running;
    store.insert_session(&row).unwrap();
    id
}

fn appoint(store: &Store, project: Uuid, session: Uuid) {
    let expected_row_version = store
        .get_harness_manager(project)
        .unwrap()
        .map_or(0, |config| config.row_version);
    store
        .configure_harness_manager(&ConfigureHarnessManagerRequestV1 {
            project_id: project,
            session_id: session,
            epic_ids: None,
            group_ids: vec![],
            expected_row_version,
        })
        .unwrap();
}

async fn portfolio(store: &SharedStore) -> Portfolio {
    let guard = store.lock().await;
    let (a, b, c) = (
        add_project(&guard, "A"),
        add_project(&guard, "B"),
        add_project(&guard, "C"),
    );
    let seat = add_session(&guard, Uuid::new_v4(), None);
    let pm_a = add_session(&guard, Uuid::new_v4(), Some(a));
    let pm_c = add_session(&guard, Uuid::new_v4(), Some(c));
    let worker = add_session(&guard, Uuid::new_v4(), Some(a));
    appoint(&guard, a, pm_a);
    appoint(&guard, c, pm_c);
    let grant = guard
        .configure_global_manager(
            &ConfigureGlobalManagerRequestV1 {
                session_id: seat,
                project_ids: vec![a, b],
                allowed_launches: vec![launch()],
                project_policy: execute_policy(),
                expected_grant_version: 0,
                idempotency_key: "grant-1".into(),
            },
            "operator:test",
        )
        .unwrap();
    Portfolio {
        a,
        b,
        c,
        seat,
        pm_a,
        pm_c,
        worker,
        grant,
    }
}

fn code(error: DaemonError) -> String {
    match error {
        DaemonError::InvalidParam(code) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn send(project_id: Uuid, key: &str) -> AgentGlobalSendRequestV1 {
    AgentGlobalSendRequestV1 {
        project_id,
        message: "Land #872 and report back.".into(),
        idempotency_key: key.into(),
    }
}

fn appoint_request(project_id: Uuid, key: &str) -> AgentGlobalAppointManagerRequestV1 {
    AgentGlobalAppointManagerRequestV1 {
        project_id,
        launch: launch(),
        query: "You are the project manager.".into(),
        idempotency_key: key.into(),
        sandbox: None,
    }
}

/// A launcher that inserts the preassigned session row, as the daemon's
/// launch funnel does, and counts its calls.
fn launcher(
    store: &SharedStore,
    project: Uuid,
    calls: &Arc<AtomicUsize>,
) -> impl FnOnce(Uuid) -> std::pin::Pin<Box<dyn Future<Output = Result<Uuid>> + Send>> {
    let store = Arc::clone(store);
    let calls = Arc::clone(calls);
    move |session_id| {
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            add_session(&*store.lock().await, session_id, Some(project));
            Ok(session_id)
        })
    }
}

async fn session_count(store: &SharedStore) -> i64 {
    store
        .lock()
        .await
        .conn
        .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn overview_lists_exactly_the_granted_projects_and_refuses_others() {
    let (control, store) = control_handle_with_store();
    let p = portfolio(&store).await;
    let overview = control
        .agent_global_overview(p.seat, AgentGlobalOverviewRequestV1 {})
        .await
        .unwrap();
    assert_eq!(overview.grant_version, p.grant.grant_version);
    assert_eq!(
        overview
            .projects
            .iter()
            .map(|project| project.project_id)
            .collect::<Vec<_>>(),
        [p.a, p.b]
    );
    let a = &overview.projects[0];
    assert_eq!(a.manager.as_ref().map(|seat| seat.session_id), Some(p.pm_a));
    assert_eq!(a.running_sessions, 2, "the PM and the worker of A run");
    assert!(overview.projects[1].manager.is_none());

    // Every verb is refused on project C.
    assert_eq!(
        code(
            control
                .agent_global_send(p.seat, send(p.c, "c"))
                .await
                .unwrap_err()
        ),
        GLOBAL_PROJECT_NOT_IN_GRANT
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let refused = global_appoint_manager_with(
        &store,
        p.seat,
        &appoint_request(p.c, "c"),
        launcher(&store, p.c, &calls),
    )
    .await
    .unwrap_err();
    assert_eq!(code(refused), GLOBAL_PROJECT_NOT_IN_GRANT);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn non_seat_revoked_grant_and_replaced_seat_are_refused() {
    let (control, store) = control_handle_with_store();
    let p = portfolio(&store).await;
    let overview = |caller| {
        let control = control.clone();
        async move {
            control
                .agent_global_overview(caller, AgentGlobalOverviewRequestV1 {})
                .await
        }
    };
    assert_eq!(
        code(overview(p.worker).await.unwrap_err()),
        GLOBAL_MANAGER_NOT_SEAT
    );
    assert_eq!(
        code(overview(p.pm_a).await.unwrap_err()),
        GLOBAL_MANAGER_NOT_SEAT
    );

    // Replace the seat: the old seat is refused, the new one served.
    let new_seat = add_session(&*store.lock().await, Uuid::new_v4(), None);
    let replaced = store
        .lock()
        .await
        .configure_global_manager(
            &ConfigureGlobalManagerRequestV1 {
                session_id: new_seat,
                project_ids: vec![p.a],
                allowed_launches: vec![launch()],
                project_policy: execute_policy(),
                expected_grant_version: p.grant.grant_version,
                idempotency_key: "grant-2".into(),
            },
            "operator:test",
        )
        .unwrap();
    assert_eq!(
        code(overview(p.seat).await.unwrap_err()),
        GLOBAL_MANAGER_NOT_SEAT
    );
    assert_eq!(
        code(
            control
                .agent_global_send(p.seat, send(p.a, "old"))
                .await
                .unwrap_err()
        ),
        GLOBAL_MANAGER_NOT_SEAT
    );
    assert!(overview(new_seat).await.is_ok());

    // Revoke: the seat is refused.
    store
        .lock()
        .await
        .revoke_global_manager(&rsi_common::global_manager::RevokeGlobalManagerRequestV1 {
            expected_grant_version: replaced.grant_version,
            idempotency_key: "revoke-1".into(),
        })
        .unwrap();
    assert_eq!(
        code(overview(new_seat).await.unwrap_err()),
        GLOBAL_MANAGER_NOT_SEAT
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn appoint_launches_appoints_and_saves_a_live_policy_and_replays() {
    let (_control, store) = control_handle_with_store();
    let p = portfolio(&store).await;
    let calls = Arc::new(AtomicUsize::new(0));

    // Project A has a PM: the appointment displaces it.
    let request = appoint_request(p.a, "gm-a-appoint-1");
    let result =
        global_appoint_manager_with(&store, p.seat, &request, launcher(&store, p.a, &calls))
            .await
            .unwrap();
    assert!(!result.deduplicated);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    {
        let guard = store.lock().await;
        let session = guard.get_session(result.session_id).unwrap().unwrap();
        assert_eq!(session.project_id, Some(p.a));
        let config = guard.get_harness_manager(p.a).unwrap().unwrap();
        assert_eq!(config.current_session_id, Some(result.session_id));
        assert_eq!(config.row_version, result.scope_version);
        let policy = guard.get_harness_manager_policy(p.a).unwrap().unwrap();
        assert!(!policy.revoked, "the saved policy is live");
        assert_eq!(policy.scope_version, result.scope_version);
        assert_eq!(policy.row_version, result.policy_version);
        assert_eq!(policy.policy, execute_policy());
    }

    // A replay returns the same session without a second launch.
    let replay =
        global_appoint_manager_with(&store, p.seat, &request, launcher(&store, p.a, &calls))
            .await
            .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.session_id, result.session_id);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Project B has no PM yet: the appointment creates one.
    let b = global_appoint_manager_with(
        &store,
        p.seat,
        &appoint_request(p.b, "gm-b-appoint-1"),
        launcher(&store, p.b, &calls),
    )
    .await
    .unwrap();
    let guard = store.lock().await;
    assert_eq!(
        guard
            .get_harness_manager(p.b)
            .unwrap()
            .unwrap()
            .current_session_id,
        Some(b.session_id)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn appoint_refuses_a_disallowed_launch_before_any_effect() {
    let (_control, store) = control_handle_with_store();
    let p = portfolio(&store).await;
    let before = session_count(&store).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut request = appoint_request(p.b, "gm-b-bad");
    request.launch.model = "claude-haiku-5-5".into();
    let error =
        global_appoint_manager_with(&store, p.seat, &request, launcher(&store, p.b, &calls))
            .await
            .unwrap_err();
    assert_eq!(code(error), GLOBAL_LAUNCH_NOT_ALLOWED);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(session_count(&store).await, before, "no session row");
    let appointments: i64 = store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT count(*) FROM global_manager_appointments",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(appointments, 0);
    // A non-seat is refused the same way.
    let error = global_appoint_manager_with(
        &store,
        p.worker,
        &appoint_request(p.b, "gm-b-worker"),
        launcher(&store, p.b, &calls),
    )
    .await
    .unwrap_err();
    assert_eq!(code(error), GLOBAL_MANAGER_NOT_SEAT);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn send_queues_a_wake_on_the_pm_seat_and_replays() {
    let (control, store) = control_handle_with_store();
    let p = portfolio(&store).await;
    let receipt = control
        .agent_global_send(p.seat, send(p.a, "route-1"))
        .await
        .unwrap();
    assert_eq!(receipt.target_session_id, p.pm_a);
    let job = store
        .lock()
        .await
        .get_scheduled_job(&receipt.message_id)
        .unwrap()
        .unwrap();
    assert_eq!(job.wake_session_id, Some(p.pm_a));
    assert_eq!(job.wake_mode, WakeMode::Resume);
    assert!(job.message.contains("Land #872 and report back."));
    let replay = control
        .agent_global_send(p.seat, send(p.a, "route-1"))
        .await
        .unwrap();
    assert_eq!(replay.message_id, receipt.message_id);
    assert!(replay.deduplicated);
    assert_eq!(
        code(
            control
                .agent_global_send(p.seat, send(p.b, "route-b"))
                .await
                .unwrap_err()
        ),
        rsi_common::global_manager::GLOBAL_PROJECT_HAS_NO_MANAGER
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn report_to_global_reaches_the_seat_only_from_a_granted_pm() {
    let (control, store) = control_handle_with_store();
    let p = portfolio(&store).await;
    let report = |key: &str| AgentReportToGlobalRequestV1 {
        message: "Landed #872.".into(),
        idempotency_key: key.into(),
    };
    let receipt = control
        .agent_report_to_global(p.pm_a, report("r1"))
        .await
        .unwrap();
    assert_eq!(receipt.target_session_id, p.seat);
    assert_eq!(receipt.project_id, p.a);
    let job = store
        .lock()
        .await
        .get_scheduled_job(&receipt.message_id)
        .unwrap()
        .unwrap();
    assert_eq!(job.wake_session_id, Some(p.seat));
    assert!(job.message.contains("Landed #872."));
    assert_eq!(
        code(
            control
                .agent_report_to_global(p.pm_c, report("r2"))
                .await
                .unwrap_err()
        ),
        GLOBAL_REPORT_NOT_AUTHORIZED,
        "the PM of an ungranted project"
    );
    assert_eq!(
        code(
            control
                .agent_report_to_global(p.worker, report("r3"))
                .await
                .unwrap_err()
        ),
        GLOBAL_REPORT_NOT_AUTHORIZED,
        "a non-PM"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn global_seat_may_watch_and_read_granted_pm_seats_only() {
    let (control, store) = control_handle_with_store();
    let p = portfolio(&store).await;
    control
        .authorize_watch_target(p.seat, p.pm_a)
        .await
        .unwrap();
    assert_eq!(
        control.agent_get_status(p.seat, p.pm_a).await.unwrap().id,
        p.pm_a
    );
    assert!(
        control
            .authorize_watch_target(p.seat, p.pm_c)
            .await
            .is_err()
    );
    assert!(
        control
            .authorize_watch_target(p.seat, p.worker)
            .await
            .is_err()
    );
    assert!(control.agent_get_status(p.seat, p.worker).await.is_err());
    // The authority is the seat's: a worker gains nothing over the PM.
    assert!(
        control
            .authorize_watch_target(p.worker, p.pm_a)
            .await
            .is_err()
    );
    // It is read-only: the seat cannot halt a PM.
    assert!(control.agent_halt(p.seat, p.pm_a).await.is_err());
    drop(store);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn catalog_advertises_the_global_role_to_the_seat_and_report_up_to_the_pm() {
    let (_control, store) = control_handle_with_store();
    let p = portfolio(&store).await;
    let guard = store.lock().await;
    let seat = guard.agent_authority_projection(p.seat).unwrap();
    assert!(seat.is_global_manager);
    assert!(seat.guidance_ids.contains(&"global_manager"));
    for verb in [
        Verb::GlobalOverview,
        Verb::GlobalSend,
        Verb::GlobalAppointManager,
    ] {
        assert!(seat.verbs.contains(&verb), "{verb:?}");
    }
    let catalog = crate::session::preamble::render_authority_catalog(p.seat, &seat, None).unwrap();
    assert!(catalog.roles.contains(&"global_manager".to_string()));
    let pm = guard.agent_authority_projection(p.pm_a).unwrap();
    assert!(pm.verbs.contains(&Verb::ReportToGlobal));
    assert!(pm.verbs.contains(&Verb::GetStatus));
    let ungranted_pm = guard.agent_authority_projection(p.pm_c).unwrap();
    assert!(ungranted_pm.verbs.contains(&Verb::GetStatus));
    assert!(!ungranted_pm.verbs.contains(&Verb::ReportToGlobal));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_grant_replaced_during_the_launch_refuses_the_appointment() {
    let (_control, store) = control_handle_with_store();
    let p = portfolio(&store).await;
    let launch_store = Arc::clone(&store);
    let grant_version = p.grant.grant_version;
    let seat = p.seat;
    let project = p.a;
    let error = global_appoint_manager_with(
        &store,
        p.seat,
        &appoint_request(p.a, "gm-a-race"),
        move |session_id| async move {
            let guard = launch_store.lock().await;
            add_session(&guard, session_id, Some(project));
            // The operator re-appoints the same seat mid-launch: a new grant.
            guard
                .configure_global_manager(
                    &ConfigureGlobalManagerRequestV1 {
                        session_id: seat,
                        project_ids: vec![project],
                        allowed_launches: vec![launch()],
                        project_policy: execute_policy(),
                        expected_grant_version: grant_version,
                        idempotency_key: "grant-race".into(),
                    },
                    "operator:test",
                )
                .unwrap();
            Ok(session_id)
        },
    )
    .await
    .unwrap_err();
    assert_eq!(code(error), GLOBAL_MANAGER_NOT_SEAT);
    let guard = store.lock().await;
    assert_eq!(
        guard
            .get_harness_manager(p.a)
            .unwrap()
            .unwrap()
            .current_session_id,
        Some(p.pm_a),
        "the PM is not displaced under a stale grant"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_crash_between_policy_save_and_completion_replays_cleanly() {
    let (_control, store) = control_handle_with_store();
    let p = portfolio(&store).await;
    let calls = Arc::new(AtomicUsize::new(0));
    store
        .lock()
        .await
        .conn
        .execute_batch(
            "CREATE TRIGGER test_crash_before_completion BEFORE UPDATE OF state ON global_manager_appointments
             WHEN NEW.state='appointed' BEGIN SELECT RAISE(ABORT,'injected crash'); END;",
        )
        .unwrap();
    let request = appoint_request(p.b, "gm-b-crash");
    let error =
        global_appoint_manager_with(&store, p.seat, &request, launcher(&store, p.b, &calls))
            .await
            .unwrap_err();
    assert!(error.to_string().contains("injected crash"), "{error}");
    let saved = store
        .lock()
        .await
        .get_harness_manager_policy(p.b)
        .unwrap()
        .unwrap();
    assert!(!saved.revoked, "the policy committed before the crash");
    store
        .lock()
        .await
        .conn
        .execute_batch("DROP TRIGGER test_crash_before_completion;")
        .unwrap();
    let replay =
        global_appoint_manager_with(&store, p.seat, &request, launcher(&store, p.b, &calls))
            .await
            .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1, "no second launch");
    assert_eq!(replay.policy_version, saved.row_version);
    assert_eq!(replay.scope_version, saved.scope_version);
    let guard = store.lock().await;
    let config = guard.get_harness_manager(p.b).unwrap().unwrap();
    assert_eq!(config.current_session_id, Some(replay.session_id));
    let policy = guard.get_harness_manager_policy(p.b).unwrap().unwrap();
    assert!(!policy.revoked);
    assert_eq!(
        policy.row_version, saved.row_version,
        "no second policy save"
    );
}
