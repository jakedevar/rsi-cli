//! Project picker overlay input handling.

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::list;
use super::project_form::{open_project_form_edit, open_project_form_new};

/// Open the project picker overlay.
pub fn open_project_picker(app: &mut App, context: crate::types::ProjectPickerContext) {
    app.overlay = OverlayState::ProjectPicker {
        filter: String::new(),
        selected_index: 0,
        context,
    };
}

/// In reassign mode, resolve the selected entry to a project_id (None = unassigned).
fn resolve_reassign_selection(app: &App, filter: &str, index: usize) -> Option<uuid::Uuid> {
    let filter_lower = filter.to_lowercase();
    let mut entries: Vec<Option<uuid::Uuid>> = Vec::new();

    // Projects (sorted by name) — NO "All" entry
    let mut sorted_projects = app.projects.clone();
    sorted_projects.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    for project in &sorted_projects {
        if filter.is_empty() || project.name.to_lowercase().contains(&filter_lower) {
            entries.push(Some(project.id));
        }
    }

    // "(unassigned)" = None
    if filter.is_empty() || "(unassigned)".to_lowercase().contains(&filter_lower) {
        entries.push(None);
    }

    entries.get(index).copied().flatten()
}

/// Handle keys in the project picker overlay.
pub(super) async fn handle_project_picker_key(app: &mut App, key: KeyEvent) {
    // Get current state
    let (filter, selected_index, context) = match &app.overlay {
        OverlayState::ProjectPicker {
            filter,
            selected_index,
            context,
        } => (filter.clone(), *selected_index, context.clone()),
        _ => return,
    };

    // Calculate filtered entry count for bounds checking
    let reassign_mode = matches!(
        context,
        crate::types::ProjectPickerContext::SessionReassign(_)
    );
    let filtered_count = get_project_picker_filtered_count(app, &filter, reassign_mode);

    // Ctrl+n = create new project
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('n') {
        open_project_form_new(app);
        return;
    }

    // Ctrl+e = edit highlighted project
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('e') {
        open_project_form_edit(app, &filter, selected_index, reassign_mode).await;
        return;
    }

    // Ctrl+d = delete highlighted project
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d') {
        delete_highlighted_project(app, &filter, selected_index, reassign_mode).await;
        return;
    }

    // Shared list navigation (j/k/g/G)
    if let OverlayState::ProjectPicker { selected_index, .. } = &mut app.overlay {
        if list::handle_list_nav_key(selected_index, filtered_count, &key) {
            return;
        }
    }

    match key.code {
        // Select and close
        KeyCode::Enter => {
            match &context {
                crate::types::ProjectPickerContext::GlobalFilter => {
                    select_project_picker_entry(app, &filter, selected_index);
                }
                crate::types::ProjectPickerContext::SessionReassign(session_id) => {
                    let session_id = *session_id;
                    let project_id = resolve_reassign_selection(app, &filter, selected_index);
                    app.reassign_session_project(session_id, project_id).await;
                    app.overlay = OverlayState::None;
                    return;
                }
            }
            app.overlay = OverlayState::None;
        }
        // Cancel
        KeyCode::Esc => {
            app.overlay = OverlayState::None;
        }
        // Typing to filter
        KeyCode::Char(c) => {
            if let OverlayState::ProjectPicker {
                filter,
                selected_index,
                ..
            } = &mut app.overlay
            {
                filter.push(c);
                *selected_index = 0; // Reset selection when filter changes
            }
        }
        // Backspace in filter
        KeyCode::Backspace => {
            if let OverlayState::ProjectPicker {
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

/// Calculate the number of filtered entries in the project picker.
fn get_project_picker_filtered_count(app: &App, filter: &str, reassign_mode: bool) -> usize {
    let filter_lower = filter.to_lowercase();
    let mut count = 0;

    if reassign_mode {
        // Reassign mode: projects + "(unassigned)"
        for project in &app.projects {
            if filter.is_empty() || project.name.to_lowercase().contains(&filter_lower) {
                count += 1;
            }
        }
        if filter.is_empty() || "(unassigned)".to_lowercase().contains(&filter_lower) {
            count += 1;
        }
    } else {
        // Workspace mode: "all projects" always at top, then projects
        if filter.is_empty() || "all projects".contains(&filter_lower) {
            count += 1;
        }
        for project in &app.projects {
            if filter.is_empty() || project.name.to_lowercase().contains(&filter_lower) {
                count += 1;
            }
        }
    }

    count
}

/// Select the entry at the given index in the filtered project picker list.
/// In GlobalFilter context: index 0 is always "all projects"; subsequent entries
/// are projects sorted by name. Selecting "all projects" switches to or opens
/// a global (project_id: None) workspace tab.
fn select_project_picker_entry(app: &mut App, filter: &str, index: usize) {
    let filter_lower = filter.to_lowercase();

    // "all projects" is always the first entry unless filtered out
    let has_all = filter.is_empty() || "all projects".contains(&filter_lower);

    if has_all && index == 0 {
        app.open_all_projects_workspace();
        return;
    }

    // Offset into projects list (skip the "all" slot if it was present)
    let project_index = if has_all { index - 1 } else { index };

    let mut sorted_projects = app.projects.clone();
    sorted_projects.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    let filtered: Vec<&rsi_common::types::Project> = sorted_projects
        .iter()
        .filter(|p| filter.is_empty() || p.name.to_lowercase().contains(&filter_lower))
        .collect();

    let Some(project) = filtered.get(project_index) else {
        return;
    };
    let project_id = project.id;

    // Switch to existing workspace tab for this project, or open a new one
    if let Some(idx) = app
        .tabs
        .iter()
        .position(|t| t.project_id == Some(project_id))
    {
        app.active_tab = idx;
        app.sync_project_filter();
        return;
    }

    app.open_project_workspace(project_id);
}

/// Get the project at the highlighted index.
/// In GlobalFilter (workspace) mode index 0 is the "all projects" entry — returns
/// None for it so Ctrl+e (edit) and Ctrl+d (delete) are no-ops on that row.
pub(super) fn get_highlighted_project<'a>(
    app: &'a App,
    filter: &str,
    selected_index: usize,
    reassign_mode: bool,
) -> Option<&'a rsi_common::types::Project> {
    let filter_lower = filter.to_lowercase();

    let mut sorted_projects = app.projects.clone();
    sorted_projects.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    let filtered_projects: Vec<&rsi_common::types::Project> = sorted_projects
        .iter()
        .filter(|p| filter.is_empty() || p.name.to_lowercase().contains(&filter_lower))
        .collect();

    let project_index = if reassign_mode {
        // Reassign mode has no "all projects" prefix entry
        selected_index
    } else {
        // Workspace mode: "all projects" occupies index 0 when not filtered out
        let has_all = filter.is_empty() || "all projects".contains(&filter_lower);
        if has_all {
            if selected_index == 0 {
                return None; // "all projects" row — no project to edit/delete
            }
            selected_index - 1
        } else {
            selected_index
        }
    };

    let project_id = filtered_projects.get(project_index).map(|p| p.id)?;
    app.projects.iter().find(|p| p.id == project_id)
}

/// Delete the highlighted project (from Ctrl+d in picker).
async fn delete_highlighted_project(
    app: &mut App,
    filter: &str,
    selected_index: usize,
    reassign_mode: bool,
) {
    let project_id = match get_highlighted_project(app, filter, selected_index, reassign_mode) {
        Some(p) => p.id,
        None => return, // Can't delete "all projects" or "unassigned"
    };

    if let Err(e) = app.client.delete_project(project_id).await {
        app.notify_error(format!("Delete project failed: {}", e));
        return;
    }

    // Remove locally
    let name = app
        .projects
        .iter()
        .find(|p| p.id == project_id)
        .map(|p| p.name.clone())
        .unwrap_or_default();
    app.projects.retain(|p| p.id != project_id);

    // Clear filter if we deleted the active project
    if app.current_project_id == Some(project_id) {
        app.set_project_filter(None);
    }

    app.notify_success(format!("Deleted project: {}", name));

    // Reset selection in picker
    if let OverlayState::ProjectPicker { selected_index, .. } = &mut app.overlay {
        *selected_index = 0;
    }
}
