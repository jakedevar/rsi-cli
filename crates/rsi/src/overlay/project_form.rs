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
    };
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
    };

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

/// Handle keys in the project form overlay.
pub(super) async fn handle_project_form_key(app: &mut App, key: KeyEvent) {
    let (focused_field, editing_id) = match &app.overlay {
        OverlayState::ProjectForm {
            focused_field,
            editing_id,
            ..
        } => (*focused_field, *editing_id),
        _ => return,
    };

    match key.code {
        // Shift+Tab: cycle fields backward (BackTab for legacy, Tab+Shift for DISAMBIGUATE)
        KeyCode::BackTab => {
            if let OverlayState::ProjectForm { focused_field, .. } = &mut app.overlay {
                *focused_field = (*focused_field + 2) % 3;
            }
        }
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
            if let OverlayState::ProjectForm { focused_field, .. } = &mut app.overlay {
                *focused_field = (*focused_field + 2) % 3;
            }
        }
        // Tab: cycle fields forward
        KeyCode::Tab => {
            if let OverlayState::ProjectForm { focused_field, .. } = &mut app.overlay {
                *focused_field = (*focused_field + 1) % 3;
            }
        }
        // Enter: save
        KeyCode::Enter => {
            submit_project_form(app, editing_id).await;
        }
        // Esc: cancel, go back to picker
        KeyCode::Esc => {
            open_project_picker(app, crate::types::ProjectPickerContext::GlobalFilter);
        }
        // Typing: update the focused field
        KeyCode::Char(c) => {
            if focused_field == 2 {
                // Color field: cycle through palette on any keystroke
                if let OverlayState::ProjectForm { color_index, .. } = &mut app.overlay {
                    *color_index = (*color_index + 1) % PROJECT_COLORS.len();
                }
            } else {
                // Text fields
                if let OverlayState::ProjectForm { name, path, .. } = &mut app.overlay {
                    match focused_field {
                        0 => name.push(c),
                        1 => path.push(c),
                        _ => {}
                    }
                }
            }
        }
        // Backspace: delete last char in focused text field
        KeyCode::Backspace => {
            if focused_field < 2
                && let OverlayState::ProjectForm { name, path, .. } = &mut app.overlay
            {
                match focused_field {
                    0 => {
                        name.pop();
                    }
                    1 => {
                        path.pop();
                    }
                    _ => {}
                }
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

    // Go back to picker
    open_project_picker(app, crate::types::ProjectPickerContext::GlobalFilter);
}
