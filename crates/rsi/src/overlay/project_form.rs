//! Project form overlay input handling (create/edit).

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::project_picker::open_project_picker;

/// Catppuccin color palette for projects: (name, hex).
pub const PROJECT_COLORS: &[(&str, &str)] = &[
    ("Blue", "#89b4fa"),
    ("Green", "#a6e3a1"),
    ("Peach", "#fab387"),
    ("Mauve", "#cba6f7"),
    ("Red", "#f38ba8"),
    ("Yellow", "#f9e2af"),
    ("Teal", "#94e2d5"),
    ("Pink", "#f5c2e7"),
];

/// Open a new project form (from Ctrl+n in picker).
pub(super) fn open_project_form_new(app: &mut App) {
    app.overlay = OverlayState::ProjectForm {
        focused_field: 0,
        name: String::new(),
        path: String::new(),
        color_index: 0,
        editing_id: None,
        text: Default::default(),
    };
    sync_project_text(app);
}

/// Open an edit form for the highlighted project (from Ctrl+e in picker).
pub(super) async fn open_project_form_edit(
    app: &mut App,
    filter: &str,
    selected_index: usize,
    reassign_mode: bool,
) {
    // Clone project data before mutating overlay
    let project_data =
        super::project_picker::get_highlighted_project(app, filter, selected_index, reassign_mode)
            .cloned();
    let project = match project_data {
        Some(p) => p,
        None => return, // "All" or "unassigned" selected -- can't edit
    };

    let color_index = PROJECT_COLORS
        .iter()
        .position(|(_, hex)| *hex == project.color)
        .unwrap_or(0);

    let path_str = project
        .path
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    let project_id = project.id;

    app.overlay = OverlayState::ProjectForm {
        focused_field: 0,
        name: project.name,
        path: path_str,
        color_index,
        editing_id: Some(project_id),
        text: Default::default(),
    };
    sync_project_text(app);

    // Fetch FLYWHEEL.md status for this project (best-effort, non-blocking on error)
    if project.path.is_some() {
        match app.client.get_project_workflow(project_id).await {
            Ok(status) => {
                app.workflow_statuses.insert(project_id, status);
            }
            Err(e) => {
                tracing::debug!(
                    project_id = %project_id,
                    error = %e,
                    "Failed to fetch project workflow status for form"
                );
            }
        }
    }
}

/// How the project form's text fields are edited: one line each, wrapped on
/// screen, no length limit.
const PROJECT_FIELD: crate::form_text::FieldSpec = crate::form_text::FieldSpec::SINGLE_LINE;

/// Point the form's text surface at its focused field (or drop it on the
/// color field). A surface already showing that field's text is kept.
pub(crate) fn sync_project_text(app: &mut App) {
    let standard = app.standard_editing();
    if let OverlayState::ProjectForm {
        focused_field,
        name,
        path,
        text,
        ..
    } = &mut app.overlay
    {
        match *focused_field {
            0 => text.load(0, name, PROJECT_FIELD, standard),
            1 => text.load(1, path, PROJECT_FIELD, standard),
            _ => text.clear(),
        }
    }
}

/// Route a key into the focused text field. Returns whether the field took it.
fn project_text_key(app: &mut App, key: KeyEvent) -> bool {
    use crate::form_text::TextKey;

    let standard = app.standard_editing();
    let OverlayState::ProjectForm {
        focused_field,
        name,
        path,
        text,
        ..
    } = &mut app.overlay
    else {
        return false;
    };
    if *focused_field > 1 || matches!(key.code, KeyCode::Tab | KeyCode::BackTab | KeyCode::Enter) {
        return false;
    }
    let field = *focused_field;
    let value = if field == 0 { &mut *name } else { &mut *path };
    text.load(field, value, PROJECT_FIELD, standard);
    // Esc at rest in normal mode cancels the form, as in the input modal.
    if key.code == KeyCode::Esc && !text.consumes_esc(standard) {
        return false;
    }
    match text.handle_key(key, PROJECT_FIELD, standard) {
        TextKey::Ignored => false,
        TextKey::Moved | TextKey::Refused => true,
        TextKey::Edited => {
            if let Some(content) = text.content() {
                *value = content;
            }
            true
        }
    }
}

/// Handle keys in the project form overlay.
pub(super) async fn handle_project_form_key(app: &mut App, key: KeyEvent) {
    let editing_id = match &app.overlay {
        OverlayState::ProjectForm { editing_id, .. } => *editing_id,
        _ => return,
    };

    if project_text_key(app, key) {
        return;
    }
    let focused_field = match &app.overlay {
        OverlayState::ProjectForm { focused_field, .. } => *focused_field,
        _ => return,
    };

    match key.code {
        // Shift+Tab: cycle fields backward (BackTab for legacy, Tab+Shift for DISAMBIGUATE)
        KeyCode::BackTab => {
            if let OverlayState::ProjectForm { focused_field, .. } = &mut app.overlay {
                *focused_field = (*focused_field + 2) % 3;
            }
            sync_project_text(app);
        }
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
            if let OverlayState::ProjectForm { focused_field, .. } = &mut app.overlay {
                *focused_field = (*focused_field + 2) % 3;
            }
            sync_project_text(app);
        }
        // Tab: cycle fields forward
        KeyCode::Tab => {
            if let OverlayState::ProjectForm { focused_field, .. } = &mut app.overlay {
                *focused_field = (*focused_field + 1) % 3;
            }
            sync_project_text(app);
        }
        // Enter: save
        KeyCode::Enter => {
            submit_project_form(app, editing_id).await;
        }
        // Esc: cancel, go back to picker
        KeyCode::Esc => {
            open_project_picker(app, crate::types::ProjectPickerContext::GlobalFilter);
        }
        // Typing on the color field: cycle through the palette on any keystroke
        KeyCode::Char(_) if focused_field == 2 => {
            if let OverlayState::ProjectForm { color_index, .. } = &mut app.overlay {
                *color_index = (*color_index + 1) % PROJECT_COLORS.len();
            }
        }
        // Left/Right on color field: cycle colors
        KeyCode::Left if focused_field == 2 => {
            if let OverlayState::ProjectForm { color_index, .. } = &mut app.overlay {
                *color_index = (*color_index + PROJECT_COLORS.len() - 1) % PROJECT_COLORS.len();
            }
        }
        KeyCode::Right if focused_field == 2 => {
            if let OverlayState::ProjectForm { color_index, .. } = &mut app.overlay {
                *color_index = (*color_index + 1) % PROJECT_COLORS.len();
            }
        }
        _ => {}
    }
}

/// Submit the project form (create or update).
async fn submit_project_form(app: &mut App, editing_id: Option<uuid::Uuid>) {
    let (name, path_str, color_index) = match &app.overlay {
        OverlayState::ProjectForm {
            name,
            path,
            color_index,
            ..
        } => (name.clone(), path.clone(), *color_index),
        _ => return,
    };

    let name = name.trim().to_string();
    if name.is_empty() {
        app.notify("Project name required");
        return;
    }

    let color = PROJECT_COLORS[color_index].1;
    let path = if path_str.trim().is_empty() {
        None
    } else {
        let trimmed = path_str.trim();
        let expanded = if trimmed.starts_with("~/") || trimmed == "~" {
            dirs::home_dir()
                .map(|h| h.join(trimmed.strip_prefix("~/").unwrap_or("")))
                .unwrap_or_else(|| std::path::PathBuf::from(trimmed))
        } else {
            std::path::PathBuf::from(trimmed)
        };
        Some(expanded)
    };

    match editing_id {
        Some(id) => {
            // Update existing project
            match app
                .client
                .update_project(id, Some(&name), path.as_deref(), None, Some(color))
                .await
            {
                Ok(_project) => {
                    app.notify_success(format!("Updated project: {}", name));
                }
                Err(e) => {
                    app.notify_error(format!("Update failed: {}", e));
                    return;
                }
            }
        }
        None => {
            // Create new project
            match app
                .client
                .create_project(&name, path.as_deref(), None, Some(color))
                .await
            {
                Ok(_project) => {
                    app.notify_success(format!("Created project: {}", name));
                }
                Err(e) => {
                    app.notify_error(format!("Create failed: {}", e));
                    return;
                }
            }
        }
    }

    // The daemon publishes no event for an operator create or edit, and the
    // poll can lag (or not run on an install with no sessions): reload the
    // list so the picker below shows the project just saved.
    app.trigger_projects_fetch_immediate().await;

    // Go back to picker
    open_project_picker(app, crate::types::ProjectPickerContext::GlobalFilter);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::app_test_helpers::with_session_list;
    use ratatui::{Terminal, backend::TestBackend};

    async fn keys(app: &mut App, input: &str) {
        let mut rest = input;
        while !rest.is_empty() {
            let (code, used) = if rest.starts_with("<Esc>") {
                (KeyCode::Esc, 5)
            } else if rest.starts_with("<Tab>") {
                (KeyCode::Tab, 5)
            } else {
                let c = rest.chars().next().expect("a character");
                (KeyCode::Char(c), c.len_utf8())
            };
            handle_project_form_key(app, KeyEvent::new(code, KeyModifiers::NONE)).await;
            rest = &rest[used..];
        }
    }

    fn form(app: &App) -> (&str, &str, usize) {
        match &app.overlay {
            OverlayState::ProjectForm {
                name,
                path,
                focused_field,
                ..
            } => (name.as_str(), path.as_str(), *focused_field),
            _ => panic!("the project form is open"),
        }
    }

    fn rows(app: &mut App, width: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, 40)).expect("test terminal");
        terminal
            .draw(|frame| crate::ui::overlay::render_overlay(frame, frame.area(), app))
            .expect("render the project form");
        let buffer = terminal.backend().buffer();
        (0..40)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[tokio::test]
    async fn a_new_project_form_edits_name_and_path_in_place() {
        let mut app = with_session_list(0);
        open_project_form_new(&mut app);
        // Opens in insert mode at the end of the (empty) name.
        keys(&mut app, "Rsi tools<Esc>0wciwkit<Esc>").await;
        assert_eq!(form(&app), ("Rsi kit", "", 0));
        keys(&mut app, "u").await;
        assert_eq!(form(&app).0, "Rsi tools");
        keys(&mut app, "<Tab>/home/me/rsi<Esc>0x").await;
        assert_eq!(form(&app), ("Rsi tools", "home/me/rsi", 1));
        // Esc at rest in normal mode cancels the form to the picker.
        keys(&mut app, "<Esc>").await;
        assert!(!matches!(app.overlay, OverlayState::ProjectForm { .. }));
    }

    #[tokio::test]
    async fn an_edit_form_keeps_the_stored_text_and_edits_it_in_place() {
        let mut app = with_session_list(0);
        app.overlay = OverlayState::ProjectForm {
            focused_field: 0,
            name: "Old name".to_string(),
            path: "/srv/app".to_string(),
            color_index: 0,
            editing_id: Some(uuid::Uuid::new_v4()),
            text: Default::default(),
        };
        sync_project_text(&mut app);
        keys(&mut app, "<Esc>0wciwtitle<Esc>").await;
        assert_eq!(form(&app), ("Old title", "/srv/app", 0));
        keys(&mut app, "<Tab>").await;
        keys(&mut app, "<Esc>0lllihello<Esc>").await;
        assert_eq!(form(&app).1, "/srhellov/app");
    }

    #[tokio::test]
    async fn a_pasted_path_goes_in_at_the_cursor_on_one_line() {
        let mut app = with_session_list(0);
        open_project_form_new(&mut app);
        keys(&mut app, "<Tab>").await;
        assert!(crate::overlay::try_paste_text_overlay(
            &mut app,
            "/home/me/one\n/two"
        ));
        let (_, path, field) = form(&app);
        assert_eq!(field, 1);
        assert_eq!(path, "/home/me/one /two");
    }

    #[tokio::test]
    async fn a_long_name_and_path_wrap_inside_the_form_with_the_mode_shown() {
        let mut app = with_session_list(0);
        open_project_form_new(&mut app);
        let long_name = "name ".repeat(30) + "NAMEEND";
        keys(&mut app, &long_name).await;
        for width in [120_u16, 200] {
            let rows = rows(&mut app, width);
            assert!(rows.iter().any(|row| row.contains("Name: name")));
            assert!(
                rows.iter().any(|row| row.contains("NAMEEND")),
                "{width} cols: the end of a long name is in view"
            );
            assert!(rows.iter().any(|row| row.contains("INSERT")));
        }
        keys(&mut app, "<Tab>").await;
        let long_path = "/segment".repeat(20) + "/PATHEND";
        keys(&mut app, &long_path).await;
        let rows = rows(&mut app, 120);
        assert!(rows.iter().any(|row| row.contains("Path: /segment")));
        assert!(rows.iter().any(|row| row.contains("PATHEND")));
        assert!(
            rows.iter().any(|row| row.contains("Color:")),
            "the color row stays below the wrapped fields"
        );
        keys(&mut app, "<Esc>").await;
        let rows = rows_after_esc(&mut app);
        assert!(rows.iter().any(|row| row.contains("NORMAL")));
    }

    fn rows_after_esc(app: &mut App) -> Vec<String> {
        rows(app, 120)
    }
}
