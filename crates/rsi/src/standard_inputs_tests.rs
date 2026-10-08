//! Standard (non-modal) editing in every text input beyond the composer
//! (#1628 slice 3). Each test drives the production key path
//! (`event::step_once`) with the operator's `editing_mode` set, and asserts
//! the positive end state.

use crate::app::App;
use crate::event::step_once;
use crate::types::{OverlayState, Pane, PopupMode};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use uuid::Uuid;

fn app_with(mode: &str) -> App {
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    set_mode(&mut app, mode);
    app
}

fn set_mode(app: &mut App, mode: &str) {
    crate::settings::DaemonFeatureEntry::update_from_json(
        &mut app.daemon_features,
        &serde_json::json!({ "editing_mode": mode }),
    );
}

async fn press(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    step_once(app, KeyEvent::new(code, mods)).await;
}

async fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, KeyCode::Char(c), KeyModifiers::NONE).await;
    }
}

fn palette_query(app: &App) -> String {
    match &app.overlay {
        OverlayState::CommandPalette { query, .. } => query.clone(),
        other => panic!("expected the command palette, got {}", overlay_name(other)),
    }
}

fn overlay_name(overlay: &OverlayState) -> &'static str {
    if matches!(overlay, OverlayState::None) {
        "none"
    } else {
        "another overlay"
    }
}

// ---- single-line fields ----

#[tokio::test]
async fn palette_query_gets_a_real_cursor_in_standard_mode() {
    let mut app = app_with("standard");
    crate::overlay::command_palette::open_command_palette(&mut app);
    type_text(&mut app, "mnage").await;
    for _ in 0..4 {
        press(&mut app, KeyCode::Left, KeyModifiers::NONE).await;
    }
    type_text(&mut app, "a").await;
    assert_eq!(palette_query(&app), "manage");
    press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL).await;
    type_text(&mut app, "rate").await;
    assert_eq!(palette_query(&app), "rate", "select-all then type replaces");
    press(&mut app, KeyCode::Backspace, KeyModifiers::CONTROL).await;
    assert_eq!(palette_query(&app), "", "Ctrl-Backspace deletes the word");
}

#[tokio::test]
async fn palette_query_stays_append_only_in_vim_mode() {
    let mut app = app_with("vim");
    crate::overlay::command_palette::open_command_palette(&mut app);
    type_text(&mut app, "ab").await;
    press(&mut app, KeyCode::Left, KeyModifiers::NONE).await;
    type_text(&mut app, "c").await;
    assert_eq!(palette_query(&app), "abc", "Vim mode appends, as before");
}

#[tokio::test]
async fn ctrl_c_copies_a_field_selection_instead_of_quitting() {
    let mut app = app_with("standard");
    crate::overlay::command_palette::open_command_palette(&mut app);
    type_text(&mut app, "abc").await;
    press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL).await;
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL).await;
    assert!(!app.quit, "Ctrl-C with a selection copies");
    assert_eq!(palette_query(&app), "abc");
    press(&mut app, KeyCode::Right, KeyModifiers::NONE).await;
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL).await;
    assert!(
        app.quit,
        "with no selection Ctrl-C keeps its global meaning"
    );
}

#[tokio::test]
async fn rename_title_is_edited_in_place() {
    let mut app = app_with("standard");
    let session_id = app.filtered_session_order[0];
    app.overlay = OverlayState::RenameSession {
        session_id,
        title: "old".to_string(),
    };
    press(&mut app, KeyCode::Home, KeyModifiers::NONE).await;
    type_text(&mut app, "very ").await;
    assert!(matches!(
        &app.overlay,
        OverlayState::RenameSession { title, .. } if title == "very old"
    ));
}

#[tokio::test]
async fn form_fields_edit_with_a_cursor_and_keep_their_character_filters() {
    let mut app = app_with("standard");
    crate::overlay::schedule_form::open_schedule_form_new(&mut app);
    type_text(&mut app, "nightly").await;
    press(&mut app, KeyCode::Home, KeyModifiers::NONE).await;
    type_text(&mut app, "my ").await;
    // Interval field (3): only digits are accepted.
    for _ in 0..3 {
        press(&mut app, KeyCode::Tab, KeyModifiers::NONE).await;
    }
    type_text(&mut app, "1x2").await;
    match &app.overlay {
        OverlayState::ScheduleForm { name, interval, .. } => {
            assert_eq!(name, "my nightly");
            assert!(interval.ends_with("12"), "digits only, got {interval:?}");
        }
        _ => panic!("expected the schedule form"),
    }
}

#[tokio::test]
async fn slash_search_line_edits_in_place_and_refilters() {
    let mut app = app_with("standard");
    app.input_mode = crate::types::InputMode::Search;
    type_text(&mut app, "ab").await;
    press(&mut app, KeyCode::Left, KeyModifiers::NONE).await;
    type_text(&mut app, "X").await;
    assert_eq!(app.search_query, "aXb");
}

// ---- InputSurface overlays ----

fn open_blank_prompt(app: &mut App) {
    crate::overlay::prompt::open_blank_popup(app);
}

fn prompt_surface(app: &App) -> &crate::input_surface::InputSurface {
    match app.focused_input_overlay().expect("a stacked prompt") {
        OverlayState::Prompt { surface, .. } => surface,
        _ => panic!("expected a prompt overlay"),
    }
}

#[tokio::test]
async fn prompt_overlay_types_vim_keys_and_esc_closes_it() {
    let mut app = app_with("standard");
    open_blank_prompt(&mut app);
    type_text(&mut app, "jk qi").await;
    let surface = prompt_surface(&app);
    assert_eq!(surface.content(), "jk qi");
    assert_eq!(surface.mode, PopupMode::Insert);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE).await;
    assert!(
        app.input_overlays.is_empty(),
        "with no Normal mode Esc closes the prompt"
    );
}

#[tokio::test]
async fn prompt_overlay_ctrl_a_selects_all_and_ctrl_c_copies_it() {
    let mut app = app_with("standard");
    open_blank_prompt(&mut app);
    type_text(&mut app, "hello").await;
    press(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL).await;
    assert_eq!(
        crate::input_surface::selected_text(prompt_surface(&app)).as_deref(),
        Some("hello"),
        "Ctrl-A selects all instead of opening the AI command"
    );
    press(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL).await;
    assert!(!app.quit, "Ctrl-C copies the overlay selection");
    type_text(&mut app, "x").await;
    assert_eq!(prompt_surface(&app).content(), "x");
}

#[tokio::test]
async fn prompt_overlay_follows_the_mode_live() {
    let mut app = app_with("vim");
    open_blank_prompt(&mut app);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE).await;
    assert_eq!(prompt_surface(&app).mode, PopupMode::Normal);
    set_mode(&mut app, "standard");
    type_text(&mut app, "ok").await;
    assert_eq!(prompt_surface(&app).content(), "ok");
    assert_eq!(prompt_surface(&app).mode, PopupMode::Insert);
}

#[test]
fn a_frame_in_standard_mode_moves_overlay_surfaces_out_of_normal_mode() {
    let mut app = app_with("vim");
    open_blank_prompt(&mut app);
    if let Some(OverlayState::Prompt { surface, .. }) = app.focused_input_overlay_mut() {
        surface.mode = PopupMode::Normal;
    }
    set_mode(&mut app, "standard");
    app.sync_standard_surfaces();
    let surface = prompt_surface(&app);
    assert!(surface.standard_editing);
    assert_eq!(surface.mode, PopupMode::Insert);
}

#[tokio::test]
async fn provider_form_fields_are_typed_into_and_esc_closes() {
    let mut app = app_with("standard");
    crate::overlay::provider_form::open_provider_form(&mut app, None);
    type_text(&mut app, "acme").await;
    match &app.overlay {
        OverlayState::ProviderForm { name, .. } => assert_eq!(name.content(), "acme"),
        _ => panic!("expected the provider form"),
    }
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE).await;
    assert!(matches!(app.overlay, OverlayState::None));
}

// ---- file viewer ----

fn app_with_viewer(mode: &str, text: &str) -> (App, Uuid) {
    let (mut app, id) = crate::app::app_test_helpers::with_session_detail();
    *app.focused_pane_mut().unwrap() = Pane::SessionDetail { session_id: id };
    set_mode(&mut app, mode);
    app.sessions.get_mut(&id).unwrap().file_viewer = Some(crate::types::FileViewerState::new(
        std::path::PathBuf::from("/tmp/standard-viewer.txt"),
        text.to_string(),
    ));
    (app, id)
}

fn viewer_text(app: &App, id: Uuid) -> String {
    app.sessions[&id]
        .file_viewer
        .as_ref()
        .expect("viewer")
        .surface
        .content()
}

#[tokio::test]
async fn standard_file_viewer_edits_directly_and_esc_closes() {
    let (mut app, id) = app_with_viewer("standard", "world");
    press(&mut app, KeyCode::Home, KeyModifiers::NONE).await;
    type_text(&mut app, "hello ").await;
    assert_eq!(viewer_text(&app, id), "hello world");
    assert!(app.sessions[&id].file_viewer.as_ref().unwrap().dirty);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE).await;
    assert!(app.sessions[&id].file_viewer.is_none(), "Esc closes");
}

#[tokio::test]
async fn vim_file_viewer_still_starts_in_normal_mode() {
    let (mut app, id) = app_with_viewer("vim", "world");
    type_text(&mut app, "jkx").await;
    assert_eq!(
        viewer_text(&app, id),
        "orld",
        "Normal-mode keys are commands in Vim mode (x deletes), not text"
    );
}

#[tokio::test]
async fn standard_file_viewer_ctrl_f_finds() {
    let (mut app, id) = app_with_viewer("standard", "alpha\nbeta\ngamma");
    press(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL).await;
    assert!(
        app.sessions[&id]
            .file_viewer
            .as_ref()
            .unwrap()
            .search
            .input_active
    );
}

// ---- help rows ----

#[test]
fn help_rows_describe_the_active_mode() {
    use crate::action_registry::{OverlayHelpClass, overlay_help_route};
    let route = overlay_help_route(OverlayHelpClass::LaunchPrompt).expect("route");
    let vim: Vec<_> = route.entries_for(false).iter().map(|e| e.keys).collect();
    let standard: Vec<_> = route.entries_for(true).iter().map(|e| e.keys).collect();
    assert!(vim.contains(&"Ctrl-A") && vim.contains(&"i / a / o"));
    assert!(standard.contains(&"Ctrl-Alt-A"), "AI command moved");
    assert!(
        standard.contains(&"Ctrl-Left / Ctrl-Right"),
        "Standard editing keys are listed"
    );
    assert!(
        standard.contains(&"Ctrl-Alt-G"),
        "help is reached by Ctrl-Alt-G"
    );
    assert_eq!(
        route.entries_for(false).len(),
        route.entries().count(),
        "Vim rows are the catalog rows unchanged"
    );
}

#[test]
fn file_viewer_help_lists_the_standard_chords() {
    use crate::action_registry::{OverlayHelpClass, overlay_help_route};
    let route = overlay_help_route(OverlayHelpClass::FileViewer).expect("route");
    let standard: Vec<_> = route.entries_for(true).iter().map(|e| e.keys).collect();
    assert!(standard.contains(&"Ctrl-F"));
    assert!(standard.contains(&"Esc, Ctrl-Q"));
}

// ---- render ----

#[test]
fn prompt_overlay_shows_the_edit_label_in_standard_mode() {
    use ratatui::{Terminal, backend::TestBackend};
    let mut app = app_with("standard");
    open_blank_prompt(&mut app);
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text: String = buffer.content.iter().map(|c| c.symbol()).collect();
    assert!(text.contains(" EDIT "), "mode badge reads EDIT");
}

#[test]
fn palette_draws_the_caret_inside_the_text_in_standard_mode() {
    use ratatui::{Terminal, backend::TestBackend, style::Modifier};
    let mut app = app_with("standard");
    crate::overlay::command_palette::open_command_palette(&mut app);
    if let OverlayState::CommandPalette { query, .. } = &mut app.overlay {
        *query = "rate".to_string();
    }
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    // The first frame binds the field cursor to this overlay.
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    app.field_edit.sync("rate");
    let mut text = "rate".to_string();
    app.field_edit
        .handle_key(&mut text, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    app.field_edit
        .handle_key(&mut text, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    terminal
        .draw(|frame| crate::ui::render(frame, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert!(
        buffer
            .content
            .iter()
            .any(|c| c.symbol() == "t" && c.modifier.contains(Modifier::REVERSED)),
        "the caret sits on the character the cursor is before"
    );
}

// ---- question modal ----

fn install_question(app: &mut App, options: usize, multi: bool) {
    let questions = vec![rsi_common::types::QuestionItem {
        question: "which one?".to_string(),
        header: "pick".to_string(),
        options: (0..options)
            .map(|i| rsi_common::types::QuestionOption {
                label: format!("option {}", i + 1),
                description: String::new(),
            })
            .collect(),
        multi_select: multi,
    }];
    app.overlay = OverlayState::QuestionModal {
        session_id: Uuid::new_v4(),
        questions,
        current_question: 0,
        cursor: vec![0],
        selections: vec![if multi {
            crate::types::QuestionSelection::Multi(vec![])
        } else {
            crate::types::QuestionSelection::Single(None)
        }],
        textarea: Box::new(tui_textarea::TextArea::default()),
        mode: PopupMode::Normal,
        pending_operator: None,
    };
}

#[tokio::test]
async fn question_modal_types_a_free_answer_without_entering_insert_mode() {
    let mut app = app_with("standard");
    install_question(&mut app, 0, false);
    type_text(&mut app, "dq jk").await;
    press(&mut app, KeyCode::Enter, KeyModifiers::SHIFT).await;
    type_text(&mut app, "ok").await;
    match &app.overlay {
        OverlayState::QuestionModal { textarea, .. } => {
            assert_eq!(textarea.lines(), &["dq jk", "ok"]);
        }
        _ => panic!("expected the question modal"),
    }
}

#[tokio::test]
async fn question_modal_picks_options_while_the_box_is_empty_then_types() {
    let mut app = app_with("standard");
    install_question(&mut app, 3, false);
    press(&mut app, KeyCode::Down, KeyModifiers::NONE).await;
    press(&mut app, KeyCode::Down, KeyModifiers::NONE).await;
    match &app.overlay {
        OverlayState::QuestionModal { selections, .. } => assert!(matches!(
            selections[0],
            crate::types::QuestionSelection::Single(Some(2))
        )),
        _ => panic!("expected the question modal"),
    }
    press(&mut app, KeyCode::Char('1'), KeyModifiers::NONE).await;
    match &app.overlay {
        OverlayState::QuestionModal { selections, .. } => assert!(matches!(
            selections[0],
            crate::types::QuestionSelection::Single(Some(0))
        )),
        _ => panic!("expected the question modal"),
    }
    // Typing a free answer deselects the option and lands in the box.
    type_text(&mut app, "other").await;
    match &app.overlay {
        OverlayState::QuestionModal {
            selections,
            textarea,
            ..
        } => {
            assert_eq!(textarea.lines(), &["other"]);
            assert!(matches!(
                selections[0],
                crate::types::QuestionSelection::Single(None)
            ));
        }
        _ => panic!("expected the question modal"),
    }
}

#[tokio::test]
async fn question_modal_esc_dismisses_and_vim_keeps_its_normal_mode() {
    let mut app = app_with("standard");
    install_question(&mut app, 0, false);
    press(&mut app, KeyCode::Esc, KeyModifiers::NONE).await;
    assert!(matches!(app.overlay, OverlayState::None));

    let mut app = app_with("vim");
    install_question(&mut app, 0, false);
    type_text(&mut app, "x").await;
    match &app.overlay {
        OverlayState::QuestionModal { textarea, mode, .. } => {
            assert_eq!(*mode, PopupMode::Normal);
            assert!(textarea.lines().iter().all(String::is_empty));
        }
        _ => panic!("expected the question modal"),
    }
}
