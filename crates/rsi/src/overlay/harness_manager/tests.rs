use super::*;
use crate::app::app_test_helpers::{baseline_session, with_focused_kind};
use crate::client::DaemonClient;
use crate::types::SessionState;
use ratatui::{Terminal, backend::TestBackend};
use rsi_common::types::SessionKind;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

fn fixture() -> (App, Session) {
    let mut app = with_focused_kind(SessionKind::Standard, None);
    let id = app.selected_session_id().unwrap();
    let session = &mut app.sessions.get_mut(&id).unwrap().session;
    session.project_id = Some(Uuid::new_v4());
    session.title = Some("Coordination desk".into());
    let session = session.clone();
    (app, session)
}

fn epic(app: &mut App, manager: &Session, id: u128, name: &str) -> Session {
    let mut session = baseline_session(Uuid::from_u128(id), SessionKind::Epic);
    session.project_id = manager.project_id;
    session.title = Some(name.into());
    app.sessions
        .insert(session.id, SessionState::new(session.clone()));
    session
}

fn config(manager: &Session, epic_ids: Vec<Uuid>, row_version: i64) -> HarnessManagerConfigV1 {
    HarnessManagerConfigV1 {
        scope_mode: rsi_common::harness_manager::HarnessManagerScopeModeV1::Selected,
        selected_epic_ids: None,
        group_ids: Vec::new(),
        project_id: manager.project_id.unwrap(),
        manager_session_id: manager.id,
        current_session_id: Some(manager.id),
        epic_ids,
        row_version,
        updated_at: chrono::Utc::now(),
    }
}

fn scope(app: &App) -> &HarnessManagerScopeState {
    match &app.overlay {
        OverlayState::HarnessManagerScope(state) => state,
        _ => panic!("manager scope should be open"),
    }
}

async fn key(app: &mut App, code: KeyCode) {
    assert!(crate::overlay::handle_overlay_key(app, KeyEvent::new(code, KeyModifiers::NONE)).await);
}

async fn command(app: &mut App, command: &str) {
    let crate::commands::CommandResult::LcAction(action) = crate::commands::parse_command(command)
    else {
        panic!("expected manager action");
    };
    crate::action_handler::dispatch_lc_action(app, action).await;
}

type RpcCase = (&'static str, Value, Value);

/// A private Unix socket proves the exact operator wire contract without
/// touching the running daemon or relying on an unimplemented backend.
async fn connect(
    app: &mut App,
    cases: Vec<RpcCase>,
) -> (tempfile::TempDir, tokio::task::JoinHandle<()>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("manager.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        for (method, params, mut response) in cases {
            let mut line = String::new();
            let size = reader.read_line(&mut line).await.unwrap();
            assert!(size > 0, "client closed before {method}");
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], method);
            assert_eq!(request["params"], params, "{method}");
            response["jsonrpc"] = json!("2.0");
            response["id"] = request["id"].clone();
            write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        }
    });
    app.client = DaemonClient::new(path);
    app.client.connect().await.unwrap();
    (dir, task)
}

fn candidates(app: &App, project: Uuid) -> Vec<HarnessManagerScopeCandidateV1> {
    app.sessions
        .values()
        .filter(|state| {
            state.session.project_id == Some(project)
                && state.session.session_kind == SessionKind::Epic
                && available(&state.session)
        })
        .map(|state| HarnessManagerScopeCandidateV1 {
            id: state.session.id,
            title: session_name(&state.session),
            kind: SessionKind::Epic,
            group_id: Some(Uuid::from_u128(9999)),
            group_title: Some("Feature group".into()),
        })
        .collect()
}

async fn connect_scope(
    app: &mut App,
    mut cases: Vec<RpcCase>,
) -> (tempfile::TempDir, tokio::task::JoinHandle<()>) {
    let project = Uuid::parse_str(cases[0].1["project_id"].as_str().unwrap()).unwrap();
    cases.insert(
        1,
        (
            "ListHarnessManagerScope",
            json!({"project_id":project,"after_id":null,"limit":64}),
            json!({"result":{"rows":candidates(app,project),"next_after_id":null}}),
        ),
    );
    connect(app, cases).await
}

async fn finished(task: tokio::task::JoinHandle<()>) {
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn appoint_filters_project_epics_space_toggles_and_enter_saves_version_zero() {
    let (mut app, manager) = fixture();
    let a = epic(&mut app, &manager, 1, "Alpha feature");
    let b = epic(&mut app, &manager, 2, "Beta feature");
    let foreign = epic(&mut app, &manager, 3, "Other project's Epic");
    app.sessions
        .get_mut(&foreign.id)
        .unwrap()
        .session
        .project_id = Some(Uuid::new_v4());
    let mut child = baseline_session(Uuid::new_v4(), SessionKind::Task);
    child.title = Some("Alpha implementation task".into());
    child.project_id = manager.project_id;
    child.parent_id = Some(a.id);
    app.sessions.insert(child.id, SessionState::new(child));
    let saved = config(&manager, vec![a.id, b.id], 1);
    let (_dir, task) = connect_scope(&mut app, vec![
        ("GetHarnessManager", json!({"project_id": manager.project_id}), json!({"result": null})),
        ("ConfigureHarnessManager", json!({"project_id": manager.project_id, "session_id": manager.id, "epic_ids": [a.id, b.id], "expected_row_version": 0}), json!({"result": saved})),
    ]).await;
    command(&mut app, "manager appoint").await;
    assert_eq!(
        scope(&app)
            .rows
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![a.id, b.id]
    );
    assert_eq!(scope(&app).rows[0].name, "Alpha feature");
    assert_eq!(scope(&app).rows[1].name, "Beta feature");
    key(&mut app, KeyCode::Char(' ')).await;
    assert!(scope(&app).selected_epics.contains(&a.id));
    assert!(!app.overlay_leader_pending);
    key(&mut app, KeyCode::Down).await;
    key(&mut app, KeyCode::Char(' ')).await;
    key(&mut app, KeyCode::Enter).await;
    assert!(matches!(app.overlay, OverlayState::None));
    assert!(
        app.notifications
            .back()
            .unwrap()
            .message
            .contains("2 Epics")
    );
    finished(task).await;
}

#[tokio::test]
async fn scope_preserves_filtered_selections_and_cancel_does_not_save() {
    let (mut app, manager) = fixture();
    let a = epic(&mut app, &manager, 1, "Alpha project identity");
    let b = epic(&mut app, &manager, 2, "Beta project identity");
    let current = config(&manager, vec![a.id], 7);
    let (_dir, task) = connect_scope(
        &mut app,
        vec![(
            "GetHarnessManager",
            json!({"project_id": manager.project_id}),
            json!({"result": current}),
        )],
    )
    .await;
    command(&mut app, "manager scope").await;
    key(&mut app, KeyCode::Char('/')).await;
    for c in "Beta".chars() {
        key(&mut app, KeyCode::Char(c)).await;
    }
    key(&mut app, KeyCode::Enter).await;
    assert_eq!(scope(&app).visible_indices(), vec![1]);
    key(&mut app, KeyCode::Char(' ')).await;
    assert_eq!(scope(&app).selected_epics, HashSet::from([a.id, b.id]));
    assert_eq!(scope(&app).expected_row_version, 7);
    key(&mut app, KeyCode::Esc).await;
    assert!(matches!(app.overlay, OverlayState::None));
    finished(task).await;
}

#[tokio::test]
async fn scope_enforces_limit_and_keeps_last_selected_epic_visible_when_scrolling() {
    let (mut app, manager) = fixture();
    for n in 1..=33 {
        epic(&mut app, &manager, n, &format!("Feature {n:02}"));
    }
    let discovered = candidates(&app, manager.project_id.unwrap());
    open_scope(
        &mut app,
        manager.project_id.unwrap(),
        manager.id,
        None,
        true,
        discovered,
    )
    .await;
    for _ in 0..33 {
        key(&mut app, KeyCode::Char(' ')).await;
        key(&mut app, KeyCode::Char('j')).await;
    }
    assert_eq!(scope(&app).selected_epics.len(), HARNESS_MANAGER_MAX_EPICS);
    assert!(
        scope(&app)
            .error
            .as_ref()
            .unwrap()
            .contains("Deselect one first")
    );
    let mut terminal = Terminal::new(TestBackend::new(96, 30)).unwrap();
    terminal
        .draw(|frame| crate::ui::overlay::render_overlay(frame, frame.area(), &mut app))
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(text.contains("Feature 33"));
    key(&mut app, KeyCode::Char('g')).await;
    key(&mut app, KeyCode::Char(' ')).await;
    key(&mut app, KeyCode::Char('G')).await;
    key(&mut app, KeyCode::Char(' ')).await;
    assert_eq!(scope(&app).selected_epics.len(), 32);
    assert!(scope(&app).selected_epics.contains(&Uuid::from_u128(33)));
}

#[tokio::test]
async fn scope_render_preserves_epic_and_manager_names_with_selection_and_revision() {
    let (mut app, manager) = fixture();
    let a = epic(&mut app, &manager, 1, "Orchestration Agent Process Fix");
    let b = epic(&mut app, &manager, 2, "Provider Integration");
    let discovered = candidates(&app, manager.project_id.unwrap());
    open_scope(
        &mut app,
        manager.project_id.unwrap(),
        manager.id,
        Some(config(&manager, vec![a.id], 9)),
        false,
        discovered,
    )
    .await;
    let mut terminal = Terminal::new(TestBackend::new(96, 30)).unwrap();
    terminal
        .draw(|frame| crate::ui::overlay::render_overlay(frame, frame.area(), &mut app))
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    for expected in [
        "Coordination desk",
        "[x]   Epic Orchestration Agent Process Fix",
        "[ ]   Epic Provider Integration",
        "0 Groups + 1 individual Epics",
        "revision 9",
        "A changed scope revokes any saved V2 policy; re-save policy after.",
        "Space toggle",
    ] {
        assert!(text.contains(expected), "missing {expected}");
    }
    assert_eq!(scope(&app).rows[1].id, b.id);
    for (width, height) in [(20, 8), (1, 1)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| crate::ui::overlay::render_overlay(frame, frame.area(), &mut app))
            .unwrap();
    }
}

#[tokio::test]
async fn stale_scope_keeps_draft_and_observed_version_without_retry() {
    let (mut app, manager) = fixture();
    let a = epic(&mut app, &manager, 1, "Alpha feature");
    let current = config(&manager, vec![a.id], 4);
    let (_dir, task) = connect_scope(
        &mut app,
        vec![
            (
                "GetHarnessManager",
                json!({"project_id": manager.project_id}),
                json!({"result": current}),
            ),
            (
                "ConfigureHarnessManager",
                json!({"project_id": manager.project_id, "session_id": manager.id, "epic_ids": [], "expected_row_version": 4}),
                json!({"error": {"code": -32000, "message": "manager_stale_version"}}),
            ),
        ],
    )
    .await;
    command(&mut app, "manager scope").await;
    key(&mut app, KeyCode::Char(' ')).await;
    key(&mut app, KeyCode::Enter).await;
    assert_eq!(scope(&app).selected_epics.len(), 0);
    assert_eq!(scope(&app).expected_row_version, 4);
    assert!(
        scope(&app)
            .error
            .as_ref()
            .unwrap()
            .contains("reopen :manager scope")
    );
    finished(task).await;
}

#[tokio::test]
async fn scope_discovers_uncached_epics_across_pages_and_preserves_group_identity() {
    let (mut app, manager) = fixture();
    let a = HarnessManagerScopeCandidateV1 {
        id: Uuid::from_u128(1),
        title: "0".into(),
        kind: SessionKind::Epic,
        group_id: Some(Uuid::from_u128(9998)),
        group_title: Some("Provider Updates".into()),
    };
    let b = HarnessManagerScopeCandidateV1 {
        id: Uuid::from_u128(2),
        title: "0".into(),
        kind: SessionKind::Epic,
        group_id: Some(Uuid::from_u128(9999)),
        group_title: Some("God Agent".into()),
    };
    let (_dir, task) = connect(
        &mut app,
        vec![
            (
                "GetHarnessManager",
                json!({"project_id":manager.project_id}),
                json!({"result":null}),
            ),
            (
                "ListHarnessManagerScope",
                json!({"project_id":manager.project_id,"after_id":null,"limit":64}),
                json!({"result":{"rows":[a],"next_after_id":a.id}}),
            ),
            (
                "ListHarnessManagerScope",
                json!({"project_id":manager.project_id,"after_id":a.id,"limit":64}),
                json!({"result":{"rows":[b],"next_after_id":null}}),
            ),
        ],
    )
    .await;
    command(&mut app, "manager appoint").await;
    assert_eq!(
        scope(&app)
            .rows
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        vec![b.id, a.id]
    );
    let mut terminal = Terminal::new(TestBackend::new(100, 25)).unwrap();
    terminal
        .draw(|frame| crate::ui::overlay::render_overlay(frame, frame.area(), &mut app))
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    for name in ["Coordination desk", "Provider Updates", "God Agent"] {
        assert!(text.contains(name), "missing {name}");
    }
    finished(task).await;
}

#[tokio::test]
async fn scope_discovery_failure_reports_error_before_opening_partial_picker() {
    let (mut app, manager) = fixture();
    epic(&mut app, &manager, 1, "Cached feature");
    let (_dir, task) = connect(
        &mut app,
        vec![
            (
                "GetHarnessManager",
                json!({"project_id":manager.project_id}),
                json!({"result":null}),
            ),
            (
                "ListHarnessManagerScope",
                json!({"project_id":manager.project_id,"after_id":null,"limit":64}),
                json!({"error":{"code":-32601,"message":"Method not found"}}),
            ),
        ],
    )
    .await;
    command(&mut app, "manager appoint").await;
    assert!(
        app.notifications
            .back()
            .unwrap()
            .message
            .contains("Update/restart rsid")
    );
    assert!(matches!(app.overlay, OverlayState::None));
    finished(task).await;
}

#[tokio::test]
async fn clear_revokes_scope_using_persisted_anchor_and_current_version() {
    let (mut app, manager) = fixture();
    let a = epic(&mut app, &manager, 1, "Alpha feature");
    let mut current = config(&manager, vec![a.id], 11);
    // Commands can be invoked while an unrelated leaf in this project is focused.
    let anchor = Uuid::new_v4();
    current.manager_session_id = anchor;
    let mut cleared = config(&manager, vec![], 12);
    cleared.manager_session_id = anchor;
    let (_dir, task) = connect(
        &mut app,
        vec![
            (
                "GetHarnessManager",
                json!({"project_id": manager.project_id}),
                json!({"result": current}),
            ),
            (
                "ConfigureHarnessManager",
                json!({"project_id": manager.project_id, "session_id": anchor, "epic_ids": [], "expected_row_version": 11}),
                json!({"result": cleared}),
            ),
        ],
    )
    .await;
    command(&mut app, "manager clear").await;
    assert!(
        app.notifications
            .back()
            .unwrap()
            .message
            .contains("supervision revoked")
    );
    finished(task).await;
}

#[tokio::test]
async fn reappointment_preserves_project_scope_with_the_focused_leaf_and_current_version() {
    let (mut app, focused) = fixture();
    let a = epic(&mut app, &focused, 1, "Feature identity");
    let mut current = config(&focused, vec![a.id], 8);
    current.manager_session_id = Uuid::new_v4();
    let (_dir, task) = connect_scope(
        &mut app,
        vec![
            (
                "GetHarnessManager",
                json!({"project_id": focused.project_id}),
                json!({"result": current}),
            ),
            (
                "ConfigureHarnessManager",
                json!({"project_id": focused.project_id, "session_id": focused.id, "epic_ids": [a.id], "expected_row_version": 8}),
                json!({"result": config(&focused, vec![a.id], 9)}),
            ),
        ],
    )
    .await;
    command(&mut app, "manager appoint").await;
    assert_eq!(scope(&app).manager_name, "Coordination desk");
    assert_eq!(scope(&app).manager_session_id, focused.id);
    assert_eq!(scope(&app).selected_epics, HashSet::from([a.id]));
    key(&mut app, KeyCode::Enter).await;
    assert!(matches!(app.overlay, OverlayState::None));
    finished(task).await;
}

#[tokio::test]
async fn manager_commands_explain_missing_project_manager_and_old_daemon() {
    let (mut app, manager) = fixture();
    app.sessions
        .get_mut(&manager.id)
        .unwrap()
        .session
        .project_id = None;
    command(&mut app, "manager").await;
    assert!(
        app.notifications
            .back()
            .unwrap()
            .message
            .contains(NO_PROJECT)
    );
    app.current_project_id = manager.project_id;
    command(&mut app, "manager appoint").await;
    assert!(
        app.notifications
            .back()
            .unwrap()
            .message
            .contains(NO_PROJECT)
    );
    app.sessions
        .get_mut(&manager.id)
        .unwrap()
        .session
        .project_id = manager.project_id;
    let (_dir, task) = connect(
        &mut app,
        vec![
            (
                "GetHarnessManager",
                json!({"project_id": manager.project_id}),
                json!({"result": null}),
            ),
            (
                "GetHarnessManager",
                json!({"project_id": manager.project_id}),
                json!({"result": null}),
            ),
            (
                "GetHarnessManager",
                json!({"project_id": manager.project_id}),
                json!({"result": null}),
            ),
            (
                "GetHarnessManager",
                json!({"project_id": manager.project_id}),
                json!({"error": {"code": -32601, "message": "Method not found"}}),
            ),
        ],
    )
    .await;
    for name in ["manager", "manager scope", "manager clear"] {
        command(&mut app, name).await;
        assert!(
            app.notifications
                .back()
                .unwrap()
                .message
                .contains(NO_MANAGER)
        );
    }
    command(&mut app, "manager appoint").await;
    assert!(
        app.notifications
            .back()
            .unwrap()
            .message
            .contains("Update/restart rsid")
    );
    finished(task).await;
}

#[tokio::test]
async fn appointment_rejects_containers_before_rpc() {
    for kind in [SessionKind::Epic, SessionKind::Group] {
        let mut app = with_focused_kind(kind, None);
        command(&mut app, "manager appoint").await;
        assert!(
            app.notifications
                .back()
                .unwrap()
                .message
                .contains("ordinary, unarchived leaf")
        );
    }
}

#[tokio::test]
async fn scope_resolves_missing_selected_epic_identity_and_allows_removal() {
    let (mut app, manager) = fixture();
    let mut archived = baseline_session(Uuid::new_v4(), SessionKind::Epic);
    archived.title = Some("Archived feature identity".into());
    archived.project_id = manager.project_id;
    archived.status = SessionStatus::Archived;
    let current = config(&manager, vec![archived.id], 3);
    let (_dir, task) = connect_scope(
        &mut app,
        vec![
            (
                "GetHarnessManager",
                json!({"project_id": manager.project_id}),
                json!({"result": current}),
            ),
            (
                "GetSession",
                json!({"session_id": archived.id}),
                json!({"result": archived}),
            ),
        ],
    )
    .await;
    command(&mut app, "manager scope").await;
    assert_eq!(scope(&app).rows[0].name, "Archived feature identity");
    key(&mut app, KeyCode::Enter).await;
    assert_eq!(
        scope(&app).error.as_deref(),
        Some("Deselect unavailable Groups/Epics before saving.")
    );
    key(&mut app, KeyCode::Char(' ')).await;
    assert_eq!(scope(&app).selected_epics.len(), 0);
    finished(task).await;
}

#[tokio::test]
async fn manager_navigation_follows_archived_intermediates_to_current_conversation() {
    let (mut app, mut manager) = fixture();
    manager.status = SessionStatus::Archived;
    let mut intermediate = manager.clone();
    intermediate.id = Uuid::new_v4();
    intermediate.continued_from = Some(manager.id);
    let mut current = manager.clone();
    current.id = Uuid::new_v4();
    current.continued_from = Some(intermediate.id);
    current.status = SessionStatus::Completed;
    current.title = Some("Current coordination desk".into());
    app.current_project_id = manager.project_id;
    app.detail_list_focused = true;
    let mut config = config(&manager, vec![], 3);
    config.current_session_id = Some(current.id);
    let (_dir, task) = connect(
        &mut app,
        vec![
            (
                "GetHarnessManager",
                json!({"project_id": manager.project_id}),
                json!({"result": config}),
            ),
            (
                "GetSession",
                json!({"session_id": current.id}),
                json!({"result": current}),
            ),
        ],
    )
    .await;
    command(&mut app, "manager").await;
    assert_eq!(app.selected_session_id(), Some(current.id));
    assert!(!app.detail_list_focused);
    assert_eq!(
        app.sessions[&current.id].session.title.as_deref(),
        Some("Current coordination desk")
    );
    assert!(
        app.notifications
            .back()
            .unwrap()
            .message
            .contains("scope is empty")
    );
    finished(task).await;
}

#[test]
fn manager_navigation_uses_daemon_authority_and_refuses_unavailable_or_cross_project_targets() {
    let (_, manager) = fixture();
    let mut current_config = config(&manager, vec![], 1);
    let mut successor = manager.clone();
    successor.id = Uuid::new_v4();
    successor.continued_from = Some(manager.id);
    successor.project_id = Some(Uuid::new_v4());
    let mut sessions = HashMap::from([
        (manager.id, manager.clone()),
        (successor.id, successor.clone()),
    ]);
    // A Fresh candidate does not displace the daemon's appointed principal.
    assert_eq!(manager_tip(&sessions, &current_config).unwrap(), manager.id);
    current_config.current_session_id = Some(successor.id);
    assert!(
        manager_tip(&sessions, &current_config)
            .unwrap_err()
            .contains(":manager appoint")
    );
    sessions.get_mut(&successor.id).unwrap().project_id = manager.project_id;
    assert_eq!(
        manager_tip(&sessions, &current_config).unwrap(),
        successor.id
    );
    // Ambiguous or invalid lineage is an explicit daemon refusal.
    current_config.current_session_id = None;
    assert!(manager_tip(&sessions, &current_config).is_err());
}

#[tokio::test]
async fn appointment_defaults_to_project_and_saves_without_expanding_epics() {
    let (mut app, manager) = fixture();
    let a = epic(&mut app, &manager, 1, "Existing project feature");
    let mut saved = config(&manager, vec![a.id], 1);
    saved.scope_mode = HarnessManagerScopeModeV1::Project;
    saved.selected_epic_ids = Some(Vec::new());
    let (_dir, task) = connect_scope(&mut app, vec![
        ("GetHarnessManager", json!({"project_id":manager.project_id}), json!({"result":null})),
        ("ConfigureHarnessManager", json!({"project_id":manager.project_id,"session_id":manager.id,"expected_row_version":0}), json!({"result":saved})),
    ]).await;
    command(&mut app, "manager appoint").await;
    assert!(scope(&app).all_project);
    assert!(scope(&app).inherited(&scope(&app).rows[0]));
    key(&mut app, KeyCode::Enter).await;
    assert!(matches!(app.overlay, OverlayState::None));
    assert!(
        app.notifications
            .back()
            .unwrap()
            .message
            .contains("whole project, including future Epics")
    );
    finished(task).await;
}

#[tokio::test]
async fn scope_selects_groups_with_inherited_epics_and_preserves_empty_groups() {
    let (mut app, manager) = fixture();
    let group = Uuid::from_u128(1);
    let child = Uuid::from_u128(2);
    let empty = Uuid::from_u128(3);
    let candidates = vec![
        HarnessManagerScopeCandidateV1 {
            id: group,
            title: "Provider Updates".into(),
            kind: SessionKind::Group,
            group_id: None,
            group_title: None,
        },
        HarnessManagerScopeCandidateV1 {
            id: child,
            title: "Pioneer integration".into(),
            kind: SessionKind::Epic,
            group_id: Some(group),
            group_title: Some("Provider Updates".into()),
        },
        HarnessManagerScopeCandidateV1 {
            id: empty,
            title: "Upcoming features".into(),
            kind: SessionKind::Group,
            group_id: None,
            group_title: None,
        },
    ];
    open_scope(
        &mut app,
        manager.project_id.unwrap(),
        manager.id,
        None,
        true,
        candidates,
    )
    .await;
    key(&mut app, KeyCode::Char(' ')).await;
    assert_eq!(scope(&app).selected_groups, HashSet::from([group]));
    assert!(!scope(&app).all_project);
    key(&mut app, KeyCode::Down).await;
    key(&mut app, KeyCode::Char(' ')).await;
    assert_eq!(
        scope(&app).error.as_deref(),
        Some("Covered by selected Group. Deselect Group to choose individual Epics.")
    );
    key(&mut app, KeyCode::Down).await;
    key(&mut app, KeyCode::Char(' ')).await;
    assert_eq!(scope(&app).request().group_ids, vec![group, empty]);
    assert_eq!(scope(&app).request().epic_ids, Some(Vec::new()));
    let mut terminal = Terminal::new(TestBackend::new(100, 25)).unwrap();
    terminal
        .draw(|frame| crate::ui::overlay::render_overlay(frame, frame.area(), &mut app))
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    for identity in [
        "Provider Updates",
        "Pioneer integration",
        "Upcoming features",
    ] {
        assert!(text.contains(identity), "missing {identity}");
    }
    assert!(text.contains("[+]   Epic"));
    key(&mut app, KeyCode::Char('a')).await;
    assert_eq!(
        scope(&app).request().scope_mode(),
        HarnessManagerScopeModeV1::Project
    );
    assert_eq!(scope(&app).request().group_ids, Vec::<Uuid>::new());
}
