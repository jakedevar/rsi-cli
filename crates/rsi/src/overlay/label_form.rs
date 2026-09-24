//! Label form overlay input handling (create/edit).

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::label_picker::open_label_picker;

/// Catppuccin color palette for labels: (name, hex).
pub const LABEL_COLORS: &[(&str, &str)] = &[
    ("Mauve", "#cba6f7"),
    ("Green", "#a6e3a1"),
    ("Peach", "#fab387"),
    ("Blue", "#89b4fa"),
    ("Red", "#f38ba8"),
    ("Yellow", "#f9e2af"),
    ("Teal", "#94e2d5"),
    ("Pink", "#f5c2e7"),
];

/// Open a new label form (from Ctrl+n in picker).
pub(super) fn open_label_form_new(app: &mut App) {
    app.overlay = OverlayState::LabelForm {
        focused_field: 0,
        name: String::new(),
        description: String::new(),
        color_index: 0,
        editing_id: None,
    };
}

/// Handle keys in the label form overlay.
pub(super) async fn handle_label_form_key(app: &mut App, key: KeyEvent) {
    let (focused_field, editing_id) = match &app.overlay {
        OverlayState::LabelForm {
            focused_field,
            editing_id,
            ..
        } => (*focused_field, *editing_id),
        _ => return,
    };

    match key.code {
        KeyCode::BackTab => {
            if let OverlayState::LabelForm { focused_field, .. } = &mut app.overlay {
                *focused_field = (*focused_field + 2) % 3;
            }
        }
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
            if let OverlayState::LabelForm { focused_field, .. } = &mut app.overlay {
                *focused_field = (*focused_field + 2) % 3;
            }
        }
        KeyCode::Tab => {
            if let OverlayState::LabelForm { focused_field, .. } = &mut app.overlay {
                *focused_field = (*focused_field + 1) % 3;
            }
        }
        KeyCode::Enter => {
            submit_label_form(app, editing_id).await;
        }
        KeyCode::Esc => {
            open_label_picker(app);
        }
        KeyCode::Char(c) => {
            if focused_field == 2 {
                if let OverlayState::LabelForm { color_index, .. } = &mut app.overlay {
                    *color_index = (*color_index + 1) % LABEL_COLORS.len();
                }
            } else if let OverlayState::LabelForm {
                name, description, ..
            } = &mut app.overlay
            {
                match focused_field {
                    0 => name.push(c),
                    1 => description.push(c),
                    _ => {}
                }
            }
        }
        KeyCode::Backspace => {
            if focused_field < 2 {
                if let OverlayState::LabelForm {
                    name, description, ..
                } = &mut app.overlay
                {
                    match focused_field {
                        0 => {
                            name.pop();
                        }
                        1 => {
                            description.pop();
                        }
                        _ => {}
                    }
                }
            }
        }
        KeyCode::Left if focused_field == 2 => {
            if let OverlayState::LabelForm { color_index, .. } = &mut app.overlay {
                *color_index = (*color_index + LABEL_COLORS.len() - 1) % LABEL_COLORS.len();
            }
        }
        KeyCode::Right if focused_field == 2 => {
            if let OverlayState::LabelForm { color_index, .. } = &mut app.overlay {
                *color_index = (*color_index + 1) % LABEL_COLORS.len();
            }
        }
        _ => {}
    }
}

async fn submit_label_form(app: &mut App, editing_id: Option<uuid::Uuid>) {
    let (name, description_str, color_index) = match &app.overlay {
        OverlayState::LabelForm {
            name,
            description,
            color_index,
            ..
        } => (name.clone(), description.clone(), *color_index),
        _ => return,
    };

    let name = name.trim().to_string();
    if name.is_empty() {
        app.notify("Label name required");
        return;
    }

    let color = LABEL_COLORS[color_index].1;
    let description = if description_str.trim().is_empty() {
        None
    } else {
        Some(description_str.trim().to_string())
    };

    match editing_id {
        Some(id) => {
            match app
                .client
                .update_label(id, Some(&name), description.as_deref(), Some(color))
                .await
            {
                Ok(_label) => {
                    app.notify_success(format!("Updated label: {}", name));
                }
                Err(e) => {
                    app.notify_error(format!("Update failed: {}", e));
                    return;
                }
            }
        }
        None => {
            match app
                .client
                .create_label(
                    &name,
                    description.as_deref(),
                    app.current_project_id,
                    Some(color),
                )
                .await
            {
                Ok(_label) => {
                    app.notify_success(format!("Created label: {}", name));
                }
                Err(e) => {
                    app.notify_error(format!("Create failed: {}", e));
                    return;
                }
            }
        }
    }

    // Refresh labels and go back to picker
    if let Ok(labels) = app.client.list_labels().await {
        app.update_labels(labels);
    }
    open_label_picker(app);
}
