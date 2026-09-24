//! File explorer overlay input handling.

use std::path::Path;

use crate::app::App;
use crate::types::{FileExplorerEntry, OverlayState};
use crossterm::event::{KeyCode, KeyEvent};

use super::list;

/// Read a single directory level and return sorted entries (dirs first, then files, alphabetical).
/// Hidden files (starting with `.`) are excluded unless `show_hidden` is true.
pub(crate) fn read_directory(
    dir: &Path,
    depth: usize,
    show_hidden: bool,
) -> Vec<FileExplorerEntry> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut dirs = Vec::new();
    let mut files = Vec::new();

    for entry in read_dir.flatten() {
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();

        if !show_hidden && name.starts_with('.') {
            continue;
        }

        if path.is_dir() {
            dirs.push(FileExplorerEntry::Directory {
                path,
                depth,
                expanded: false,
            });
        } else {
            files.push(FileExplorerEntry::File { path, depth });
        }
    }

    // Sort alphabetically (case-insensitive) within each group
    let sort_key = |e: &FileExplorerEntry| {
        e.path()
            .file_name()
            .unwrap_or_default()
            .to_ascii_lowercase()
    };
    dirs.sort_by_key(|e| sort_key(e));
    files.sort_by_key(|e| sort_key(e));

    dirs.extend(files);
    dirs
}

/// Open the file explorer overlay rooted at the current project's directory.
pub fn open_file_explorer(app: &mut App) {
    let root = match app.current_project().and_then(|p| p.path.clone()) {
        Some(path) => path,
        None => {
            app.notify("No project selected — file explorer requires an active project");
            return;
        }
    };

    let entries = read_directory(&root, 0, false);

    app.overlay = OverlayState::FileExplorer {
        root,
        entries,
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
}

pub(super) fn handle_file_explorer_key(app: &mut App, key: KeyEvent) {
    let total = if let OverlayState::FileExplorer { entries, .. } = &app.overlay {
        entries.len()
    } else {
        return;
    };

    // If finder is active, route all keys through finder handler
    if let OverlayState::FileExplorer {
        finder_active: true,
        ..
    } = &app.overlay
    {
        handle_finder_key(app, key);
        return;
    }

    // Ctrl+L: shift focus to the file viewer (explorer stays open)
    if key
        .modifiers
        .contains(crossterm::event::KeyModifiers::CONTROL)
        && key.code == KeyCode::Char('l')
    {
        if let OverlayState::FileExplorer {
            explorer_focused, ..
        } = &mut app.overlay
        {
            *explorer_focused = false;
        }
        return;
    }

    if total == 0 {
        if matches!(
            key.code,
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char(' ')
        ) {
            app.overlay = OverlayState::None;
        }
        return;
    }

    // Clear pending chords on any key that isn't the expected second key
    let clear_pending = !matches!(
        key.code,
        KeyCode::Char('y') | KeyCode::Char('n') | KeyCode::Char('d')
    );

    // Handle pending yank chord (y was pressed previously)
    if let OverlayState::FileExplorer {
        pending_yank: true,
        entries,
        selected_index,
        ..
    } = &app.overlay
    {
        let idx = *selected_index;
        match key.code {
            KeyCode::Char('y') => {
                // yy — copy full path
                let path_str = entries[idx].path().display().to_string();
                crate::clipboard::osc52_copy(&path_str);
                app.notify_success(format!("Copied: {}", path_str));
                if let OverlayState::FileExplorer { pending_yank, .. } = &mut app.overlay {
                    *pending_yank = false;
                }
                return;
            }
            KeyCode::Char('n') => {
                // yn — copy filename
                let name = entries[idx]
                    .path()
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                crate::clipboard::osc52_copy(&name);
                app.notify_success(format!("Copied: {}", name));
                if let OverlayState::FileExplorer { pending_yank, .. } = &mut app.overlay {
                    *pending_yank = false;
                }
                return;
            }
            _ => {
                if let OverlayState::FileExplorer { pending_yank, .. } = &mut app.overlay {
                    *pending_yank = false;
                }
            }
        }
    }

    // Handle pending delete chord (d was pressed previously)
    if let OverlayState::FileExplorer {
        pending_delete: true,
        ..
    } = &app.overlay
    {
        if key.code == KeyCode::Char('d') {
            handle_delete(app);
            return;
        } else if let OverlayState::FileExplorer { pending_delete, .. } = &mut app.overlay {
            *pending_delete = false;
        }
    }

    if clear_pending
        && let OverlayState::FileExplorer {
            pending_yank,
            pending_delete,
            ..
        } = &mut app.overlay
    {
        *pending_yank = false;
        *pending_delete = false;
    }

    // Shared list navigation (j/k/g/G)
    let nav_consumed = if let OverlayState::FileExplorer { selected_index, .. } = &mut app.overlay {
        list::handle_list_nav_key(selected_index, total, &key)
    } else {
        false
    };
    if nav_consumed {
        return;
    }

    match key.code {
        KeyCode::Enter | KeyCode::Char('l') => {
            handle_enter(app);
        }
        KeyCode::Char('h') => {
            handle_parent_jump(app);
        }
        KeyCode::Char('y') => {
            if let OverlayState::FileExplorer { pending_yank, .. } = &mut app.overlay {
                *pending_yank = true;
            }
        }
        KeyCode::Char('d') => {
            if let OverlayState::FileExplorer { pending_delete, .. } = &mut app.overlay {
                *pending_delete = true;
            }
        }
        KeyCode::Char('u') => {
            handle_undo(app);
        }
        KeyCode::Char('.') => {
            handle_toggle_hidden(app);
        }
        KeyCode::Char('/') => {
            activate_finder(app);
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            app.overlay = OverlayState::None;
        }
        KeyCode::Char(' ') => {
            // Space+e toggle — close if already open
            app.overlay = OverlayState::None;
        }
        _ => {}
    }
}

/// Paste text into the focused explorer's fuzzy finder.
///
/// The tree has no text field, so it owns and acknowledges the paste instead
/// of allowing it to fall through to an obscured input surface.
pub(super) fn paste_text(app: &mut App, text: &str) -> bool {
    let (finder_active, explorer_focused) = match &app.overlay {
        OverlayState::FileExplorer {
            finder_active,
            explorer_focused,
            ..
        } => (*finder_active, *explorer_focused),
        _ => return false,
    };
    if !explorer_focused {
        return false;
    }
    if !finder_active {
        app.notify("Open finder with / before pasting");
        return true;
    }

    // Finder queries are single-line, matching ordinary typed query behavior.
    let pasted: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    if pasted.is_empty() {
        return true;
    }

    if let OverlayState::FileExplorer { finder_query, .. } = &mut app.overlay {
        finder_query.push_str(&pasted);
    }
    rescore_finder(app);
    true
}

/// Paste system clipboard content into the explorer's active finder.
pub(super) fn paste_clipboard(app: &mut App) -> bool {
    if !matches!(
        &app.overlay,
        OverlayState::FileExplorer {
            explorer_focused: true,
            ..
        }
    ) {
        return false;
    }

    let paste_dir = app.paste_dir.clone();
    match crate::clipboard::read_clipboard(&paste_dir) {
        crate::clipboard::ClipboardContent::Text(text) => {
            let handled = paste_text(app, &text);
            debug_assert!(handled);
        }
        crate::clipboard::ClipboardContent::Image { .. } => {
            app.notify("Image paste isn't supported in file explorer");
        }
        crate::clipboard::ClipboardContent::Empty => app.notify("Clipboard empty"),
    }
    true
}

/// Activate the fuzzy finder sub-mode. Populates cache on first activation.
fn activate_finder(app: &mut App) {
    if let OverlayState::FileExplorer {
        root,
        show_hidden,
        finder_active,
        finder_query,
        finder_cache,
        finder_results,
        finder_selected,
        ..
    } = &mut app.overlay
    {
        *finder_active = true;
        *finder_query = String::new();
        *finder_selected = 0;
        // Populate cache if empty (first activation or root changed)
        if finder_cache.is_empty() {
            *finder_cache = crate::file_utils::walk_files_scoped(root, *show_hidden, Some(1));
        }
        // Empty query = show all (capped at display)
        *finder_results = (0..finder_cache.len().min(100)).collect();
    }
}

/// Handle Enter/l — toggle directory expansion or open file in viewer.
fn handle_enter(app: &mut App) {
    let (idx, is_dir, expanded) = if let OverlayState::FileExplorer {
        entries,
        selected_index,
        ..
    } = &app.overlay
    {
        let idx = *selected_index;
        match &entries[idx] {
            FileExplorerEntry::Directory { expanded, .. } => (idx, true, *expanded),
            FileExplorerEntry::File { .. } => (idx, false, false),
        }
    } else {
        return;
    };

    if is_dir {
        if expanded {
            // Collapse: remove all children with depth > this dir's depth
            if let OverlayState::FileExplorer {
                entries,
                show_hidden,
                ..
            } = &mut app.overlay
            {
                let depth = entries[idx].depth();
                let mut count = 0;
                for e in entries.iter().skip(idx + 1) {
                    if e.depth() > depth {
                        count += 1;
                    } else {
                        break;
                    }
                }
                entries.drain(idx + 1..idx + 1 + count);
                if let FileExplorerEntry::Directory { expanded, .. } = &mut entries[idx] {
                    *expanded = false;
                }
                let _ = show_hidden; // suppress warning
            }
        } else {
            // Expand: read directory and insert children
            if let OverlayState::FileExplorer {
                entries,
                show_hidden,
                ..
            } = &mut app.overlay
            {
                let path = entries[idx].path().to_path_buf();
                let depth = entries[idx].depth();
                let new_entries = read_directory(&path, depth + 1, *show_hidden);
                let insert_pos = idx + 1;
                let new_len = new_entries.len();
                entries.splice(insert_pos..insert_pos, new_entries);
                if let FileExplorerEntry::Directory { expanded, .. } = &mut entries[idx] {
                    *expanded = true;
                }
                let _ = new_len; // suppress warning
            }
        }
    } else {
        // File — open in the session viewer.
        open_file_in_viewer(app, idx);
    }
}

/// Open the file at the given index in the file viewer (called from tree mode).
fn open_file_in_viewer(app: &mut App, entry_idx: usize) {
    let path = if let OverlayState::FileExplorer { entries, .. } = &app.overlay {
        entries[entry_idx].path().to_path_buf()
    } else {
        return;
    };
    open_file_by_path(app, &path);
}

/// Open a file by absolute path in the file viewer.
pub fn open_file_by_path(app: &mut App, path: &Path) {
    // Find the target session
    let session_id = match app.focused_pane() {
        Some(crate::types::Pane::SessionDetail { session_id }) => *session_id,
        _ => match app.selected_session_id() {
            Some(id) => id,
            None => {
                app.notify("No session to attach file viewer to");
                return;
            }
        },
    };

    // Guard: file must be within project scope
    if let Some(project) = app.current_project() {
        if let Some(ref project_path) = project.path {
            if !crate::file_utils::is_within_project_scope(project_path, path) {
                app.notify_error("File is outside project scope");
                return;
            }
        }
    }

    // Guard: must be a regular file
    if !path.is_file() {
        app.notify_error("Not a file");
        return;
    }

    // Guard: size ≤ 1MB
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            app.notify_error(format!("Cannot read: {}", e));
            return;
        }
    };
    if metadata.len() > 1_048_576 {
        app.notify_error(format!(
            "File too large: {} bytes (max 1MB)",
            metadata.len()
        ));
        return;
    }

    // Read file content
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            app.notify_error(format!("Cannot read: {}", e));
            return;
        }
    };

    // Get the session state
    let path_display = path.display().to_string();
    let conflict_detected = {
        let state = match app.sessions.get_mut(&session_id) {
            Some(s) => s,
            None => return,
        };

        crate::file_viewer::activate_cached_viewer(state, path.to_path_buf(), content)
    };

    if conflict_detected {
        app.notify_error(format!(
            "External change detected: {path_display} (use :e! to reload or :w! to overwrite)"
        ));
    }

    // Shift focus to the file viewer (Ctrl+H returns to explorer)
    if let OverlayState::FileExplorer {
        explorer_focused, ..
    } = &mut app.overlay
    {
        *explorer_focused = false;
    }
}

/// Handle keys when the fuzzy finder sub-mode is active.
fn handle_finder_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            // Exit finder back to tree view
            if let OverlayState::FileExplorer {
                finder_active,
                finder_query,
                finder_selected,
                ..
            } = &mut app.overlay
            {
                *finder_active = false;
                *finder_query = String::new();
                *finder_selected = 0;
            }
        }
        KeyCode::Enter => {
            handle_finder_enter(app);
        }
        KeyCode::Char('j') | KeyCode::Down => {
            if let OverlayState::FileExplorer {
                finder_results,
                finder_selected,
                ..
            } = &mut app.overlay
            {
                if *finder_selected + 1 < finder_results.len() {
                    *finder_selected += 1;
                }
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if let OverlayState::FileExplorer {
                finder_selected, ..
            } = &mut app.overlay
            {
                *finder_selected = finder_selected.saturating_sub(1);
            }
        }
        KeyCode::Backspace => {
            if let OverlayState::FileExplorer { finder_query, .. } = &mut app.overlay {
                finder_query.pop();
            }
            rescore_finder(app);
        }
        KeyCode::Char(c) => {
            if let OverlayState::FileExplorer { finder_query, .. } = &mut app.overlay {
                finder_query.push(c);
            }
            rescore_finder(app);
        }
        _ => {}
    }
}

/// Re-score finder_cache against finder_query, update finder_results.
fn rescore_finder(app: &mut App) {
    use fuzzy_matcher::FuzzyMatcher;
    use fuzzy_matcher::skim::SkimMatcherV2;

    if let OverlayState::FileExplorer {
        finder_query,
        finder_cache,
        finder_results,
        finder_selected,
        ..
    } = &mut app.overlay
    {
        *finder_selected = 0;
        if finder_query.is_empty() {
            *finder_results = (0..finder_cache.len().min(100)).collect();
            return;
        }
        let matcher = SkimMatcherV2::default();
        let mut scored: Vec<(usize, i64)> = finder_cache
            .iter()
            .enumerate()
            .filter_map(|(i, path)| {
                let path_str = path.to_string_lossy();
                matcher
                    .fuzzy_match(&path_str, finder_query)
                    .map(|score| (i, score))
            })
            .collect();
        scored.sort_by(|a, b| b.1.cmp(&a.1));
        scored.truncate(100);
        *finder_results = scored.into_iter().map(|(i, _)| i).collect();
    }
}

/// Open the selected finder result in the file viewer.
fn handle_finder_enter(app: &mut App) {
    let path = if let OverlayState::FileExplorer {
        root,
        finder_cache,
        finder_results,
        finder_selected,
        ..
    } = &app.overlay
    {
        let Some(&cache_idx) = finder_results.get(*finder_selected) else {
            return;
        };
        let Some(rel) = finder_cache.get(cache_idx) else {
            return;
        };
        root.join(rel)
    } else {
        return;
    };

    // Deactivate finder
    if let OverlayState::FileExplorer {
        finder_active,
        finder_query,
        finder_selected,
        ..
    } = &mut app.overlay
    {
        *finder_active = false;
        *finder_query = String::new();
        *finder_selected = 0;
    }

    // Reuse existing open_file_by_path logic
    open_file_by_path(app, &path);
}

/// Jump to the parent directory entry in the tree.
fn handle_parent_jump(app: &mut App) {
    if let OverlayState::FileExplorer {
        entries,
        selected_index,
        ..
    } = &mut app.overlay
    {
        let current_depth = entries[*selected_index].depth();
        if current_depth == 0 {
            return;
        }
        // Scan backward for first entry with depth < current
        for i in (0..*selected_index).rev() {
            if entries[i].depth() < current_depth {
                *selected_index = i;
                return;
            }
        }
    }
}

/// Delete the selected file (files only, not directories).
fn handle_delete(app: &mut App) {
    // Extract what we need before mutating
    let (idx, is_dir, path) = if let OverlayState::FileExplorer {
        entries,
        selected_index,
        pending_delete,
        ..
    } = &mut app.overlay
    {
        *pending_delete = false;
        let idx = *selected_index;
        if idx >= entries.len() {
            return;
        }
        (
            idx,
            entries[idx].is_dir(),
            entries[idx].path().to_path_buf(),
        )
    } else {
        return;
    };

    // Guard: file must be within project scope
    if let Some(project) = app.current_project() {
        if let Some(ref project_path) = project.path {
            if !crate::file_utils::is_within_project_scope(project_path, &path) {
                app.notify_error("Cannot delete: file is outside project scope");
                return;
            }
        }
    }

    if is_dir {
        app.notify("Cannot delete directories");
        return;
    }

    // Read file content for undo
    let content = match std::fs::read(&path) {
        Ok(c) => c,
        Err(e) => {
            app.notify_error(format!("Cannot read for delete: {}", e));
            return;
        }
    };

    // Delete the file
    if let Err(e) = std::fs::remove_file(&path) {
        app.notify_error(format!("Delete failed: {}", e));
        return;
    }

    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    // Now mutate the overlay state
    if let OverlayState::FileExplorer {
        entries,
        selected_index,
        trash,
        ..
    } = &mut app.overlay
    {
        trash.push((path, content));
        entries.remove(idx);
        if *selected_index >= entries.len() && !entries.is_empty() {
            *selected_index = entries.len() - 1;
        }
    }

    app.notify_success(format!("Deleted: {} (u to undo)", name));
}

/// Undo the last deletion.
fn handle_undo(app: &mut App) {
    // Pop the trash entry first
    let item = if let OverlayState::FileExplorer { trash, .. } = &mut app.overlay {
        trash.pop()
    } else {
        return;
    };

    let (path, content) = match item {
        Some(t) => t,
        None => {
            app.notify("Nothing to undo");
            return;
        }
    };

    // Restore the file
    if let Err(e) = std::fs::write(&path, &content) {
        app.notify_error(format!("Restore failed: {}", e));
        return;
    }

    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    // Rebuild the tree to include the restored file
    if let OverlayState::FileExplorer {
        root,
        entries,
        selected_index,
        show_hidden,
        ..
    } = &mut app.overlay
    {
        *entries = read_directory(root, 0, *show_hidden);
        *selected_index = 0;
    }

    app.notify_success(format!("Restored: {}", name));
}

/// Toggle hidden file visibility.
fn handle_toggle_hidden(app: &mut App) {
    if let OverlayState::FileExplorer {
        root,
        entries,
        selected_index,
        show_hidden,
        finder_cache,
        ..
    } = &mut app.overlay
    {
        *show_hidden = !*show_hidden;
        *entries = read_directory(root, 0, *show_hidden);
        *selected_index = 0;
        *finder_cache = Vec::new(); // Force re-walk on next finder activation
    }
}

#[cfg(test)]
mod conflict_open_tests {
    use super::*;

    #[test]
    #[allow(clippy::unwrap_used)]
    fn explorer_open_preserves_dirty_draft_and_reports_conflict() {
        let path = std::env::temp_dir().join(format!("rsi-explorer-{}.rs", uuid::Uuid::new_v4()));
        std::fs::write(&path, "original").unwrap();
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session_id = app.filtered_session_order[0];
        open_file_by_path(&mut app, &path);
        let viewer = app
            .sessions
            .get_mut(&session_id)
            .unwrap()
            .file_viewer
            .as_mut()
            .unwrap();
        viewer
            .surface
            .textarea
            .move_cursor(tui_textarea::CursorMove::End);
        viewer.surface.textarea.insert_str(" draft");
        viewer.dirty = true;
        std::fs::write(&path, "external").unwrap();

        open_file_by_path(&mut app, &path);

        let viewer = app
            .sessions
            .get(&session_id)
            .unwrap()
            .file_viewer
            .as_ref()
            .unwrap();
        assert_eq!(viewer.surface.content(), "original draft");
        assert_eq!(viewer.external_conflict.as_deref(), Some("external"));
        let notice = app.notifications.back().unwrap();
        assert_eq!(notice.kind, crate::types::NotificationKind::OperationFailed);
        assert!(notice.message.contains("External change detected"));
        std::fs::remove_file(path).unwrap();
    }
}
