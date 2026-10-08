//! Global manager v0 verb tests (#872 Slice B): they assert the stated intent
//! (scope, seat, appointment, mail, report-up and watch authority).

use super::*;
use crate::session::agent_verbs::tests::{control_handle_with_store, test_session};
use crate::session::{SessionManager, WatchFirePlan};
use rsi_common::agent_control_schema::AgentControlVerbV1 as Verb;
use rsi_common::global_manager::{
    ConfigureGlobalManagerRequestV1, GLOBAL_LAUNCH_NOT_ALLOWED, GLOBAL_MANAGER_NOT_SEAT,
    GLOBAL_PROJECT_NOT_IN_GRANT, GLOBAL_REPORT_NOT_AUTHORIZED, GlobalManagerGrantV1,
};
use rsi_common::harness_manager::ConfigureHarnessManagerRequestV1;
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
        // #1314: each appointed PM is a creation charged to the grantor.
        max_created_sessions: 8,
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
        // #1412: storage inherits the live grant; Inspect exposes the finite
        // effective launch list rather than the empty inheritance marker.
        assert_eq!(policy.policy, execute_policy());
        let inspected = guard
            .manager_v2_inspect(result.session_id, &Default::default())
            .unwrap();
        let mut handed = execute_policy();
        handed.allowed_launches = vec![launch()];
        assert_eq!(inspected.policy.unwrap().policy, handed);
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

/// #1314: both appointment verbs pass the grantor's manager resource gates
/// before any launch: a paused grantor policy, then a spent creation
/// allowance, refuse with no launch, session or appointment row.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn appointments_obey_the_grantors_resource_gates_before_any_launch() {
    let (_control, store) = control_handle_with_store();
    let p = portfolio(&store).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let regrant = |key: &str, edit: fn(&mut ManagerPolicyV2)| {
        let store = Arc::clone(&store);
        let key = key.to_string();
        async move {
            let guard = store.lock().await;
            let current = guard.global_seat_grant(p.seat).unwrap();
            let mut policy = execute_policy();
            edit(&mut policy);
            guard
                .configure_global_manager(
                    &ConfigureGlobalManagerRequestV1 {
                        session_id: p.seat,
                        project_ids: vec![p.a, p.b],
                        allowed_launches: vec![launch()],
                        project_policy: policy,
                        expected_grant_version: current.grant_version,
                        idempotency_key: key,
                    },
                    "operator:test",
                )
                .unwrap();
        }
    };
    let appointments = |store: &Store| -> i64 {
        store
            .conn
            .query_row(
                "SELECT count(*) FROM manager_portfolio_appointments",
                [],
                |row| row.get(0),
            )
            .unwrap()
    };
    regrant("grant-paused", |policy| policy.paused = true).await;
    let before = session_count(&store).await;
    let error = global_appoint_manager_with(
        &store,
        p.seat,
        &appoint_request(p.b, "gm-b-paused"),
        launcher(&store, p.b, &calls),
    )
    .await
    .unwrap_err();
    assert_eq!(code(error), "manager_v2_policy_paused");
    let child = AgentManagerAppointChildRequestV1::from(&appoint_request(p.b, "pm-b-paused"));
    let error = appoint_child_with(&store, p.seat, &child, |session_id, project| {
        launcher(&store, project, &calls)(session_id)
    })
    .await
    .unwrap_err();
    assert_eq!(code(error), "manager_v2_policy_paused");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(session_count(&store).await, before, "no session row");
    assert_eq!(appointments(&*store.lock().await), 0);

    regrant("grant-one-seat", |policy| policy.max_created_sessions = 1).await;
    global_appoint_manager_with(
        &store,
        p.seat,
        &appoint_request(p.b, "gm-b-first"),
        launcher(&store, p.b, &calls),
    )
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let error = global_appoint_manager_with(
        &store,
        p.seat,
        &appoint_request(p.b, "gm-b-second"),
        launcher(&store, p.b, &calls),
    )
    .await
    .unwrap_err();
    assert_eq!(code(error), "manager_v2_creation_limit");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the refusal launched nothing"
    );
    assert_eq!(appointments(&*store.lock().await), 1);
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
            "SELECT count(*) FROM manager_portfolio_appointments",
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
    // Without SessionControl in its policy the seat cannot halt its PM
    // (#1239 admits a direct child seat only with that capability).
    assert!(control.agent_halt(p.seat, p.pm_a).await.is_err());
    drop(store);
}

/// #1393: exercise native arming, not just the authorization helper. An
/// operator-appointed PM is observable without a SessionControl grant.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn portfolio_terminal_watch_arms_on_operator_pm_and_fires_on_terminal() {
    use crate::session::harness::tools::HarnessTool;

    let dir = crate::test_support::disk_backed_tempdir("portfolio-watch-");
    let manager = SessionManager::new(
        Arc::new(crate::bus::EventBus::new(16)),
        Store::open_in_memory().unwrap(),
        false,
        dir.path().join("daemon.sock"),
        None,
        Vec::new(),
        crate::config::RuntimeConfig::from_config(&crate::config::Config::from_env()),
        dir.path().join("sandboxes"),
    )
    .unwrap();
    let store = manager.store();
    let p = portfolio(store).await;
    let control = manager.agent_control();
    let tool = portfolio_watch_tool(&control, p.seat);
    let result = tool.execute(portfolio_watch_args(p.pm_a), dir.path()).await;
    assert!(result.success, "{:?}", result.error_msg);
    let job = store.lock().await.list_scheduled_jobs().unwrap().remove(0);
    assert_eq!(job.wake_mode, WakeMode::OnTerminal(p.pm_a));
    assert_eq!(job.wake_session_id, Some(p.seat));
    assert!(job.enabled);
    assert_eq!(
        manager.plan_terminal_watch_fire(&job).await.unwrap(),
        WatchFirePlan::NotReady,
        "a running PM does not fire its watch"
    );
    // The watch grants observation only; the fixture grants no SessionControl.
    assert!(control.agent_halt(p.seat, p.pm_a).await.is_err());
    store
        .lock()
        .await
        .update_session_status(p.pm_a, SessionStatus::Completed)
        .unwrap();
    match manager.plan_terminal_watch_fire(&job).await.unwrap() {
        WatchFirePlan::Deliver {
            tip,
            message,
            job_ids,
            ..
        } => {
            assert_eq!(tip, p.seat);
            assert_eq!(job_ids, vec![job.id]);
            assert!(message.contains("Completed"));
            assert!(message.contains(&p.pm_a.to_string()[..8]));
        }
        other => panic!("terminal PM must deliver its watch: {other:?}"),
    }
}

fn portfolio_watch_tool(
    control: &crate::session::agent_verbs::AgentControlHandle,
    caller: Uuid,
) -> crate::session::harness::tools::schedule_wake::ScheduleWakeTool {
    crate::session::harness::tools::schedule_wake::ScheduleWakeTool::new(
        Arc::clone(&control.store),
        Some(caller),
        PathBuf::from("/tmp/global-manager"),
        Some(SessionProvider::Claude),
        None,
        None,
        Some(control.clone()),
    )
}

fn portfolio_watch_args(target: Uuid) -> serde_json::Value {
    serde_json::json!({
        "mode": "on_terminal",
        "watch_session_id": target,
        "message": "Read the terminal seat's events.",
    })
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
            "CREATE TRIGGER test_crash_before_completion BEFORE UPDATE OF state ON manager_portfolio_appointments
             WHEN NEW.state='appointed' BEGIN SELECT RAISE(ABORT,'injected crash'); END;",
        )
        .unwrap();
    let request = appoint_request(p.b, "gm-b-crash");
    let error =
        global_appoint_manager_with(&store, p.seat, &request, launcher(&store, p.b, &calls))
            .await
            .unwrap_err();
    assert!(error.to_string().contains("injected crash"), "{error}");
    // #1289: the seat change, policy save and completion are one
    // transaction, so the crash left no PM and no policy behind.
    assert!(
        store
            .lock()
            .await
            .get_harness_manager(p.b)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .lock()
            .await
            .get_harness_manager_policy(p.b)
            .unwrap()
            .is_none()
    );
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
    let guard = store.lock().await;
    let config = guard.get_harness_manager(p.b).unwrap().unwrap();
    assert_eq!(config.current_session_id, Some(replay.session_id));
    let policy = guard.get_harness_manager_policy(p.b).unwrap().unwrap();
    assert!(!policy.revoked);
    assert_eq!(replay.policy_version, policy.row_version);
    assert_eq!(replay.scope_version, policy.scope_version);
}

// #1239 (hierarchy S5): delegation at every portfolio level.

/// Each finite allowance lower per level; the PM verb set at every
/// level, with SessionControl so a node may halt its direct child seats.
fn tier_policy(level: u16) -> ManagerPolicyV2 {
    // (containers, created, active) per level: five siblings at level 1
    // and two at levels 2 and 3 still sum below their parent (#1302), inside
    // the policy bounds (64, 1024, 1..=100).
    let (containers, created, active) =
        [(60, 1000, 96), (11, 190, 18), (5, 60, 6), (2, 20, 2)][usize::from(level)];
    ManagerPolicyV2 {
        mode: ManagerOperatingModeV2::Execute,
        capabilities: vec![
            ManagerCapabilityV2::IssueCoordinate,
            ManagerCapabilityV2::SessionCreate,
            ManagerCapabilityV2::SessionControl,
        ],
        max_created_containers: containers,
        max_created_sessions: created,
        max_active_sessions: active,
        ..ManagerPolicyV2::default()
    }
}

/// A global over [A, B, C] whose seat may delegate.
async fn delegating_global(store: &SharedStore) -> (Uuid, [Uuid; 3]) {
    let guard = store.lock().await;
    let projects = [
        add_project(&guard, "A"),
        add_project(&guard, "B"),
        add_project(&guard, "C"),
    ];
    let seat = add_session(&guard, Uuid::new_v4(), None);
    guard
        .configure_global_manager(
            &ConfigureGlobalManagerRequestV1 {
                session_id: seat,
                project_ids: projects.to_vec(),
                allowed_launches: vec![launch()],
                project_policy: tier_policy(0),
                expected_grant_version: 0,
                idempotency_key: "delegating-grant".into(),
            },
            "operator:test",
        )
        .unwrap();
    (seat, projects)
}

fn child_request(projects: &[Uuid], level: u16, key: &str) -> AgentManagerAppointChildRequestV1 {
    AgentManagerAppointChildRequestV1 {
        target: rsi_common::portfolio_delegation::AppointChildTargetV1::Portfolio {
            node_id: None,
            tier_label: Some("area-lead".into()),
            project_ids: projects.to_vec(),
            allowed_launches: Vec::new(),
            policy: Some(tier_policy(level)),
            child_policy: None,
            max_direct_reports: None,
            launch_project_id: None,
            expected_grant_version: None,
        },
        launch: launch(),
        query: "You manage your projects.".into(),
        idempotency_key: key.into(),
        sandbox: Some(false),
    }
}

/// A launcher that persists the reserved session in the project it is
/// handed, as the daemon's launch funnel does, and counts its calls.
fn child_launcher(
    store: &SharedStore,
    calls: &Arc<AtomicUsize>,
) -> impl FnOnce(Uuid, Uuid) -> std::pin::Pin<Box<dyn Future<Output = Result<Uuid>> + Send>> {
    let store = Arc::clone(store);
    let calls = Arc::clone(calls);
    move |session_id, project| {
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            add_session(&*store.lock().await, session_id, Some(project));
            Ok(session_id)
        })
    }
}

/// Issue AC1 through the daemon path: the child node is created over [B, C]
/// with its seat launched in B, a replay launches nothing, and an
/// overlapping sibling is refused before any launch.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn appoint_child_launches_a_narrower_child_node_and_replays() {
    let (_control, store) = control_handle_with_store();
    let (seat, [_a, b, c]) = delegating_global(&store).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let request = child_request(&[b, c], 1, "child-bc");
    let first = appoint_child_with(&store, seat, &request, child_launcher(&store, &calls))
        .await
        .unwrap();
    assert!(!first.deduplicated);
    let node_id = first.node_id.expect("a child node");
    assert_eq!(first.target_ref, format!("portfolio:{node_id}"));
    {
        let guard = store.lock().await;
        let node = guard.get_portfolio_node(node_id).unwrap().unwrap();
        assert_eq!(node.grant.seat_session_id, first.session_id);
        assert_eq!(node.grant.project_ids, vec![b, c]);
        assert_eq!(node.authority_epoch, first.scope_version);
        assert_eq!(node.grant.grant_version, first.policy_version);
        let session = guard.get_session(first.session_id).unwrap().unwrap();
        assert_eq!(session.project_id, Some(b));
    }
    let replay = appoint_child_with(&store, seat, &request, child_launcher(&store, &calls))
        .await
        .unwrap();
    assert!(replay.deduplicated);
    assert_eq!(replay.session_id, first.session_id);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "no second launch");
    let before = session_count(&store).await;
    let error = appoint_child_with(
        &store,
        seat,
        &child_request(&[c], 1, "child-c"),
        child_launcher(&store, &calls),
    )
    .await
    .unwrap_err();
    assert_eq!(
        code(error),
        rsi_common::portfolio_nodes::MANAGER_SCOPE_OVERLAP
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(session_count(&store).await, before, "no session row");
}

/// Plan §2.4 session control over child seats: a node reads and watches its
/// grandchild's seat, halts only its direct child seat, and its child never
/// reaches it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_node_controls_its_direct_child_seat_and_reads_its_grandchild() {
    let (control, store) = control_handle_with_store();
    let (seat, [_a, b, c]) = delegating_global(&store).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let child = appoint_child_with(
        &store,
        seat,
        &child_request(&[b, c], 1, "child"),
        child_launcher(&store, &calls),
    )
    .await
    .unwrap();
    let grandchild = appoint_child_with(
        &store,
        child.session_id,
        &child_request(&[c], 2, "grandchild"),
        child_launcher(&store, &calls),
    )
    .await
    .unwrap();
    control
        .authorize_watch_target(seat, grandchild.session_id)
        .await
        .unwrap();
    assert_eq!(
        control
            .agent_get_status(seat, grandchild.session_id)
            .await
            .unwrap()
            .id,
        grandchild.session_id
    );
    let denied = |error: DaemonError| error.to_string().contains("agent_verb_scope_denied");
    // The grandchild is not a direct child: no halt.
    assert!(denied(
        control
            .agent_halt(seat, grandchild.session_id)
            .await
            .unwrap_err()
    ));
    // The direct child seat is in reach (the halt passes authorization).
    if let Err(error) = control.agent_halt(seat, child.session_id).await {
        assert!(!denied(error), "the direct child seat is in reach");
    }
    // Never up: the child reaches neither its parent's seat for a read nor
    // for a halt.
    assert!(
        control
            .authorize_watch_target(child.session_id, seat)
            .await
            .is_err()
    );
    assert!(denied(
        control
            .agent_halt(child.session_id, seat)
            .await
            .unwrap_err()
    ));
}

/// #1393/#1239: native watches share the portfolio read reach, including a
/// descendant's published lineage tip. Siblings, ancestors and PM seats of
/// uncovered projects keep the same scope refusal and create no watch row.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn portfolio_terminal_watch_matches_descendant_and_pm_read_reach() {
    use crate::session::harness::tools::HarnessTool;

    let (control, store) = control_handle_with_store();
    let (seat, [a, b, c]) = delegating_global(&store).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let child = appoint_child_with(
        &store,
        seat,
        &child_request(&[b, c], 1, "child"),
        child_launcher(&store, &calls),
    )
    .await
    .unwrap();
    let sibling = appoint_child_with(
        &store,
        seat,
        &child_request(&[a], 1, "sibling"),
        child_launcher(&store, &calls),
    )
    .await
    .unwrap();
    let grandchild = appoint_child_with(
        &store,
        child.session_id,
        &child_request(&[c], 2, "grandchild"),
        child_launcher(&store, &calls),
    )
    .await
    .unwrap();
    let (tip, pm, uncovered_pm, sibling_tip) = {
        let guard = store.lock().await;
        let pm = add_session(&guard, Uuid::new_v4(), Some(b));
        appoint(&guard, b, pm); // Operator-appointed, not launched by this node.
        let uncovered = add_project(&guard, "uncovered");
        let uncovered_pm = add_session(&guard, Uuid::new_v4(), Some(uncovered));
        appoint(&guard, uncovered, uncovered_pm);
        let successor = |predecessor, project| {
            let mut row = test_session(Uuid::new_v4(), PathBuf::from("/tmp/global-manager"));
            row.project_id = Some(project);
            row.continued_from = Some(predecessor);
            row.status = SessionStatus::Running;
            guard.insert_session(&row).unwrap();
            guard
                .update_session_status(predecessor, SessionStatus::Completed)
                .unwrap();
            assert!(guard.transfer_global_seat(predecessor, row.id).unwrap());
            row.id
        };
        (
            successor(grandchild.session_id, c),
            pm,
            uncovered_pm,
            successor(sibling.session_id, a),
        )
    };
    for caller in [seat, child.session_id] {
        let tool = portfolio_watch_tool(&control, caller);
        for target in [grandchild.session_id, tip, pm] {
            control
                .agent_read_session_events(
                    caller,
                    serde_json::from_value(serde_json::json!({"session_id": target})).unwrap(),
                )
                .await
                .unwrap();
            let armed = tool
                .execute(portfolio_watch_args(target), std::path::Path::new("/tmp"))
                .await;
            assert!(
                armed.success,
                "{caller} watches {target}: {:?}",
                armed.error_msg
            );
            let guard = store.lock().await;
            assert!(guard.list_scheduled_jobs().unwrap().iter().any(|job| {
                job.enabled
                    && job.wake_session_id == Some(caller)
                    && job.wake_mode == WakeMode::OnTerminal(target)
            }));
        }
    }
    let tool = portfolio_watch_tool(&control, child.session_id);
    let before = store.lock().await.list_scheduled_jobs().unwrap().len();
    for target in [seat, sibling.session_id, sibling_tip, uncovered_pm] {
        let refused = tool
            .execute(portfolio_watch_args(target), std::path::Path::new("/tmp"))
            .await;
        assert!(!refused.success);
        assert!(
            refused
                .error_msg
                .unwrap()
                .contains("agent_verb_scope_denied")
        );
    }
    assert_eq!(
        store.lock().await.list_scheduled_jobs().unwrap().len(),
        before
    );
    // Observing a grandchild does not authorize control of it.
    let error = control.agent_halt(seat, tip).await.unwrap_err();
    assert!(error.to_string().contains("agent_verb_scope_denied"));
}

/// Issue AC6: the catalog lists the delegation verbs for portfolio seats,
/// not for a project manager.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn the_catalog_lists_delegation_verbs_for_portfolio_seats_only() {
    let (_control, store) = control_handle_with_store();
    let p = portfolio(&store).await;
    let guard = store.lock().await;
    let seat = guard.agent_authority_projection(p.seat).unwrap();
    let pm = guard.agent_authority_projection(p.pm_a).unwrap();
    for verb in [Verb::ManagerAppointChild, Verb::ManagerRevokeChild] {
        assert!(seat.verbs.contains(&verb), "{verb:?}");
        assert!(!pm.verbs.contains(&verb), "{verb:?}");
    }
}

/// `AgentManagerRevokeChild` through the control handle: a node-granted
/// child is revoked, an operator-granted one is refused.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn revoke_child_revokes_a_node_granted_child_only() {
    let (control, store) = control_handle_with_store();
    let (seat, [a, b, c]) = delegating_global(&store).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let child = appoint_child_with(
        &store,
        seat,
        &child_request(&[b, c], 1, "child"),
        child_launcher(&store, &calls),
    )
    .await
    .unwrap();
    let operator_child = {
        let guard = store.lock().await;
        let global = guard.portfolio_node_for_seat(seat).unwrap().unwrap();
        let operator_seat = add_session(&guard, Uuid::new_v4(), None);
        guard
            .configure_portfolio_node(
                &rsi_common::portfolio_nodes::ConfigurePortfolioNodeRequestV1 {
                    node_id: None,
                    parent_node_id: Some(global.node_id),
                    adopt_node_ids: Vec::new(),
                    expected_parent_grant_version: None,
                    tier_label: "ops".into(),
                    seat_session_id: operator_seat,
                    project_ids: vec![a],
                    allowed_launches: vec![launch()],
                    policy: tier_policy(1),
                    child_policy: None,
                    max_direct_reports: 5,
                    expected_node_grant_version: 0,
                    expected_authority_epoch: 0,
                    idempotency_key: "operator-child".into(),
                },
                crate::store::portfolio_nodes::PortfolioGrantor::Operator,
                "operator:test",
            )
            .unwrap()
    };
    let refused = control
        .agent_manager_revoke_child(
            seat,
            AgentManagerRevokeChildRequestV1 {
                node_id: operator_child.node_id,
                expected_grant_version: operator_child.grant.grant_version,
                idempotency_key: "revoke-ops".into(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(
        code(refused),
        rsi_common::portfolio_delegation::MANAGER_CHILD_OPERATOR_GRANTED
    );
    let revoked = control
        .agent_manager_revoke_child(
            seat,
            AgentManagerRevokeChildRequestV1 {
                node_id: child.node_id.unwrap(),
                expected_grant_version: child.policy_version,
                idempotency_key: "revoke-child".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(revoked.state, "revoked");
    assert_eq!(revoked.revoked, vec![child.node_id.unwrap()]);
    assert!(!revoked.deduplicated);
}

/// #1288: an unfinished appointment's replay is authorized again before it
/// launches; after an operator re-grant of the grantor it is refused and
/// nothing launches.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn a_stale_unfinished_replay_never_launches() {
    let (_control, store) = control_handle_with_store();
    let (seat, [a, _b, _c]) = delegating_global(&store).await;
    let request = AgentManagerAppointChildRequestV1::from(&appoint_request(a, "pm-a"));
    // Admitted, then the launch never happened (a crash before it).
    let admitted = store
        .lock()
        .await
        .begin_child_appointment(seat, &request)
        .unwrap();
    {
        let guard = store.lock().await;
        let grant = guard.active_global_grant().unwrap().unwrap();
        guard
            .configure_global_manager(
                &ConfigureGlobalManagerRequestV1 {
                    session_id: seat,
                    project_ids: grant.project_ids.clone(),
                    allowed_launches: vec![launch()],
                    project_policy: tier_policy(0),
                    expected_grant_version: grant.grant_version,
                    idempotency_key: "regrant".into(),
                },
                "operator:test",
            )
            .unwrap();
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let error = appoint_child_with(&store, seat, &request, child_launcher(&store, &calls))
        .await
        .unwrap_err();
    assert_eq!(code(error), GLOBAL_MANAGER_NOT_SEAT);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no launch");
    assert!(
        store
            .lock()
            .await
            .get_session(admitted.session_id)
            .unwrap()
            .is_none()
    );
}

/// #1238: the agent verbs route one parent up and to a descendant down
/// through the control handle; the catalog advertises them to node seats and
/// the escalation verbs to the portfolio seat, never to a worker.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn report_up_and_send_down_route_through_the_handle_and_the_catalog() {
    use rsi_common::manager_tier_routing::{
        AgentReportUpRequestV1, AgentSendDownRequestV1, MANAGER_TIER_NOT_NODE_SEAT,
        ManagerNodeRefV1, OPERATOR_REF,
    };
    let (control, store) = control_handle_with_store();
    let p = portfolio(&store).await;
    let up = control
        .agent_report_up(
            p.pm_a,
            AgentReportUpRequestV1 {
                message: "Landed #1238.".into(),
                idempotency_key: "up-1".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(up.target_session_id, Some(p.seat));
    // The seat is a root: its report is an operator notice.
    let top = control
        .agent_report_up(
            p.seat,
            AgentReportUpRequestV1 {
                message: "Portfolio green.".into(),
                idempotency_key: "up-2".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(top.target_ref, OPERATOR_REF);
    let down = control
        .agent_send_down(
            p.seat,
            AgentSendDownRequestV1 {
                target: ManagerNodeRefV1::Project { project_id: p.a },
                message: "Status?".into(),
                idempotency_key: "down-1".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(down.target_session_id, Some(p.pm_a));
    assert_eq!(
        code(
            control
                .agent_report_up(
                    p.worker,
                    AgentReportUpRequestV1 {
                        message: "hi".into(),
                        idempotency_key: "up-3".into(),
                    },
                )
                .await
                .unwrap_err()
        ),
        MANAGER_TIER_NOT_NODE_SEAT
    );
    let guard = store.lock().await;
    let pm = guard.agent_authority_projection(p.pm_a).unwrap();
    let seat = guard.agent_authority_projection(p.seat).unwrap();
    let worker = guard.agent_authority_projection(p.worker).unwrap();
    for verb in [Verb::ReportUp, Verb::SendDown] {
        assert!(pm.verbs.contains(&verb), "{verb:?}");
        assert!(seat.verbs.contains(&verb), "{verb:?}");
        assert!(!worker.verbs.contains(&verb), "{verb:?}");
    }
    for verb in [Verb::ManagerListEscalations, Verb::ManagerResolveEscalation] {
        assert!(seat.verbs.contains(&verb), "{verb:?}");
    }
}

/// #1627: the workspace seat lists the sessions it rotated through, newest
/// first, bounded by `SEAT_PREDECESSOR_LIMIT`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test]
async fn workspace_seat_lists_its_rotation_lineage_bounded() {
    let (control, store) = control_handle_with_store();
    let p = portfolio(&store).await;
    let before = control.operator_global_workspace().await.unwrap();
    assert!(before.seat.unwrap().predecessors.is_empty());

    let mut chain = vec![p.seat];
    for _ in 0..(SEAT_PREDECESSOR_LIMIT + 2) {
        let guard = store.lock().await;
        let previous = *chain.last().unwrap();
        let mut row = test_session(Uuid::new_v4(), PathBuf::from("/tmp/global-manager"));
        row.continued_from = Some(previous);
        row.status = SessionStatus::Running;
        guard.insert_session(&row).unwrap();
        guard
            .update_session_status(previous, SessionStatus::Completed)
            .unwrap();
        assert!(guard.transfer_global_seat(previous, row.id).unwrap());
        chain.push(row.id);
    }
    let workspace = control.operator_global_workspace().await.unwrap();
    let seat = workspace.seat.unwrap();
    assert_eq!(Some(&seat.session_id), chain.last());
    let expected: Vec<Uuid> = chain
        .iter()
        .rev()
        .skip(1)
        .take(SEAT_PREDECESSOR_LIMIT)
        .copied()
        .collect();
    assert_eq!(
        seat.predecessors
            .iter()
            .map(|prior| prior.session_id)
            .collect::<Vec<_>>(),
        expected
    );
    assert!(seat.predecessors_truncated);
    assert_eq!(seat.predecessors[0].status, SessionStatus::Completed);
}
