//! #2502: the Budget panel shows every cap of the global node and the PM
//! below it as used / limit with the binding one marked, and a raise edits
//! the right grant through the existing operator RPC, then reads it back.

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use rsi_common::global_manager::GlobalManagerWorkspaceV1;
use rsi_common::harness_manager_v2::{
    HarnessManagerPolicyConfigV2, ManagerOperatingModeV2, ManagerPolicyV2,
};
use rsi_common::manager_budget::{
    BUDGET_RESETS_TEXT, BudgetCapFieldV1, BudgetCapV1, BudgetNodeKindV1, BudgetNodeV1,
    BudgetProjectV1, GlobalManagerBudgetV1, mark_binding,
};
use rsi_common::portfolio_nodes::PortfolioNodeV1;
use uuid::Uuid;

use super::*;
use crate::app::app_test_helpers::with_session_list;
use crate::overlay::global_manager_workspace::tests::{fake_daemon, grant};
use crate::overlay::global_manager_workspace::{self as gm, GlobalManagerWorkspaceState};

const GM: Uuid = Uuid::from_u128(0x1a2b_3c4d_0000_0000_0000_0000_0000_0001);
const SEAT: Uuid = Uuid::from_u128(0x5eed);

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn caps(created: (f64, f64), active: (f64, f64), spend: Option<f64>) -> Vec<BudgetCapV1> {
    vec![
        BudgetCapV1 {
            field: BudgetCapFieldV1::CreatedSessions,
            used: created.0,
            limit: Some(created.1),
            binding: false,
            spend_unknown: false,
        },
        BudgetCapV1 {
            field: BudgetCapFieldV1::ActiveSessions,
            used: active.0,
            limit: Some(active.1),
            binding: false,
            spend_unknown: false,
        },
        BudgetCapV1 {
            field: BudgetCapFieldV1::CreatedContainers,
            used: 2.0,
            limit: Some(64.0),
            binding: false,
            spend_unknown: false,
        },
        BudgetCapV1 {
            field: BudgetCapFieldV1::SpendUsd,
            used: 12.5,
            limit: spend,
            binding: false,
            spend_unknown: false,
        },
    ]
}

/// The operator's case: the global node's 388 created sessions are spent
/// while the PM's own 1024 have room, so the global cap binds.
fn budget(project: Uuid, gm_created_limit: f64) -> GlobalManagerBudgetV1 {
    let mut chain = vec![
        BudgetNodeV1 {
            kind: BudgetNodeKindV1::Portfolio,
            node_id: GM,
            label: "global".into(),
            version: 3,
            epoch: 3,
            counted_since: Some("2026-10-01T00:00:00.000000000Z".into()),
            caps: caps((388.0, gm_created_limit), (3.0, 8.0), None),
        },
        BudgetNodeV1 {
            kind: BudgetNodeKindV1::Project,
            node_id: project,
            label: "rsi".into(),
            version: 7,
            epoch: 2,
            counted_since: None,
            caps: caps((120.0, 1024.0), (3.0, 4.0), Some(100.0)),
        },
    ];
    mark_binding(&mut chain);
    GlobalManagerBudgetV1 {
        node_id: GM,
        node_label: "global".into(),
        projects: vec![BudgetProjectV1 {
            project_id: project,
            project_name: "rsi".into(),
            chain,
        }],
        resets: BUDGET_RESETS_TEXT.into(),
    }
}

fn portfolio_node(project: Uuid) -> PortfolioNodeV1 {
    let mut grant = grant(SEAT, &[project], "active");
    grant.project_policy.max_created_sessions = 388;
    grant.project_policy.max_active_sessions = 8;
    PortfolioNodeV1 {
        node_id: GM,
        tier_label: "global".into(),
        state: "active".into(),
        authority_epoch: 3,
        parent_node_id: None,
        grantor: "operator".into(),
        seat_root_session_id: SEAT,
        max_direct_reports: 5,
        child_policy: None,
        grant,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn app_with_workspace() -> crate::app::App {
    let mut app = with_session_list(0);
    app.overlay = OverlayState::GlobalManagerWorkspace(Box::new(GlobalManagerWorkspaceState {
        snapshot: Some(GlobalManagerWorkspaceV1::default()),
        ..GlobalManagerWorkspaceState::default()
    }));
    app
}

fn panel_of(app: &crate::app::App) -> &BudgetPanel {
    match &app.overlay {
        OverlayState::GlobalManagerWorkspace(state) => {
            state.budget.as_ref().expect("the Budget panel is open")
        }
        _ => panic!("the global manager workspace is open"),
    }
}

/// Each line of the buffer, right-trimmed.
fn screen(app: &mut crate::app::App, width: u16, height: u16) -> String {
    let _theme = crate::ui::theme::pin_theme_state();
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| crate::ui::overlay::render_overlay(frame, frame.area(), app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_budget_panel_shows_the_gm_node_and_pm_caps_with_the_binding_one_marked() {
    let project = Uuid::new_v4();
    let mut app = app_with_workspace();
    let daemon = fake_daemon(&mut app).await;
    daemon.serve(
        "GetGlobalManagerBudget",
        serde_json::to_value(budget(project, 388.0)).unwrap(),
    );
    gm::handle_key(&mut app, key(KeyCode::Char('b'))).await;
    assert_eq!(daemon.calls("GetGlobalManagerBudget"), 1);
    let panel = panel_of(&app);
    assert_eq!(panel.rows().len(), 8, "four caps for each of two nodes");
    let text = screen(&mut app, 140, 40);
    for needle in [
        "Above all projects / Budget",
        "1 project",
        "no timed reset",
        "RSI",
        "PORTFOLIO global · governs this node and everything under it · grant v3",
        "counted since 10-01 00:00",
        "PM rsi · governs this PM's own launches · policy v7",
        // The spent global cap binds: glyph, colour and the words.
        "× created sessions",
        "388 / 388",
        "binding · spent: raise this one",
        // The PM's own created sessions have room and do not bind.
        "120 / 1024",
        "904 left",
        // The PM's spend cap is the only one: it binds.
        "12.50 / 100.00",
        "j/k move · + raise one step · 0-9 type a limit",
    ] {
        assert!(text.contains(needle), "{needle} missing from\n{text}");
    }
    let row = text
        .lines()
        .find(|line| line.contains("388 / 388"))
        .unwrap();
    assert!(row.contains("binding"), "{row}");
    let pm_row = text
        .lines()
        .find(|line| line.contains("120 / 1024"))
        .unwrap();
    assert!(pm_row.contains("904 left"), "{pm_row}");
}

#[tokio::test]
async fn plus_raises_the_gm_node_grant_through_configure_portfolio_node_and_reads_it_back() {
    let project = Uuid::new_v4();
    let mut app = app_with_workspace();
    let daemon = fake_daemon(&mut app).await;
    daemon.serve(
        "GetGlobalManagerBudget",
        serde_json::to_value(budget(project, 388.0)).unwrap(),
    );
    gm::handle_key(&mut app, key(KeyCode::Char('b'))).await;
    // The first row is the global node's created sessions: + stages a raise.
    gm::handle_key(&mut app, key(KeyCode::Char('+'))).await;
    let pending = panel_of(&app).pending.clone().expect("a staged raise");
    assert_eq!(
        (pending.kind, pending.node_id, pending.field),
        (
            BudgetNodeKindV1::Portfolio,
            GM,
            BudgetCapFieldV1::CreatedSessions
        )
    );
    assert_eq!((pending.from, pending.to), (Some(388.0), Some(420.0)));
    let text = screen(&mut app, 140, 40);
    for needle in [
        "Set portfolio global created sessions in rsi: 388 → 420",
        "Governs this node and everything under it",
        "RPC: ConfigurePortfolioNode (expects grant v3 · epoch 3)",
        "Enter/y save and read back",
    ] {
        assert!(text.contains(needle), "{needle} missing from\n{text}");
    }
    assert_eq!(
        daemon.calls("ConfigurePortfolioNode"),
        0,
        "nothing sent yet"
    );

    let node = portfolio_node(project);
    let mut saved = node.clone();
    saved.grant.grant_version = 4;
    saved.authority_epoch = 4;
    saved.grant.project_policy.max_created_sessions = 420;
    daemon.serve("GetPortfolioNode", serde_json::to_value(&node).unwrap());
    daemon.serve(
        "ConfigurePortfolioNode",
        serde_json::to_value(&saved).unwrap(),
    );
    daemon.serve(
        "GetGlobalManagerBudget",
        serde_json::to_value(budget(project, 420.0)).unwrap(),
    );
    gm::handle_key(&mut app, key(KeyCode::Enter)).await;
    let params = daemon
        .last_params("ConfigurePortfolioNode")
        .expect("the raise edits the global node's grant");
    assert_eq!(params["node_id"], serde_json::json!(GM));
    assert_eq!(params["policy"]["max_created_sessions"], 420);
    assert_eq!(params["expected_node_grant_version"], 3);
    assert_eq!(params["expected_authority_epoch"], 3);
    assert_eq!(params["confirm_cap_reductions"], true);
    assert_eq!(daemon.calls("ConfigureHarnessManagerPolicy"), 0);
    assert_eq!(daemon.calls("GetGlobalManagerBudget"), 2, "read back");
    let panel = panel_of(&app);
    assert!(panel.pending.is_none());
    let notice = panel.notice.clone().unwrap_or_default();
    assert!(
        notice.contains("Saved portfolio global created sessions: 388 → 420 (v4); read back 420"),
        "{notice} / {:?}",
        panel.error
    );
}

#[tokio::test]
async fn a_typed_number_edits_the_pm_policy_through_configure_harness_manager_policy() {
    let project = Uuid::new_v4();
    let mut app = app_with_workspace();
    let daemon = fake_daemon(&mut app).await;
    daemon.serve(
        "GetGlobalManagerBudget",
        serde_json::to_value(budget(project, 388.0)).unwrap(),
    );
    gm::handle_key(&mut app, key(KeyCode::Char('b'))).await;
    // Rows 0-3 are the global node's caps; row 4 is the PM's created sessions.
    for _ in 0..4 {
        gm::handle_key(&mut app, key(KeyCode::Char('j'))).await;
    }
    for ch in ['9', '0', '0'] {
        gm::handle_key(&mut app, key(KeyCode::Char(ch))).await;
    }
    assert_eq!(panel_of(&app).entry.as_deref(), Some("900"));
    gm::handle_key(&mut app, key(KeyCode::Enter)).await;
    let pending = panel_of(&app).pending.clone().expect("a staged edit");
    assert_eq!(
        (pending.kind, pending.node_id, pending.to),
        (BudgetNodeKindV1::Project, project, Some(900.0))
    );
    let text = screen(&mut app, 140, 40);
    assert!(
        text.contains("Governs this PM's own launches only; the portfolio caps above still apply."),
        "{text}"
    );
    let current = HarnessManagerPolicyConfigV2 {
        project_id: project,
        manager_session_id: Uuid::new_v4(),
        scope_version: 2,
        row_version: 7,
        policy: ManagerPolicyV2 {
            mode: ManagerOperatingModeV2::Execute,
            max_created_sessions: 1024,
            ..ManagerPolicyV2::default()
        },
        updated_at: Utc::now(),
        revoked: false,
    };
    let mut saved = current.clone();
    saved.row_version = 8;
    saved.policy.max_created_sessions = 900;
    daemon.serve(
        "GetHarnessManagerPolicy",
        serde_json::to_value(&current).unwrap(),
    );
    daemon.serve(
        "ConfigureHarnessManagerPolicy",
        serde_json::to_value(&saved).unwrap(),
    );
    gm::handle_key(&mut app, key(KeyCode::Enter)).await;
    let params = daemon
        .last_params("ConfigureHarnessManagerPolicy")
        .expect("the edit goes to the PM's own policy");
    assert_eq!(params["project_id"], serde_json::json!(project));
    assert_eq!(params["expected_policy_version"], 7);
    assert_eq!(params["expected_scope_version"], 2);
    assert_eq!(params["policy"]["max_created_sessions"], 900);
    assert_eq!(daemon.calls("ConfigurePortfolioNode"), 0);
    // The fake still serves the old snapshot, so the read-back disagrees and
    // the panel says so instead of claiming success.
    let error = panel_of(&app).error.clone().unwrap_or_default();
    assert!(error.contains("reads back 1024"), "{error}");
    // b (or Esc) returns to the seats.
    gm::handle_key(&mut app, key(KeyCode::Char('b'))).await;
    match &app.overlay {
        OverlayState::GlobalManagerWorkspace(state) => assert!(state.budget.is_none()),
        _ => panic!("the workspace stays open"),
    }
}

fn select(app: &mut crate::app::App, row: usize) {
    match &mut app.overlay {
        OverlayState::GlobalManagerWorkspace(state) => {
            state.budget.as_mut().expect("the panel is open").selected = row;
        }
        _ => panic!("the global manager workspace is open"),
    }
}

async fn type_number(app: &mut crate::app::App, text: &str) {
    for ch in text.chars() {
        gm::handle_key(app, key(KeyCode::Char(ch))).await;
    }
    gm::handle_key(app, key(KeyCode::Enter)).await;
}

/// The PM policy the fake daemon serves for a PM edit.
fn pm_policy(project: Uuid, edit: impl Fn(&mut ManagerPolicyV2)) -> HarnessManagerPolicyConfigV2 {
    let mut policy = ManagerPolicyV2 {
        mode: ManagerOperatingModeV2::Execute,
        max_created_sessions: 1024,
        ..ManagerPolicyV2::default()
    };
    edit(&mut policy);
    HarnessManagerPolicyConfigV2 {
        project_id: project,
        manager_session_id: Uuid::new_v4(),
        scope_version: 2,
        row_version: 7,
        policy,
        updated_at: Utc::now(),
        revoked: false,
    }
}

/// Rows of `budget(..)`: 0-3 the global node (created, active, containers,
/// spend), 4-7 the PM's.
const GM_ACTIVE: usize = 1;
const GM_CREATED: usize = 0;
const GM_SPEND: usize = 3;
const PM_SPEND: usize = 7;

/// #2522: `+` never turns an unlimited spend into a finite cap (that would
/// lower the allowance, and a portfolio save confirms reductions on its
/// own); typing a number installs one.
#[tokio::test]
async fn plus_leaves_unlimited_spend_unchanged_for_the_portfolio_and_the_pm() {
    let project = Uuid::new_v4();
    let mut snapshot = budget(project, 388.0);
    snapshot.projects[0].chain[1]
        .caps
        .iter_mut()
        .find(|cap| cap.field == BudgetCapFieldV1::SpendUsd)
        .unwrap()
        .limit = None;
    let mut app = app_with_workspace();
    let daemon = fake_daemon(&mut app).await;
    daemon.serve(
        "GetGlobalManagerBudget",
        serde_json::to_value(&snapshot).unwrap(),
    );
    gm::handle_key(&mut app, key(KeyCode::Char('b'))).await;
    for (row, name) in [(GM_SPEND, "portfolio"), (PM_SPEND, "PM")] {
        select(&mut app, row);
        let (_, _, cap) = panel_of(&app)
            .resolve(panel_of(&app).selected_row().unwrap())
            .unwrap();
        assert_eq!(cap.limit, None, "{name} spend is uncapped");
        gm::handle_key(&mut app, key(KeyCode::Char('+'))).await;
        let panel = panel_of(&app);
        assert!(
            panel.pending.is_none(),
            "{name}: + staged {:?}",
            panel.pending
        );
        let notice = panel.notice.clone().unwrap_or_default();
        assert!(
            notice.contains("Spend is unlimited") && notice.contains("unchanged"),
            "{name}: {notice}"
        );
    }
    assert_eq!(daemon.calls("ConfigurePortfolioNode"), 0);
    assert_eq!(daemon.calls("ConfigureHarnessManagerPolicy"), 0);
    // A typed number is the explicit way to install a finite cap.
    select(&mut app, PM_SPEND);
    type_number(&mut app, "25").await;
    let pending = panel_of(&app).pending.clone().expect("a typed cap stages");
    assert_eq!((pending.from, pending.to), (None, Some(25.0)));
    // A finite cap still steps up monotonically while staged.
    gm::handle_key(&mut app, key(KeyCode::Char('+'))).await;
    assert_eq!(panel_of(&app).pending.as_ref().unwrap().to, Some(35.0));
}

/// #2523: an accepted entry is normalized before it is staged, so the
/// review shows what the daemon stores and the read-back agrees with it.
#[tokio::test]
async fn typed_entries_are_normalized_before_review_and_match_the_read_back() {
    let project = Uuid::new_v4();
    let mut app = app_with_workspace();
    let daemon = fake_daemon(&mut app).await;
    daemon.serve(
        "GetGlobalManagerBudget",
        serde_json::to_value(budget(project, 388.0)).unwrap(),
    );
    gm::handle_key(&mut app, key(KeyCode::Char('b'))).await;

    // Active sessions 0 is stored as 1; created sessions 12.5 as 13.
    for (row, entry, staged) in [
        (GM_ACTIVE, "0", Some(1.0)),
        (GM_CREATED, "12.5", Some(13.0)),
        (PM_SPEND, "0", None),
    ] {
        select(&mut app, row);
        type_number(&mut app, entry).await;
        let pending = panel_of(&app).pending.clone().expect("staged");
        assert_eq!(pending.to, staged, "entry {entry}");
        gm::handle_key(&mut app, key(KeyCode::Esc)).await;
    }

    // Save created sessions 12.5: the daemon stores 13 and the panel reads
    // back 13, which is a success, not a mismatch.
    select(&mut app, GM_CREATED);
    type_number(&mut app, "12.5").await;
    let text = screen(&mut app, 140, 40);
    assert!(
        text.contains("Set portfolio global created sessions in rsi: 388 → 13"),
        "{text}"
    );
    let node = portfolio_node(project);
    let mut saved = node.clone();
    saved.grant.grant_version = 4;
    saved.authority_epoch = 4;
    saved.grant.project_policy.max_created_sessions = 13;
    daemon.serve("GetPortfolioNode", serde_json::to_value(&node).unwrap());
    daemon.serve(
        "ConfigurePortfolioNode",
        serde_json::to_value(&saved).unwrap(),
    );
    daemon.serve(
        "GetGlobalManagerBudget",
        serde_json::to_value(budget(project, 13.0)).unwrap(),
    );
    gm::handle_key(&mut app, key(KeyCode::Enter)).await;
    let params = daemon.last_params("ConfigurePortfolioNode").unwrap();
    assert_eq!(params["policy"]["max_created_sessions"], 13);
    let panel = panel_of(&app);
    assert_eq!(panel.error, None);
    let notice = panel.notice.clone().unwrap_or_default();
    assert!(notice.contains("388 → 13 (v4); read back 13"), "{notice}");

    // Spend 0 means no cap: the review says so and the read-back of an
    // uncapped spend is a success.
    select(&mut app, PM_SPEND);
    type_number(&mut app, "0").await;
    assert_eq!(panel_of(&app).pending.as_ref().unwrap().to, None);
    let current = pm_policy(project, |policy| policy.max_spend_usd = Some(100.0));
    let mut saved_pm = current.clone();
    saved_pm.row_version = 8;
    saved_pm.policy.max_spend_usd = None;
    daemon.serve(
        "GetHarnessManagerPolicy",
        serde_json::to_value(&current).unwrap(),
    );
    daemon.serve(
        "ConfigureHarnessManagerPolicy",
        serde_json::to_value(&saved_pm).unwrap(),
    );
    let mut uncapped = budget(project, 13.0);
    uncapped.projects[0].chain[1]
        .caps
        .iter_mut()
        .find(|cap| cap.field == BudgetCapFieldV1::SpendUsd)
        .unwrap()
        .limit = None;
    daemon.serve(
        "GetGlobalManagerBudget",
        serde_json::to_value(&uncapped).unwrap(),
    );
    gm::handle_key(&mut app, key(KeyCode::Enter)).await;
    let params = daemon.last_params("ConfigureHarnessManagerPolicy").unwrap();
    assert_eq!(params["policy"]["max_spend_usd"], serde_json::Value::Null);
    let panel = panel_of(&app);
    assert_eq!(panel.error, None, "an uncapped read-back is a success");
    assert!(
        panel
            .notice
            .clone()
            .unwrap_or_default()
            .contains("read back none"),
        "{:?}",
        panel.notice
    );
}

/// #2519: a finite spend cap with an unknown spend admits nothing
/// (`manager_v2_spend_unknown`), so the panel shows the cap blocked, not
/// "100 left", and marks it binding.
#[tokio::test]
async fn an_unknown_spend_under_a_finite_cap_shows_blocked_not_headroom() {
    let project = Uuid::new_v4();
    let mut snapshot = budget(project, 388.0);
    {
        let pm = &mut snapshot.projects[0].chain[1];
        let spend = pm
            .caps
            .iter_mut()
            .find(|cap| cap.field == BudgetCapFieldV1::SpendUsd)
            .unwrap();
        spend.used = 0.0;
        spend.limit = Some(100.0);
        spend.spend_unknown = true;
    }
    mark_binding(&mut snapshot.projects[0].chain);
    let spend = snapshot.projects[0].chain[1]
        .cap(BudgetCapFieldV1::SpendUsd)
        .unwrap();
    assert_eq!(spend.headroom(), Some(0.0));
    assert!(spend.binding, "admission refuses, so this cap binds");
    let mut app = app_with_workspace();
    let daemon = fake_daemon(&mut app).await;
    daemon.serve(
        "GetGlobalManagerBudget",
        serde_json::to_value(&snapshot).unwrap(),
    );
    gm::handle_key(&mut app, key(KeyCode::Char('b'))).await;
    let text = screen(&mut app, 200, 40);
    let line = text
        .lines()
        .find(|line| line.contains("spend (USD)") && line.contains("0.00 / 100.00"))
        .unwrap_or_else(|| panic!("PM spend row missing from\n{text}"));
    assert!(line.contains("blocked"), "{line}");
    assert!(line.contains("manager_v2_spend_unknown"), "{line}");
}

/// #2518: the panel's reset statement says creations restart on a grant
/// edit but spend does not.
#[test]
fn the_reset_statement_separates_creation_counts_from_spend() {
    assert!(BUDGET_RESETS_TEXT.contains("new count of creations"));
    assert!(BUDGET_RESETS_TEXT.contains("spend is NOT reset"));
}
