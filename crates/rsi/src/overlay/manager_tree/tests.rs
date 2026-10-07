use std::sync::{Arc, Mutex};

use super::*;
use crate::client::DaemonClient;
use crate::types::Pane;
use crossterm::event::KeyModifiers;
use rsi_common::global_manager::GlobalManagerGrantV1;
use rsi_common::harness_manager::{HarnessManagerConfigV1, HarnessManagerScopeModeV1};
use rsi_common::manager_nodes::{
    ManagerNodeAllowanceV1, ManagerNodeGrantStateV1, ManagerNodeGrantV1, ManagerNodeSelectorV1,
    ManagerNodeStateV1, ManagerNodeViewV1,
};
use rsi_common::manager_tree::{ManagerTreeGrantV1, ManagerTreeLoadV1, ManagerTreeSeatV1};
use rsi_common::types::SessionStatus;
use serde_json::{Value, json};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn row(
    key: &str,
    parent: Option<&str>,
    depth: u16,
    kind: ManagerTreeKindV1,
    focus: Option<Uuid>,
) -> ManagerTreeRowV1 {
    ManagerTreeRowV1 {
        key: key.into(),
        parent_key: parent.map(str::to_string),
        depth,
        kind,
        label: key.into(),
        tier_label: None,
        grantor: None,
        project_id: None,
        node_id: None,
        epic_id: None,
        scope: None,
        seat: focus.map(|session_id| ManagerTreeSeatV1 {
            session_id,
            status: SessionStatus::Running,
            model: Some("claude-opus-5-5".into()),
            context_fill_pct: Some(41.6),
            updated_at: chrono::Utc::now(),
        }),
        focus_session_id: focus,
        grant: None,
        launches: Vec::new(),
        load: ManagerTreeLoadV1 {
            running_workers: Some(2),
            direct_reports: None,
            pending_escalations: Some(1),
            pending_decisions: Some(0),
        },
        complete: true,
    }
}

fn page(total: u64, next_after: Option<&str>) -> GetManagerTreeResultV1 {
    GetManagerTreeResultV1 {
        rows: vec![],
        next_after: next_after.map(str::to_string),
        total_rows: total,
        complete: true,
        global_grant_version: Some(3),
    }
}

fn tree(focus: Uuid) -> ManagerTreeState {
    let mut state = ManagerTreeState::default();
    let rows = vec![
        row("global", None, 0, ManagerTreeKindV1::Global, None),
        row(
            "project:a",
            Some("global"),
            1,
            ManagerTreeKindV1::Project,
            Some(focus),
        ),
        row(
            "area:x",
            Some("project:a"),
            2,
            ManagerTreeKindV1::Area,
            None,
        ),
        row("epic:e", Some("area:x"), 3, ManagerTreeKindV1::Epic, None),
        row(
            "project:b",
            Some("global"),
            1,
            ManagerTreeKindV1::Project,
            None,
        ),
    ];
    state.install(rows, &page(5, None));
    state
}

fn tree_state(app: &App) -> &ManagerTreeState {
    let OverlayState::ManagerTree(state) = &app.overlay else {
        panic!("manager tree overlay closed")
    };
    state
}

fn visible_keys(state: &ManagerTreeState) -> Vec<&str> {
    state
        .visible()
        .iter()
        .map(|i| state.rows[*i].key.as_str())
        .collect()
}

// ---- keyboard flow -------------------------------------------------------

#[tokio::test]
async fn keys_navigate_fold_and_close() {
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    app.overlay = OverlayState::ManagerTree(Box::new(tree(Uuid::new_v4())));
    for code in [KeyCode::Char('j'), KeyCode::Char('j'), KeyCode::Char('j')] {
        handle_key(&mut app, key(code)).await;
    }
    assert_eq!(tree_state(&app).selected_row().unwrap().key, "epic:e");
    // h on a leaf folds its parent and selects it; the subtree disappears.
    handle_key(&mut app, key(KeyCode::Char('h'))).await;
    let state = tree_state(&app);
    assert_eq!(state.selected_row().unwrap().key, "area:x");
    assert_eq!(
        visible_keys(state),
        ["global", "project:a", "area:x", "project:b"]
    );
    handle_key(&mut app, key(KeyCode::Char('l'))).await;
    assert_eq!(tree_state(&app).visible().len(), 5);
    handle_key(&mut app, key(KeyCode::Char('G'))).await;
    handle_key(&mut app, key(KeyCode::Char('g'))).await;
    assert_eq!(tree_state(&app).selected_row().unwrap().key, "global");
    handle_key(&mut app, key(KeyCode::Esc)).await;
    assert!(matches!(app.overlay, OverlayState::None));
}

#[tokio::test]
async fn space_toggles_and_fold_all_keeps_the_selection_on_its_ancestor() {
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    app.overlay = OverlayState::ManagerTree(Box::new(tree(Uuid::new_v4())));
    handle_key(&mut app, key(KeyCode::Char('j'))).await; // project:a
    handle_key(&mut app, key(KeyCode::Char(' '))).await;
    assert_eq!(
        visible_keys(tree_state(&app)),
        ["global", "project:a", "project:b"]
    );
    handle_key(&mut app, key(KeyCode::Char(' '))).await;
    assert_eq!(tree_state(&app).visible().len(), 5);
    for _ in 0..2 {
        handle_key(&mut app, key(KeyCode::Char('j'))).await;
    }
    assert_eq!(tree_state(&app).selected_row().unwrap().key, "epic:e");
    handle_key(&mut app, key(KeyCode::Char('H'))).await;
    let state = tree_state(&app);
    assert_eq!(visible_keys(state), ["global"]);
    assert_eq!(state.selected_row().unwrap().key, "global");
    // Expand-all restores every row and keeps the selected key.
    handle_key(&mut app, key(KeyCode::Char('L'))).await;
    let state = tree_state(&app);
    assert_eq!(state.visible().len(), 5);
    assert_eq!(state.selected_row().unwrap().key, "global");
}

#[tokio::test]
async fn page_keys_move_by_the_drawn_viewport() {
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    let state = tree(Uuid::new_v4());
    state.viewport.set(2);
    app.overlay = OverlayState::ManagerTree(Box::new(state));
    handle_key(&mut app, key(KeyCode::PageDown)).await;
    assert_eq!(tree_state(&app).selected, 2);
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
    )
    .await;
    assert_eq!(tree_state(&app).selected, 4);
    handle_key(&mut app, key(KeyCode::PageDown)).await;
    assert_eq!(tree_state(&app).selected, 4, "clamps at the last row");
    handle_key(&mut app, key(KeyCode::PageUp)).await;
    assert_eq!(tree_state(&app).selected, 2);
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    )
    .await;
    assert_eq!(tree_state(&app).selected, 0);
}

#[tokio::test]
async fn enter_on_an_epic_opens_its_lead_and_remembers_the_selection() {
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    let session = *app.sessions.keys().next().unwrap();
    let mut state = tree(Uuid::new_v4());
    state.rows[3].focus_session_id = Some(session);
    state.collapsed.insert("project:b".into());
    app.overlay = OverlayState::ManagerTree(Box::new(state));
    for _ in 0..3 {
        handle_key(&mut app, key(KeyCode::Char('j'))).await;
    }
    handle_key(&mut app, key(KeyCode::Enter)).await;
    assert!(matches!(app.overlay, OverlayState::None));
    assert!(
        matches!(app.focused_pane(), Some(Pane::SessionDetail { session_id }) if *session_id == session)
    );
    let memory = app.manager_tree_memory.clone().expect("memory saved");
    assert_eq!(memory.selected_key.as_deref(), Some("epic:e"));
    assert!(memory.collapsed.contains("project:b"));
    // Reopening restores the selection and the fold.
    let mut reopened = tree(session);
    reopened.restore(&memory);
    assert_eq!(reopened.selected_row().unwrap().key, "epic:e");
}

/// #1240: Enter on a Portfolio, Project or Area row opens that node's
/// manager console (a pre-#1236 global row opens the `gm` console).
#[tokio::test]
async fn enter_on_a_manager_node_row_opens_its_console() {
    let project = Uuid::new_v4();
    let area = Uuid::new_v4();
    let portfolio = Uuid::new_v4();
    let mut rows = tree(Uuid::new_v4()).rows;
    rows[0].kind = ManagerTreeKindV1::Portfolio;
    rows[0].node_id = Some(portfolio);
    rows[1].project_id = Some(project);
    rows[2].node_id = Some(area);
    assert_eq!(
        console_target(&rows[0]),
        Some(Console::Node(ManagerNodeRefV1::Portfolio {
            node_id: portfolio
        }))
    );
    assert_eq!(
        console_target(&rows[1]),
        Some(Console::Node(ManagerNodeRefV1::Project {
            project_id: project
        }))
    );
    assert_eq!(
        console_target(&rows[2]),
        Some(Console::Node(ManagerNodeRefV1::Area { node_id: area }))
    );
    assert_eq!(
        console_target(&rows[3]),
        None,
        "an Epic row jumps to its lead"
    );
    assert_eq!(
        console_target(&tree(Uuid::new_v4()).rows[0]),
        Some(Console::Global)
    );

    for (moves, expected) in [
        (0, ManagerNodeRefV1::Portfolio { node_id: portfolio }),
        (
            1,
            ManagerNodeRefV1::Project {
                project_id: project,
            },
        ),
        (2, ManagerNodeRefV1::Area { node_id: area }),
    ] {
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let mut state = tree(Uuid::new_v4());
        state.rows[0].kind = ManagerTreeKindV1::Portfolio;
        state.rows[0].node_id = Some(portfolio);
        state.rows[1].project_id = Some(project);
        state.rows[2].node_id = Some(area);
        app.overlay = OverlayState::ManagerTree(Box::new(state));
        for _ in 0..moves {
            handle_key(&mut app, key(KeyCode::Char('j'))).await;
        }
        handle_key(&mut app, key(KeyCode::Enter)).await;
        let OverlayState::GlobalManagerWorkspace(console) = &app.overlay else {
            panic!("the console opens for {expected:?}");
        };
        assert_eq!(console.target, Some(expected));
        assert!(
            app.manager_tree_memory.is_some(),
            "the tree remembers its place"
        );
    }
}

#[tokio::test]
async fn enter_on_an_epic_without_a_lead_keeps_the_view_open() {
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    app.overlay = OverlayState::ManagerTree(Box::new(tree(Uuid::new_v4())));
    for _ in 0..3 {
        handle_key(&mut app, key(KeyCode::Char('j'))).await;
    }
    handle_key(&mut app, key(KeyCode::Enter)).await;
    let state = tree_state(&app);
    let notice = state.notice.as_ref().expect("notice");
    assert_eq!(notice.tone, Tone::Error);
    assert!(notice.text.contains("no seat session"), "{notice:?}");
}

#[tokio::test]
async fn o_opens_the_selected_project_workspace() {
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    let project = Uuid::new_v4();
    let mut state = tree(Uuid::new_v4());
    state.rows[1].project_id = Some(project);
    app.overlay = OverlayState::ManagerTree(Box::new(state));
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    handle_key(&mut app, key(KeyCode::Char('o'))).await;
    assert!(matches!(app.overlay, OverlayState::None));
    assert_eq!(app.tabs[app.active_tab].project_id, Some(project));
    assert_eq!(
        app.manager_tree_memory
            .as_ref()
            .and_then(|memory| memory.selected_key.as_deref()),
        Some("project:a")
    );
}

// ---- counts and refresh -------------------------------------------------

#[test]
fn counts_show_totals_and_mark_unknown_values() {
    let mut state = tree(Uuid::new_v4());
    assert_eq!(state.count_line(), "5 nodes · global grant v3");
    state.total_rows = 12;
    state.complete = false;
    let line = state.count_line();
    assert!(line.starts_with("5 of 12 nodes loaded"), "{line}");
    assert!(line.contains("traversal incomplete"), "{line}");
    assert_eq!(state.unloaded_rows(), 7);
    let mut unknown = row("area:x", None, 0, ManagerTreeKindV1::Area, None);
    unknown.load.running_workers = None;
    unknown.complete = false;
    let text = row_text(&unknown);
    assert!(text.contains("run ? reports ? esc 1 dec 0"), "{text}");
    assert!(text.contains("children incomplete"), "{text}");
    assert!(text.contains("no seat"), "{text}");
}

#[test]
fn row_text_shows_seat_health_scope_grant_and_load() {
    let mut node = row(
        "area:x",
        None,
        2,
        ManagerTreeKindV1::Area,
        Some(Uuid::new_v4()),
    );
    node.scope = Some("0 groups, 1 epic".into());
    node.grant = Some(ManagerTreeGrantV1 {
        grant_version: 2,
        capabilities: vec![rsi_common::harness_manager_v2::ManagerCapabilityV2::WorkPlan],
        max_active_sessions: 3,
        max_created_sessions: 5,
        max_created_containers: 0,
        max_direct_reports: 4,
        max_spend_usd: None,
        reserved: vec![rsi_common::manager_tree::ManagerTreeReservedV1 {
            resource_kind: "active_sessions".into(),
            amount: 2,
        }],
    });
    let text = row_text(&node);
    for needle in [
        "AREA area:x",
        "Running claude-opus-5-5 ctx 42%",
        "0 groups, 1 epic",
        "caps WorkPlan",
        "allow active 3 sessions 5 reports 4 reserved active_sessions=2",
        "run 2 reports ? esc 1 dec 0",
    ] {
        assert!(text.contains(needle), "{needle} missing from {text}");
    }
}

#[test]
fn row_text_shows_the_effective_launch_set() {
    let mut node = row("project:p", None, 1, ManagerTreeKindV1::Project, None);
    assert!(launches_text(&node).is_none());
    node.launches = vec![
        rsi_common::harness_manager_v2::ManagerLaunchChoiceV2 {
            provider: rsi_common::types::SessionProvider::Claude,
            model: "claude-sonnet-5-5".into(),
            effort: Some("high".into()),
        },
        rsi_common::harness_manager_v2::ManagerLaunchChoiceV2 {
            provider: rsi_common::types::SessionProvider::Codex,
            model: "gpt-6-astra".into(),
            effort: Some("xhigh".into()),
        },
    ];
    let text = row_text(&node);
    assert!(
        text.contains("launches sonnet-5-5/high Codex:gpt-6-astra/xhigh"),
        "{text}"
    );
}

#[test]
fn install_keeps_the_selection_on_the_same_row_after_a_refresh() {
    let mut state = tree(Uuid::new_v4());
    state.selected = 4;
    assert_eq!(state.selected_row().unwrap().key, "project:b");
    let mut rows = state.rows.clone();
    rows.remove(3); // the Epic moved away between refreshes
    let mut result = page(4, None);
    result.global_grant_version = Some(4);
    state.install(rows, &result);
    assert_eq!(state.selected_row().unwrap().key, "project:b");
    assert_eq!(state.total_rows, 4);
    assert_eq!(state.global_grant_version, Some(4));
}

#[test]
fn impact_counts_descendants_and_marks_incomplete_and_unloaded() {
    let mut state = tree(Uuid::new_v4());
    let impact = state.impact(1); // project:a
    assert_eq!(impact.descendants, vec![2, 3]);
    assert_eq!((impact.projects, impact.areas, impact.epics), (0, 1, 1));
    assert!(!impact.incomplete && !impact.unloaded);
    let global = state.impact(0);
    assert_eq!(global.descendants.len(), 4);
    assert_eq!(global.projects, 2);
    assert_eq!(global.seated, 1);
    // A partial subtree at the end of the loaded pages says so.
    state.rows[3].complete = false;
    state.next_after = Some("project:b".into());
    state.total_rows = 9;
    assert!(state.impact(1).incomplete);
    assert!(state.impact(4).unloaded);
    let lines = state.impact_lines(4).join("\n");
    assert!(lines.contains("4 more node(s) not loaded"), "{lines}");
}

#[test]
fn impact_lists_affected_seats_with_an_explicit_more_marker() {
    let mut state = ManagerTreeState::default();
    let mut rows = vec![row(
        "global",
        None,
        0,
        ManagerTreeKindV1::Global,
        Some(Uuid::new_v4()),
    )];
    for n in 0..12 {
        rows.push(row(
            &format!("project:{n}"),
            Some("global"),
            1,
            ManagerTreeKindV1::Project,
            Some(Uuid::new_v4()),
        ));
    }
    state.install(rows, &page(13, None));
    let lines = state.impact_lines(0);
    assert!(lines.iter().any(|line| line == "Affected seats (13):"));
    assert!(
        lines
            .iter()
            .any(|line| line == "  … 5 more affected seat(s) not listed"),
        "{lines:?}"
    );
}

// ---- action availability -------------------------------------------------

fn candidate(project: Option<Uuid>) -> Candidate {
    Candidate {
        id: Uuid::new_v4(),
        name: "next seat".into(),
        project_id: project,
        eligible: true,
    }
}

fn disabled(
    row: &ManagerTreeRowV1,
    cand: Option<&Candidate>,
    action: TreeAction,
) -> Option<String> {
    actions::availability(row, cand)
        .into_iter()
        .find(|entry| entry.action == action)
        .unwrap()
        .disabled
}

#[test]
fn availability_follows_each_rows_backing_rpc() {
    let project = Uuid::new_v4();
    let cand = candidate(Some(project));
    let global = row(
        "global",
        None,
        0,
        ManagerTreeKindV1::Global,
        Some(Uuid::new_v4()),
    );
    for action in &TreeAction::ALL[..4] {
        assert_eq!(disabled(&global, Some(&cand), *action), None, "{action:?}");
    }
    // #1237: only a portfolio row gains a manager above or moves.
    for action in [TreeAction::Above, TreeAction::MoveUnder] {
        assert!(
            disabled(&global, Some(&cand), action)
                .unwrap()
                .contains("only a portfolio node"),
            "{action:?}"
        );
    }
    let labels: Vec<String> = actions::availability(&global, Some(&cand))
        .into_iter()
        .map(|entry| entry.label)
        .collect();
    assert_eq!(
        labels,
        [
            "preview impact",
            "replace seat → next seat",
            "edit granted projects",
            "revoke global grant",
            "appoint a manager above",
            "move under"
        ]
    );

    let mut project_row = row(
        "project:p",
        Some("global"),
        1,
        ManagerTreeKindV1::Project,
        None,
    );
    project_row.project_id = Some(project);
    assert_eq!(
        disabled(&project_row, Some(&cand), TreeAction::Appoint),
        None
    );
    assert!(
        disabled(&project_row, Some(&cand), TreeAction::Revoke)
            .unwrap()
            .contains("no manager appointed")
    );
    let other = candidate(Some(Uuid::new_v4()));
    assert!(
        disabled(&project_row, Some(&other), TreeAction::Appoint)
            .unwrap()
            .contains("another project")
    );
    assert!(
        disabled(&project_row, None, TreeAction::Appoint)
            .unwrap()
            .contains("focus the session")
    );

    let mut area = row(
        "area:a",
        Some("project:p"),
        2,
        ManagerTreeKindV1::Area,
        Some(Uuid::new_v4()),
    );
    area.grant = Some(ManagerTreeGrantV1 {
        grant_version: 3,
        capabilities: vec![],
        max_active_sessions: 2,
        max_created_sessions: 4,
        max_created_containers: 0,
        max_direct_reports: 2,
        max_spend_usd: None,
        reserved: vec![],
    });
    area.load.direct_reports = Some(0);
    assert!(
        disabled(&area, Some(&cand), TreeAction::Appoint)
            .unwrap()
            .contains("no RPC moves an area seat")
    );
    assert_eq!(disabled(&area, Some(&cand), TreeAction::Edit), None);
    assert_eq!(disabled(&area, Some(&cand), TreeAction::Revoke), None);
    area.load.direct_reports = Some(2);
    assert!(
        disabled(&area, Some(&cand), TreeAction::Edit)
            .unwrap()
            .contains("2 direct report(s)")
    );

    let epic = row("epic:e", Some("area:a"), 3, ManagerTreeKindV1::Epic, None);
    assert_eq!(disabled(&epic, None, TreeAction::Preview), None);
    for action in [TreeAction::Appoint, TreeAction::Edit, TreeAction::Revoke] {
        assert!(disabled(&epic, None, action).is_some(), "{action:?}");
    }
}

#[tokio::test]
async fn a_disabled_action_reports_its_reason_and_opens_nothing() {
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    app.overlay = OverlayState::ManagerTree(Box::new(tree(Uuid::new_v4())));
    for _ in 0..3 {
        handle_key(&mut app, key(KeyCode::Char('j'))).await;
    }
    handle_key(&mut app, key(KeyCode::Char('x'))).await;
    let state = tree_state(&app);
    assert!(state.modal.is_none());
    let notice = state.notice.as_ref().unwrap();
    assert!(notice.text.contains("unavailable"), "{notice:?}");
}

#[tokio::test]
async fn preview_shows_impact_and_action_availability_without_rpc() {
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    app.overlay = OverlayState::ManagerTree(Box::new(tree(Uuid::new_v4())));
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    handle_key(&mut app, key(KeyCode::Char('p'))).await;
    let state = tree_state(&app);
    let Some(TreeModal::Preview { title, lines }) = &state.modal else {
        panic!("preview missing")
    };
    assert_eq!(title, "Impact of PROJECT project:a");
    let text = lines.join("\n");
    assert!(text.contains("2 descendant node(s) loaded"), "{text}");
    assert!(text.contains("  x  revoke supervision"), "{text}");
    handle_key(&mut app, key(KeyCode::Esc)).await;
    assert!(tree_state(&app).modal.is_none());
}

// ---- action outcomes through a scripted daemon -----------------------------

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

/// A one-connection fake daemon answering by method; every call is recorded.
async fn fake_daemon(
    mut handler: impl FnMut(&str, &Value) -> Result<Value, String> + Send + 'static,
) -> (App, Calls, tempfile::TempDir) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let directory = crate::test_support::short_socket_dir("rsi-mtree");
    let socket = directory.path().join("d.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let calls: Calls = Arc::default();
    let record = calls.clone();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let request: Value = serde_json::from_str(&line).unwrap();
            let method = request["method"].as_str().unwrap_or_default().to_string();
            let params = request["params"].clone();
            record
                .lock()
                .unwrap()
                .push((method.clone(), params.clone()));
            let response = match handler(&method, &params) {
                Ok(result) => json!({"jsonrpc": "2.0", "id": request["id"], "result": result}),
                Err(message) => json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "error": {"code": -32000, "message": message},
                }),
            };
            if write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    app.client = DaemonClient::new(socket);
    app.client.connect().await.unwrap();
    app.poll.connected = true;
    (app, calls, directory)
}

fn methods(calls: &Calls) -> Vec<String> {
    calls
        .lock()
        .unwrap()
        .iter()
        .map(|(method, _)| method.clone())
        .collect()
}

fn params_of(calls: &Calls, method: &str) -> Value {
    calls
        .lock()
        .unwrap()
        .iter()
        .find(|(name, _)| name == method)
        .map(|(_, params)| params.clone())
        .unwrap_or_else(|| panic!("{method} was not called"))
}

struct Ids {
    project: Uuid,
    root: Uuid,
    node: Uuid,
    seat: Uuid,
    node_seat: Uuid,
}

impl Ids {
    fn new() -> Self {
        Self {
            project: Uuid::new_v4(),
            root: Uuid::new_v4(),
            node: Uuid::new_v4(),
            seat: Uuid::new_v4(),
            node_seat: Uuid::new_v4(),
        }
    }

    fn rows(&self, grant_version: i64) -> Vec<ManagerTreeRowV1> {
        let mut project = row(
            &format!("project:{}", self.project),
            Some("global"),
            1,
            ManagerTreeKindV1::Project,
            Some(self.seat),
        );
        project.project_id = Some(self.project);
        project.node_id = Some(self.root);
        project.label = "rsi".into();
        let mut area = row(
            &format!("area:{}", self.node),
            Some(&format!("project:{}", self.project)),
            2,
            ManagerTreeKindV1::Area,
            Some(self.node_seat),
        );
        area.project_id = Some(self.project);
        area.node_id = Some(self.node);
        area.load.direct_reports = Some(0);
        area.grant = Some(ManagerTreeGrantV1 {
            grant_version,
            capabilities: vec![],
            max_active_sessions: 2,
            max_created_sessions: 4,
            max_created_containers: 0,
            max_direct_reports: 2,
            max_spend_usd: None,
            reserved: vec![],
        });
        let mut epic = row(
            "epic:e",
            Some(&format!("area:{}", self.node)),
            3,
            ManagerTreeKindV1::Epic,
            None,
        );
        epic.project_id = Some(self.project);
        vec![
            row(
                "global",
                None,
                0,
                ManagerTreeKindV1::Global,
                Some(Uuid::new_v4()),
            ),
            project,
            area,
            epic,
        ]
    }

    fn tree_page(&self, grant_version: i64) -> Value {
        serde_json::to_value(GetManagerTreeResultV1 {
            rows: self.rows(grant_version),
            next_after: None,
            total_rows: 4,
            complete: true,
            global_grant_version: Some(2),
        })
        .unwrap()
    }

    fn state(&self) -> ManagerTreeState {
        let mut state = ManagerTreeState::default();
        let mut result = page(4, None);
        result.global_grant_version = Some(2);
        state.install(self.rows(3), &result);
        state
    }

    fn node_view(&self, node_id: Uuid, grant_version: i64, reports: u16) -> ManagerNodeViewV1 {
        let mut policy = crate::overlay::global_manager_command::default_project_policy();
        let root = node_id == self.root;
        policy.max_active_sessions = if root { 8 } else { 2 };
        policy.max_created_sessions = if root { 16 } else { 4 };
        let grant = ManagerNodeGrantV1 {
            capabilities: policy.capabilities.clone(),
            allowed_launches: policy.allowed_launches.clone(),
            allowance: ManagerNodeAllowanceV1 {
                max_created_containers: policy.max_created_containers,
                max_created_sessions: policy.max_created_sessions,
                max_active_sessions: policy.max_active_sessions,
                max_build_slots: 0,
                max_disk_gib: 0,
                provider_limits: policy.provider_limits.clone(),
                max_spend_usd: policy.max_spend_usd,
            },
            max_direct_reports: if root { 5 } else { 2 },
        };
        ManagerNodeViewV1 {
            node_id,
            parent_node_id: (!root).then_some(self.root),
            seat_root_session_id: if root { self.seat } else { self.node_seat },
            project_id: self.project,
            selector: Some(ManagerNodeSelectorV1::Project),
            state: ManagerNodeStateV1::Active,
            grant_state: ManagerNodeGrantStateV1::Granted,
            grant: Some(grant),
            policy: Some(policy),
            grant_version,
            policy_version: if root { 4 } else { 1 },
            authority_epoch: if root { 9 } else { 7 },
            direct_reports: reports,
            updated_at: chrono::Utc::now(),
        }
    }
}

fn select(app: &mut App, key_name: &str) {
    let OverlayState::ManagerTree(state) = &mut app.overlay else {
        panic!("tree closed")
    };
    assert!(state.select_key(key_name), "{key_name} not visible");
}

#[tokio::test]
async fn revoke_area_node_confirms_impact_then_sends_the_reviewed_versions() {
    let ids = Ids::new();
    let (node_view, tree_page) = (
        serde_json::to_value(ids.node_view(ids.node, 3, 0)).unwrap(),
        ids.tree_page(4),
    );
    let revoked = serde_json::to_value(ids.node_view(ids.node, 4, 0)).unwrap();
    let (mut app, calls, _dir) = fake_daemon(move |method, _| match method {
        "GetManagerNode" => Ok(node_view.clone()),
        "RevokeManagerNode" => Ok(revoked.clone()),
        "GetManagerTree" => Ok(tree_page.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    app.overlay = OverlayState::ManagerTree(Box::new(ids.state()));
    select(&mut app, &format!("area:{}", ids.node));
    handle_key(&mut app, key(KeyCode::Char('x'))).await;
    let state = tree_state(&app);
    let Some(TreeModal::Confirm(pending)) = &state.modal else {
        panic!("confirm missing: {:?}", state.notice)
    };
    assert!(pending.destructive);
    assert_eq!(
        pending.rpc,
        "RevokeManagerNode (expects grant v3 · epoch 7)"
    );
    let text = pending.lines.join("\n");
    assert!(text.contains("1 descendant node(s) loaded"), "{text}");
    assert!(text.contains("Affected seats (1):"), "{text}");
    // Enter does not confirm a destructive action; nothing is sent yet.
    handle_key(&mut app, key(KeyCode::Enter)).await;
    assert!(matches!(
        tree_state(&app).modal,
        Some(TreeModal::Confirm(_))
    ));
    assert_eq!(methods(&calls), ["GetManagerNode"]);
    handle_key(&mut app, key(KeyCode::Char('y'))).await;
    assert_eq!(
        methods(&calls),
        ["GetManagerNode", "RevokeManagerNode", "GetManagerTree"]
    );
    let sent = params_of(&calls, "RevokeManagerNode");
    assert_eq!(sent["expected_grant_version"], 3);
    assert_eq!(sent["expected_authority_epoch"], 7);
    assert_eq!(sent["node_id"], ids.node.to_string());
    let state = tree_state(&app);
    assert!(state.modal.is_none());
    let notice = state.notice.as_ref().unwrap();
    assert_eq!(notice.tone, Tone::Success);
    assert!(
        notice.text.contains("revoked. Tree refreshed."),
        "{notice:?}"
    );
    // The refreshed snapshot is installed and the selection stays put.
    assert_eq!(
        state
            .selected_row()
            .unwrap()
            .grant
            .as_ref()
            .unwrap()
            .grant_version,
        4
    );
}

#[tokio::test]
async fn a_stale_node_version_refreshes_instead_of_confirming() {
    let ids = Ids::new();
    let (node_view, tree_page) = (
        serde_json::to_value(ids.node_view(ids.node, 5, 0)).unwrap(),
        ids.tree_page(5),
    );
    let (mut app, calls, _dir) = fake_daemon(move |method, _| match method {
        "GetManagerNode" => Ok(node_view.clone()),
        "GetManagerTree" => Ok(tree_page.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    app.overlay = OverlayState::ManagerTree(Box::new(ids.state()));
    select(&mut app, &format!("area:{}", ids.node));
    handle_key(&mut app, key(KeyCode::Char('x'))).await;
    assert_eq!(methods(&calls), ["GetManagerNode", "GetManagerTree"]);
    let state = tree_state(&app);
    assert!(state.modal.is_none());
    let notice = state.notice.as_ref().unwrap();
    assert_eq!(notice.tone, Tone::Error);
    assert!(notice.text.contains("stale"), "{notice:?}");
    assert_eq!(
        state
            .selected_row()
            .unwrap()
            .grant
            .as_ref()
            .unwrap()
            .grant_version,
        5
    );
}

#[tokio::test]
async fn a_refused_commit_refreshes_and_says_why() {
    let ids = Ids::new();
    let (node_view, tree_page) = (
        serde_json::to_value(ids.node_view(ids.node, 3, 0)).unwrap(),
        ids.tree_page(3),
    );
    let (mut app, calls, _dir) = fake_daemon(move |method, _| match method {
        "GetManagerNode" => Ok(node_view.clone()),
        "RevokeManagerNode" => Err("manager_node_stale_update".into()),
        "GetManagerTree" => Ok(tree_page.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    app.overlay = OverlayState::ManagerTree(Box::new(ids.state()));
    select(&mut app, &format!("area:{}", ids.node));
    handle_key(&mut app, key(KeyCode::Char('x'))).await;
    handle_key(&mut app, key(KeyCode::Char('y'))).await;
    assert_eq!(
        methods(&calls),
        ["GetManagerNode", "RevokeManagerNode", "GetManagerTree"]
    );
    let notice = tree_state(&app).notice.clone().unwrap();
    assert_eq!(notice.tone, Tone::Error);
    assert!(
        notice.text.contains("changed after the preview")
            && notice.text.contains("manager_node_stale_update"),
        "{notice:?}"
    );
}

#[tokio::test]
async fn cancelling_a_confirm_sends_nothing() {
    let ids = Ids::new();
    let node_view = serde_json::to_value(ids.node_view(ids.node, 3, 0)).unwrap();
    let (mut app, calls, _dir) = fake_daemon(move |method, _| match method {
        "GetManagerNode" => Ok(node_view.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    app.overlay = OverlayState::ManagerTree(Box::new(ids.state()));
    select(&mut app, &format!("area:{}", ids.node));
    handle_key(&mut app, key(KeyCode::Char('x'))).await;
    handle_key(&mut app, key(KeyCode::Esc)).await;
    assert_eq!(methods(&calls), ["GetManagerNode"]);
    let state = tree_state(&app);
    assert!(state.modal.is_none());
    assert!(
        state
            .notice
            .as_ref()
            .unwrap()
            .text
            .contains("nothing was sent")
    );
}

#[tokio::test]
async fn edit_area_allowance_mirrors_policy_and_sends_parent_versions() {
    let ids = Ids::new();
    let node = serde_json::to_value(ids.node_view(ids.node, 3, 0)).unwrap();
    let parent = serde_json::to_value(ids.node_view(ids.root, 6, 1)).unwrap();
    let saved = serde_json::to_value(ids.node_view(ids.node, 4, 0)).unwrap();
    let (node_id, tree_page) = (ids.node.to_string(), ids.tree_page(4));
    let (mut app, calls, _dir) = fake_daemon(move |method, params| match method {
        "GetManagerNode" if params["node_id"] == node_id.as_str() => Ok(node.clone()),
        "GetManagerNode" => Ok(parent.clone()),
        "ConfigureManagerNode" => Ok(saved.clone()),
        "GetManagerTree" => Ok(tree_page.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    app.overlay = OverlayState::ManagerTree(Box::new(ids.state()));
    select(&mut app, &format!("area:{}", ids.node));
    handle_key(&mut app, key(KeyCode::Char('e'))).await;
    assert!(matches!(
        tree_state(&app).modal,
        Some(TreeModal::EditNode(_))
    ));
    // No change: Enter explains instead of confirming.
    handle_key(&mut app, key(KeyCode::Enter)).await;
    let Some(TreeModal::EditNode(editor)) = &tree_state(&app).modal else {
        panic!("editor closed")
    };
    assert_eq!(editor.error.as_deref(), Some("No change to save."));
    handle_key(&mut app, key(KeyCode::Char('-'))).await; // active 2 -> 1
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    handle_key(&mut app, key(KeyCode::Char('-'))).await; // created 4 -> 3
    handle_key(&mut app, key(KeyCode::Enter)).await;
    let Some(TreeModal::Confirm(pending)) = &tree_state(&app).modal else {
        panic!("confirm missing")
    };
    assert!(pending.destructive, "narrowing an allowance is destructive");
    assert!(
        pending
            .lines
            .contains(&"max active sessions: 2 → 1".to_string())
    );
    assert!(
        pending
            .lines
            .contains(&"max created sessions: 4 → 3".to_string())
    );
    handle_key(&mut app, key(KeyCode::Char('y'))).await;
    let sent = params_of(&calls, "ConfigureManagerNode");
    assert_eq!(sent["node_id"], ids.node.to_string());
    assert_eq!(sent["parent_node_id"], ids.root.to_string());
    assert_eq!(sent["seat_root_session_id"], ids.node_seat.to_string());
    assert_eq!(sent["expected_node_grant_version"], 3);
    assert_eq!(sent["expected_parent_grant_version"], 6);
    assert_eq!(sent["expected_parent_policy_version"], 4);
    assert_eq!(sent["expected_parent_authority_epoch"], 9);
    assert_eq!(sent["grant"]["allowance"]["max_active_sessions"], 1);
    assert_eq!(sent["policy"]["max_active_sessions"], 1);
    assert_eq!(sent["grant"]["allowance"]["max_created_sessions"], 3);
    assert_eq!(sent["policy"]["max_created_sessions"], 3);
    let notice = tree_state(&app).notice.clone().unwrap();
    assert_eq!(notice.tone, Tone::Success, "{notice:?}");
    assert!(notice.text.contains("saved as grant v4"), "{notice:?}");
}

fn harness_config(ids: &Ids, row_version: i64, epics: Vec<Uuid>) -> HarnessManagerConfigV1 {
    HarnessManagerConfigV1 {
        project_id: ids.project,
        manager_session_id: ids.seat,
        current_session_id: Some(ids.seat),
        epic_ids: epics.clone(),
        // No explicit Epics means the whole project (an empty selection is revoked).
        scope_mode: if epics.is_empty() {
            HarnessManagerScopeModeV1::Project
        } else {
            HarnessManagerScopeModeV1::Selected
        },
        selected_epic_ids: (!epics.is_empty()).then_some(epics),
        group_ids: vec![],
        row_version,
        updated_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn replace_project_seat_with_the_focused_session_keeps_the_scope() {
    let ids = Ids::new();
    let epic = Uuid::new_v4();
    let config = serde_json::to_value(harness_config(&ids, 11, vec![epic])).unwrap();
    let saved = config.clone();
    let tree_page = ids.tree_page(3);
    let (mut app, calls, _dir) = fake_daemon(move |method, _| match method {
        "GetHarnessManager" => Ok(config.clone()),
        "ConfigureHarnessManager" => Ok(saved.clone()),
        "GetManagerTree" => Ok(tree_page.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    let focused = app.selected_session_id().unwrap();
    app.sessions.get_mut(&focused).unwrap().session.project_id = Some(ids.project);
    app.overlay = OverlayState::ManagerTree(Box::new(ids.state()));
    select(&mut app, &format!("project:{}", ids.project));
    handle_key(&mut app, key(KeyCode::Char('a'))).await;
    let Some(TreeModal::Confirm(pending)) = &tree_state(&app).modal else {
        panic!("confirm missing: {:?}", tree_state(&app).notice)
    };
    assert!(pending.destructive, "replacing a live seat is destructive");
    assert_eq!(pending.rpc, "ConfigureHarnessManager (expects row v11)");
    assert!(
        pending
            .lines
            .contains(&"Scope: kept: 0 group(s), 1 epic(s).".to_string()),
        "{:?}",
        pending.lines
    );
    handle_key(&mut app, key(KeyCode::Char('y'))).await;
    let sent = params_of(&calls, "ConfigureHarnessManager");
    assert_eq!(sent["session_id"], focused.to_string());
    assert_eq!(sent["expected_row_version"], 11);
    assert_eq!(sent["epic_ids"], json!([epic.to_string()]));
    let notice = tree_state(&app).notice.clone().unwrap();
    assert_eq!(notice.tone, Tone::Success, "{notice:?}");
    assert!(notice.text.contains("seat saved (row v11)"), "{notice:?}");
}

#[tokio::test]
async fn revoke_project_supervision_sends_an_empty_scope() {
    let ids = Ids::new();
    let config = serde_json::to_value(harness_config(&ids, 11, vec![])).unwrap();
    let saved = config.clone();
    let tree_page = ids.tree_page(3);
    let (mut app, calls, _dir) = fake_daemon(move |method, _| match method {
        "GetHarnessManager" => Ok(config.clone()),
        "ConfigureHarnessManager" => Ok(saved.clone()),
        "GetManagerTree" => Ok(tree_page.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    app.overlay = OverlayState::ManagerTree(Box::new(ids.state()));
    select(&mut app, &format!("project:{}", ids.project));
    handle_key(&mut app, key(KeyCode::Char('x'))).await;
    handle_key(&mut app, key(KeyCode::Char('y'))).await;
    let sent = params_of(&calls, "ConfigureHarnessManager");
    assert_eq!(sent["session_id"], ids.seat.to_string());
    assert_eq!(sent["epic_ids"], json!([]));
    assert_eq!(sent["expected_row_version"], 11);
    assert!(
        tree_state(&app)
            .notice
            .as_ref()
            .unwrap()
            .text
            .contains("supervision revoked")
    );
}

fn global_grant(seat: Uuid, version: i64, projects: Vec<Uuid>) -> GlobalManagerGrantV1 {
    let now = chrono::Utc::now();
    GlobalManagerGrantV1 {
        grant_id: Uuid::new_v4(),
        grant_version: version,
        seat_session_id: seat,
        state: "active".into(),
        project_ids: projects,
        allowed_launches: crate::overlay::global_manager_command::default_allowed_launches(),
        project_policy: crate::overlay::global_manager_command::default_project_policy(),
        operator_origin: "operator_rpc".into(),
        created_at: now,
        updated_at: now,
    }
}

fn project(name: &str) -> rsi_common::types::Project {
    let now = chrono::Utc::now();
    rsi_common::types::Project {
        id: Uuid::new_v4(),
        name: name.into(),
        path: None,
        description: None,
        color: rsi_common::types::Project::DEFAULT_COLOR.into(),
        context_files: None,
        created_at: now,
        updated_at: now,
    }
}

#[tokio::test]
async fn edit_global_projects_toggles_the_grant() {
    let ids = Ids::new();
    let (alpha, beta, gamma) = (project("alpha"), project("beta"), project("gamma"));
    let seat = Uuid::new_v4();
    let grant = serde_json::to_value(global_grant(seat, 2, vec![alpha.id, beta.id])).unwrap();
    let saved = serde_json::to_value(global_grant(seat, 3, vec![alpha.id, gamma.id])).unwrap();
    let tree_page = ids.tree_page(3);
    let (mut app, calls, _dir) = fake_daemon(move |method, _| match method {
        "GetGlobalManager" => Ok(grant.clone()),
        "ConfigureGlobalManager" => Ok(saved.clone()),
        "GetManagerTree" => Ok(tree_page.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    app.projects = vec![gamma.clone(), alpha.clone(), beta.clone()];
    app.overlay = OverlayState::ManagerTree(Box::new(ids.state()));
    handle_key(&mut app, key(KeyCode::Char('e'))).await;
    let Some(TreeModal::EditGlobal(editor)) = &tree_state(&app).modal else {
        panic!("editor missing: {:?}", tree_state(&app).notice)
    };
    let names: Vec<(&str, bool)> = editor
        .projects
        .iter()
        .map(|(_, name, granted)| (name.as_str(), *granted))
        .collect();
    assert_eq!(names, [("alpha", true), ("beta", true), ("gamma", false)]);
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    handle_key(&mut app, key(KeyCode::Char(' '))).await; // drop beta
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    handle_key(&mut app, key(KeyCode::Char(' '))).await; // add gamma
    handle_key(&mut app, key(KeyCode::Enter)).await;
    let Some(TreeModal::Confirm(pending)) = &tree_state(&app).modal else {
        panic!("confirm missing")
    };
    assert!(pending.destructive);
    assert_eq!(pending.lines[0], "+ grant: gamma");
    assert!(
        pending.lines[1].starts_with("- remove: beta"),
        "{:?}",
        pending.lines
    );
    handle_key(&mut app, key(KeyCode::Char('y'))).await;
    let sent = params_of(&calls, "ConfigureGlobalManager");
    assert_eq!(sent["session_id"], seat.to_string());
    assert_eq!(sent["expected_grant_version"], 2);
    assert_eq!(
        sent["project_ids"],
        json!([alpha.id.to_string(), gamma.id.to_string()])
    );
    assert!(
        tree_state(&app)
            .notice
            .as_ref()
            .unwrap()
            .text
            .contains("saved as v3")
    );
}

#[tokio::test]
async fn edit_global_caps_raises_the_per_project_active_cap() {
    let ids = Ids::new();
    let (alpha, beta) = (project("alpha"), project("beta"));
    let seat = Uuid::new_v4();
    let grant = serde_json::to_value(global_grant(seat, 2, vec![alpha.id, beta.id])).unwrap();
    let mut raised = global_grant(seat, 3, vec![alpha.id, beta.id]);
    raised.project_policy.max_active_sessions = 14;
    let saved = serde_json::to_value(raised).unwrap();
    let tree_page = ids.tree_page(3);
    let (mut app, calls, _dir) = fake_daemon(move |method, _| match method {
        "GetGlobalManager" => Ok(grant.clone()),
        "ConfigureGlobalManager" => Ok(saved.clone()),
        "GetManagerTree" => Ok(tree_page.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    app.projects = vec![alpha.clone(), beta.clone()];
    app.overlay = OverlayState::ManagerTree(Box::new(ids.state()));
    handle_key(&mut app, key(KeyCode::Char('e'))).await;
    // Past the two projects, the first cap row is the active cap.
    for _ in 0..2 {
        handle_key(&mut app, key(KeyCode::Char('j'))).await;
    }
    handle_key(&mut app, key(KeyCode::Char('L'))).await;
    let Some(TreeModal::EditGlobal(editor)) = &tree_state(&app).modal else {
        panic!("editor missing: {:?}", tree_state(&app).notice)
    };
    assert_eq!(editor.caps.max_active_sessions, 14);
    handle_key(&mut app, key(KeyCode::Enter)).await;
    let Some(TreeModal::Confirm(pending)) = &tree_state(&app).modal else {
        panic!("confirm missing")
    };
    assert!(!pending.destructive);
    assert_eq!(pending.lines[0], "max active sessions: 4 → 14");
    handle_key(&mut app, key(KeyCode::Char('y'))).await;
    let sent = params_of(&calls, "ConfigureGlobalManager");
    assert_eq!(sent["session_id"], seat.to_string());
    assert_eq!(sent["expected_grant_version"], 2);
    assert_eq!(sent["project_policy"]["max_active_sessions"], 14);
    assert_eq!(
        sent["project_ids"],
        json!([alpha.id.to_string(), beta.id.to_string()])
    );
    assert!(
        tree_state(&app)
            .notice
            .as_ref()
            .unwrap()
            .text
            .contains("saved as v3")
    );
}

#[tokio::test]
async fn edit_global_caps_lowering_a_cap_is_destructive_and_unchanged_caps_save_nothing() {
    let ids = Ids::new();
    let alpha = project("alpha");
    let seat = Uuid::new_v4();
    let grant = serde_json::to_value(global_grant(seat, 2, vec![alpha.id])).unwrap();
    let tree_page = ids.tree_page(3);
    let (mut app, _calls, _dir) = fake_daemon(move |method, _| match method {
        "GetGlobalManager" => Ok(grant.clone()),
        "GetManagerTree" => Ok(tree_page.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    app.projects = vec![alpha.clone()];
    app.overlay = OverlayState::ManagerTree(Box::new(ids.state()));
    handle_key(&mut app, key(KeyCode::Char('e'))).await;
    handle_key(&mut app, key(KeyCode::Enter)).await;
    let Some(TreeModal::EditGlobal(editor)) = &tree_state(&app).modal else {
        panic!("editor missing")
    };
    assert_eq!(editor.error.as_deref(), Some("No change to save."));
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    handle_key(&mut app, key(KeyCode::Char('h'))).await;
    handle_key(&mut app, key(KeyCode::Enter)).await;
    let Some(TreeModal::Confirm(pending)) = &tree_state(&app).modal else {
        panic!("confirm missing")
    };
    assert!(pending.destructive);
    assert_eq!(pending.lines[0], "max active sessions: 4 → 3");
}

#[tokio::test]
async fn global_actions_refresh_when_the_grant_version_moved() {
    let ids = Ids::new();
    let grant = serde_json::to_value(global_grant(Uuid::new_v4(), 9, vec![ids.project])).unwrap();
    let tree_page = ids.tree_page(3);
    let (mut app, calls, _dir) = fake_daemon(move |method, _| match method {
        "GetGlobalManager" => Ok(grant.clone()),
        "GetManagerTree" => Ok(tree_page.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    app.overlay = OverlayState::ManagerTree(Box::new(ids.state()));
    handle_key(&mut app, key(KeyCode::Char('x'))).await;
    assert_eq!(methods(&calls), ["GetGlobalManager", "GetManagerTree"]);
    let state = tree_state(&app);
    assert!(state.modal.is_none());
    assert!(state.notice.as_ref().unwrap().text.contains("stale"));
}

#[tokio::test]
async fn moving_past_the_last_loaded_row_fetches_the_next_page() {
    let ids = Ids::new();
    let mut extra = row("project:z", None, 0, ManagerTreeKindV1::Project, None);
    extra.label = "zeta".into();
    let next = serde_json::to_value(GetManagerTreeResultV1 {
        rows: vec![extra],
        next_after: None,
        total_rows: 5,
        complete: true,
        global_grant_version: Some(2),
    })
    .unwrap();
    let (mut app, calls, _dir) = fake_daemon(move |method, _| match method {
        "GetManagerTree" => Ok(next.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    let mut state = ids.state();
    state.next_after = Some("epic:e".into());
    state.total_rows = 5;
    app.overlay = OverlayState::ManagerTree(Box::new(state));
    handle_key(&mut app, key(KeyCode::Char('G'))).await;
    assert_eq!(params_of(&calls, "GetManagerTree")["after"], "epic:e");
    let state = tree_state(&app);
    assert_eq!(state.rows.len(), 5);
    assert!(state.next_after.is_none());
    assert_eq!(state.count_line(), "5 nodes · global grant v2");
    // Already at the end: n explains rather than refetching.
    handle_key(&mut app, key(KeyCode::Char('n'))).await;
    assert_eq!(methods(&calls).len(), 1);
    assert!(
        tree_state(&app)
            .notice
            .as_ref()
            .unwrap()
            .text
            .contains("Every node is loaded")
    );
}

#[tokio::test]
async fn refresh_reinstalls_the_snapshot_and_keeps_the_selection() {
    let ids = Ids::new();
    let tree_page = ids.tree_page(8);
    let (mut app, _calls, _dir) = fake_daemon(move |method, _| match method {
        "GetManagerTree" => Ok(tree_page.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    app.overlay = OverlayState::ManagerTree(Box::new(ids.state()));
    select(&mut app, &format!("area:{}", ids.node));
    handle_key(&mut app, key(KeyCode::Char('r'))).await;
    let state = tree_state(&app);
    let row = state.selected_row().unwrap();
    assert_eq!(row.key, format!("area:{}", ids.node));
    assert_eq!(row.grant.as_ref().unwrap().grant_version, 8);
    assert_eq!(state.notice.as_ref().unwrap().tone, Tone::Info);
}

// ---- portfolio rows (#1236) -------------------------------------------------

fn portfolio_view(
    label: &str,
    node_id: Uuid,
    seat: Uuid,
    version: i64,
    epoch: i64,
    projects: Vec<Uuid>,
) -> rsi_common::portfolio_nodes::PortfolioNodeV1 {
    let now = chrono::Utc::now();
    rsi_common::portfolio_nodes::PortfolioNodeV1 {
        node_id,
        tier_label: label.into(),
        state: "active".into(),
        authority_epoch: epoch,
        parent_node_id: None,
        grantor: "operator".into(),
        seat_root_session_id: seat,
        max_direct_reports: 5,
        child_policy: None,
        grant: global_grant(seat, version, projects),
        created_at: now,
        updated_at: now,
    }
}

fn portfolio_state(node_id: Uuid, seat: Uuid, project: Uuid, version: i64) -> ManagerTreeState {
    let key = format!("portfolio:{node_id}");
    let mut node = row(&key, None, 0, ManagerTreeKindV1::Portfolio, Some(seat));
    node.node_id = Some(node_id);
    node.label = "pinnacle manager".into();
    node.tier_label = Some("pinnacle".into());
    node.grant = Some(ManagerTreeGrantV1 {
        grant_version: version,
        capabilities: vec![],
        max_active_sessions: 4,
        max_created_sessions: 8,
        max_created_containers: 0,
        max_direct_reports: 5,
        max_spend_usd: None,
        reserved: vec![],
    });
    let mut child = row(
        &format!("project:{project}"),
        Some(&key),
        1,
        ManagerTreeKindV1::Project,
        None,
    );
    child.project_id = Some(project);
    let mut state = ManagerTreeState::default();
    let mut result = page(2, None);
    result.global_grant_version = None;
    state.install(vec![node, child], &result);
    state
}

#[test]
fn portfolio_rows_offer_seat_projects_and_revoke_actions() {
    let node_id = Uuid::new_v4();
    let state = portfolio_state(node_id, Uuid::new_v4(), Uuid::new_v4(), 6);
    let actions = actions::availability(&state.rows[0], None);
    let labels: Vec<&str> = actions.iter().map(|entry| entry.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "preview impact",
            "replace seat",
            "edit granted projects",
            "revoke pinnacle node",
            "appoint a swarm manager above",
            "move under a sibling (m here, then m on the new parent)"
        ]
    );
    assert!(
        disabled(&state.rows[0], None, TreeAction::Above)
            .unwrap()
            .contains("focus the session")
    );
    assert_eq!(disabled(&state.rows[0], None, TreeAction::MoveUnder), None);
    assert_eq!(actions::label_above("global"), "pinnacle");
    assert_eq!(actions::label_above("region"), "above region");
    assert_eq!(kind_tag(ManagerTreeKindV1::Portfolio), "PORTFOLIO");
    assert_eq!(state.impact(0).projects, 1);
}

#[tokio::test]
async fn revoke_portfolio_node_sends_the_reviewed_versions() {
    let (node_id, seat, project) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let current = portfolio_view("pinnacle", node_id, seat, 6, 5, vec![project]);
    let mut revoked = current.clone();
    revoked.state = "revoked".into();
    let (current, revoked) = (
        serde_json::to_value(current).unwrap(),
        serde_json::to_value(revoked).unwrap(),
    );
    let tree_page = serde_json::to_value(GetManagerTreeResultV1 {
        rows: portfolio_state(node_id, seat, project, 6).rows,
        next_after: None,
        total_rows: 2,
        complete: true,
        global_grant_version: None,
    })
    .unwrap();
    let (mut app, calls, _dir) = fake_daemon(move |method, _| match method {
        "GetPortfolioNode" => Ok(current.clone()),
        "RevokePortfolioNode" => Ok(revoked.clone()),
        "GetManagerTree" => Ok(tree_page.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    app.overlay = OverlayState::ManagerTree(Box::new(portfolio_state(node_id, seat, project, 6)));
    handle_key(&mut app, key(KeyCode::Char('x'))).await;
    let Some(TreeModal::Confirm(pending)) = &tree_state(&app).modal else {
        panic!("confirm missing: {:?}", tree_state(&app).notice)
    };
    assert!(pending.destructive);
    assert_eq!(
        pending.rpc,
        "RevokePortfolioNode (expects grant v6 · epoch 5)"
    );
    handle_key(&mut app, key(KeyCode::Char('y'))).await;
    let sent = params_of(&calls, "RevokePortfolioNode");
    assert_eq!(sent["node_id"], node_id.to_string());
    assert_eq!(sent["expected_grant_version"], 6);
    assert_eq!(sent["expected_authority_epoch"], 5);
    assert!(
        tree_state(&app)
            .notice
            .as_ref()
            .unwrap()
            .text
            .contains("pinnacle node")
    );
}

#[tokio::test]
async fn edit_portfolio_projects_saves_through_configure_portfolio_node() {
    let (node_id, seat) = (Uuid::new_v4(), Uuid::new_v4());
    let (alpha, beta) = (project("alpha"), project("beta"));
    let current = portfolio_view("global", node_id, seat, 3, 2, vec![alpha.id]);
    let saved = portfolio_view("global", node_id, seat, 7, 7, vec![alpha.id, beta.id]);
    let (current, saved) = (
        serde_json::to_value(current).unwrap(),
        serde_json::to_value(saved).unwrap(),
    );
    let tree_page = serde_json::to_value(GetManagerTreeResultV1 {
        rows: portfolio_state(node_id, seat, alpha.id, 3).rows,
        next_after: None,
        total_rows: 2,
        complete: true,
        global_grant_version: None,
    })
    .unwrap();
    let (mut app, calls, _dir) = fake_daemon(move |method, _| match method {
        "GetPortfolioNode" => Ok(current.clone()),
        "ConfigurePortfolioNode" => Ok(saved.clone()),
        "GetManagerTree" => Ok(tree_page.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    app.projects = vec![alpha.clone(), beta.clone()];
    app.overlay = OverlayState::ManagerTree(Box::new(portfolio_state(node_id, seat, alpha.id, 3)));
    handle_key(&mut app, key(KeyCode::Char('e'))).await;
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    handle_key(&mut app, key(KeyCode::Char(' '))).await; // add beta
    handle_key(&mut app, key(KeyCode::Enter)).await;
    handle_key(&mut app, key(KeyCode::Char('y'))).await;
    let sent = params_of(&calls, "ConfigurePortfolioNode");
    assert_eq!(sent["node_id"], node_id.to_string());
    assert_eq!(sent["tier_label"], "global");
    assert_eq!(sent["seat_session_id"], seat.to_string());
    assert_eq!(sent["expected_node_grant_version"], 3);
    assert_eq!(sent["expected_authority_epoch"], 2);
    assert_eq!(
        sent["project_ids"],
        json!([alpha.id.to_string(), beta.id.to_string()])
    );
}

// ---- levels above a node (#1237) --------------------------------------------

/// `A` on a portfolio row appoints the focused session as a manager above the
/// node: one `ConfigurePortfolioNode` adopting it, behind `y`, with the
/// node's subtree impact shown first.
#[tokio::test]
async fn appoint_above_adopts_the_node_after_a_confirm() {
    let (node_id, seat, project) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let current = portfolio_view("global", node_id, seat, 6, 5, vec![project]);
    let (current, saved) = (
        serde_json::to_value(&current).unwrap(),
        serde_json::to_value(portfolio_view(
            "pinnacle",
            Uuid::new_v4(),
            Uuid::new_v4(),
            9,
            9,
            vec![project],
        ))
        .unwrap(),
    );
    let tree_page = serde_json::to_value(GetManagerTreeResultV1 {
        rows: portfolio_state(node_id, seat, project, 6).rows,
        next_after: None,
        total_rows: 2,
        complete: true,
        global_grant_version: None,
    })
    .unwrap();
    let (mut app, calls, _dir) = fake_daemon(move |method, _| match method {
        "GetPortfolioNode" => Ok(current.clone()),
        "ConfigurePortfolioNode" => Ok(saved.clone()),
        "GetManagerTree" => Ok(tree_page.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    let focused = app.selected_session_id().expect("a focused session");
    app.overlay = OverlayState::ManagerTree(Box::new(portfolio_state(node_id, seat, project, 6)));
    handle_key(&mut app, key(KeyCode::Char('A'))).await;
    let Some(TreeModal::Confirm(pending)) = &tree_state(&app).modal else {
        panic!("confirm missing: {:?}", tree_state(&app).notice)
    };
    assert!(pending.destructive);
    assert!(pending.rpc.contains("adopts"), "{}", pending.rpc);
    assert!(
        pending
            .lines
            .iter()
            .any(|line| line.contains("move one level down")),
        "{:?}",
        pending.lines
    );
    assert!(
        methods(&calls)
            .iter()
            .all(|method| method != "ConfigurePortfolioNode")
    );
    handle_key(&mut app, key(KeyCode::Char('y'))).await;
    let sent: rsi_common::portfolio_nodes::ConfigurePortfolioNodeRequestV1 =
        serde_json::from_value(params_of(&calls, "ConfigurePortfolioNode")).unwrap();
    assert_eq!(sent.node_id, None);
    assert_eq!(sent.parent_node_id, None);
    assert_eq!(sent.adopt_node_ids, [node_id]);
    assert_eq!(sent.tier_label, "pinnacle");
    assert_eq!(sent.seat_session_id, focused);
    assert_eq!(sent.project_ids, [project]);
}

/// Two sibling roots X and Y, each over its own project.
fn sibling_state(x: (Uuid, Uuid, Uuid), y: (Uuid, Uuid, Uuid)) -> ManagerTreeState {
    let mut rows = Vec::new();
    for (node_id, seat, project) in [x, y] {
        let key = format!("portfolio:{node_id}");
        let mut node = row(&key, None, 0, ManagerTreeKindV1::Portfolio, Some(seat));
        node.node_id = Some(node_id);
        node.label = "global manager".into();
        node.tier_label = Some("global".into());
        node.grant = Some(ManagerTreeGrantV1 {
            grant_version: 3,
            capabilities: vec![],
            max_active_sessions: 4,
            max_created_sessions: 8,
            max_created_containers: 0,
            max_direct_reports: 5,
            max_spend_usd: None,
            reserved: vec![],
        });
        let mut child = row(
            &format!("project:{project}"),
            Some(&key),
            1,
            ManagerTreeKindV1::Project,
            None,
        );
        child.project_id = Some(project);
        rows.push(node);
        rows.push(child);
    }
    let mut state = ManagerTreeState::default();
    let mut result = page(4, None);
    result.global_grant_version = None;
    state.install(rows, &result);
    state
}

/// `m` marks a node and `m` on a sibling moves it under that sibling: the
/// sibling widens to cover it and adopts it, behind `y`. `Esc` cancels a
/// pending move without closing the tree.
#[tokio::test]
async fn move_under_marks_then_adopts_into_the_sibling() {
    use rsi_common::grant_narrowing::{grant_narrows, portfolio_bounds, portfolio_grant_bounds};
    let x = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let y = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let x_view = portfolio_view("global", x.0, x.1, 3, 3, vec![x.2]);
    let y_view = portfolio_view("global", y.0, y.1, 4, 4, vec![y.2]);
    let views = [
        (x.0.to_string(), serde_json::to_value(&x_view).unwrap()),
        (y.0.to_string(), serde_json::to_value(&y_view).unwrap()),
    ];
    let saved = serde_json::to_value(&y_view).unwrap();
    let tree_page = serde_json::to_value(GetManagerTreeResultV1 {
        rows: sibling_state(x, y).rows,
        next_after: None,
        total_rows: 4,
        complete: true,
        global_grant_version: None,
    })
    .unwrap();
    let (mut app, calls, _dir) = fake_daemon(move |method, params| match method {
        "GetPortfolioNode" => views
            .iter()
            .find(|(id, _)| params["node_id"] == id.as_str())
            .map(|(_, view)| view.clone())
            .ok_or_else(|| "unknown node".into()),
        "ConfigurePortfolioNode" => Ok(saved.clone()),
        "GetManagerTree" => Ok(tree_page.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    app.overlay = OverlayState::ManagerTree(Box::new(sibling_state(x, y)));
    // Mark X, then cancel with Esc: the tree stays open.
    handle_key(&mut app, key(KeyCode::Char('m'))).await;
    assert_eq!(
        tree_state(&app).move_source.as_ref().map(|s| s.0),
        Some(x.0)
    );
    handle_key(&mut app, key(KeyCode::Esc)).await;
    assert!(tree_state(&app).move_source.is_none());
    assert!(matches!(app.overlay, OverlayState::ManagerTree(_)));
    // Mark X again, select Y, press m: a confirm with X's impact.
    handle_key(&mut app, key(KeyCode::Char('m'))).await;
    assert!(
        tree_state(&app)
            .notice
            .as_ref()
            .unwrap()
            .text
            .starts_with("Moving global manager")
    );
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    handle_key(&mut app, key(KeyCode::Char('j'))).await;
    assert_eq!(tree_state(&app).selected_row().unwrap().node_id, Some(y.0));
    handle_key(&mut app, key(KeyCode::Char('m'))).await;
    let Some(TreeModal::Confirm(pending)) = &tree_state(&app).modal else {
        panic!("confirm missing: {:?}", tree_state(&app).notice)
    };
    assert!(pending.destructive);
    assert!(
        pending.title.starts_with("Move the global node"),
        "{}",
        pending.title
    );
    assert!(
        pending
            .lines
            .iter()
            .any(|line| line.contains("1 descendant node(s)")),
        "{:?}",
        pending.lines
    );
    handle_key(&mut app, key(KeyCode::Char('y'))).await;
    let sent: rsi_common::portfolio_nodes::ConfigurePortfolioNodeRequestV1 =
        serde_json::from_value(params_of(&calls, "ConfigurePortfolioNode")).unwrap();
    assert_eq!(sent.node_id, Some(y.0));
    assert_eq!(sent.adopt_node_ids, [x.0]);
    assert_eq!(sent.project_ids, [y.2, x.2]);
    assert_eq!(sent.expected_node_grant_version, 4);
    assert_eq!(sent.expected_authority_epoch, 4);
    assert_eq!(
        grant_narrows(
            &portfolio_grant_bounds(&x_view.grant, x_view.max_direct_reports),
            &portfolio_bounds(
                &sent.project_ids,
                &sent.allowed_launches,
                &sent.policy,
                sent.max_direct_reports
            )
        ),
        Ok(())
    );
}

#[tokio::test]
async fn portfolio_cap_preview_requires_y_before_retrying_with_confirmation() {
    use actions::{PendingAction, PreparedRequest};
    let (node_id, seat) = (Uuid::new_v4(), Uuid::new_v4());
    let alpha = project("Koplik");
    let node = portfolio_view("global", node_id, seat, 3, 2, vec![alpha.id]);
    let request = rsi_common::portfolio_nodes::ConfigurePortfolioNodeRequestV1 {
        node_id: Some(node_id),
        parent_node_id: None,
        adopt_node_ids: vec![],
        expected_parent_grant_version: None,
        tier_label: "global".into(),
        seat_session_id: seat,
        project_ids: vec![alpha.id],
        allowed_launches: node.grant.allowed_launches.clone(),
        policy: node.grant.project_policy.clone(),
        child_policy: None,
        max_direct_reports: node.max_direct_reports,
        expected_node_grant_version: 3,
        expected_authority_epoch: 2,
        idempotency_key: "lower-caps".into(),
    };
    let saved = serde_json::to_value(node).unwrap();
    let tree_page = serde_json::to_value(GetManagerTreeResultV1 {
        rows: portfolio_state(node_id, seat, alpha.id, 3).rows,
        next_after: None,
        total_rows: 2,
        complete: true,
        global_grant_version: None,
    })
    .unwrap();
    let (mut app, calls, _dir) = fake_daemon(move |method, params| match method {
        "ConfigurePortfolioNode" if params["confirm_cap_reductions"] == true => Ok(saved.clone()),
        "ConfigurePortfolioNode" => Err(format!(
            "{}: Koplik: active sessions: 20 → 4; Other: created sessions: 50 → 10",
            rsi_common::portfolio_nodes::PORTFOLIO_CAP_REDUCTION_CONFIRMATION_REQUIRED
        )),
        "GetManagerTree" => Ok(tree_page.clone()),
        other => Err(format!("unexpected {other}")),
    })
    .await;
    let mut state = portfolio_state(node_id, seat, alpha.id, 3);
    state.modal = Some(TreeModal::Confirm(Box::new(PendingAction {
        title: "Save grant".into(),
        destructive: false,
        lines: vec![],
        rpc: "ConfigurePortfolioNode".into(),
        request: PreparedRequest::ConfigurePortfolio(Box::new(request.clone())),
    })));
    app.overlay = OverlayState::ManagerTree(Box::new(state));
    handle_key(&mut app, key(KeyCode::Enter)).await;
    let Some(TreeModal::Confirm(preview)) = &tree_state(&app).modal else {
        panic!("confirmation missing")
    };
    assert!(preview.destructive);
    let text = preview.lines.join("\n");
    assert!(text.contains("Koplik: active sessions: 20 → 4"), "{text}");
    assert!(text.contains("Other: created sessions: 50 → 10"), "{text}");
    handle_key(&mut app, key(KeyCode::Enter)).await;
    assert_eq!(methods(&calls), ["ConfigurePortfolioNode"]);
    handle_key(&mut app, key(KeyCode::Char('y'))).await;
    let sent = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(method, _)| method == "ConfigurePortfolioNode")
        .map(|(_, params)| params.clone())
        .collect::<Vec<_>>();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["confirm_cap_reductions"], true);
    let confirmed: rsi_common::portfolio_nodes::PortfolioCapConfirmation<
        rsi_common::portfolio_nodes::ConfigurePortfolioNodeRequestV1,
    > = serde_json::from_value(sent[1].clone()).unwrap();
    assert_eq!(confirmed.request, request);
    assert!(tree_state(&app).modal.is_none());
}
