//! Tests for the global dashboard (#3317).

use chrono::{Duration as ChronoDuration, Utc};
use crossterm::event::KeyCode;
use rsi_common::global_dashboard::{
    DashboardAgentGroup, DashboardAgents, DashboardIssues, DashboardLandings, DashboardModelUsage,
    DashboardPmSeat, DashboardProviderCount, DashboardRollingHealth, DashboardTotals,
    DashboardUsage, DashboardWindow, GlobalDashboard, ProjectDashboard,
};
use rsi_common::global_manager::GlobalManagerWorkspaceV1;
use rsi_common::types::{Project, SessionKind, SessionStatus};
use uuid::Uuid;

use super::*;
use crate::app::app_test_helpers::{baseline_session, with_session_list};
use crate::overlay::global_manager_workspace::tests::{
    FakeDaemon, fake_daemon, grant, key, project,
};
use crate::types::SessionState;

pub(crate) fn agents(
    total: u64,
    roles: [u64; 4],
    providers: &[(&str, u64)],
    models: &[(&str, &str, Option<&str>, u64)],
) -> DashboardAgents {
    DashboardAgents {
        total,
        managers: roles[0],
        leads: roles[1],
        workers: roles[2],
        reviewers: roles[3],
        other: total.saturating_sub(roles.iter().sum()),
        by_provider: providers
            .iter()
            .map(|(provider, count)| DashboardProviderCount {
                provider: (*provider).into(),
                count: *count,
            })
            .collect(),
        by_model: models
            .iter()
            .map(|(provider, model, effort, count)| DashboardAgentGroup {
                provider: (*provider).into(),
                model: (*model).into(),
                effort: effort.map(Into::into),
                count: *count,
            })
            .collect(),
    }
}

pub(crate) fn usage(input: u64, output: u64, cost: f64) -> DashboardUsage {
    DashboardUsage {
        invocations: 10,
        input_tokens: input,
        output_tokens: output,
        estimated_cost_usd: cost,
        ..DashboardUsage::default()
    }
}

pub(crate) fn dash_project(
    id: Uuid,
    name: &str,
    is_home: bool,
    agents: DashboardAgents,
    usage: DashboardUsage,
    issues: DashboardIssues,
) -> ProjectDashboard {
    ProjectDashboard {
        id,
        name: name.into(),
        path: None,
        is_home,
        agents,
        pm_seat: None,
        usage_by_model: vec![DashboardModelUsage {
            provider: "Claude".into(),
            model: "claude-opus-5-5".into(),
            usage: usage.clone(),
        }],
        usage,
        issues,
        landings: DashboardLandings::default(),
        rolling: None,
    }
}

pub(crate) fn issues(
    open: u64,
    wip: u64,
    created: u64,
    closed: u64,
    requests: u64,
) -> DashboardIssues {
    DashboardIssues {
        open,
        in_progress: wip,
        created_in_window: created,
        closed_in_window: closed,
        open_operator_requests: requests,
    }
}

/// The projects of the fixture, in the order the daemon lists them (the home
/// project first, then by name).
pub(crate) struct Fixture {
    pub home: Project,
    pub busy: Project,
    pub idle: Project,
    pub snapshot: GlobalDashboard,
}

pub(crate) fn fixture() -> Fixture {
    let (home, busy, idle) = (project("rsi"), project("dictate-agent"), project("notes"));
    let mut home_row = dash_project(
        home.id,
        "rsi",
        true,
        agents(
            6,
            [1, 1, 3, 1],
            &[("Codex", 2), ("Claude", 4)],
            &[
                ("Claude", "claude-opus-5-5", Some("xhigh"), 1),
                ("Claude", "claude-sonnet-5-5", Some("high"), 3),
                ("Codex", "gpt-6-astra", None, 2),
            ],
        ),
        usage(1_250_000, 80_000, 3.2),
        issues(12, 3, 5, 7, 1),
    );
    home_row.pm_seat = Some(DashboardPmSeat {
        session_id: Uuid::new_v4(),
        status: "Running".into(),
    });
    home_row.landings = DashboardLandings {
        published: 4,
        refused: 1,
    };
    home_row.rolling = Some(DashboardRollingHealth {
        qa_green_sha: Some("abc".into()),
        qa_green_age_secs: Some(3 * 3600),
        open_qa_issues: 0,
    });
    let mut busy_row = dash_project(
        busy.id,
        "dictate-agent",
        false,
        agents(9, [1, 2, 5, 1], &[("Claude", 9)], &[]),
        usage(900_000, 40_000, 11.0),
        issues(4, 2, 1, 0, 0),
    );
    busy_row.rolling = Some(DashboardRollingHealth {
        qa_green_sha: None,
        qa_green_age_secs: None,
        open_qa_issues: 2,
    });
    let idle_row = dash_project(
        idle.id,
        "notes",
        false,
        agents(0, [0; 4], &[], &[]),
        usage(0, 0, 0.0),
        issues(0, 0, 0, 0, 0),
    );
    let snapshot = GlobalDashboard {
        as_of: Utc::now() - ChronoDuration::seconds(12),
        window: "24h".into(),
        window_seconds: 86400,
        totals: DashboardTotals {
            agents: agents(16, [2, 3, 8, 2], &[("Claude", 13), ("Codex", 2)], &[]),
            usage: usage(2_150_000, 120_000, 14.2),
            usage_by_model: vec![],
            issues: issues(16, 5, 6, 7, 1),
            landings: DashboardLandings {
                published: 4,
                refused: 1,
            },
            incomplete: false,
        },
        projects: vec![home_row, idle_row, busy_row],
        projects_incomplete: false,
        incomplete_reasons: vec![],
    };
    Fixture {
        home,
        busy,
        idle,
        snapshot,
    }
}

pub(crate) fn loaded(snapshot: GlobalDashboard) -> GlobalDashboardState {
    let mut state = GlobalDashboardState::default();
    state.install(snapshot, Utc::now());
    state
}

/// An app on the home project's tab with all three projects.
pub(crate) fn app_with(f: &Fixture, state: GlobalDashboardState) -> App {
    let mut app = with_session_list(0);
    app.projects = vec![f.home.clone(), f.busy.clone(), f.idle.clone()];
    app.tabs[0].project_id = Some(f.home.id);
    app.sync_project_filter();
    app.overlay = OverlayState::GlobalDashboard(Box::new(state));
    app
}

pub(crate) fn dashboard(app: &App) -> &GlobalDashboardState {
    match &app.overlay {
        OverlayState::GlobalDashboard(state) => state,
        _ => panic!("the dashboard is open"),
    }
}

fn names(state: &GlobalDashboardState) -> Vec<String> {
    state
        .sorted_projects()
        .iter()
        .map(|p| p.name.clone())
        .collect()
}

fn serve(daemon: &FakeDaemon, f: &Fixture, seat: Option<Uuid>) {
    daemon.serve(
        "GetGlobalDashboard",
        serde_json::to_value(&f.snapshot).unwrap(),
    );
    daemon.serve(
        "GetGlobalManager",
        match seat {
            Some(id) => {
                serde_json::to_value(grant(id, &[f.home.id, f.busy.id, f.idle.id], "active"))
                    .unwrap()
            }
            None => serde_json::Value::Null,
        },
    );
}

/// Add the seat's session to the app's cache.
fn cache_seat(app: &mut App, seat_id: Uuid, status: SessionStatus) {
    let mut session = baseline_session(seat_id, SessionKind::Standard);
    session.status = status;
    app.sessions.insert(seat_id, SessionState::new(session));
}

#[test]
fn rows_lead_with_home_then_live_agents_then_name() {
    let f = fixture();
    let state = loaded(f.snapshot.clone());
    assert_eq!(names(&state), ["rsi", "dictate-agent", "notes"]);
    // TOTAL follows the projects and is the last selectable row.
    assert_eq!(state.row_count(), 4);
    let rows = state.rows();
    assert_eq!(rows.last().map(|r| r.name), Some("TOTAL"));
}

#[test]
fn equal_agents_fall_back_to_name() {
    let mut f = fixture();
    f.snapshot.projects[1].agents = agents(3, [0, 0, 3, 0], &[("Claude", 3)], &[]);
    f.snapshot.projects[2].agents = agents(3, [0, 0, 3, 0], &[("Claude", 3)], &[]);
    let state = loaded(f.snapshot);
    assert_eq!(names(&state), ["rsi", "dictate-agent", "notes"]);
}

#[tokio::test]
async fn s_cycles_the_sort_and_reorders_keeping_the_selection() {
    let f = fixture();
    let mut app = app_with(&f, loaded(f.snapshot.clone()));
    // Select dictate-agent (row 1 under the default order).
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    let selected = |app: &App| {
        dashboard(app)
            .selected_project()
            .map(|p| p.name.clone())
            .unwrap_or_default()
    };
    assert_eq!(selected(&app), "dictate-agent");
    assert_eq!(dashboard(&app).sort, DashboardSort::Agents);
    for expected in [
        DashboardSort::Tokens,
        DashboardSort::Cost,
        DashboardSort::Issues,
        DashboardSort::Agents,
    ] {
        handle_key(&mut app, key(KeyCode::Char('s'))).await;
        assert_eq!(dashboard(&app).sort, expected);
        assert_eq!(
            selected(&app),
            "dictate-agent",
            "the selection follows the project across a re-sort"
        );
    }
}

#[test]
fn the_sort_key_decides_the_order_below_the_home_project() {
    let mut f = fixture();
    // notes becomes the most expensive and the heaviest in tokens and Issues;
    // dictate-agent keeps the most agents.
    f.snapshot.projects[1].usage.estimated_cost_usd = 99.0;
    f.snapshot.projects[1].usage.input_tokens = 5_000_000;
    f.snapshot.projects[1].issues = issues(30, 0, 0, 0, 0);
    let mut state = loaded(f.snapshot);
    assert_eq!(names(&state), ["rsi", "dictate-agent", "notes"]);
    for sort in [
        DashboardSort::Tokens,
        DashboardSort::Cost,
        DashboardSort::Issues,
    ] {
        state.sort = sort;
        assert_eq!(names(&state), ["rsi", "notes", "dictate-agent"], "{sort:?}");
    }
    state.sort = DashboardSort::Agents;
    assert_eq!(names(&state), ["rsi", "dictate-agent", "notes"]);
}

#[tokio::test]
async fn w_cycles_the_window_and_asks_the_daemon_for_it() {
    let f = fixture();
    let mut app = app_with(&f, loaded(f.snapshot.clone()));
    let daemon = fake_daemon(&mut app).await;
    serve(&daemon, &f, None);
    assert_eq!(dashboard(&app).window, DashboardWindow::Day);
    handle_key(&mut app, key(KeyCode::Char('w'))).await;
    assert_eq!(dashboard(&app).window, DashboardWindow::Week);
    assert_eq!(
        daemon.last_params("GetGlobalDashboard").unwrap()["window"],
        "7d"
    );
    handle_key(&mut app, key(KeyCode::Char('w'))).await;
    assert_eq!(dashboard(&app).window, DashboardWindow::Hour);
    assert_eq!(
        daemon.last_params("GetGlobalDashboard").unwrap()["window"],
        "1h"
    );
    handle_key(&mut app, key(KeyCode::Char('w'))).await;
    assert_eq!(dashboard(&app).window, DashboardWindow::Day);
    assert_eq!(daemon.calls("GetGlobalDashboard"), 3);
}

#[tokio::test]
async fn gd_and_the_dashboard_command_open_the_view_and_q_or_esc_return() {
    let f = fixture();
    let mut app = app_with(&f, GlobalDashboardState::default());
    app.overlay = OverlayState::None;
    let daemon = fake_daemon(&mut app).await;
    serve(&daemon, &f, None);

    // `gd` dispatches OpenGlobalDashboard.
    crate::action_handler::dispatch_lc_action(
        &mut app,
        crate::modalkit_types::LcAction::OpenGlobalDashboard,
    )
    .await;
    assert!(is_open(&app));
    assert_eq!(dashboard(&app).row_count(), 4, "the snapshot is installed");
    handle_key(&mut app, key(KeyCode::Char('q'))).await;
    assert!(matches!(app.overlay, OverlayState::None));

    // `:dashboard` and its `:gd` alias reach the same view.
    for command in ["dashboard", "gd"] {
        crate::action_handler::dispatch_catalog_command(&mut app, command).await;
        assert!(is_open(&app), ":{command}");
        handle_key(&mut app, key(KeyCode::Esc)).await;
        assert!(matches!(app.overlay, OverlayState::None), ":{command}");
    }
    assert_eq!(daemon.calls("GetGlobalDashboard"), 3);
}

#[tokio::test]
async fn enter_opens_the_selected_projects_tab() {
    let f = fixture();
    let mut app = app_with(&f, loaded(f.snapshot.clone()));
    assert_eq!(app.active_project_id(), Some(f.home.id));
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    handle_key(&mut app, key(KeyCode::Enter)).await;
    assert!(matches!(app.overlay, OverlayState::None));
    assert_eq!(app.active_project_id(), Some(f.busy.id));
}

#[tokio::test]
async fn enter_on_total_stays_with_a_way_forward() {
    let f = fixture();
    let mut app = app_with(&f, loaded(f.snapshot.clone()));
    handle_key(&mut app, key(KeyCode::Char('G'))).await;
    assert!(dashboard(&app).total_selected());
    handle_key(&mut app, key(KeyCode::Enter)).await;
    assert!(is_open(&app));
    assert!(
        dashboard(&app)
            .error
            .as_deref()
            .is_some_and(|e| e.contains("TOTAL is not a project"))
    );
}

#[tokio::test]
async fn typing_sends_to_the_top_level_seat_through_the_gm_send_path() {
    let f = fixture();
    let seat_id = Uuid::new_v4();
    let mut state = loaded(f.snapshot.clone());
    state.seat = TopSeat::Seat(seat_id);
    let mut app = app_with(&f, state);
    cache_seat(&mut app, seat_id, SessionStatus::Completed);
    let daemon = fake_daemon(&mut app).await;
    daemon.serve("ContinueSession", serde_json::json!({"ok": true}));
    app.settings.submit_on_enter = true;
    app.poll.connected = true;

    handle_key(&mut app, key(KeyCode::Char('i'))).await;
    assert_eq!(dashboard(&app).focus, DashboardFocus::Input);
    // Space is typed text here, not the overlay leader.
    for ch in "status please".chars() {
        assert!(crate::overlay::handle_overlay_key(&mut app, key(KeyCode::Char(ch))).await);
    }
    assert_eq!(
        app.sessions[&seat_id].input_bar.surface.textarea.lines(),
        ["status please"]
    );
    handle_key(&mut app, key(KeyCode::Enter)).await;
    assert_eq!(daemon.calls("ContinueSession"), 1, "the gm send path");
    let params = daemon.last_params("ContinueSession").unwrap();
    assert_eq!(params["session_id"], serde_json::json!(seat_id));
    assert_eq!(params["query"], "status please");
    assert!(is_open(&app), "the dashboard stays open after a send");
    assert_eq!(dashboard(&app).focus, DashboardFocus::Input, "keep typing");
    // Esc leaves typing; a second Esc closes the view.
    handle_key(&mut app, key(KeyCode::Esc)).await;
    assert_eq!(dashboard(&app).focus, DashboardFocus::Table);
    handle_key(&mut app, key(KeyCode::Esc)).await;
    assert!(matches!(app.overlay, OverlayState::None));
}

#[tokio::test]
async fn typing_without_a_live_seat_says_how_to_start_one() {
    let f = fixture();
    let mut state = loaded(f.snapshot.clone());
    state.seat = TopSeat::Absent;
    let mut app = app_with(&f, state);
    handle_key(&mut app, key(KeyCode::Char('i'))).await;
    assert_eq!(dashboard(&app).focus, DashboardFocus::Table);
    let error = dashboard(&app).error.clone().unwrap();
    assert!(error.contains(START_HINT), "{error}");
}

#[tokio::test]
async fn archived_seat_sessions_are_not_live() {
    let f = fixture();
    let seat_id = Uuid::new_v4();
    let mut state = loaded(f.snapshot.clone());
    state.seat = TopSeat::Seat(seat_id);
    let mut app = app_with(&f, state);
    cache_seat(&mut app, seat_id, SessionStatus::Archived);
    assert_eq!(live_seat(&app), None);
    cache_seat(&mut app, seat_id, SessionStatus::Running);
    assert_eq!(live_seat(&app), Some(seat_id));
}

#[tokio::test]
async fn n_starts_a_manager_through_the_appoint_flow_hosted_in_the_home_project() {
    let f = fixture();
    let mut state = loaded(f.snapshot.clone());
    state.seat = TopSeat::Absent;
    // The tab shows another project, so the host comes from the dashboard.
    let mut app = app_with(&f, state);
    app.tabs[0].project_id = Some(f.idle.id);
    app.sync_project_filter();
    let daemon = fake_daemon(&mut app).await;
    daemon.serve(
        "GetGlobalManagerWorkspace",
        serde_json::to_value(GlobalManagerWorkspaceV1::default()).unwrap(),
    );
    handle_key(&mut app, key(KeyCode::Char('n'))).await;
    let OverlayState::GlobalManagerWorkspace(workspace) = &app.overlay else {
        panic!("the appoint flow is the gm console's form");
    };
    let form = workspace.launch.as_ref().expect("the launch form is open");
    assert_eq!(
        form.role,
        crate::overlay::global_manager_workspace_launch::LaunchRole::Global
    );
    assert_eq!(form.effective_host(), Some(f.home.id));
}

#[tokio::test]
async fn n_with_a_live_seat_points_at_typing_instead() {
    let f = fixture();
    let seat_id = Uuid::new_v4();
    let mut state = loaded(f.snapshot.clone());
    state.seat = TopSeat::Seat(seat_id);
    let mut app = app_with(&f, state);
    cache_seat(&mut app, seat_id, SessionStatus::Running);
    handle_key(&mut app, key(KeyCode::Char('n'))).await;
    assert!(is_open(&app));
    assert!(
        dashboard(&app)
            .notice
            .as_deref()
            .is_some_and(|n| n.contains("already live"))
    );
}

#[tokio::test]
async fn gm_inside_the_dashboard_opens_the_manager_workspace() {
    let f = fixture();
    let mut app = app_with(&f, loaded(f.snapshot.clone()));
    let daemon = fake_daemon(&mut app).await;
    daemon.serve(
        "GetGlobalManagerWorkspace",
        serde_json::to_value(GlobalManagerWorkspaceV1::default()).unwrap(),
    );
    handle_key(&mut app, key(KeyCode::Char('g'))).await;
    handle_key(&mut app, key(KeyCode::Char('m'))).await;
    assert!(matches!(
        app.overlay,
        OverlayState::GlobalManagerWorkspace(..)
    ));
}

#[tokio::test]
async fn refresh_failure_keeps_the_last_snapshot_and_says_so() {
    let f = fixture();
    let mut app = app_with(&f, loaded(f.snapshot.clone()));
    let daemon = fake_daemon(&mut app).await;
    daemon.serve_error("GetGlobalDashboard", "boom");
    daemon.serve("GetGlobalManager", serde_json::Value::Null);
    handle_key(&mut app, key(KeyCode::Char('r'))).await;
    let state = dashboard(&app);
    assert_eq!(state.row_count(), 4, "the last snapshot stays");
    let error = state.error.clone().unwrap();
    assert!(error.contains("showing the last snapshot"), "{error}");
}

#[tokio::test]
async fn tick_refreshes_at_most_every_ten_seconds() {
    let f = fixture();
    let mut app = app_with(&f, loaded(f.snapshot.clone()));
    let daemon = fake_daemon(&mut app).await;
    serve(&daemon, &f, None);
    tick(&mut app).await; // never attempted: due
    assert_eq!(daemon.calls("GetGlobalDashboard"), 1);
    tick(&mut app).await; // just attempted: not due
    tick(&mut app).await;
    assert_eq!(daemon.calls("GetGlobalDashboard"), 1, "bounded poll");
    assert_eq!(AUTO_REFRESH, std::time::Duration::from_secs(10));
}

#[test]
fn t_toggles_the_pane_against_the_width_default() {
    let f = fixture();
    let mut state = loaded(f.snapshot);
    assert!(!state.pane_visible(120), "narrow: hidden by default");
    assert!(
        state.pane_visible(PANE_AUTO_MIN_WIDTH),
        "wide: shown by default"
    );
    state.pane_override = Some(true);
    assert!(state.pane_visible(120));
    state.pane_override = Some(false);
    assert!(!state.pane_visible(200));
}

#[test]
fn counts_costs_ages_and_providers_format_compactly() {
    assert_eq!(fmt_count(0), "0");
    assert_eq!(fmt_count(812), "812");
    assert_eq!(fmt_count(1_300), "1.3k");
    assert_eq!(fmt_count(340_000), "340k");
    assert_eq!(fmt_count(1_250_000), "1.2M");
    assert_eq!(fmt_count(12_400_000), "12M");
    assert_eq!(fmt_count(3_000_000_000), "3.0B");
    assert_eq!(fmt_cost(0.0), "$0");
    assert_eq!(fmt_cost(3.2), "$3.20");
    assert_eq!(fmt_cost(123.4), "$123");
    assert_eq!(fmt_cost(1234.0), "$1.2k");
    assert_eq!(fmt_age(12), "12s");
    assert_eq!(fmt_age(300), "5m");
    assert_eq!(fmt_age(3 * 3600), "3h");
    assert_eq!(fmt_age(2 * 86400), "2d");
    assert_eq!(fmt_age(-5), "0s");
    let a = agents(
        7,
        [0; 4],
        &[("Codex", 2), ("Claude", 4), ("Pioneer", 1)],
        &[],
    );
    assert_eq!(providers_text(&a, 2), "Claude 4 · Codex 2 +1");
    assert_eq!(providers_text(&a, 3), "Claude 4 · Codex 2 · Pioneer 1");
    assert_eq!(providers_text(&agents(0, [0; 4], &[], &[]), 2), "-");
}

#[test]
fn narrow_tables_drop_the_least_important_columns_first() {
    let wide = layout(260);
    assert_eq!(wide.len(), Col::ALL.len(), "everything fits at 260");
    assert!(wide.iter().all(|(_, w)| *w > 0));
    let narrow = layout(60);
    let cols: Vec<Col> = narrow.iter().map(|(c, _)| *c).collect();
    for kept in [Col::Project, Col::Agents, Col::Cost, Col::Open] {
        assert!(cols.contains(&kept), "{kept:?} survives a narrow table");
    }
    assert!(!cols.contains(&Col::Reviewers));
    assert!(!cols.contains(&Col::Providers));
    let used: usize = 2 + narrow.iter().map(|(_, w)| *w).sum::<usize>() + narrow.len() - 1;
    assert!(used <= 60, "{used}");
}

#[test]
fn qa_cell_prefers_the_red_count_over_the_green_age() {
    let f = fixture();
    let state = loaded(f.snapshot);
    let rows = state.rows();
    assert_eq!(cell_text(Col::Qa, &rows[0]), "3h", "home: green age");
    assert_eq!(cell_text(Col::Qa, &rows[1]), "2 red", "dictate-agent: red");
    assert_eq!(cell_text(Col::Qa, &rows[2]), "-", "notes: no QA data");
    assert_eq!(cell_text(Col::Qa, &rows[3]), "2 red", "TOTAL sums the reds");
}

#[test]
fn incomplete_follows_every_flag_the_rpc_sets() {
    let f = fixture();
    assert!(!loaded(f.snapshot.clone()).incomplete());
    let mut snapshot = f.snapshot.clone();
    snapshot.totals.incomplete = true;
    assert!(loaded(snapshot).incomplete());
    let mut snapshot = f.snapshot.clone();
    snapshot.projects_incomplete = true;
    assert!(loaded(snapshot).incomplete());
    let mut snapshot = f.snapshot;
    snapshot.incomplete_reasons = vec!["landings".into()];
    assert!(loaded(snapshot).incomplete());
}

#[test]
fn detail_lines_name_agents_by_model_usage_and_issue_flow() {
    let f = fixture();
    let state = loaded(f.snapshot);
    let lines = state.detail_lines();
    assert!(
        lines[0].starts_with("rsi · home project · PM seat Running"),
        "{lines:?}"
    );
    assert!(
        lines[1].contains("6 live · 1 mgr · 1 lead · 3 wrk · 1 rev"),
        "{lines:?}"
    );
    let by_model = lines.iter().find(|l| l.starts_with("By model")).unwrap();
    assert!(
        by_model.contains("Claude claude-opus-5-5 xhigh ×1"),
        "{by_model}"
    );
    assert!(by_model.contains("Codex gpt-6-astra ×2"), "{by_model}");
    let usage = lines.iter().find(|l| l.starts_with("Usage 24h")).unwrap();
    assert!(
        usage.contains("Claude claude-opus-5-5 1.2M in 80k out $3.20"),
        "{usage}"
    );
    let flow = lines.iter().find(|l| l.starts_with("Issues")).unwrap();
    assert!(
        flow.contains("12 open · 3 in progress · 5 created · 7 closed in 24h · 1 operator request · landings 4 published, 1 refused"),
        "{flow}"
    );
}

#[test]
fn total_detail_lists_why_a_snapshot_is_incomplete() {
    let f = fixture();
    let mut snapshot = f.snapshot;
    snapshot.incomplete_reasons = vec!["sessions".into(), "landings".into()];
    snapshot.totals.incomplete = true;
    let mut state = loaded(snapshot);
    state.selected = state.row_count() - 1;
    let lines = state.detail_lines();
    assert!(lines[0].starts_with("TOTAL"), "{lines:?}");
    assert!(
        lines
            .iter()
            .any(|l| l == "Incomplete (lower bound): sessions, landings"),
        "{lines:?}"
    );
}
