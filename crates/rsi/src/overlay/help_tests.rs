//! Contextual help coverage for overlays that own their key handling (#547).
//!
//! Each test opens help from a real overlay state, asserts the help origin
//! names the overlay's catalog class, and asserts the rendered help lists the
//! overlay's close key plus at least one class-specific action.
#![allow(clippy::unwrap_used)]

use super::*;
use crate::action_registry::{
    ActionAvailability, ActionContext, HelpOrigin, OverlayHelpClass, request_for_key,
};
use crate::client::DaemonClient;
use crate::state::{DevState, PersistedState};
use crate::types::{OverlayState, Pane};
use crate::ui::overlay::keybindings_help::contextual_lines;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::path::PathBuf;

fn test_app() -> App {
    DevState::clear();
    PersistedState::default().save();
    let mut app = App::new(DaemonClient::new(PathBuf::from("/tmp/test.sock")));
    if let Some(Pane::SessionList {
        selected_index,
        selected_session,
        ..
    }) = app.focused_pane_mut()
    {
        *selected_index = 0;
        *selected_session = None;
    }
    app
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn help_chord() -> KeyEvent {
    KeyEvent::new(
        KeyCode::Char('g'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    )
}

fn render(app: &App, filter: &str) -> String {
    contextual_lines(app, filter)
        .iter()
        .map(|line| format!("{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Open help from the current overlay, capture origin and full help text,
/// close help, and return both.
fn help_for(app: &mut App) -> (HelpOrigin, String) {
    keybindings_help::open_contextual_help(app);
    let origin = match &app.overlay {
        OverlayState::KeybindingsHelp { origin, .. } => *origin,
        _ => panic!("help did not open"),
    };
    let text = render(app, "");
    keybindings_help::close_keybindings_help(app);
    (origin, text)
}

/// Assert help names `class`, lists every `expected` snippet, and advertises
/// the always-available help chord.
fn assert_help(app: &mut App, class: OverlayHelpClass, expected: &[&str]) {
    let (origin, text) = help_for(app);
    assert_eq!(origin, HelpOrigin::OverlayClass(class));
    for snippet in expected.iter().chain(std::iter::once(&"Ctrl-Alt-G")) {
        assert!(
            text.contains(snippet),
            "{class:?} help lists {snippet:?}; got:\n{text}"
        );
    }
}

#[tokio::test]
async fn create_entity_form_help_follows_insert_and_normal_modes_and_restores_draft() {
    let mut app = test_app();
    create_entity_form::open_create_entity_form(
        &mut app,
        rsi_common::types::SessionKind::Group,
        None,
    )
    .await;
    if let OverlayState::CreateEntityForm { insert_mode, .. } = &mut app.overlay {
        *insert_mode = true;
    }
    for ch in "Unsaved draft".chars() {
        handle_overlay_key(&mut app, key(KeyCode::Char(ch))).await;
    }
    assert_help(
        &mut app,
        OverlayHelpClass::CreateEntityInsert,
        &["Ctrl-D", "Discard draft and close", "Commit tag chip"],
    );

    // Opening and closing help through the production chord keeps the text.
    crate::event::step_once(&mut app, help_chord()).await;
    assert!(matches!(
        app.overlay,
        OverlayState::KeybindingsHelp {
            origin: HelpOrigin::OverlayClass(OverlayHelpClass::CreateEntityInsert),
            ..
        }
    ));
    crate::event::step_once(&mut app, help_chord()).await;
    assert!(matches!(
        &app.overlay,
        OverlayState::CreateEntityForm { name, insert_mode: true, .. }
            if name == "Unsaved draft"
    ));

    handle_overlay_key(&mut app, key(KeyCode::Esc)).await;
    assert_help(
        &mut app,
        OverlayHelpClass::CreateEntityNormal,
        &["Esc", "Save draft and close", "gp", "Open parent picker"],
    );
    assert!(matches!(
        &app.overlay,
        OverlayState::CreateEntityForm { name, insert_mode: false, .. }
            if name == "Unsaved draft"
    ));
}

#[tokio::test]
async fn picker_overlays_list_close_and_selection_keys() {
    let mut app = test_app();
    sort_picker::open_sort_picker(&mut app);
    assert_help(
        &mut app,
        OverlayHelpClass::SortPicker,
        &["Esc / q", "Apply sort order", "Space ;"],
    );

    let mut app = test_app();
    theme_picker::open_theme_picker(&mut app);
    assert_help(
        &mut app,
        OverlayHelpClass::ThemePicker,
        &["Revert preview and close", "Apply numbered theme"],
    );

    let mut app = test_app();
    app.model_dropdown = crate::types::ModelDropdownState::new(
        app.selected_provider,
        app.available_models.clone(),
        app.selected_model.as_deref(),
    );
    assert_help(
        &mut app,
        OverlayHelpClass::ModelPicker,
        &["Close model picker", "Next / previous provider"],
    );
}

#[tokio::test]
async fn file_explorer_finder_and_viewer_help_describe_their_handlers() {
    let mut app = test_app();
    app.overlay = OverlayState::FileExplorer {
        root: PathBuf::from("/tmp"),
        entries: Vec::new(),
        selected_index: 0,
        scroll_offset: 0,
        show_hidden: false,
        trash: Vec::new(),
        pending_yank: false,
        pending_delete: false,
        finder_active: false,
        finder_query: String::new(),
        finder_cache: Vec::new(),
        finder_results: Vec::new(),
        finder_selected: 0,
        explorer_focused: true,
    };
    assert_help(
        &mut app,
        OverlayHelpClass::FileExplorer,
        &["Close explorer", "Toggle hidden files", "Open fuzzy finder"],
    );
    if let OverlayState::FileExplorer { finder_active, .. } = &mut app.overlay {
        *finder_active = true;
    }
    assert_help(
        &mut app,
        OverlayHelpClass::FileExplorerFinder,
        &[
            "Return to explorer tree",
            "Return to tree, then close explorer",
            "Open selected file",
        ],
    );

    let mut app = crate::app::app_test_helpers::with_session_list(1);
    let session_id = app.selected_session_id().unwrap();
    app.sessions.get_mut(&session_id).unwrap().file_viewer = Some(
        crate::types::FileViewerState::new(PathBuf::from("/tmp/notes.md"), "x".into()),
    );
    assert_help(
        &mut app,
        OverlayHelpClass::FileViewer,
        &["Close viewer", "Search forward / backward", "Save file"],
    );
}

#[tokio::test]
async fn command_palette_help_lists_run_and_close() {
    let mut app = test_app();
    command_palette::open_command_palette(&mut app);
    assert_help(
        &mut app,
        OverlayHelpClass::CommandPalette,
        &["Leave argument edit, then close", "Run selected command"],
    );
    assert!(matches!(app.overlay, OverlayState::CommandPalette { .. }));
}

#[tokio::test]
async fn browsers_and_modals_list_close_and_class_actions() {
    let mut app = test_app();
    app.overlay = OverlayState::NotificationBrowser {
        selected_index: 0,
        scroll_offset: 0,
    };
    assert_help(
        &mut app,
        OverlayHelpClass::Notifications,
        &["Esc / q", "Dismiss all active notifications"],
    );

    app.overlay = OverlayState::TrashBrowser {
        sessions: Vec::new(),
        items: Vec::new(),
        selected_index: 0,
        scroll_offset: 0,
    };
    assert_help(
        &mut app,
        OverlayHelpClass::TrashBrowser,
        &["Esc / q", "Purge session permanently", "Restore session"],
    );

    let mut app = crate::app::app_test_helpers::with_session_list(1);
    let session_id = app.selected_session_id().unwrap();
    app.sessions
        .get_mut(&session_id)
        .unwrap()
        .session
        .pending_question = Some(rsi_common::types::PendingQuestion {
        questions: vec![rsi_common::types::QuestionItem {
            question: "Proceed?".into(),
            header: "Gate".into(),
            options: vec![rsi_common::types::QuestionOption {
                label: "Yes".into(),
                description: String::new(),
            }],
            multi_select: false,
        }],
    });
    question_modal::open_question_modal(&mut app);
    assert_help(
        &mut app,
        OverlayHelpClass::QuestionNormal,
        &["Hide question", "Decline question"],
    );
    handle_overlay_key(&mut app, key(KeyCode::Char('i'))).await;
    assert_help(
        &mut app,
        OverlayHelpClass::QuestionInsert,
        &["Return to normal mode", "Submit answers"],
    );

    let mut app = test_app();
    color_customizer::open_color_customizer(&mut app);
    assert_help(
        &mut app,
        OverlayHelpClass::ColorCustomizer,
        &["Esc / q", "Reset field to default"],
    );
}

#[tokio::test]
async fn graph_review_help_follows_empty_navigate_and_detail_modes() {
    let mut app = test_app();
    graph::open_graph_review(&mut app).await;
    assert_help(
        &mut app,
        OverlayHelpClass::GraphEmpty,
        &["Close graph review", "Open topology picker"],
    );
    let draft_id = match &app.overlay {
        OverlayState::GraphReview { draft_id, .. } => *draft_id,
        _ => panic!("graph review closed"),
    };
    app.graph_draft_mut(&draft_id).unwrap().workflow.nodes.push(
        rsi_graph::format::NodeDef::action("draft-node", "Draft Node"),
    );
    assert_help(
        &mut app,
        OverlayHelpClass::GraphNavigate,
        &["Close graph review", "Open node detail"],
    );
    handle_overlay_key(&mut app, key(KeyCode::Enter)).await;
    assert_help(
        &mut app,
        OverlayHelpClass::GraphDetail,
        &["Back out of detail", "Edit node name / instructions"],
    );
}

#[tokio::test]
async fn recursive_dag_help_lists_run_controls() {
    let mut app = test_app();
    app.overlay =
        OverlayState::RecursiveDagBrowser(crate::types::RecursiveDagBrowserState::loading(None));
    assert_help(
        &mut app,
        OverlayHelpClass::DagBrowser,
        &["Esc / q", "Start fake / live run", "Reveal control details"],
    );
}

#[tokio::test]
async fn help_search_finds_overlay_catalog_entries() {
    let mut app = test_app();
    app.overlay = OverlayState::TrashBrowser {
        sessions: Vec::new(),
        items: Vec::new(),
        selected_index: 0,
        scroll_offset: 0,
    };
    keybindings_help::open_contextual_help(&mut app);
    let found = render(&app, "PURGE permanently");
    assert!(found.contains("Purge session permanently"), "{found}");
    assert!(found.contains("Trash"), "search keeps the class heading");
    assert_eq!(
        contextual_lines(&app, "purge impossible").len(),
        1,
        "unmatched overlay search shows the explicit no-results row"
    );
}

#[tokio::test]
async fn discovery_only_overlay_keys_stay_with_the_overlay_handler() {
    let mut app = test_app();
    sort_picker::open_sort_picker(&mut app);
    let context = ActionContext::from_app(&app);
    assert_eq!(
        context.origin,
        HelpOrigin::OverlayClass(OverlayHelpClass::SortPicker)
    );
    // The catalog documents `j`, but the registry resolves no action for it.
    assert!(matches!(
        request_for_key(&context, key(KeyCode::Char('j'))),
        ActionAvailability::Unavailable { .. }
    ));
    // The overlay's own handler still receives the key and moves selection.
    assert!(handle_overlay_key(&mut app, key(KeyCode::Char('j'))).await);
    assert!(matches!(
        app.overlay,
        OverlayState::SortPicker { selected_index: 1 }
    ));
    // Contextual help itself stays registry-dispatchable from the overlay.
    assert!(matches!(
        request_for_key(&context, help_chord()),
        ActionAvailability::Available(request)
            if request.id == crate::action_registry::ActionId::ContextHelp
    ));
}

#[tokio::test]
#[allow(clippy::expect_used)]
async fn prompt_owned_model_picker_help_describes_the_picker_in_both_prompt_slots() {
    let open_dropdown = |app: &App| {
        crate::types::ModelDropdownState::new(
            app.selected_provider,
            app.available_models.clone(),
            app.selected_model.as_deref(),
        )
    };

    // Prompt held in `app.overlay` (ContinueSession-style path).
    let mut app = test_app();
    prompt::open_typed_prompt(&mut app, rsi_common::types::SessionKind::Task, None);
    app.overlay = app.input_overlays.pop().expect("typed prompt should open");
    let dropdown = open_dropdown(&app);
    if let OverlayState::Prompt { model_dropdown, .. } = &mut app.overlay {
        *model_dropdown = dropdown;
    }
    assert_help(
        &mut app,
        OverlayHelpClass::ModelPicker,
        &["Close model picker", "Next / previous provider"],
    );
    assert!(matches!(
        &app.overlay,
        OverlayState::Prompt { model_dropdown, .. } if model_dropdown.open
    ));

    // Prompt held in the input-overlay stack (typed prompt path).
    let mut app = test_app();
    prompt::open_typed_prompt(&mut app, rsi_common::types::SessionKind::Task, None);
    let dropdown = open_dropdown(&app);
    let idx = app.focused_input_idx;
    if let Some(OverlayState::Prompt { model_dropdown, .. }) = app.input_overlays.get_mut(idx) {
        *model_dropdown = dropdown;
    }
    assert_help(
        &mut app,
        OverlayHelpClass::ModelPicker,
        &["Close model picker", "Next / previous provider"],
    );
    assert!(matches!(
        app.input_overlays.get(idx),
        Some(OverlayState::Prompt { model_dropdown, .. }) if model_dropdown.open
    ));
}
