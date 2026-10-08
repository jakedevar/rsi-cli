//! Standard (non-modal) editing in the inputs slice 3 left append-only
//! (#1628 slice 3b): Issues editor, satellite registry form, settlement
//! authorization, create-entity topology filter and the model-dropdown
//! filter. Each test drives the production key path with the operator's
//! `editing_mode` set and asserts the positive end state.

use crate::app::App;
use crate::event::step_once;
use crate::types::{OverlayState, Pane, SatelliteRegistryForm};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn app_with(mode: &str) -> App {
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    crate::settings::DaemonFeatureEntry::update_from_json(
        &mut app.daemon_features,
        &serde_json::json!({ "editing_mode": mode }),
    );
    app
}

async fn press(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    step_once(app, KeyEvent::new(code, mods)).await;
}

async fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c), KeyModifiers::NONE).await;
    }
}

// ---- operator source-worktree settlement authorization ----

fn settlement_app(mode: &str) -> App {
    let mut app = app_with(mode);
    app.overlay = OverlayState::SourceWorktreeSettlement(
        crate::types::SourceWorktreeSettlementOverlayState {
            cohorts: Vec::new(),
            selected_index: 0,
            scroll_offset: 0,
            audit: None,
            receipt: None,
            authorization_input: String::new(),
            authorization_active: true,
            idempotency_key: None,
            last_error: None,
        },
    );
    app
}

fn authorization(app: &App) -> String {
    match &app.overlay {
        OverlayState::SourceWorktreeSettlement(state) => state.authorization_input.clone(),
        _ => panic!("settlement overlay should be open"),
    }
}

#[tokio::test]
async fn settlement_authorization_is_edited_in_place_in_standard_mode() {
    let mut app = settlement_app("standard");
    type_text(&mut app, "APPLY thing").await;
    press(&mut app, KeyCode::Left, KeyModifiers::ALT).await;
    type_text(&mut app, "the ").await;
    assert_eq!(authorization(&app), "APPLY the thing");
    press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL).await;
    type_text(&mut app, "x").await;
    assert_eq!(authorization(&app), "x");
}

#[tokio::test]
async fn settlement_authorization_stays_append_only_in_vim_mode() {
    let mut app = settlement_app("vim");
    type_text(&mut app, "ab").await;
    press(&mut app, KeyCode::Left, KeyModifiers::NONE).await;
    type_text(&mut app, "c").await;
    assert_eq!(authorization(&app), "abc");
}

// ---- satellite registry form ----

fn satellite_app(mode: &str, form: SatelliteRegistryForm) -> App {
    let mut app = app_with(mode);
    app.overlay =
        OverlayState::SatelliteRegistry(Box::new(crate::types::SatelliteRegistryOverlayState {
            registry: rsi_common::satellite::SatelliteRegistryV1 {
                revision: 0,
                peers: Vec::new(),
            },
            selected_peer: 0,
            selected_link: 0,
            selected_session: 0,
            tab: crate::types::SatelliteRegistryTab::Peers,
            sessions: None,
            form: Some(form),
            last_probe: None,
            last_error: None,
        }));
    app
}

fn peer_form(field: usize) -> SatelliteRegistryForm {
    SatelliteRegistryForm::Peer {
        peer_id: rsi_common::satellite::SatelliteUuidV1(uuid::Uuid::new_v4()),
        label: String::new(),
        expected_installation_id: String::new(),
        enabled: false,
        read_enabled: false,
        dispatch_enabled: false,
        dispatch_scope: String::new(),
        repair_quarantine: false,
        field,
    }
}

fn satellite_form(app: &App) -> &SatelliteRegistryForm {
    match &app.overlay {
        OverlayState::SatelliteRegistry(state) => state.form.as_ref().expect("form open"),
        _ => panic!("satellite registry should be open"),
    }
}

#[tokio::test]
async fn satellite_form_text_fields_get_a_cursor_and_keep_their_filters() {
    let mut app = satellite_app("standard", peer_form(0));
    type_text(&mut app, "hub-b").await;
    press(&mut app, KeyCode::Home, KeyModifiers::NONE).await;
    type_text(&mut app, "my ").await;
    match satellite_form(&app) {
        SatelliteRegistryForm::Peer { label, .. } => assert_eq!(label, "my hub-b"),
        _ => unreachable!(),
    }
    // The dispatch scope takes only the id alphabet; a letter outside it is
    // swallowed and ids edit in place.
    let mut app = satellite_app("standard", peer_form(5));
    type_text(&mut app, "ab,zcd").await;
    press(&mut app, KeyCode::Left, KeyModifiers::NONE).await;
    press(&mut app, KeyCode::Left, KeyModifiers::NONE).await;
    type_text(&mut app, "9").await;
    match satellite_form(&app) {
        SatelliteRegistryForm::Peer { dispatch_scope, .. } => {
            assert_eq!(dispatch_scope, "ab,9cd");
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn satellite_form_toggles_and_vim_mode_are_unchanged() {
    let mut app = satellite_app("standard", peer_form(2));
    press(&mut app, KeyCode::Char(' '), KeyModifiers::NONE).await;
    match satellite_form(&app) {
        SatelliteRegistryForm::Peer { enabled, .. } => assert!(*enabled, "Space toggles"),
        _ => unreachable!(),
    }
    let mut app = satellite_app("vim", peer_form(0));
    type_text(&mut app, "ab").await;
    press(&mut app, KeyCode::Left, KeyModifiers::NONE).await;
    type_text(&mut app, "c").await;
    match satellite_form(&app) {
        SatelliteRegistryForm::Peer { label, .. } => assert_eq!(label, "abc"),
        _ => unreachable!(),
    }
}

// ---- issue editor ----

fn issue_editor(title: &str) -> crate::types::IssueEditorState {
    crate::types::IssueEditorState {
        mode: crate::types::IssueEditorMode::Create,
        title: title.to_string(),
        body: String::new(),
        priority: None,
        assignee: None,
        labels: Vec::new(),
        status: None,
        dependency_issue_id: None,
        dependency_direction: rsi_common::issue_workspace::IssueDependencyDirectionV1::BlockedBy,
        dependency_candidates: Vec::new(),
        dependency_selected_row: 0,
        dependency_text: String::new(),
        filter_draft: None,
        filter_saved_view: None,
        filter_mine_assignee: None,
        base_issue: None,
        latest_issue: None,
        stale_conflict: false,
        active_field: 0,
        dirty: false,
        discard_armed: false,
        retry_key: "k".to_string(),
        error: None,
        submitted: false,
    }
}

fn issues_app(mode: &str) -> App {
    let mut app = app_with(mode);
    app.poll.authoritative_config_ready = true;
    let pane_id = app.active_tab().focused_pane;
    let mut state = crate::types::IssueWorkspaceState::new(Some(uuid::Uuid::new_v4()));
    state.transient.editor = Some(issue_editor("Draft"));
    *app.active_tab_mut()
        .layout
        .find_pane_mut(pane_id)
        .expect("focused pane") = Pane::Issues(state);
    app
}

fn editor_title(app: &App) -> String {
    match app.focused_pane() {
        Some(Pane::Issues(state)) => state
            .transient
            .editor
            .as_ref()
            .expect("editor open")
            .title
            .clone(),
        _ => panic!("issues pane should be focused"),
    }
}

#[tokio::test]
async fn issue_editor_title_is_edited_with_a_cursor_in_standard_mode() {
    let mut app = issues_app("standard");
    press(&mut app, KeyCode::Home, KeyModifiers::NONE).await;
    type_text(&mut app, "My ").await;
    assert_eq!(editor_title(&app), "My Draft");
    press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL).await;
    type_text(&mut app, "New").await;
    assert_eq!(editor_title(&app), "New", "select-all then type replaces");
    press(&mut app, KeyCode::Backspace, KeyModifiers::CONTROL).await;
    assert_eq!(editor_title(&app), "", "Ctrl-Backspace deletes the word");
}

#[tokio::test]
async fn issue_editor_ctrl_c_copies_a_selection_instead_of_quitting() {
    let mut app = issues_app("standard");
    press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL).await;
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL).await;
    assert!(!app.quit, "Ctrl-C with a selection copies");
    assert_eq!(editor_title(&app), "Draft");
}

#[tokio::test]
async fn issue_editor_stays_append_only_in_vim_mode() {
    let mut app = issues_app("vim");
    press(&mut app, KeyCode::Left, KeyModifiers::NONE).await;
    type_text(&mut app, "!").await;
    assert_eq!(editor_title(&app), "Draft!", "Vim mode appends, as before");
}

// ---- model-dropdown filter ----

#[test]
fn model_dropdown_filter_edits_in_place_in_standard_mode_only() {
    use crate::widget::model_dropdown::{ModelDropdownAction, handle_model_dropdown_key};
    let models = vec![
        ("alpha-1".to_string(), "alpha-1".to_string()),
        ("beta-2".to_string(), "beta-2".to_string()),
    ];
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    let mut state = crate::types::ModelDropdownState::new(
        rsi_common::types::SessionProvider::Claude,
        models.clone(),
        None,
    );
    let typed = |state: &mut crate::types::ModelDropdownState, text: &str, standard| {
        for c in text.chars() {
            handle_model_dropdown_key(state, &key(KeyCode::Char(c)), &[], standard);
        }
    };
    typed(&mut state, "/", true);
    assert!(state.filter_editing);
    typed(&mut state, "bta", true);
    handle_model_dropdown_key(&mut state, &key(KeyCode::Left), &[], true);
    handle_model_dropdown_key(&mut state, &key(KeyCode::Left), &[], true);
    typed(&mut state, "e", true);
    assert_eq!(state.filter_query, "beta");
    assert_eq!(
        handle_model_dropdown_key(&mut state, &key(KeyCode::Home), &[], true),
        ModelDropdownAction::Consumed
    );
    typed(&mut state, "x", true);
    assert_eq!(state.filter_query, "xbeta");

    // Vim mode keeps the append-only query.
    let mut vim = crate::types::ModelDropdownState::new(
        rsi_common::types::SessionProvider::Claude,
        models,
        None,
    );
    typed(&mut vim, "/", false);
    typed(&mut vim, "ab", false);
    handle_model_dropdown_key(&mut vim, &key(KeyCode::Left), &[], false);
    typed(&mut vim, "c", false);
    assert_eq!(vim.filter_query, "abc");
}

// ---- file viewer selection highlight ----

#[tokio::test]
async fn file_viewer_highlights_its_selection_in_standard_mode() {
    let _theme = crate::ui::theme::pin_theme_state();
    use ratatui::{Terminal, backend::TestBackend};
    let (mut app, id) = crate::app::app_test_helpers::with_session_detail();
    *app.focused_pane_mut().unwrap() = Pane::SessionDetail { session_id: id };
    crate::settings::DaemonFeatureEntry::update_from_json(
        &mut app.daemon_features,
        &serde_json::json!({ "editing_mode": "standard" }),
    );
    app.sessions.get_mut(&id).unwrap().file_viewer = Some(crate::types::FileViewerState::new(
        std::path::PathBuf::from("/tmp/standard-viewer.txt"),
        "selected words".to_string(),
    ));
    let draw = |app: &mut App| {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, app))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .filter(|c| c.bg == crate::ui::theme::visual_selection_bg())
            .count()
    };
    assert_eq!(
        draw(&mut app),
        0,
        "nothing is highlighted without a selection"
    );
    press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL).await;
    assert!(
        draw(&mut app) >= "selected words".len(),
        "select-all highlights the text"
    );
}

// ---- create-entity topology filter ----

async fn topology_app(mode: &str) -> App {
    let mut app = app_with(mode);
    crate::overlay::create_entity_form::open_create_entity_form(
        &mut app,
        rsi_common::types::SessionKind::Epic,
        None,
    )
    .await;
    let topology = |name: &str| rsi_common::types::Topology {
        id: uuid::Uuid::new_v4(),
        name: name.to_string(),
        definition: rsi_common::types::TopologyDefinition {
            nodes: Vec::new(),
            edges: Vec::new(),
            until: None,
        },
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };
    if let OverlayState::CreateEntityForm {
        focused_field,
        topology_choices,
        ..
    } = &mut app.overlay
    {
        *focused_field = crate::types::CreateEntityField::Topology;
        *topology_choices = vec![topology("alpha"), topology("alpine"), topology("beta")];
    } else {
        panic!("the create form should be open");
    }
    app
}

fn topology_filter(app: &App) -> String {
    match &app.overlay {
        OverlayState::CreateEntityForm {
            topology_filter, ..
        } => topology_filter.clone(),
        _ => panic!("the create form should be open"),
    }
}

#[tokio::test]
async fn topology_filter_is_edited_in_place_and_every_printable_key_types() {
    let mut app = topology_app("standard").await;
    // `j`, `k` and `G` are list keys in Vim but ordinary letters here.
    type_text(&mut app, "jkalp").await;
    assert_eq!(topology_filter(&app), "jkalp");
    press(&mut app, KeyCode::Home, KeyModifiers::NONE).await;
    press(&mut app, KeyCode::Delete, KeyModifiers::NONE).await;
    press(&mut app, KeyCode::Delete, KeyModifiers::NONE).await;
    assert_eq!(topology_filter(&app), "alp");
    press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL).await;
    type_text(&mut app, "be").await;
    assert_eq!(topology_filter(&app), "be");
    press(&mut app, KeyCode::Down, KeyModifiers::NONE).await;
    assert_eq!(topology_filter(&app), "be", "Down navigates the list");
}

#[tokio::test]
async fn topology_filter_keeps_vim_list_keys_in_vim_mode() {
    let mut app = topology_app("vim").await;
    type_text(&mut app, "al").await;
    press(&mut app, KeyCode::Left, KeyModifiers::NONE).await;
    type_text(&mut app, "p").await;
    assert_eq!(topology_filter(&app), "alp", "Vim mode appends, as before");
}
