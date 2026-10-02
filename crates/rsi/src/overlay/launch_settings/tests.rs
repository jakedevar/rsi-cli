//! New-session prompt: `?` help, the settings side, header chips, the clean
//! mode row, and the manager appointed at launch.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::action_registry::{HelpOrigin, OverlayHelpClass};
use crate::client::DaemonClient;
use crate::overlay::handle_overlay_key;
use crate::state::{DevState, PersistedState};
use crate::types::{OverlayState, Pane, PopupMode, PromptLaunchSettings};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rsi_common::harness_manager::HarnessManagerConfigV1;
use rsi_common::harness_manager_presets::classify_manager_policy;
use rsi_common::harness_manager_v2::{
    HarnessManagerPolicyConfigV2, ManagerCapabilityV2, ManagerOperatingModeV2,
};
use rsi_common::types::SessionProvider;
use std::path::PathBuf;

fn test_app_at(socket_path: PathBuf) -> App {
    DevState::clear();
    PersistedState::default().save();
    let mut app = App::new(DaemonClient::new(socket_path));
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

fn test_app() -> App {
    test_app_at(PathBuf::from("/tmp/test.sock"))
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

async fn press(app: &mut App, event: KeyEvent) {
    handle_overlay_key(app, event).await;
}

async fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        press(app, key(KeyCode::Char(c))).await;
    }
}

fn focused_prompt(app: &App) -> &OverlayState {
    app.focused_input_overlay().expect("prompt is open")
}

fn launch(app: &App) -> &PromptLaunchSettings {
    match focused_prompt(app) {
        OverlayState::Prompt { launch, .. } => launch,
        _ => panic!("focused overlay is a prompt"),
    }
}

fn prompt_text(app: &App) -> String {
    match focused_prompt(app) {
        OverlayState::Prompt { surface, .. } => surface.textarea.lines().join("\n"),
        _ => panic!("focused overlay is a prompt"),
    }
}

fn prompt_mode(app: &App) -> PopupMode {
    match focused_prompt(app) {
        OverlayState::Prompt { surface, .. } => surface.mode,
        _ => panic!("focused overlay is a prompt"),
    }
}

fn sandbox_enabled(app: &App) -> bool {
    match focused_prompt(app) {
        OverlayState::Prompt {
            sandbox_enabled, ..
        } => *sandbox_enabled,
        _ => panic!("focused overlay is a prompt"),
    }
}

fn selected(app: &App) -> Option<LaunchSettingRow> {
    match focused_prompt(app) {
        OverlayState::Prompt {
            purpose, launch, ..
        } => setting_rows(purpose, launch).get(launch.selected).copied(),
        _ => None,
    }
}

/// Render the overlay layer and return one string per screen row. The
/// prompt's working directory is pinned so header widths are stable.
fn render_rows(app: &mut App) -> Vec<String> {
    for overlay in &mut app.input_overlays {
        if let OverlayState::Prompt { working_dir, .. } = overlay {
            *working_dir = PathBuf::from("/tmp");
        }
    }
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 40)).expect("terminal");
    terminal
        .draw(|frame| crate::ui::overlay::render_overlay(frame, frame.area(), app))
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

fn row_containing<'a>(rows: &'a [String], needle: &str) -> &'a str {
    rows.iter()
        .find(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("a row contains {needle:?}:\n{}", rows.join("\n")))
}

fn add_container(app: &mut App, kind: SessionKind, title: &str, parent: Option<Uuid>) -> Uuid {
    let id = Uuid::new_v4();
    let mut session = crate::app::app_test_helpers::baseline_session(id, kind);
    session.title = Some(title.to_string());
    session.project_id = app.current_project_id;
    session.parent_id = parent;
    app.upsert_session(session);
    id
}

fn claude_app() -> App {
    let mut app = test_app();
    app.selected_provider = SessionProvider::Claude;
    app.selected_model = Some("claude-opus-5".to_string());
    app.selected_effort = None;
    app
}

// === `?` help =================================================================

#[tokio::test]
async fn question_mark_in_normal_mode_opens_prompt_help_and_restores_the_draft() {
    let mut app = test_app();
    crate::overlay::open_blank_popup(&mut app);
    type_text(&mut app, "draft").await;
    press(&mut app, key(KeyCode::Esc)).await;

    press(&mut app, key(KeyCode::Char('?'))).await;
    assert!(matches!(
        app.overlay,
        OverlayState::KeybindingsHelp {
            origin: HelpOrigin::OverlayClass(OverlayHelpClass::LaunchPrompt),
            ..
        }
    ));
    let help = crate::ui::overlay::keybindings_help::contextual_lines(&app, "")
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    for entry in [
        "New Session Prompt",
        "Launch the session",
        "Flip to launch settings",
        "Cycle effort level",
        "Toggle sandbox",
        "Close and keep the draft",
    ] {
        assert!(help.contains(entry), "help lists {entry:?}:\n{help}");
    }

    // `?` closes help again and the prompt is exactly where it was.
    press(&mut app, key(KeyCode::Char('?'))).await;
    assert!(matches!(app.overlay, OverlayState::None));
    assert_eq!(prompt_text(&app), "draft");
    assert_eq!(prompt_mode(&app), PopupMode::Normal);
}

#[tokio::test]
async fn shifted_question_mark_also_opens_help() {
    let mut app = test_app();
    crate::overlay::open_blank_popup(&mut app);
    press(&mut app, key(KeyCode::Esc)).await;
    press(
        &mut app,
        KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT),
    )
    .await;
    assert!(matches!(app.overlay, OverlayState::KeybindingsHelp { .. }));
}

#[tokio::test]
async fn question_mark_is_text_in_insert_mode_and_a_target_after_f() {
    let mut app = test_app();
    crate::overlay::open_blank_popup(&mut app);
    type_text(&mut app, "a?b").await;
    assert_eq!(prompt_text(&app), "a?b");
    assert!(matches!(app.overlay, OverlayState::None));

    // `f?` completes a char search instead of opening help.
    press(&mut app, key(KeyCode::Esc)).await;
    press(&mut app, key(KeyCode::Char('0'))).await;
    press(&mut app, key(KeyCode::Char('f'))).await;
    press(&mut app, key(KeyCode::Char('?'))).await;
    assert!(matches!(app.overlay, OverlayState::None));
    match focused_prompt(&app) {
        OverlayState::Prompt { surface, .. } => assert_eq!(surface.textarea.cursor(), (0, 1)),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn continue_prompt_question_mark_opens_continue_help() {
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    let session_id = app.selected_session_id().expect("fixture session");
    crate::overlay::open_continue_popup(&mut app, session_id);
    press(&mut app, key(KeyCode::Esc)).await;
    press(&mut app, key(KeyCode::Char('?'))).await;
    assert!(matches!(
        app.overlay,
        OverlayState::KeybindingsHelp {
            origin: HelpOrigin::OverlayClass(OverlayHelpClass::ContinuePrompt),
            ..
        }
    ));
    press(&mut app, key(KeyCode::Char('?'))).await;
    assert!(matches!(
        &app.overlay,
        OverlayState::Prompt {
            purpose: PromptPurpose::ContinueSession(id),
            launch,
            ..
        } if *id == session_id && !launch.open
    ));
}

// === Flipping =================================================================

#[tokio::test]
async fn ctrl_o_flips_from_insert_mode_and_keeps_typing_where_it_left_off() {
    let mut app = test_app();
    crate::overlay::open_blank_popup(&mut app);
    type_text(&mut app, "abc").await;

    press(&mut app, ctrl(KeyCode::Char('o'))).await;
    assert!(launch(&app).open);
    // Plain keys on the settings side never reach the hidden text.
    type_text(&mut app, "xd").await;
    assert_eq!(prompt_text(&app), "abc");

    press(&mut app, ctrl(KeyCode::Char('o'))).await;
    assert!(!launch(&app).open);
    assert_eq!(prompt_mode(&app), PopupMode::Insert);
    type_text(&mut app, "d").await;
    assert_eq!(prompt_text(&app), "abcd");
}

#[tokio::test]
async fn tab_flips_in_normal_mode_and_esc_flips_back() {
    let mut app = test_app();
    crate::overlay::open_blank_popup(&mut app);
    press(&mut app, key(KeyCode::Esc)).await;

    press(&mut app, key(KeyCode::Tab)).await;
    assert!(launch(&app).open);
    press(&mut app, key(KeyCode::Esc)).await;
    assert!(!launch(&app).open);
    assert_eq!(
        app.input_overlays.len(),
        1,
        "flipping back keeps the prompt"
    );
    assert_eq!(prompt_mode(&app), PopupMode::Normal);

    press(&mut app, key(KeyCode::Tab)).await;
    assert!(launch(&app).open);
    press(&mut app, key(KeyCode::Tab)).await;
    assert!(!launch(&app).open);
}

#[tokio::test]
async fn regular_slot_prompt_flips_and_routes_settings_keys() {
    let mut app = claude_app();
    app.poll.sandbox_supported = true;
    crate::overlay::open_blank_popup(&mut app);
    app.overlay = app.input_overlays.pop().expect("prompt");

    press(&mut app, ctrl(KeyCode::Char('o'))).await;
    press(&mut app, key(KeyCode::Char('j'))).await;
    press(&mut app, key(KeyCode::Char('j'))).await;
    press(&mut app, key(KeyCode::Char(' '))).await;
    assert!(matches!(
        &app.overlay,
        OverlayState::Prompt {
            launch,
            sandbox_enabled: false,
            ..
        } if launch.open && launch.selected == 2
    ));
}

#[tokio::test]
async fn continue_prompt_has_no_settings_side() {
    let mut app = crate::app::app_test_helpers::with_session_list(1);
    let session_id = app.selected_session_id().expect("fixture session");
    crate::overlay::open_continue_popup(&mut app, session_id);
    press(&mut app, ctrl(KeyCode::Char('o'))).await;
    assert!(matches!(
        &app.overlay,
        OverlayState::Prompt { launch, .. } if !launch.open
    ));
}

// === Settings rows ============================================================

#[tokio::test]
async fn settings_rows_change_model_effort_and_sandbox() {
    let mut app = claude_app();
    app.poll.sandbox_supported = true;
    crate::overlay::open_blank_popup(&mut app);
    press(&mut app, ctrl(KeyCode::Char('o'))).await;
    assert_eq!(selected(&app), Some(LaunchSettingRow::Model));

    press(&mut app, key(KeyCode::Char('j'))).await;
    assert_eq!(selected(&app), Some(LaunchSettingRow::Effort));
    press(&mut app, key(KeyCode::Char('l'))).await;
    assert_eq!(app.selected_effort.as_deref(), Some("xhigh"));
    press(&mut app, key(KeyCode::Right)).await;
    assert_eq!(app.selected_effort.as_deref(), Some("max"));
    press(&mut app, key(KeyCode::Char('h'))).await;
    press(&mut app, key(KeyCode::Left)).await;
    assert_eq!(app.selected_effort.as_deref(), Some("high"));

    press(&mut app, key(KeyCode::Down)).await;
    assert_eq!(selected(&app), Some(LaunchSettingRow::Sandbox));
    assert!(sandbox_enabled(&app));
    press(&mut app, key(KeyCode::Char(' '))).await;
    assert!(!sandbox_enabled(&app));
    press(&mut app, key(KeyCode::Enter)).await;
    assert!(sandbox_enabled(&app));

    press(&mut app, key(KeyCode::Char('g'))).await;
    assert_eq!(selected(&app), Some(LaunchSettingRow::Model));
    press(&mut app, key(KeyCode::Enter)).await;
    assert!(matches!(
        focused_prompt(&app),
        OverlayState::Prompt { model_dropdown, .. } if model_dropdown.open
    ));
    assert!(app.needs_model_refresh);
}

#[tokio::test]
async fn backward_effort_from_the_implicit_default_steps_below_it() {
    let mut app = claude_app();
    crate::overlay::open_blank_popup(&mut app);
    press(&mut app, ctrl(KeyCode::Char('o'))).await;
    press(&mut app, key(KeyCode::Char('j'))).await;
    press(&mut app, key(KeyCode::Char('h'))).await;
    assert_eq!(app.selected_effort.as_deref(), Some("high"));
}

#[tokio::test]
async fn sandbox_row_reports_missing_daemon_support() {
    let mut app = test_app();
    crate::overlay::open_blank_popup(&mut app);
    press(&mut app, ctrl(KeyCode::Char('o'))).await;
    press(&mut app, key(KeyCode::Char('j'))).await;
    press(&mut app, key(KeyCode::Char('j'))).await;
    press(&mut app, key(KeyCode::Char(' '))).await;
    assert!(sandbox_enabled(&app), "unsupported sandbox is not toggled");
    assert!(
        app.notifications
            .iter()
            .any(|notification| notification.message.contains("Sandbox unavailable"))
    );
}

#[tokio::test]
async fn settings_side_keeps_launch_chords_and_swallows_text_tools() {
    let mut app = claude_app();
    app.poll.sandbox_supported = true;
    crate::overlay::open_blank_popup(&mut app);
    type_text(&mut app, "keep").await;
    press(&mut app, ctrl(KeyCode::Char('o'))).await;

    press(&mut app, ctrl(KeyCode::Char('e'))).await;
    assert_eq!(app.selected_effort.as_deref(), Some("xhigh"));
    press(&mut app, ctrl(KeyCode::Char('b'))).await;
    assert!(!sandbox_enabled(&app));
    // Text tools would act on hidden text; the settings side ignores them.
    press(&mut app, ctrl(KeyCode::Char('w'))).await;
    press(&mut app, key(KeyCode::Backspace)).await;
    assert_eq!(prompt_text(&app), "keep");
    assert!(launch(&app).open);

    // Ctrl+Q still closes the prompt and keeps the draft.
    press(&mut app, ctrl(KeyCode::Char('q'))).await;
    assert!(app.input_overlays.is_empty());
    assert_eq!(app.blank_draft, vec!["keep".to_string()]);
}

#[tokio::test]
async fn taskrabbit_settings_side_offers_no_manager() {
    let mut app = test_app();
    app.current_project_id = Some(Uuid::new_v4());
    crate::overlay::open_taskrabbit_popup(&mut app);
    press(&mut app, ctrl(KeyCode::Char('o'))).await;
    press(&mut app, key(KeyCode::Char('G'))).await;
    assert_eq!(selected(&app), Some(LaunchSettingRow::Sandbox));
    press(&mut app, key(KeyCode::Char(' '))).await;
    assert_eq!(launch(&app).manager, None);
}

// === Manager appointment plan =================================================

#[tokio::test]
async fn manager_row_plans_scope_and_policy_and_clears_cleanly() {
    let mut app = test_app();
    let project_id = Uuid::new_v4();
    app.current_project_id = Some(project_id);
    let group = add_container(&mut app, SessionKind::Group, "Alpha Group", None);
    let epic = add_container(&mut app, SessionKind::Epic, "Alpha Epic", Some(group));
    crate::overlay::open_blank_popup(&mut app);
    press(&mut app, ctrl(KeyCode::Char('o'))).await;
    press(&mut app, key(KeyCode::Char('G'))).await;
    assert_eq!(selected(&app), Some(LaunchSettingRow::Manager));

    press(&mut app, key(KeyCode::Char(' '))).await;
    assert_eq!(
        launch(&app).manager,
        Some(ManagerLaunchPlan {
            project_id,
            scope: ManagerLaunchScope::Project,
            preset: ManagerPolicyPreset::Execute,
        })
    );

    press(&mut app, key(KeyCode::Char('j'))).await;
    assert_eq!(selected(&app), Some(LaunchSettingRow::ManagerScope));
    press(&mut app, key(KeyCode::Char('l'))).await;
    assert_eq!(
        launch(&app).manager.map(|plan| plan.scope),
        Some(ManagerLaunchScope::Group(group))
    );
    press(&mut app, key(KeyCode::Char('l'))).await;
    assert_eq!(
        launch(&app).manager.map(|plan| plan.scope),
        Some(ManagerLaunchScope::Epic(epic))
    );
    press(&mut app, key(KeyCode::Char('l'))).await;
    assert_eq!(
        launch(&app).manager.map(|plan| plan.scope),
        Some(ManagerLaunchScope::Project)
    );
    press(&mut app, key(KeyCode::Char('h'))).await;
    assert_eq!(
        launch(&app).manager.map(|plan| plan.scope),
        Some(ManagerLaunchScope::Epic(epic))
    );

    press(&mut app, key(KeyCode::Char('j'))).await;
    assert_eq!(selected(&app), Some(LaunchSettingRow::ManagerPolicy));
    press(&mut app, key(KeyCode::Char('l'))).await;
    assert_eq!(
        launch(&app).manager.map(|plan| plan.preset),
        Some(ManagerPolicyPreset::FullProjectControl)
    );
    press(&mut app, key(KeyCode::Char('l'))).await;
    assert_eq!(
        launch(&app).manager.map(|plan| plan.preset),
        Some(ManagerPolicyPreset::Observe)
    );

    // Turning the appointment off drops its rows and keeps the selection valid.
    press(&mut app, key(KeyCode::Char('k'))).await;
    press(&mut app, key(KeyCode::Char('k'))).await;
    press(&mut app, key(KeyCode::Char(' '))).await;
    assert_eq!(launch(&app).manager, None);
    assert_eq!(selected(&app), Some(LaunchSettingRow::Manager));
    press(&mut app, key(KeyCode::Char('j'))).await;
    assert_eq!(selected(&app), Some(LaunchSettingRow::Manager));
}

#[tokio::test]
async fn manager_row_needs_a_project() {
    let mut app = test_app();
    app.current_project_id = None;
    crate::overlay::open_blank_popup(&mut app);
    press(&mut app, ctrl(KeyCode::Char('o'))).await;
    press(&mut app, key(KeyCode::Char('G'))).await;
    press(&mut app, key(KeyCode::Char(' '))).await;
    assert_eq!(launch(&app).manager, None);
    assert!(
        app.notifications
            .iter()
            .any(|notification| notification.message.contains(":projects"))
    );
}

#[tokio::test]
async fn manager_scope_choices_skip_other_projects_archived_and_ungrouped_epics() {
    let mut app = test_app();
    let project_id = Uuid::new_v4();
    app.current_project_id = Some(project_id);
    let beta = add_container(&mut app, SessionKind::Group, "Beta", None);
    let alpha = add_container(&mut app, SessionKind::Group, "alpha", None);
    let alpha_epic = add_container(&mut app, SessionKind::Epic, "Epic A", Some(alpha));
    let archived = add_container(&mut app, SessionKind::Epic, "Archived", Some(alpha));
    if let Some(state) = app.sessions.get_mut(&archived) {
        state.session.status = SessionStatus::Archived;
    }
    add_container(&mut app, SessionKind::Epic, "Loose", None);
    app.current_project_id = Some(Uuid::new_v4());
    add_container(&mut app, SessionKind::Group, "Elsewhere", None);

    assert_eq!(
        manager_scope_choices(&app, project_id),
        vec![
            ManagerLaunchScope::Project,
            ManagerLaunchScope::Group(alpha),
            ManagerLaunchScope::Epic(alpha_epic),
            ManagerLaunchScope::Group(beta),
        ]
    );
    assert_eq!(
        manager_scope_label(&app, ManagerLaunchScope::Group(alpha)),
        format!("{} alpha", crate::ui::glyphs::GROUP_CONTAINER)
    );
    assert_eq!(
        manager_scope_label(&app, ManagerLaunchScope::Project),
        "whole project"
    );
}

#[test]
fn manager_plan_builds_the_scope_editor_request_shapes() {
    let project_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let group = Uuid::new_v4();
    let epic = Uuid::new_v4();
    let plan = |scope| ManagerLaunchPlan {
        project_id,
        scope,
        preset: ManagerPolicyPreset::Execute,
    };

    let project = plan(ManagerLaunchScope::Project).scope_request(session_id, 4);
    assert_eq!(project.epic_ids, None);
    assert!(project.group_ids.is_empty());
    assert_eq!(project.scope_mode(), HarnessManagerScopeModeV1::Project);
    assert_eq!(project.expected_row_version, 4);
    assert_eq!(project.session_id, session_id);
    assert!(project.validate().is_ok());

    let grouped = plan(ManagerLaunchScope::Group(group)).scope_request(session_id, 0);
    assert_eq!(grouped.epic_ids, Some(Vec::new()));
    assert_eq!(grouped.group_ids, vec![group]);
    assert_eq!(grouped.scope_mode(), HarnessManagerScopeModeV1::Selected);
    assert!(!grouped.is_revocation());
    assert!(grouped.validate().is_ok());

    let single = plan(ManagerLaunchScope::Epic(epic)).scope_request(session_id, 0);
    assert_eq!(single.epic_ids, Some(vec![epic]));
    assert!(single.group_ids.is_empty());
    assert!(!single.is_revocation());
    assert!(single.validate().is_ok());
}

#[test]
fn launch_policies_validate_and_classify_as_their_preset() {
    for scope in [
        ManagerLaunchScope::Project,
        ManagerLaunchScope::Group(Uuid::new_v4()),
    ] {
        for preset in MANAGER_LAUNCH_PRESETS {
            let plan = ManagerLaunchPlan {
                project_id: Uuid::new_v4(),
                scope,
                preset,
            };
            let policy = plan.policy();
            assert!(policy.validate().is_ok(), "{preset:?} {scope:?}");
            assert_eq!(
                classify_manager_policy(&policy, plan.scope_mode()).preset,
                preset,
                "the policy editor shows the preset the prompt granted"
            );
        }
    }
    let execute = ManagerLaunchPlan {
        project_id: Uuid::new_v4(),
        scope: ManagerLaunchScope::Project,
        preset: ManagerPolicyPreset::Execute,
    }
    .policy();
    assert_eq!(execute.mode, ManagerOperatingModeV2::Execute);
    assert!(
        execute
            .capabilities
            .contains(&ManagerCapabilityV2::SessionCreate)
    );
    assert!(execute.max_created_sessions > 0);
    let observe = ManagerLaunchPlan {
        project_id: Uuid::new_v4(),
        scope: ManagerLaunchScope::Project,
        preset: ManagerPolicyPreset::Observe,
    }
    .policy();
    assert_eq!(observe.mode, ManagerOperatingModeV2::Monitor);
    assert!(observe.capabilities.is_empty());
}

// === Rendering ================================================================

/// Words of a rendered row with the popup's border glyphs removed.
fn row_words(row: &str) -> Vec<&str> {
    row.split_whitespace()
        .filter(|word| !word.chars().all(|c| matches!(c, '│' | '┃' | '║')))
        .collect()
}

#[tokio::test]
async fn mode_row_holds_only_the_mode_and_a_help_pointer() {
    let mut app = claude_app();
    app.poll.sandbox_supported = true;
    crate::overlay::open_blank_popup(&mut app);

    // Insert mode: `?` types text, so the row is the mode badge alone.
    let rows = render_rows(&mut app);
    assert_eq!(row_words(row_containing(&rows, " INSERT ")), vec!["INSERT"]);

    // Normal mode: the badge and the pointer to `?` help; every key lives in help.
    press(&mut app, key(KeyCode::Esc)).await;
    let rows = render_rows(&mut app);
    assert_eq!(
        row_words(row_containing(&rows, " NORMAL ")),
        vec!["NORMAL", "?", "help"]
    );

    // A half-typed command keeps `?` as its argument, so no pointer shows.
    press(&mut app, key(KeyCode::Char('d'))).await;
    let rows = render_rows(&mut app);
    assert_eq!(row_words(row_containing(&rows, " d... ")), vec!["d..."]);
}

#[tokio::test]
async fn header_shows_effort_level_sandbox_and_manager_chips() {
    let mut app = claude_app();
    app.poll.sandbox_supported = true;
    let project_id = Uuid::new_v4();
    app.current_project_id = Some(project_id);
    crate::overlay::open_blank_popup(&mut app);

    let rows = render_rows(&mut app);
    let header = row_containing(&rows, "cwd: ");
    assert!(header.contains("claude-opus-5"), "{header}");
    assert!(
        header.contains("xhigh"),
        "implicit default effort: {header}"
    );
    assert!(header.contains("⊡ sandbox"), "{header}");

    press(&mut app, ctrl(KeyCode::Char('b'))).await;
    press(&mut app, ctrl(KeyCode::Char('o'))).await;
    press(&mut app, key(KeyCode::Char('G'))).await;
    press(&mut app, key(KeyCode::Char(' '))).await;
    let rows = render_rows(&mut app);
    let header = row_containing(&rows, "cwd: ");
    assert!(header.contains("⊡ no sandbox"), "{header}");
    assert!(header.contains("★ manager · Execute"), "{header}");
}

#[tokio::test]
async fn settings_side_renders_rows_selection_and_description() {
    let mut app = claude_app();
    app.poll.sandbox_supported = true;
    app.current_project_id = Some(Uuid::new_v4());
    crate::overlay::open_blank_popup(&mut app);
    press(&mut app, ctrl(KeyCode::Char('o'))).await;

    let rows = render_rows(&mut app);
    assert!(row_containing(&rows, " SETTINGS ").contains("? help"));
    assert!(row_containing(&rows, "▸ Model").contains("claude-opus-5"));
    assert!(row_containing(&rows, "Effort").contains("xhigh (default)"));
    assert!(row_containing(&rows, "Sandbox").contains("⊡ on"));
    assert!(row_containing(&rows, "Manager").contains("★ off"));
    row_containing(&rows, LaunchSettingRow::Model.description());

    press(&mut app, key(KeyCode::Char('G'))).await;
    press(&mut app, key(KeyCode::Char(' '))).await;
    press(&mut app, key(KeyCode::Char('j'))).await;
    let rows = render_rows(&mut app);
    assert!(row_containing(&rows, "Manager").contains("★ appoint on launch"));
    assert!(row_containing(&rows, "▸ Scope").contains("‹ whole project ›"));
    assert!(row_containing(&rows, "Policy").contains("Execute"));
    row_containing(&rows, LaunchSettingRow::ManagerScope.description());
}

#[tokio::test]
async fn manager_row_names_the_manager_it_replaces() {
    let mut app = test_app();
    let project_id = Uuid::new_v4();
    app.current_project_id = Some(project_id);
    let current = Uuid::new_v4();
    let mut session =
        crate::app::app_test_helpers::baseline_session(current, SessionKind::Standard);
    session.title = Some("Standing manager".to_string());
    session.project_id = Some(project_id);
    app.upsert_session(session);
    app.manager_roster.by_project.insert(
        project_id,
        crate::app::manager_roster::ManagerRosterEntry {
            session_id: current,
            tier: crate::app::manager_roster::ManagerTier::Project,
        },
    );
    crate::overlay::open_blank_popup(&mut app);
    press(&mut app, ctrl(KeyCode::Char('o'))).await;
    press(&mut app, key(KeyCode::Char('G'))).await;
    press(&mut app, key(KeyCode::Char(' '))).await;
    let rows = render_rows(&mut app);
    assert!(row_containing(&rows, "Manager").contains("replaces Standing manager"));
}

// === Launch with a planned manager ===========================================

fn manager_config(project_id: Uuid, session_id: Uuid, row_version: i64) -> HarnessManagerConfigV1 {
    HarnessManagerConfigV1 {
        project_id,
        manager_session_id: session_id,
        current_session_id: Some(session_id),
        epic_ids: Vec::new(),
        scope_mode: HarnessManagerScopeModeV1::Project,
        selected_epic_ids: None,
        group_ids: Vec::new(),
        row_version,
        updated_at: chrono::Utc::now(),
    }
}

/// Serve `LaunchSession` on the first connection, then answer the manager
/// RPCs on the second. `refuse_scope` rejects `ConfigureHarnessManager`.
fn spawn_launch_daemon(
    listener: tokio::net::UnixListener,
    project_id: Uuid,
    returned_id: Uuid,
    refuse_scope: bool,
) -> tokio::task::JoinHandle<Vec<serde_json::Value>> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    tokio::spawn(async move {
        let mut seen = Vec::new();
        let (stream, _) = listener.accept().await.expect("launch client");
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        let request: serde_json::Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(request["method"], "LaunchSession");
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": request["id"].clone(),
            "result": {"session_id": returned_id},
        });
        writer
            .write_all(format!("{response}\n").as_bytes())
            .await
            .unwrap();
        seen.push(request);

        let (stream, _) = listener.accept().await.expect("manager client");
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            let result = match request["method"].as_str().unwrap() {
                "GetHarnessManager" | "GetHarnessManagerPolicy" => Ok(serde_json::Value::Null),
                "ConfigureHarnessManager" if refuse_scope => Err("manager_project_limit_reached"),
                "ConfigureHarnessManager" => {
                    Ok(serde_json::to_value(manager_config(project_id, returned_id, 1)).unwrap())
                }
                "ConfigureHarnessManagerPolicy" => {
                    let policy: ManagerPolicyV2 =
                        serde_json::from_value(request["params"]["policy"].clone()).unwrap();
                    Ok(serde_json::to_value(HarnessManagerPolicyConfigV2 {
                        project_id,
                        manager_session_id: returned_id,
                        scope_version: 1,
                        row_version: 1,
                        policy,
                        updated_at: chrono::Utc::now(),
                        revoked: false,
                    })
                    .unwrap())
                }
                other => panic!("unexpected manager RPC {other}"),
            };
            let response = match result {
                Ok(result) => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"].clone(),
                    "result": result,
                }),
                Err(message) => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": request["id"].clone(),
                    "error": {"code": -32000, "message": message},
                }),
            };
            writer
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
            seen.push(request);
        }
        seen
    })
}

async fn launch_blank_with_planned_manager(app: &mut App) -> crate::app::InteractiveLaunchResult {
    app.poll.connected = true;
    app.poll.authoritative_config_ready = true;
    crate::overlay::open_blank_popup(app);
    type_text(app, "run the project").await;
    press(app, ctrl(KeyCode::Char('o'))).await;
    press(app, key(KeyCode::Char('G'))).await;
    press(app, key(KeyCode::Char(' '))).await;
    // Launch straight from the settings side.
    press(app, ctrl(KeyCode::Enter)).await;
    assert!(app.interactive_launch_pending.is_some());
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        app.interactive_launch_rx.recv(),
    )
    .await
    .expect("launch result deadline")
    .expect("launch result")
}

#[tokio::test]
async fn launch_appoints_the_new_session_and_grants_the_planned_policy() {
    let temp_dir = tempfile::tempdir().expect("socket directory");
    let socket_path = temp_dir.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).expect("listener");
    let project_id = Uuid::new_v4();
    let returned_id = Uuid::new_v4();
    let server = spawn_launch_daemon(listener, project_id, returned_id, false);

    let mut app = test_app_at(socket_path);
    app.current_project_id = Some(project_id);
    let result = launch_blank_with_planned_manager(&mut app).await;
    let summary = result
        .manager
        .clone()
        .expect("a manager outcome")
        .expect("appointment succeeds");
    assert_eq!(
        summary,
        "Manager appointed for the whole project with the Execute policy"
    );
    assert!(app.apply_interactive_launch_result(result));
    assert!(
        app.input_overlays.is_empty(),
        "accepted launch closes the prompt"
    );
    assert!(
        app.notifications
            .iter()
            .any(|notification| notification.message == summary)
    );

    drop(app);
    let seen = server.await.expect("daemon");
    let methods: Vec<&str> = seen
        .iter()
        .map(|request| request["method"].as_str().unwrap())
        .collect();
    assert_eq!(
        methods,
        vec![
            "LaunchSession",
            "GetHarnessManager",
            "ConfigureHarnessManager",
            "GetHarnessManagerPolicy",
            "ConfigureHarnessManagerPolicy",
        ]
    );
    assert_eq!(seen[0]["params"]["project_id"], project_id.to_string());
    let scope = &seen[2]["params"];
    assert_eq!(scope["session_id"], returned_id.to_string());
    assert_eq!(scope["project_id"], project_id.to_string());
    assert_eq!(scope["expected_row_version"], 0);
    assert!(scope.get("epic_ids").is_none(), "project scope omits Epics");
    let policy = &seen[4]["params"];
    assert_eq!(policy["expected_scope_version"], 1);
    assert_eq!(policy["expected_policy_version"], 0);
    assert_eq!(policy["policy"]["mode"], "execute");
}

#[tokio::test]
async fn refused_appointment_still_lands_the_launch_and_names_the_retry() {
    let temp_dir = tempfile::tempdir().expect("socket directory");
    let socket_path = temp_dir.path().join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).expect("listener");
    let project_id = Uuid::new_v4();
    let returned_id = Uuid::new_v4();
    let server = spawn_launch_daemon(listener, project_id, returned_id, true);

    let mut app = test_app_at(socket_path);
    app.current_project_id = Some(project_id);
    let result = launch_blank_with_planned_manager(&mut app).await;
    assert_eq!(result.accepted, Ok(returned_id));
    let error = result
        .manager
        .clone()
        .expect("a manager outcome")
        .expect_err("appointment refused");
    assert!(error.contains("not appointed manager"), "{error}");
    assert!(error.contains("manager_project_limit_reached"), "{error}");
    assert!(error.contains(":manager appoint"), "{error}");
    assert!(app.apply_interactive_launch_result(result));
    assert!(app.input_overlays.is_empty());
    assert!(
        app.notifications
            .iter()
            .any(|notification| notification.message == error)
    );
    drop(app);
    server.await.expect("daemon");
}
