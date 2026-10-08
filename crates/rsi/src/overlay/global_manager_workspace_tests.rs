//! Tests for the global manager workspace (#1213).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rsi_common::global_manager::{
    GlobalIssueCountsV1, GlobalManagerGrantV1, GlobalManagerWorkspaceV1, GlobalPmPolicyV1,
    GlobalPmSeatV1, GlobalProjectOverviewV1, GlobalSeatSessionV1, GlobalWorkspaceProjectV1,
    SeatPredecessorV1,
};
use rsi_common::harness_manager_v2::{
    ManagerLaunchChoiceV2, ManagerOperatingModeV2, ManagerPolicyV2,
};
use rsi_common::types::{Project, SessionKind, SessionProvider, SessionStatus};
use serde_json::Value;
use uuid::Uuid;

use super::*;
use crate::app::app_test_helpers::{baseline_session, with_session_list};
use crate::overlay::global_manager_workspace_launch::LaunchField;
use crate::types::{Pane, PopupMode, SessionState};

pub(crate) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(crate) fn project(name: &str) -> Project {
    let now = Utc::now();
    Project {
        id: Uuid::new_v4(),
        name: name.into(),
        path: None,
        description: None,
        color: Project::DEFAULT_COLOR.into(),
        context_files: None,
        created_at: now,
        updated_at: now,
    }
}

pub(crate) fn grant(seat: Uuid, projects: &[Uuid], state: &str) -> GlobalManagerGrantV1 {
    GlobalManagerGrantV1 {
        grant_id: Uuid::new_v4(),
        grant_version: 3,
        seat_session_id: seat,
        state: state.into(),
        project_ids: projects.to_vec(),
        allowed_launches: vec![ManagerLaunchChoiceV2 {
            provider: SessionProvider::Claude,
            model: "claude-opus-5-5".into(),
            effort: Some("high".into()),
        }],
        project_policy: ManagerPolicyV2 {
            mode: ManagerOperatingModeV2::Execute,
            ..ManagerPolicyV2::default()
        },
        operator_origin: "operator_rpc".into(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

pub(crate) fn seat(
    id: Uuid,
    project_id: Option<Uuid>,
    status: SessionStatus,
) -> GlobalSeatSessionV1 {
    GlobalSeatSessionV1 {
        session_id: id,
        project_id,
        status,
        provider: SessionProvider::Claude,
        model: Some("claude-opus-5-5".into()),
        context_fill_pct: Some(37.4),
        cost_usd: Some(4.2),
        updated_at: Utc::now(),
        pending_question: false,
        predecessors: Vec::new(),
        predecessors_truncated: false,
    }
}

pub(crate) fn pm(id: Uuid, status: SessionStatus) -> GlobalPmSeatV1 {
    GlobalPmSeatV1 {
        session_id: id,
        status,
        provider: SessionProvider::Claude,
        model: Some("claude-opus-5-5".into()),
        effort: Some("high".into()),
        context_fill_pct: Some(52.0),
        cost_usd: Some(1.5),
        updated_at: Utc::now(),
        scope_version: 2,
        pending_question: false,
    }
}

pub(crate) fn policy(paused: bool, revoked: bool) -> GlobalPmPolicyV1 {
    GlobalPmPolicyV1 {
        effective_caps: None,
        policy_version: 1,
        mode: ManagerOperatingModeV2::Execute,
        revoked,
        paused,
        capabilities: vec![],
    }
}

pub(crate) fn portfolio(
    project: &Project,
    manager: Option<GlobalPmSeatV1>,
    policy: Option<GlobalPmPolicyV1>,
    scope_revoked: bool,
) -> GlobalWorkspaceProjectV1 {
    GlobalWorkspaceProjectV1 {
        overview: GlobalProjectOverviewV1 {
            project_id: project.id,
            name: project.name.clone(),
            path: None,
            manager,
            policy,
            issues: GlobalIssueCountsV1 {
                open: 12,
                in_progress: 3,
                open_operator_requests: 1,
            },
            running_sessions: 5,
            waiting_approval_sessions: 1,
            pending_questions: 0,
            pending_approvals: 2,
        },
        scope_revoked,
    }
}

/// A two-project portfolio: the seat lives in `b`, `a` has a running PM and
/// `b` has none.
pub(crate) struct Fixture {
    pub a: Project,
    pub b: Project,
    pub seat_id: Uuid,
    pub pm_id: Uuid,
    pub snapshot: GlobalManagerWorkspaceV1,
}

pub(crate) fn fixture() -> Fixture {
    let (a, b) = (project("rsi"), project("dictate-agent"));
    let (seat_id, pm_id) = (Uuid::new_v4(), Uuid::new_v4());
    let snapshot = GlobalManagerWorkspaceV1 {
        grant: Some(grant(seat_id, &[a.id, b.id], "active")),
        seat: Some(seat(seat_id, Some(b.id), SessionStatus::Completed)),
        projects: vec![
            portfolio(
                &a,
                Some(pm(pm_id, SessionStatus::Running)),
                Some(policy(false, false)),
                false,
            ),
            portfolio(&b, None, None, false),
        ],
        missing_project_ids: vec![],
    };
    Fixture {
        a,
        b,
        seat_id,
        pm_id,
        snapshot,
    }
}

/// An app on project `a`'s tab with both projects and both seat sessions.
fn app_on_a(fixture: &Fixture) -> App {
    let mut app = with_session_list(0);
    app.projects = vec![fixture.a.clone(), fixture.b.clone()];
    app.tabs[0].project_id = Some(fixture.a.id);
    app.sync_project_filter();
    for (id, project_id) in [
        (fixture.seat_id, fixture.b.id),
        (fixture.pm_id, fixture.a.id),
    ] {
        let mut session = baseline_session(id, SessionKind::Standard);
        session.project_id = Some(project_id);
        app.sessions.insert(id, SessionState::new(session));
    }
    app
}

fn open_with(app: &mut App, snapshot: GlobalManagerWorkspaceV1) {
    let mut state = GlobalManagerWorkspaceState::default();
    state.install(snapshot, Utc::now());
    app.overlay = OverlayState::GlobalManagerWorkspace(Box::new(state));
}

fn workspace(app: &App) -> &GlobalManagerWorkspaceState {
    match &app.overlay {
        OverlayState::GlobalManagerWorkspace(state) => state,
        _ => panic!("the global manager workspace is open"),
    }
}

/// A scripted daemon: answers each method from `responses` (an error when a
/// method is missing) and records the methods and params it saw. It accepts
/// any number of connections (the PM appointment opens its own client).
pub(crate) struct FakeDaemon {
    pub responses: Arc<Mutex<HashMap<String, Value>>>,
    pub seen: Arc<Mutex<Vec<String>>>,
    pub params: Arc<Mutex<Vec<(String, Value)>>>,
}

pub(crate) async fn fake_daemon(app: &mut App) -> FakeDaemon {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let path = crate::test_support::short_socket_path("global-workspace");
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let responses: Arc<Mutex<HashMap<String, Value>>> = Arc::default();
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let params: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let (served, log, logged_params) = (responses.clone(), seen.clone(), params.clone());
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (served, log, logged_params) = (served.clone(), log.clone(), logged_params.clone());
            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let request: Value = serde_json::from_str(&line).unwrap();
                    let method = request["method"].as_str().unwrap_or_default().to_string();
                    log.lock().unwrap().push(method.clone());
                    logged_params
                        .lock()
                        .unwrap()
                        .push((method.clone(), request["params"].clone()));
                    let answer = served.lock().unwrap().get(&method).cloned();
                    let response = match answer {
                        // #1544: refuse with `message` until the request
                        // carries confirm_cap_reductions:true, then answer.
                        Some(result) if result.get("__refuse_unless_confirmed").is_some() => {
                            if request["params"]["confirm_cap_reductions"] == true {
                                serde_json::json!({"jsonrpc":"2.0","id":request["id"],
                                    "result":result["result"]})
                            } else {
                                serde_json::json!({"jsonrpc":"2.0","id":request["id"],
                                    "error":{"code":-32602,"message":result["__refuse_unless_confirmed"]}})
                            }
                        }
                        Some(result) => {
                            serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                        }
                        None => serde_json::json!({"jsonrpc":"2.0","id":request["id"],
                            "error":{"code":-32601,"message":format!("no route for {method}")}}),
                    };
                    if writer
                        .write_all(format!("{response}\n").as_bytes())
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    app.client = crate::client::DaemonClient::new(path);
    app.client.connect().await.unwrap();
    FakeDaemon {
        responses,
        seen,
        params,
    }
}

impl FakeDaemon {
    pub(crate) fn serve(&self, method: &str, result: Value) {
        self.responses.lock().unwrap().insert(method.into(), result);
    }

    /// Refuse `method` with `message` until its params carry
    /// `confirm_cap_reductions:true`, then answer `result` (#1544).
    pub(crate) fn serve_cap_refusal(&self, method: &str, message: &str, result: Value) {
        self.serve(
            method,
            serde_json::json!({"__refuse_unless_confirmed": message, "result": result}),
        );
    }

    pub(crate) fn calls(&self, method: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| *seen == method)
            .count()
    }

    /// The params of the last call to `method`.
    pub(crate) fn last_params(&self, method: &str) -> Option<Value> {
        self.params
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(seen, _)| seen == method)
            .map(|(_, params)| params.clone())
    }
}

#[test]
fn seat_health_distinguishes_revoked_missing_waiting_and_live_seats() {
    let id = Uuid::new_v4();
    let active = grant(id, &[], "active");
    assert_eq!(
        seat_health(
            &grant(id, &[], "revoked"),
            Some(&seat(id, None, SessionStatus::Running))
        ),
        SeatHealth::Revoked
    );
    assert_eq!(seat_health(&active, None), SeatHealth::Missing);
    let mut waiting = seat(id, None, SessionStatus::Completed);
    waiting.pending_question = true;
    assert_eq!(seat_health(&active, Some(&waiting)), SeatHealth::Waiting);
    for (status, health) in [
        (SessionStatus::WaitingApproval, SeatHealth::Waiting),
        (SessionStatus::Running, SeatHealth::Active),
        (SessionStatus::Completed, SeatHealth::Idle),
        (SessionStatus::Failed, SeatHealth::Stopped),
        (SessionStatus::Archived, SeatHealth::Missing),
    ] {
        assert_eq!(seat_health(&active, Some(&seat(id, None, status))), health);
    }
}

#[test]
fn pm_health_distinguishes_missing_paused_waiting_and_revoked_seats() {
    let p = project("p");
    let id = Uuid::new_v4();
    let running = || Some(pm(id, SessionStatus::Running));
    assert_eq!(
        pm_health(&portfolio(&p, None, None, false)),
        SeatHealth::Missing
    );
    assert_eq!(
        pm_health(&portfolio(&p, None, None, true)),
        SeatHealth::Revoked,
        "a revoked scope"
    );
    assert_eq!(
        pm_health(&portfolio(&p, running(), Some(policy(false, true)), false)),
        SeatHealth::Revoked,
        "a revoked policy"
    );
    assert_eq!(
        pm_health(&portfolio(&p, running(), Some(policy(true, false)), false)),
        SeatHealth::Paused
    );
    let mut asking = pm(id, SessionStatus::Running);
    asking.pending_question = true;
    assert_eq!(
        pm_health(&portfolio(
            &p,
            Some(asking),
            Some(policy(false, false)),
            false
        )),
        SeatHealth::Waiting
    );
    assert_eq!(
        pm_health(&portfolio(&p, running(), Some(policy(false, false)), false)),
        SeatHealth::Active
    );
}

#[test]
fn rows_list_the_seat_projects_and_missing_projects_in_order() {
    let mut f = fixture();
    let gone = Uuid::new_v4();
    f.snapshot.missing_project_ids.push(gone);
    let mut state = GlobalManagerWorkspaceState::default();
    assert!(state.rows().is_empty(), "nothing before the first snapshot");
    state.install(f.snapshot.clone(), Utc::now());
    assert_eq!(
        state.rows(),
        vec![
            WorkspaceRow::Seat,
            WorkspaceRow::Project(0),
            WorkspaceRow::Project(1),
            WorkspaceRow::MissingProject(0),
        ]
    );
    assert!(missing_project_text(gone).contains("no longer exists"));
    assert_eq!(state.seat_session_id(), Some(f.seat_id));

    let mut none = GlobalManagerWorkspaceState::default();
    none.install(GlobalManagerWorkspaceV1::default(), Utc::now());
    assert_eq!(none.rows(), Vec::<WorkspaceRow>::new());
    assert_eq!(
        none.grant_line(&[]),
        "No global manager is appointed.".to_string()
    );
}

#[test]
fn install_keeps_the_selection_on_the_same_project_after_a_refresh() {
    let f = fixture();
    let mut state = GlobalManagerWorkspaceState::default();
    state.install(f.snapshot.clone(), Utc::now());
    state.selected = 2; // project b
    let mut reordered = f.snapshot.clone();
    reordered.projects.reverse();
    state.error = Some("Refresh failed".into());
    state.install(reordered, Utc::now());
    assert_eq!(state.selected_row(), Some(WorkspaceRow::Project(0)));
    assert_eq!(
        state.project(0).map(|p| p.overview.project_id),
        Some(f.b.id)
    );
    assert_eq!(state.error, None, "a good refresh clears the stale error");
}

#[test]
fn row_texts_carry_health_model_and_portfolio_signals() {
    let f = fixture();
    let mut state = GlobalManagerWorkspaceState::default();
    state.install(f.snapshot.clone(), Utc::now());
    let seats = state.seats();
    let global = seat_row_text(&seats[0], 96);
    for needle in ["global manager", "IDLE", "claude-opus-5-5", "37%", "24/6/2"] {
        assert!(global.contains(needle), "{needle} in {global}");
    }
    let row = seat_row_text(&seats[1], 96);
    for needle in ["rsi", "ACTIVE", "claude-opus-5-5", "52%", "12/3/1"] {
        assert!(row.contains(needle), "{needle} in {row}");
    }
    assert!(seat_row_text(&seats[2], 46).contains("MISSING"));
    let summary = seat_summary(&seats[1]).join("\n");
    for needle in [
        "PM · rsi",
        "ACTIVE",
        "claude-opus-5-5/high",
        "ctx 52%",
        "$1.50",
        "Issues 12 open, 3 in progress, 1 operator request",
        "5 running, 1 waiting, 0 questions, 2 approvals",
    ] {
        assert!(summary.contains(needle), "{needle} in {summary}");
    }
    let names = state.grant_line(&[f.a.clone(), f.b.clone()]);
    for needle in [
        "Grant v3 active",
        "2 projects",
        "rsi, dictate-agent",
        "Execute",
    ] {
        assert!(names.contains(needle), "{needle} in {names}");
    }
}

#[test]
fn list_columns_grow_with_the_list_width() {
    let f = fixture();
    let mut state = GlobalManagerWorkspaceState::default();
    state.install(f.snapshot.clone(), Utc::now());
    let seat = &state.seats()[1];
    let narrow = seat_row_text(seat, 46);
    let wide = seat_row_text(seat, 96);
    assert!(narrow.chars().count() <= 46, "{narrow}");
    assert!(seat_header(46).contains("ISSUES"));
    assert!(seat_header(96).contains("MODEL"));
    assert!(seat_header(96).contains("APPR"));
    assert!(seat_header(96).contains("UPDATED"));
    assert!(wide.contains("claude-opus-5-5"), "{wide}");
}

#[test]
fn seats_carry_level_depth_and_the_session_the_conversation_binds_to() {
    let mut f = fixture();
    let gone = Uuid::new_v4();
    f.snapshot.missing_project_ids.push(gone);
    let mut state = GlobalManagerWorkspaceState::default();
    state.install(f.snapshot.clone(), Utc::now());
    let seats = state.seats();
    assert_eq!(
        seats
            .iter()
            .map(|s| (s.level, s.depth, s.session_id))
            .collect::<Vec<_>>(),
        vec![
            (SeatLevel::Global, 0, Some(f.seat_id)),
            (SeatLevel::Project, 1, Some(f.pm_id)),
            (SeatLevel::Project, 1, None),
            (SeatLevel::Project, 1, None),
        ]
    );
    assert_eq!(state.conversation_session_id(), Some(f.seat_id));
    state.selected = 1;
    assert_eq!(state.conversation_session_id(), Some(f.pm_id));
    state.selected = 2;
    assert_eq!(state.conversation_session_id(), None, "b has no PM");
    assert_eq!(
        seats[2].health.action().map(|a| a.contains("Press n")),
        Some(true)
    );
    assert!(state.select_where(|seat| seat.level == SeatLevel::Global));
    assert_eq!(state.selected, 0);
}

#[tokio::test]
async fn enter_on_the_seat_opens_the_global_manager_session_on_its_project_tab() {
    let f = fixture();
    let mut app = app_on_a(&f);
    open_with(&mut app, f.snapshot.clone());
    handle_key(&mut app, key(KeyCode::Enter)).await;
    assert!(matches!(app.overlay, OverlayState::None));
    assert_eq!(app.active_project_id(), Some(f.b.id));
    assert!(matches!(
        app.focused_pane(),
        Some(Pane::SessionDetail { session_id }) if *session_id == f.seat_id
    ));
}

#[tokio::test]
async fn talk_selects_the_global_seat_and_types_to_it_in_place() {
    let f = fixture();
    let mut app = app_on_a(&f);
    let tab = app.active_tab;
    open_with(&mut app, f.snapshot.clone());
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    handle_key(&mut app, key(KeyCode::Char('t'))).await;
    let state = workspace(&app);
    assert_eq!(state.selected_row(), Some(WorkspaceRow::Seat));
    assert_eq!(state.focus, WorkspaceFocus::Input);
    assert_eq!(app.active_tab, tab, "no tab switch");
    assert_eq!(
        app.sessions[&f.seat_id].input_bar.surface.mode,
        PopupMode::Insert
    );
}

#[tokio::test]
async fn enter_on_a_project_opens_its_pm_or_its_session_list() {
    let f = fixture();
    let mut app = app_on_a(&f);
    open_with(&mut app, f.snapshot.clone());
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    handle_key(&mut app, key(KeyCode::Enter)).await;
    assert_eq!(app.active_project_id(), Some(f.a.id));
    assert!(matches!(
        app.focused_pane(),
        Some(Pane::SessionDetail { session_id }) if *session_id == f.pm_id
    ));

    // Project b has no PM: Enter lands on b's session list.
    let mut app = app_on_a(&f);
    open_with(&mut app, f.snapshot.clone());
    handle_key(&mut app, key(KeyCode::Char('G'))).await;
    handle_key(&mut app, key(KeyCode::Enter)).await;
    assert!(matches!(app.overlay, OverlayState::None));
    assert_eq!(app.active_project_id(), Some(f.b.id));
    assert!(matches!(app.focused_pane(), Some(Pane::SessionList { .. })));
}

#[tokio::test]
async fn p_opens_the_selected_projects_tab_and_missing_rows_explain_themselves() {
    let mut f = fixture();
    f.snapshot.missing_project_ids.push(Uuid::new_v4());
    let mut app = app_on_a(&f);
    open_with(&mut app, f.snapshot.clone());
    handle_key(&mut app, key(KeyCode::Char('G'))).await;
    handle_key(&mut app, key(KeyCode::Enter)).await;
    let error = workspace(&app).error.clone().unwrap();
    assert!(error.contains("no longer exists"), "{error}");
    assert!(error.contains(LAUNCH_HINT), "{error}");
    handle_key(&mut app, key(KeyCode::Char('k'))).await;
    handle_key(&mut app, key(KeyCode::Char('p'))).await;
    assert!(matches!(app.overlay, OverlayState::None));
    assert_eq!(app.active_project_id(), Some(f.b.id));
}

#[tokio::test]
async fn without_a_grant_enter_and_talk_point_at_launching_one() {
    let mut app = with_session_list(1);
    open_with(&mut app, GlobalManagerWorkspaceV1::default());
    handle_key(&mut app, key(KeyCode::Enter)).await;
    let error = workspace(&app).error.clone().unwrap();
    assert!(error.contains(LAUNCH_HINT), "{error}");
    assert!(error.contains(APPOINT_HINT), "{error}");
    handle_key(&mut app, key(KeyCode::Char('t'))).await;
    assert!(
        workspace(&app)
            .error
            .as_deref()
            .is_some_and(|error| error.contains(LAUNCH_HINT))
    );
    handle_key(&mut app, key(KeyCode::Esc)).await;
    assert!(matches!(app.overlay, OverlayState::None));
}

#[tokio::test]
async fn open_loads_and_refresh_shows_a_revoked_grant() {
    let f = fixture();
    let mut app = app_on_a(&f);
    let daemon = fake_daemon(&mut app).await;
    daemon.serve(
        "GetGlobalManagerWorkspace",
        serde_json::to_value(&f.snapshot).unwrap(),
    );
    open(&mut app).await;
    assert_eq!(workspace(&app).rows().len(), 3);
    assert_eq!(
        workspace(&app).grant().map(|g| g.state.as_str()),
        Some("active")
    );

    let mut revoked = f.snapshot.clone();
    revoked.grant.as_mut().unwrap().state = "revoked".into();
    daemon.serve(
        "GetGlobalManagerWorkspace",
        serde_json::to_value(&revoked).unwrap(),
    );
    handle_key(&mut app, key(KeyCode::Char('r'))).await;
    let state = workspace(&app);
    assert_eq!(
        seat_health(state.grant().unwrap(), None),
        SeatHealth::Revoked
    );
    assert_eq!(daemon.calls("GetGlobalManagerWorkspace"), 2);

    // `gm` again focuses the open view and reloads it.
    open(&mut app).await;
    assert_eq!(daemon.calls("GetGlobalManagerWorkspace"), 3);
}

#[tokio::test]
async fn a_failed_refresh_keeps_the_last_snapshot_and_says_how_to_retry() {
    let f = fixture();
    let mut app = app_on_a(&f);
    let daemon = fake_daemon(&mut app).await;
    daemon.serve(
        "GetGlobalManagerWorkspace",
        serde_json::to_value(&f.snapshot).unwrap(),
    );
    open(&mut app).await;
    daemon.responses.lock().unwrap().clear();
    refresh(&mut app).await;
    let state = workspace(&app);
    assert_eq!(state.rows().len(), 3, "the last snapshot stays on screen");
    let error = state.error.clone().unwrap();
    assert!(error.contains("showing the last snapshot"), "{error}");
    assert!(error.contains("press r to retry"), "{error}");
}

#[tokio::test]
async fn a_manager_command_refreshes_the_open_workspace() {
    let f = fixture();
    let mut app = app_on_a(&f);
    let daemon = fake_daemon(&mut app).await;
    daemon.serve(
        "GetGlobalManagerWorkspace",
        serde_json::to_value(&f.snapshot).unwrap(),
    );
    daemon.serve(
        "GetGlobalManager",
        serde_json::to_value(f.snapshot.grant.clone()).unwrap(),
    );
    open(&mut app).await;
    crate::action_handler::dispatch_command(&mut app, "manager global").await;
    assert!(matches!(
        app.overlay,
        OverlayState::GlobalManagerWorkspace(..)
    ));
    assert_eq!(daemon.calls("GetGlobalManager"), 1);
    assert_eq!(daemon.calls("GetGlobalManagerWorkspace"), 2);
}

#[tokio::test]
async fn tick_refreshes_only_when_due_and_open() {
    let f = fixture();
    let mut app = app_on_a(&f);
    let daemon = fake_daemon(&mut app).await;
    daemon.serve(
        "GetGlobalManagerWorkspace",
        serde_json::to_value(&f.snapshot).unwrap(),
    );
    tick(&mut app).await;
    assert_eq!(daemon.calls("GetGlobalManagerWorkspace"), 0, "closed");
    app.overlay = OverlayState::GlobalManagerWorkspace(Box::default());
    tick(&mut app).await;
    assert_eq!(daemon.calls("GetGlobalManagerWorkspace"), 1, "due");
    tick(&mut app).await;
    assert_eq!(daemon.calls("GetGlobalManagerWorkspace"), 1, "not yet due");
    if let OverlayState::GlobalManagerWorkspace(state) = &mut app.overlay {
        state.last_attempt = Some(Instant::now() - AUTO_REFRESH);
    }
    tick(&mut app).await;
    assert_eq!(daemon.calls("GetGlobalManagerWorkspace"), 2, "due again");
}

#[tokio::test]
async fn gm_command_and_registered_action_open_the_workspace_from_any_tab() {
    let f = fixture();
    for tab_project in [Some(f.a.id), Some(f.b.id), None] {
        let mut app = app_on_a(&f);
        app.tabs[0].project_id = tab_project;
        app.sync_project_filter();
        let daemon = fake_daemon(&mut app).await;
        daemon.serve(
            "GetGlobalManagerWorkspace",
            serde_json::to_value(&f.snapshot).unwrap(),
        );
        crate::action_handler::dispatch_command(&mut app, "gm").await;
        let state = workspace(&app);
        assert_eq!(state.rows().len(), 3, "same view from tab {tab_project:?}");
        app.overlay = OverlayState::None;
        assert!(
            crate::action_handler::dispatch_registered_action(
                &mut app,
                crate::action_registry::ActionRequest::plain(
                    crate::action_registry::ActionId::GlobalManagerWorkspace,
                ),
            )
            .await
        );
        assert_eq!(workspace(&app).rows().len(), 3);
    }
}

#[tokio::test]
async fn colon_opens_the_command_palette_over_the_workspace_and_esc_returns() {
    let f = fixture();
    let mut app = app_on_a(&f);
    open_with(&mut app, f.snapshot.clone());
    handle_key(&mut app, key(KeyCode::Char(':'))).await;
    assert!(matches!(app.overlay, OverlayState::CommandPalette { .. }));
    crate::overlay::command_palette::handle_key(&mut app, key(KeyCode::Esc)).await;
    assert!(matches!(
        app.overlay,
        OverlayState::GlobalManagerWorkspace(..)
    ));
}

// === #1231: conversation, input and seat switching ==========================

fn press_text(text: &str) -> Vec<KeyEvent> {
    text.chars().map(|ch| key(KeyCode::Char(ch))).collect()
}

#[tokio::test]
async fn typing_sends_to_the_selected_seat_without_leaving_the_workspace() {
    let f = fixture();
    let mut app = app_on_a(&f);
    let daemon = fake_daemon(&mut app).await;
    daemon.serve("ContinueSession", serde_json::json!({"ok": true}));
    open_with(&mut app, f.snapshot.clone());
    // Select project a's PM and type to it.
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    handle_key(&mut app, key(KeyCode::Char('i'))).await;
    assert_eq!(workspace(&app).focus, WorkspaceFocus::Input);
    // Space goes to the draft, not the overlay leader.
    for k in press_text("status please") {
        assert!(crate::overlay::handle_overlay_key(&mut app, k).await);
    }
    assert_eq!(
        app.sessions[&f.pm_id].input_bar.surface.textarea.lines(),
        ["status please"]
    );
    app.settings.submit_on_enter = true;
    app.poll.connected = true;
    // An idle seat gets a continue (a busy one queues the message).
    app.sessions.get_mut(&f.pm_id).unwrap().session.status = SessionStatus::Completed;
    handle_key(&mut app, key(KeyCode::Enter)).await;
    assert_eq!(daemon.calls("ContinueSession"), 1);
    let params = daemon.last_params("ContinueSession").unwrap();
    assert_eq!(params["session_id"], serde_json::json!(f.pm_id));
    assert_eq!(params["query"], "status please");
    assert!(app.sessions[&f.pm_id].input_bar.surface.textarea.lines() == [""]);
    assert!(matches!(
        app.overlay,
        OverlayState::GlobalManagerWorkspace(..)
    ));
    assert_eq!(workspace(&app).focus, WorkspaceFocus::Input, "keep typing");
    // Esc leaves typing; a second Esc closes the view.
    handle_key(&mut app, key(KeyCode::Esc)).await;
    assert_eq!(workspace(&app).focus, WorkspaceFocus::Seats);
    handle_key(&mut app, key(KeyCode::Esc)).await;
    assert!(matches!(app.overlay, OverlayState::None));
}

#[tokio::test]
async fn drafts_stay_with_their_seat_when_switching_seats() {
    let f = fixture();
    let mut app = app_on_a(&f);
    open_with(&mut app, f.snapshot.clone());
    handle_key(&mut app, key(KeyCode::Char('i'))).await;
    for k in press_text("to global") {
        handle_key(&mut app, k).await;
    }
    handle_key(&mut app, key(KeyCode::Esc)).await;
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    assert_eq!(workspace(&app).conversation_session_id(), Some(f.pm_id));
    handle_key(&mut app, key(KeyCode::Char('i'))).await;
    for k in press_text("to pm") {
        handle_key(&mut app, k).await;
    }
    assert_eq!(
        app.sessions[&f.seat_id].input_bar.surface.textarea.lines(),
        ["to global"]
    );
    assert_eq!(
        app.sessions[&f.pm_id].input_bar.surface.textarea.lines(),
        ["to pm"]
    );
}

#[tokio::test]
async fn seats_without_a_live_session_refuse_typing_with_a_way_forward() {
    let f = fixture();
    let mut app = app_on_a(&f);
    open_with(&mut app, f.snapshot.clone());
    handle_key(&mut app, key(KeyCode::Char('G'))).await; // project b: no PM
    handle_key(&mut app, key(KeyCode::Char('i'))).await;
    let state = workspace(&app);
    assert_eq!(state.focus, WorkspaceFocus::Seats);
    assert!(state.error.as_deref().unwrap().contains(LAUNCH_HINT));
    handle_key(&mut app, key(KeyCode::Tab)).await;
    assert_eq!(workspace(&app).focus, WorkspaceFocus::Seats);

    // An archived seat session is read-only.
    let mut app = app_on_a(&f);
    app.sessions.get_mut(&f.seat_id).unwrap().session.status = SessionStatus::Archived;
    open_with(&mut app, f.snapshot.clone());
    handle_key(&mut app, key(KeyCode::Char('i'))).await;
    let error = workspace(&app).error.clone().unwrap();
    assert!(error.contains("read-only"), "{error}");
}

#[tokio::test]
async fn switching_to_an_uncached_seat_loads_its_session() {
    let f = fixture();
    let mut app = app_on_a(&f);
    app.sessions.remove(&f.pm_id);
    let daemon = fake_daemon(&mut app).await;
    let mut pm_session = baseline_session(f.pm_id, SessionKind::Standard);
    pm_session.project_id = Some(f.a.id);
    daemon.serve("GetSession", serde_json::to_value(&pm_session).unwrap());
    open_with(&mut app, f.snapshot.clone());
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    assert_eq!(daemon.calls("GetSession"), 1);
    assert!(app.sessions.contains_key(&f.pm_id));
    assert_eq!(workspace(&app).error, None);

    // A seat session the daemon no longer has is reported, not fatal.
    daemon.responses.lock().unwrap().remove("GetSession");
    app.sessions.remove(&f.seat_id);
    handle_key(&mut app, key(KeyCode::Char('k'))).await;
    let error = workspace(&app).error.clone().unwrap();
    assert!(error.contains("unavailable"), "{error}");
    assert!(error.contains("n to replace it"), "{error}");
}

#[tokio::test]
async fn conversation_focus_scrolls_the_transcript_and_esc_returns() {
    let f = fixture();
    let mut app = app_on_a(&f);
    open_with(&mut app, f.snapshot.clone());
    {
        let state = app.sessions.get_mut(&f.seat_id).unwrap();
        state.total_content_height = 200;
        state.last_viewport_height = 20;
        state.scroll_offset = 180;
        state.follow_tail = true;
    }
    handle_key(&mut app, key(KeyCode::Tab)).await;
    assert_eq!(workspace(&app).focus, WorkspaceFocus::Conversation);
    handle_key(&mut app, key(KeyCode::Char('k'))).await;
    let state = &app.sessions[&f.seat_id];
    assert_eq!(state.scroll_offset, 177);
    assert!(!state.follow_tail);
    handle_key(&mut app, key(KeyCode::Char('G'))).await;
    assert!(app.sessions[&f.seat_id].follow_tail);
    handle_key(&mut app, key(KeyCode::Esc)).await;
    assert_eq!(workspace(&app).focus, WorkspaceFocus::Seats);
    assert!(matches!(
        app.overlay,
        OverlayState::GlobalManagerWorkspace(..)
    ));
}

// === #1231: launch and appoint ==============================================

fn launched_snapshot(f: &Fixture, seat_id: Uuid) -> GlobalManagerWorkspaceV1 {
    let mut snapshot = f.snapshot.clone();
    snapshot.grant.as_mut().unwrap().seat_session_id = seat_id;
    snapshot.grant.as_mut().unwrap().grant_version = 4;
    snapshot.seat = Some(seat(seat_id, Some(f.a.id), SessionStatus::Running));
    snapshot
}

#[tokio::test]
async fn n_launches_and_appoints_a_global_manager_in_one_flow() {
    let f = fixture();
    let mut app = app_on_a(&f);
    let daemon = fake_daemon(&mut app).await;
    let new_seat = Uuid::new_v4();
    let mut new_session = baseline_session(new_seat, SessionKind::Standard);
    new_session.project_id = Some(f.a.id);
    daemon.serve("LaunchSession", serde_json::json!({"session_id": new_seat}));
    daemon.serve(
        "GetGlobalManager",
        serde_json::to_value(f.snapshot.grant.clone()).unwrap(),
    );
    let mut granted = f.snapshot.grant.clone().unwrap();
    granted.seat_session_id = new_seat;
    granted.grant_version = 4;
    daemon.serve(
        "ConfigureGlobalManager",
        serde_json::to_value(&granted).unwrap(),
    );
    daemon.serve(
        "GetGlobalManagerWorkspace",
        serde_json::to_value(launched_snapshot(&f, new_seat)).unwrap(),
    );
    daemon.serve("GetSession", serde_json::to_value(&new_session).unwrap());
    open_with(&mut app, f.snapshot.clone());

    handle_key(&mut app, key(KeyCode::Char('n'))).await;
    let form = workspace(&app).launch.clone().expect("the form opens");
    assert_eq!(form.role, LaunchRole::Global);
    assert!(
        form.scope.iter().all(|entry| entry.checked),
        "the grant's projects"
    );
    handle_key(&mut app, key(KeyCode::Enter)).await;

    assert_eq!(daemon.calls("LaunchSession"), 1);
    let launch = daemon.last_params("LaunchSession").unwrap();
    assert_eq!(launch["provider"], "Claude");
    assert_eq!(launch["model"], "claude-opus-5-5");
    assert_eq!(launch["effort"], "high");
    assert_eq!(
        launch["project_id"],
        serde_json::json!(f.a.id),
        "the tab's project"
    );
    assert!(launch["query"].as_str().unwrap().contains("global manager"));
    let configure = daemon.last_params("ConfigureGlobalManager").unwrap();
    assert_eq!(configure["session_id"], serde_json::json!(new_seat));
    assert_eq!(configure["expected_grant_version"], 3);
    assert_eq!(
        configure["project_ids"],
        serde_json::json!([f.a.id, f.b.id])
    );
    let state = workspace(&app);
    assert!(state.launch.is_none(), "success closes the form");
    let notice = state.notice.clone().unwrap();
    assert!(
        notice.contains("appointed (grant v4, 2 projects)"),
        "{notice}"
    );
    assert!(notice.contains("replaces the previous seat"), "{notice}");
    assert_eq!(state.conversation_session_id(), Some(new_seat));
    assert!(
        app.sessions.contains_key(&new_seat),
        "its conversation loads"
    );
}

#[tokio::test]
async fn a_refused_appointment_stays_inline_and_enter_retries_only_the_appointment() {
    let f = fixture();
    let mut app = app_on_a(&f);
    let daemon = fake_daemon(&mut app).await;
    let new_seat = Uuid::new_v4();
    daemon.serve("LaunchSession", serde_json::json!({"session_id": new_seat}));
    daemon.serve("GetGlobalManager", Value::Null);
    open_with(&mut app, GlobalManagerWorkspaceV1::default());
    handle_key(&mut app, key(KeyCode::Char('n'))).await;
    handle_key(&mut app, key(KeyCode::Enter)).await;
    let form = workspace(&app).launch.clone().expect("the form stays open");
    let error = form.error.clone().unwrap();
    assert!(error.contains("Launched session"), "{error}");
    assert!(error.contains("Enter retries the appointment"), "{error}");
    assert_eq!(form.launched, Some(new_seat));
    assert_eq!(form.value(LaunchField::Submit), "[ Retry the appointment ]");

    let mut granted = f.snapshot.grant.clone().unwrap();
    granted.seat_session_id = new_seat;
    granted.grant_version = 1;
    daemon.serve(
        "ConfigureGlobalManager",
        serde_json::to_value(&granted).unwrap(),
    );
    daemon.serve(
        "GetGlobalManagerWorkspace",
        serde_json::to_value(launched_snapshot(&f, new_seat)).unwrap(),
    );
    handle_key(&mut app, key(KeyCode::Enter)).await;
    assert_eq!(daemon.calls("LaunchSession"), 1, "not relaunched");
    assert_eq!(daemon.calls("ConfigureGlobalManager"), 2);
    let configure = daemon.last_params("ConfigureGlobalManager").unwrap();
    assert_eq!(configure["session_id"], serde_json::json!(new_seat));
    assert_eq!(configure["expected_grant_version"], 0, "first appointment");
    assert!(workspace(&app).launch.is_none());
}

/// Focus `id` in the session list so `:manager ... appoint` picks it as the seat.
fn focus_session(app: &mut App, id: Uuid) {
    if let Pane::SessionList {
        selected_session, ..
    } = app.session_list_pane_mut()
    {
        *selected_session = Some(id);
    }
}

const CAP_REFUSAL: &str = "portfolio_cap_reduction_confirmation_required: Koplik: active sessions: 20 → 4; Other: created sessions: 50 → 10. Confirm with confirm_cap_reductions:true after reviewing these changes.";

#[tokio::test]
async fn launch_and_appoint_previews_a_cap_reduction_and_y_confirms_it() {
    let f = fixture();
    let mut app = app_on_a(&f);
    let daemon = fake_daemon(&mut app).await;
    let new_seat = Uuid::new_v4();
    daemon.serve("LaunchSession", serde_json::json!({"session_id": new_seat}));
    daemon.serve("GetGlobalManager", Value::Null);
    let mut granted = f.snapshot.grant.clone().unwrap();
    granted.seat_session_id = new_seat;
    granted.grant_version = 1;
    daemon.serve_cap_refusal(
        "ConfigureGlobalManager",
        CAP_REFUSAL,
        serde_json::to_value(&granted).unwrap(),
    );
    daemon.serve(
        "GetGlobalManagerWorkspace",
        serde_json::to_value(launched_snapshot(&f, new_seat)).unwrap(),
    );
    open_with(&mut app, GlobalManagerWorkspaceV1::default());
    handle_key(&mut app, key(KeyCode::Char('n'))).await;
    handle_key(&mut app, key(KeyCode::Enter)).await;

    let form = workspace(&app).launch.clone().expect("the form stays open");
    assert!(form.cap_confirm);
    let error = form.error.clone().unwrap();
    assert!(error.contains("Koplik: active sessions: 20 → 4"), "{error}");
    assert!(
        error.contains("Other: created sessions: 50 → 10"),
        "{error}"
    );
    assert!(error.contains("Press y to confirm"), "{error}");
    assert_eq!(form.launched, Some(new_seat));

    // Enter must not repeat the refused request.
    handle_key(&mut app, key(KeyCode::Enter)).await;
    assert_eq!(daemon.calls("ConfigureGlobalManager"), 1);
    assert!(workspace(&app).launch.as_ref().unwrap().cap_confirm);

    handle_key(&mut app, key(KeyCode::Char('y'))).await;
    assert_eq!(daemon.calls("LaunchSession"), 1, "not relaunched");
    assert_eq!(daemon.calls("ConfigureGlobalManager"), 2);
    let configure = daemon.last_params("ConfigureGlobalManager").unwrap();
    assert_eq!(configure["confirm_cap_reductions"], true);
    assert_eq!(configure["session_id"], serde_json::json!(new_seat));
    let state = workspace(&app);
    assert!(state.launch.is_none(), "the confirmed appointment closes");
    let notice = state.notice.clone().unwrap();
    assert!(notice.contains("appointed (grant v1"), "{notice}");
}

#[tokio::test]
async fn n_leaves_the_cap_preview_and_esc_keeps_the_session_unappointed() {
    let f = fixture();
    let mut app = app_on_a(&f);
    let daemon = fake_daemon(&mut app).await;
    let new_seat = Uuid::new_v4();
    daemon.serve("LaunchSession", serde_json::json!({"session_id": new_seat}));
    daemon.serve("GetGlobalManager", Value::Null);
    daemon.serve_cap_refusal("ConfigureGlobalManager", CAP_REFUSAL, Value::Null);
    open_with(&mut app, GlobalManagerWorkspaceV1::default());
    handle_key(&mut app, key(KeyCode::Char('n'))).await;
    handle_key(&mut app, key(KeyCode::Enter)).await;
    handle_key(&mut app, key(KeyCode::Char('n'))).await;
    let form = workspace(&app).launch.clone().unwrap();
    assert!(!form.cap_confirm);
    assert_eq!(form.error, None);
    assert_eq!(form.launched, Some(new_seat));
    handle_key(&mut app, key(KeyCode::Esc)).await;
    assert!(workspace(&app).launch.is_none());
    assert_eq!(daemon.calls("ConfigureGlobalManager"), 1);
}

#[tokio::test]
async fn manager_global_appoint_opens_the_cap_preview_and_y_resends_confirmed() {
    use crate::overlay::manager_tree::TreeModal;
    let f = fixture();
    let mut app = app_on_a(&f);
    focus_session(&mut app, f.seat_id);
    let daemon = fake_daemon(&mut app).await;
    daemon.serve("GetGlobalManager", Value::Null);
    daemon.serve(
        "GetManagerTree",
        serde_json::json!({"rows": [], "next_after": null, "total_rows": 0, "complete": true, "global_grant_version": null}),
    );
    let mut granted = f.snapshot.grant.clone().unwrap();
    granted.seat_session_id = f.seat_id;
    daemon.serve_cap_refusal(
        "ConfigureGlobalManager",
        CAP_REFUSAL,
        serde_json::to_value(&granted).unwrap(),
    );
    crate::overlay::global_manager_command::dispatch_global_command(&mut app, "appoint").await;
    let OverlayState::ManagerTree(tree) = &app.overlay else {
        panic!("the manager tree opens on the confirm step")
    };
    let Some(TreeModal::Confirm(pending)) = &tree.modal else {
        panic!("the cap preview is a confirm step")
    };
    let text = pending.lines.join("\n");
    assert!(text.contains("Koplik: active sessions: 20 → 4"), "{text}");
    assert!(pending.destructive);
    assert_eq!(daemon.calls("ConfigureGlobalManager"), 1);

    crate::overlay::manager_tree::handle_key(&mut app, key(KeyCode::Enter)).await;
    assert_eq!(
        daemon.calls("ConfigureGlobalManager"),
        1,
        "Enter is refused"
    );
    crate::overlay::manager_tree::handle_key(&mut app, key(KeyCode::Char('y'))).await;
    assert_eq!(daemon.calls("ConfigureGlobalManager"), 2);
    let sent = daemon.last_params("ConfigureGlobalManager").unwrap();
    assert_eq!(sent["confirm_cap_reductions"], true);
    assert_eq!(sent["session_id"], serde_json::json!(f.seat_id));
    let OverlayState::ManagerTree(tree) = &app.overlay else {
        panic!("the tree stays open")
    };
    assert!(tree.modal.is_none());
    let notice = tree.notice.as_ref().unwrap();
    assert!(
        notice.text.contains("Global grant saved"),
        "{}",
        notice.text
    );
}

#[tokio::test]
async fn manager_portfolio_appoint_opens_the_cap_preview_and_y_resends_confirmed() {
    use crate::overlay::manager_tree::TreeModal;
    let f = fixture();
    let mut app = app_on_a(&f);
    focus_session(&mut app, f.seat_id);
    let daemon = fake_daemon(&mut app).await;
    daemon.serve("ListPortfolioNodes", serde_json::json!({"nodes": []}));
    daemon.serve(
        "GetManagerTree",
        serde_json::json!({"rows": [], "next_after": null, "total_rows": 0, "complete": true, "global_grant_version": null}),
    );
    // A refusal that is not a cap reduction stays a plain error.
    crate::overlay::portfolio_command::dispatch_portfolio_command(&mut app, "appoint area").await;
    assert!(!matches!(app.overlay, OverlayState::ManagerTree(..)));
    assert_eq!(
        daemon.calls("ConfigurePortfolioNode"),
        1,
        "the unrouted refusal"
    );

    let node = serde_json::json!(null);
    daemon.serve_cap_refusal("ConfigurePortfolioNode", CAP_REFUSAL, node);
    crate::overlay::portfolio_command::dispatch_portfolio_command(&mut app, "appoint area").await;
    let OverlayState::ManagerTree(tree) = &app.overlay else {
        panic!("the manager tree opens on the confirm step")
    };
    let Some(TreeModal::Confirm(pending)) = &tree.modal else {
        panic!("the cap preview is a confirm step")
    };
    assert!(
        pending
            .lines
            .join("\n")
            .contains("Other: created sessions: 50 → 10"),
        "{:?}",
        pending.lines
    );
    crate::overlay::manager_tree::handle_key(&mut app, key(KeyCode::Char('y'))).await;
    assert_eq!(daemon.calls("ConfigurePortfolioNode"), 3);
    assert_eq!(
        daemon.last_params("ConfigurePortfolioNode").unwrap()["confirm_cap_reductions"],
        true
    );
}

#[tokio::test]
async fn validation_errors_show_inline_without_calling_the_daemon() {
    let f = fixture();
    let mut app = app_on_a(&f);
    let daemon = fake_daemon(&mut app).await;
    open_with(&mut app, f.snapshot.clone());
    handle_key(&mut app, key(KeyCode::Char('n'))).await;
    // Uncheck every project.
    for _ in 0..4 {
        handle_key(&mut app, key(KeyCode::Tab)).await;
    }
    assert_eq!(
        workspace(&app).launch.as_ref().unwrap().field,
        LaunchField::Scope
    );
    handle_key(&mut app, key(KeyCode::Char('a'))).await;
    handle_key(&mut app, key(KeyCode::Enter)).await;
    let error = workspace(&app)
        .launch
        .as_ref()
        .unwrap()
        .error
        .clone()
        .unwrap();
    assert!(error.contains("at least one project"), "{error}");
    assert_eq!(daemon.calls("LaunchSession"), 0);
    // Esc cancels the form and keeps the workspace.
    handle_key(&mut app, key(KeyCode::Esc)).await;
    assert!(workspace(&app).launch.is_none());
}

#[tokio::test]
async fn n_on_a_project_row_launches_and_appoints_its_pm() {
    use rsi_common::harness_manager::{HarnessManagerConfigV1, HarnessManagerScopeModeV1};
    use rsi_common::harness_manager_v2::HarnessManagerPolicyConfigV2;
    let f = fixture();
    let mut app = app_on_a(&f);
    let daemon = fake_daemon(&mut app).await;
    let new_pm = Uuid::new_v4();
    daemon.serve("LaunchSession", serde_json::json!({"session_id": new_pm}));
    daemon.serve("GetHarnessManager", Value::Null);
    daemon.serve("GetHarnessManagerPolicy", Value::Null);
    daemon.serve(
        "ConfigureHarnessManager",
        serde_json::to_value(HarnessManagerConfigV1 {
            project_id: f.b.id,
            manager_session_id: new_pm,
            current_session_id: Some(new_pm),
            epic_ids: Vec::new(),
            scope_mode: HarnessManagerScopeModeV1::Project,
            selected_epic_ids: None,
            group_ids: Vec::new(),
            row_version: 1,
            updated_at: Utc::now(),
        })
        .unwrap(),
    );
    daemon.serve(
        "ConfigureHarnessManagerPolicy",
        serde_json::to_value(HarnessManagerPolicyConfigV2 {
            project_id: f.b.id,
            manager_session_id: new_pm,
            scope_version: 1,
            row_version: 1,
            policy: crate::overlay::global_manager_command::default_project_policy(),
            updated_at: Utc::now(),
            revoked: false,
        })
        .unwrap(),
    );
    let mut snapshot = f.snapshot.clone();
    snapshot.projects[1].overview.manager = Some(pm(new_pm, SessionStatus::Running));
    daemon.serve(
        "GetGlobalManagerWorkspace",
        serde_json::to_value(&snapshot).unwrap(),
    );
    let mut new_session = baseline_session(new_pm, SessionKind::Standard);
    new_session.project_id = Some(f.b.id);
    daemon.serve("GetSession", serde_json::to_value(&new_session).unwrap());
    open_with(&mut app, f.snapshot.clone());
    handle_key(&mut app, key(KeyCode::Char('G'))).await; // project b: MISSING PM
    handle_key(&mut app, key(KeyCode::Char('n'))).await;
    let form = workspace(&app).launch.clone().unwrap();
    assert_eq!(form.role, LaunchRole::Project(f.b.id));
    assert!(!form.fields().contains(&LaunchField::Scope));
    handle_key(&mut app, key(KeyCode::Enter)).await;

    let launch = daemon.last_params("LaunchSession").unwrap();
    assert_eq!(launch["project_id"], serde_json::json!(f.b.id));
    let scope = daemon.last_params("ConfigureHarnessManager").unwrap();
    assert_eq!(scope["project_id"], serde_json::json!(f.b.id));
    assert_eq!(scope["session_id"], serde_json::json!(new_pm));
    assert_eq!(daemon.calls("ConfigureHarnessManagerPolicy"), 1);
    let state = workspace(&app);
    assert!(
        state.launch.is_none(),
        "{:?}",
        state.launch.as_ref().map(|f| &f.error)
    );
    assert!(state.notice.clone().unwrap().contains("Manager appointed"));
    assert_eq!(state.conversation_session_id(), Some(new_pm));
}

#[tokio::test]
async fn live_events_reach_the_open_conversation_through_the_push_stream() {
    let f = fixture();
    let mut app = app_on_a(&f);
    open_with(&mut app, f.snapshot.clone());
    let event = rsi_common::types::ConversationEvent {
        id: 9,
        session_id: f.seat_id,
        sequence: 1,
        event_type: rsi_common::types::EventType::Message,
        role: Some(rsi_common::types::Role::Assistant),
        content: "PM for rsi is healthy".into(),
        created_at: Utc::now(),
        tool_name: None,
        tool_input: None,
        offload_id: None,
        tool_use_id: None,
        metadata: None,
    };
    assert!(app.apply_push_event(rsi_common::rpc::BusEvent {
        event_type: "conversation_event".into(),
        timestamp: Utc::now(),
        data: serde_json::json!({"session_id": f.seat_id, "event": event}),
    }));
    assert_eq!(
        open_conversation_session(&app),
        Some(f.seat_id),
        "the workspace shows the seat"
    );
    assert_eq!(app.sessions[&f.seat_id].events.len(), 1);
}

#[test]
fn project_console_shows_effective_caps_from_overview() {
    let mut f = fixture();
    f.snapshot.projects[0]
        .overview
        .policy
        .as_mut()
        .unwrap()
        .effective_caps = Some(
        rsi_common::portfolio_nodes::ManagerResourceCapsV1::from_policy(&ManagerPolicyV2 {
            max_active_sessions: 4,
            max_created_sessions: 50,
            ..ManagerPolicyV2::default()
        }),
    );
    let mut state = GlobalManagerWorkspaceState::default();
    state.install(f.snapshot, Utc::now());
    assert!(
        state.select_where(
            |seat| seat.level == SeatLevel::Project && seat.project_id == Some(f.a.id)
        )
    );
    assert!(
        state
            .selected_cap_line()
            .unwrap()
            .contains("Effective caps · active 4 · created sessions 50")
    );
}

fn prior(id: Uuid, project_id: Uuid, status: SessionStatus) -> SeatPredecessorV1 {
    SeatPredecessorV1 {
        session_id: id,
        project_id: Some(project_id),
        status,
        provider: SessionProvider::Claude,
        model: Some("claude-opus-5-5".into()),
        context_fill_pct: Some(91.0),
        cost_usd: Some(7.5),
        updated_at: Utc::now(),
    }
}

/// The fixture's seat after two rotations: newest predecessor first.
fn rotated(f: &Fixture) -> (GlobalManagerWorkspaceV1, Uuid, Uuid) {
    let (older, oldest) = (Uuid::new_v4(), Uuid::new_v4());
    let mut snapshot = f.snapshot.clone();
    let seat = snapshot.seat.as_mut().unwrap();
    seat.predecessors = vec![
        prior(older, f.b.id, SessionStatus::Completed),
        prior(oldest, f.b.id, SessionStatus::Archived),
    ];
    (snapshot, older, oldest)
}

#[test]
fn predecessors_are_indented_selectable_rows_under_their_seat() {
    let f = fixture();
    let (snapshot, older, oldest) = rotated(&f);
    let mut state = GlobalManagerWorkspaceState::default();
    state.install(snapshot, Utc::now());
    assert_eq!(
        state.rows(),
        vec![
            WorkspaceRow::Seat,
            WorkspaceRow::Predecessor {
                child: None,
                index: 0
            },
            WorkspaceRow::Predecessor {
                child: None,
                index: 1
            },
            WorkspaceRow::Project(0),
            WorkspaceRow::Project(1),
        ]
    );
    let seats = state.seats();
    assert_eq!(
        seats
            .iter()
            .map(|seat| (seat.depth, seat.session_id))
            .collect::<Vec<_>>(),
        vec![
            (0, Some(f.seat_id)),
            (1, Some(older)),
            (1, Some(oldest)),
            (1, Some(f.pm_id)),
            (1, None),
        ]
    );
    assert_eq!(seats[1].level, SeatLevel::Global);
    assert_eq!(seats[1].health, SeatHealth::Idle);
    assert_eq!(
        seats[2].health,
        SeatHealth::Idle,
        "an archived prior is history, not missing"
    );
    assert!(seat_row_text(&seats[1], 60).contains("prior"));
    state.selected = 1;
    assert_eq!(state.conversation_session_id(), Some(older));
}

#[test]
fn a_seat_without_rotations_adds_no_history_rows() {
    let f = fixture();
    let mut state = GlobalManagerWorkspaceState::default();
    state.install(f.snapshot.clone(), Utc::now());
    assert!(
        !state
            .rows()
            .iter()
            .any(|row| matches!(row, WorkspaceRow::Predecessor { .. }))
    );
}

#[tokio::test]
async fn enter_on_a_predecessor_opens_that_session_and_typing_is_refused() {
    let f = fixture();
    let (snapshot, older, _) = rotated(&f);
    let mut app = app_on_a(&f);
    let mut session = baseline_session(older, SessionKind::Standard);
    session.project_id = Some(f.b.id);
    app.sessions.insert(older, SessionState::new(session));
    open_with(&mut app, snapshot);
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    handle_key(&mut app, key(KeyCode::Char('i'))).await;
    let state = workspace(&app);
    assert_eq!(state.focus, WorkspaceFocus::Seats, "history is read only");
    assert!(
        state
            .error
            .as_deref()
            .is_some_and(|e| e.contains("read-only history"))
    );
    handle_key(&mut app, key(KeyCode::Enter)).await;
    assert!(matches!(app.overlay, OverlayState::None));
    assert_eq!(app.active_project_id(), Some(f.b.id));
    assert!(matches!(
        app.focused_pane(),
        Some(Pane::SessionDetail { session_id }) if *session_id == older
    ));
}

#[tokio::test]
async fn s_reports_the_selected_seat_sessions_status_read_only() {
    let f = fixture();
    let (snapshot, older, _) = rotated(&f);
    let mut app = app_on_a(&f);
    let mut session = baseline_session(older, SessionKind::Standard);
    session.status = SessionStatus::Completed;
    app.sessions.insert(older, SessionState::new(session));
    open_with(&mut app, snapshot);
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    handle_key(&mut app, key(KeyCode::Char('s'))).await;
    let notice = workspace(&app).notice.clone().unwrap();
    assert!(notice.contains("status Completed"), "{notice}");
    assert!(notice.contains("earlier session, read only"), "{notice}");
    assert!(notice.contains(&older.to_string()[..8]), "{notice}");
}

#[tokio::test]
async fn x_halts_a_running_seat_through_the_session_list_interrupt() {
    let f = fixture();
    let mut app = app_on_a(&f);
    let daemon = fake_daemon(&mut app).await;
    daemon.serve("InterruptSession", serde_json::json!({}));
    daemon.serve(
        "GetGlobalManagerWorkspace",
        serde_json::to_value(&f.snapshot).unwrap(),
    );
    app.sessions.get_mut(&f.seat_id).unwrap().session.status = SessionStatus::Running;
    open_with(&mut app, f.snapshot.clone());
    handle_key(&mut app, key(KeyCode::Char('x'))).await;
    assert_eq!(daemon.calls("InterruptSession"), 1);
    let params = daemon.last_params("InterruptSession").unwrap();
    assert_eq!(params["session_id"], f.seat_id.to_string());
    assert_eq!(params["pause_level"], "soft");
    assert!(
        workspace(&app)
            .notice
            .as_deref()
            .is_some_and(|n| n.contains("Halt requested"))
    );
}

#[tokio::test]
async fn x_refuses_a_seat_session_that_is_not_running() {
    let f = fixture();
    let mut app = app_on_a(&f);
    let daemon = fake_daemon(&mut app).await;
    app.sessions.get_mut(&f.seat_id).unwrap().session.status = SessionStatus::Completed;
    open_with(&mut app, f.snapshot.clone());
    handle_key(&mut app, key(KeyCode::Char('x'))).await;
    assert_eq!(daemon.calls("InterruptSession"), 0);
    assert!(
        workspace(&app)
            .error
            .as_deref()
            .is_some_and(|e| e.contains("can be halted"))
    );
}

#[tokio::test]
async fn a_refuses_the_current_seat_and_archives_an_earlier_session() {
    let f = fixture();
    let (snapshot, older, _) = rotated(&f);
    let mut app = app_on_a(&f);
    let daemon = fake_daemon(&mut app).await;
    daemon.serve(
        "ArchiveSession",
        serde_json::to_value(
            rsi_common::archive_cleanup::ArchiveSessionResultV1::no_cleanup_required(),
        )
        .unwrap(),
    );
    daemon.serve(
        "GetGlobalManagerWorkspace",
        serde_json::to_value(&snapshot).unwrap(),
    );
    let mut session = baseline_session(older, SessionKind::Standard);
    session.status = SessionStatus::Completed;
    app.sessions.insert(older, SessionState::new(session));
    open_with(&mut app, snapshot);

    // The current seat keeps its authority: no archive call.
    handle_key(&mut app, key(KeyCode::Char('a'))).await;
    assert_eq!(daemon.calls("ArchiveSession"), 0);
    assert!(
        workspace(&app)
            .error
            .as_deref()
            .is_some_and(|e| e.contains("current seat"))
    );

    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    handle_key(&mut app, key(KeyCode::Char('a'))).await;
    assert_eq!(daemon.calls("ArchiveSession"), 1);
    assert_eq!(
        daemon.last_params("ArchiveSession").unwrap()["session_id"],
        older.to_string()
    );
    assert!(
        !app.sessions.contains_key(&older),
        "archived session leaves the live set"
    );
}
