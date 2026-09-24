//! Label picker overlay input handling.

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::label_form::open_label_form_new;
use super::list;

/// Open the label picker overlay.
pub fn open_label_picker(app: &mut App) {
    app.overlay = OverlayState::LabelPicker {
        filter: String::new(),
        selected_index: 0,
    };
}

/// Handle keys in the label picker overlay.
pub(super) async fn handle_label_picker_key(app: &mut App, key: KeyEvent) {
    let (filter, selected_index) = match &app.overlay {
        OverlayState::LabelPicker {
            filter,
            selected_index,
        } => (filter.clone(), *selected_index),
        _ => return,
    };

    let filtered_count = get_label_picker_filtered_count(app, &filter);

    // Ctrl+n = create new label
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('n') {
        open_label_form_new(app);
        return;
    }

    // Ctrl+e = edit highlighted label
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('e') {
        edit_highlighted_label(app, &filter, selected_index);
        return;
    }

    // Ctrl+d = delete highlighted label
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
        delete_highlighted_label(app, &filter, selected_index).await;
        return;
    }

    // Shared list navigation (j/k/g/G)
    if let OverlayState::LabelPicker { selected_index, .. } = &mut app.overlay {
        if list::handle_list_nav_key(selected_index, filtered_count, &key) {
            return;
        }
    }

    match key.code {
        KeyCode::Enter => {
            assign_selected_label(app, &filter, selected_index).await;
            app.overlay = OverlayState::None;
        }
        KeyCode::Esc => {
            app.overlay = OverlayState::None;
        }
        KeyCode::Char(c) => {
            if let OverlayState::LabelPicker {
                filter,
                selected_index,
                ..
            } = &mut app.overlay
            {
                filter.push(c);
                *selected_index = 0;
            }
        }
        KeyCode::Backspace => {
            if let OverlayState::LabelPicker {
                filter,
                selected_index,
                ..
            } = &mut app.overlay
            {
                filter.pop();
                *selected_index = 0;
            }
        }
        _ => {}
    }
}

fn get_label_picker_filtered_count(app: &App, filter: &str) -> usize {
    let filter_lower = filter.to_lowercase();
    let mut count = 0;
    for label in &app.labels {
        if filter.is_empty() || label.name.to_lowercase().contains(&filter_lower) {
            count += 1;
        }
    }
    // "(none)" entry to unassign
    if filter.is_empty() || "(none)".contains(&filter_lower) {
        count += 1;
    }
    count
}

/// Resolve the selected index to a label id (None = unassign).
fn resolve_selection(app: &App, filter: &str, index: usize) -> Option<uuid::Uuid> {
    let filter_lower = filter.to_lowercase();
    let mut entries: Vec<Option<uuid::Uuid>> = Vec::new();

    let mut sorted = app.labels.clone();
    sorted.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    for label in &sorted {
        if filter.is_empty() || label.name.to_lowercase().contains(&filter_lower) {
            entries.push(Some(label.id));
        }
    }
    // "(none)" = remove from label
    if filter.is_empty() || "(none)".contains(&filter_lower) {
        entries.push(None);
    }

    entries.get(index).copied().flatten()
}

async fn assign_selected_label(app: &mut App, filter: &str, index: usize) {
    let group_id = resolve_selection(app, filter, index);
    // Get focused session ID
    let session_id = match app.selected_session_id() {
        Some(id) => id,
        None => {
            app.notify("No session selected");
            return;
        }
    };

    if let Err(e) = app.client.update_session_label(session_id, group_id).await {
        app.notify_error(format!("Label assignment failed: {}", e));
        return;
    }

    // Update local state
    if let Some(state) = app.sessions.get_mut(&session_id) {
        state.session.group_id = group_id;
    }

    let label_name = match group_id {
        Some(gid) => app
            .labels
            .iter()
            .find(|g| g.id == gid)
            .map(|g| g.name.clone())
            .unwrap_or_else(|| "unknown".to_string()),
        None => "none".to_string(),
    };
    app.notify_success(format!("Label: {}", label_name));
    app.invalidate_card_cache();
}

fn edit_highlighted_label(app: &mut App, filter: &str, selected_index: usize) {
    let label_id = match resolve_selection(app, filter, selected_index) {
        Some(id) => id,
        None => return, // "(none)" selected
    };
    let label = match app.labels.iter().find(|g| g.id == label_id).cloned() {
        Some(g) => g,
        None => return,
    };
    let color_index = super::label_form::LABEL_COLORS
        .iter()
        .position(|(_, hex)| *hex == label.color)
        .unwrap_or(0);
    app.overlay = OverlayState::LabelForm {
        focused_field: 0,
        name: label.name,
        description: label.description.unwrap_or_default(),
        color_index,
        editing_id: Some(label.id),
    };
}

async fn delete_highlighted_label(app: &mut App, filter: &str, selected_index: usize) {
    let label_id = match resolve_selection(app, filter, selected_index) {
        Some(id) => id,
        None => return,
    };
    if let Err(e) = app.client.delete_label(label_id).await {
        app.notify_error(format!("Delete label failed: {}", e));
        return;
    }
    let name = app
        .labels
        .iter()
        .find(|g| g.id == label_id)
        .map(|g| g.name.clone())
        .unwrap_or_default();
    app.labels.retain(|g| g.id != label_id);
    // Clear group_id from local sessions
    for (_, state) in app.sessions.iter_mut() {
        if state.session.group_id == Some(label_id) {
            state.session.group_id = None;
        }
    }
    app.notify_success(format!("Deleted label: {}", name));
    if let OverlayState::LabelPicker { selected_index, .. } = &mut app.overlay {
        *selected_index = 0;
    }
}
