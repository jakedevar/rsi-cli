//! Key handling for the per-session file viewer/editor.
//!
//! Includes search (/, ?, n, N, *, #), code folding (za, zo, zc, zM, zR),
//! cursor-fold interaction logic, command mode (:w, :wq, :q, :q!, :e!, :{n}),
//! bracket auto-pairing, and markdown preview toggle.

use crate::file_viewer_commands::{FileViewerCommand, parse_file_command};
use crate::input_surface::{self, InputAction, InputSurfaceConfig};
use crate::types::{FileViewerState, FoldState, Pane, PopupMode};
use crate::vim_textarea::{self, FileEditorCtx};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use uuid::Uuid;

/// Activate a path already read from disk. All viewer entry points share this
/// cache transition so a dirty draft and the newly observed disk text coexist.
pub(crate) fn activate_cached_viewer(
    state: &mut crate::types::SessionState,
    path: std::path::PathBuf,
    content: String,
) -> bool {
    if let Some(existing) = state.file_viewer.take() {
        state
            .file_viewer_cache
            .insert(existing.file_path.clone(), existing);
    }
    let viewer = match state.file_viewer_cache.remove(&path) {
        Some(cached) if cached.disk_content == content => cached,
        Some(mut cached) if cached.dirty => {
            cached.external_conflict = Some(content);
            cached
        }
        _ => FileViewerState::new(path, content),
    };
    let conflict = viewer.external_conflict.is_some();
    state.file_viewer = Some(viewer);
    state.clear_next_render = true;
    conflict
}

/// Open `path` in the file viewer for `session_id`. Reuses the existing
/// `file_viewer_cache: HashMap<PathBuf, FileViewerState>` so revisiting a path
/// preserves cursor position / fold state. Mirrors the canonical pattern at
/// `crates/rsi/src/overlay/file_explorer.rs:410-432`.
///
/// Detects external file changes by comparing fresh disk content to the
/// cached viewer's `disk_content` snapshot. On conflict with unsaved edits,
/// the draft is preserved and `external_conflict` is set for explicit
/// resolution (`:e!` to reload, `:w!` to overwrite). A clean cached viewer
/// is silently reloaded from disk.
///
/// Used by `LcAction::OpenRecentFileN` (`gf<num>`) to open recent files
/// from the bottom-strip queue zone. No-op (with a notification) if the file
/// cannot be read from disk.
pub fn open_path_in_session(app: &mut crate::app::App, session_id: Uuid, path: std::path::PathBuf) {
    let path_display = path.display().to_string();
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            app.notify_error(format!("open {path_display} failed: {e}"));
            return;
        }
    };

    let conflict_detected = {
        let state = match app.sessions.get_mut(&session_id) {
            Some(s) => s,
            None => return,
        };

        activate_cached_viewer(state, path, content)
    };

    if conflict_detected {
        app.notify_error(format!(
            "External change detected: {path_display} (use :e! to reload or :w! to overwrite)"
        ));
    }
}

/// Save the file viewer's content to disk. Shared by Ctrl+S and :w/:w! commands.
///
/// When `force` is false, the save is fenced against observed disk content:
/// the current disk content is read and compared to `disk_content` (the
/// snapshot from the last load or save). If they differ, the save is refused
/// and `external_conflict` is set so the user can resolve explicitly.
///
/// When `force` is true (e.g. `:w!`), the fence is skipped and the buffer is
/// written unconditionally, overwriting any external changes.
///
/// After a successful write, `disk_content` is updated to the saved content
/// and `external_conflict` is cleared.
///
/// Race reasoning: the read-compare-write sequence has an inherent TOCTOU
/// window between the disk read and the write. Full atomicity would require
/// OS-level file locking (out of scope). The fence catches the common case
/// where an external edit happened before the save was initiated. A same-
/// content metadata-only change (e.g. touch) does not trigger a conflict
/// because we compare content, not timestamps. A deleted-then-recreated file
/// is detected as a content change if the new content differs. If the file
/// was deleted and not recreated, the write proceeds (creating a new file),
/// which is the expected behavior for a user-initiated save.
fn save_file(app: &mut crate::app::App, session_id: Uuid, force: bool) -> bool {
    let (content, path, disk_snapshot) = {
        let s = app.sessions.get(&session_id).unwrap();
        let v = s.file_viewer.as_ref().unwrap();
        (
            v.surface.content(),
            v.file_path.clone(),
            v.disk_content.clone(),
        )
    };

    // Fence: unless forced, refuse to overwrite if the disk changed since
    // the viewer was last loaded or saved.
    if !force {
        match std::fs::read_to_string(&path) {
            Ok(current_disk) => {
                if current_disk != disk_snapshot {
                    // Disk changed externally since last load/save.
                    if current_disk == content {
                        // The buffer matches the external content — no data
                        // loss risk. Update the snapshot and clear dirty
                        // without a redundant write.
                        if let Some(s) = app.sessions.get_mut(&session_id)
                            && let Some(v) = s.file_viewer.as_mut()
                        {
                            v.dirty = false;
                            v.disk_content = current_disk;
                            v.external_conflict = None;
                        }
                        let display_path = path.display().to_string();
                        app.notify_success(format!("Already up to date: {display_path}"));
                        return true;
                    }
                    // Real conflict: external content differs from both the
                    // snapshot and the buffer. Refuse and surface the conflict.
                    if let Some(s) = app.sessions.get_mut(&session_id)
                        && let Some(v) = s.file_viewer.as_mut()
                    {
                        v.external_conflict = Some(current_disk);
                    }
                    let display_path = path.display().to_string();
                    app.notify_error(format!(
                        "Save refused — file changed on disk: {display_path} (use :w! to overwrite or :e! to reload)"
                    ));
                    return false;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // File was deleted; the write will create it — proceed.
            }
            Err(_) => {
                // File exists but is unreadable (permissions, non-UTF-8,
                // etc.). The write would succeed and destroy the external
                // content without us having verified the snapshot. Refuse.
                if let Some(s) = app.sessions.get_mut(&session_id)
                    && let Some(v) = s.file_viewer.as_mut()
                {
                    v.external_conflict = Some(String::from("<unreadable>"));
                }
                let display_path = path.display().to_string();
                app.notify_error(format!(
                    "Save refused — cannot verify file state: {display_path} (use :w! to overwrite or :e! to reload)"
                ));
                return false;
            }
        }
    }

    match std::fs::write(&path, &content) {
        Ok(()) => {
            let display_path = path.display().to_string();
            // Recompute git gutter after save, update disk snapshot, clear conflict
            if let Some(s) = app.sessions.get_mut(&session_id)
                && let Some(v) = s.file_viewer.as_mut()
            {
                v.dirty = false;
                v.disk_content = content;
                v.external_conflict = None;
                let line_count = v.surface.textarea.lines().len();
                v.git_gutter.line_states =
                    crate::git_gutter::compute_git_gutter(&v.file_path, line_count);
            }
            app.notify_success(format!("Saved: {display_path}"));
            true
        }
        Err(e) => {
            let display_path = path.display().to_string();
            app.notify_error(format!("Save failed: {display_path}: {e}"));
            false
        }
    }
}

/// Close the file viewer, caching it for later restoration.
fn close_viewer(app: &mut crate::app::App, session_id: Uuid) {
    if let Some(state) = app.sessions.get_mut(&session_id) {
        if let Some(viewer) = state.file_viewer.take() {
            state
                .file_viewer_cache
                .insert(viewer.file_path.clone(), viewer);
        }
        state.clear_next_render = true;
    }
}

/// Handle a key event when the file viewer is active.
/// Returns true if the key was consumed (caller should skip normal dispatch).
pub fn handle_file_viewer_key(app: &mut crate::app::App, key: KeyEvent) -> bool {
    // Relevant when focused pane is SessionDetail or SessionList with a file viewer open
    let (session_id, on_session_list) = match app.focused_pane() {
        Some(Pane::SessionDetail { session_id }) => (*session_id, false),
        Some(Pane::SessionList {
            selected_session: Some(id),
            ..
        }) => (*id, true),
        _ => return false,
    };

    let state = match app.sessions.get_mut(&session_id) {
        Some(s) => s,
        None => return false,
    };

    let viewer = match state.file_viewer.as_mut() {
        Some(v) => v,
        None => return false,
    };

    // --- Ctrl+S: save file ---
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
        // Drop borrow before calling save_file
        save_file(app, session_id, false);
        return true;
    }

    // --- Command mode: intercept all keys when active ---
    if viewer.command.active {
        return handle_file_command_key(app, session_id, key);
    }

    // --- Search input active: collect characters ---
    if viewer.search.input_active {
        handle_search_input(viewer, key);
        return true;
    }

    // --- Space+key (normal mode): leader sequences ---
    if viewer.surface.mode == PopupMode::Normal {
        if viewer.pending_leader {
            viewer.pending_leader = false;
            if key.code == KeyCode::Char('q') {
                // Close file viewer — cache it to preserve undo/redo history
                close_viewer(app, session_id);
                return true;
            }
            if key.code == KeyCode::Char('m') {
                // Toggle markdown preview (only for .md files)
                if let Some(s) = app.sessions.get_mut(&session_id) {
                    if let Some(v) = s.file_viewer.as_mut() {
                        if v.is_markdown() {
                            v.markdown_preview = !v.markdown_preview;
                            v.markdown_cache = None; // invalidate cache
                        }
                    }
                }
                return true;
            }
            if key.code == KeyCode::Char(' ') {
                // Space+Space → open telescope file picker
                crate::overlay::telescope::open_telescope(app);
                return true;
            }
            // Space was consumed but second key wasn't recognized — fall through to InputSurface
        } else if key.code == KeyCode::Char(' ') {
            viewer.pending_leader = true;
            return true;
        }

        // --- z-prefix fold commands ---
        if viewer.pending_z {
            viewer.pending_z = false;
            match key.code {
                KeyCode::Char('a') => {
                    toggle_fold_at_cursor(viewer);
                    return true;
                }
                KeyCode::Char('o') => {
                    set_fold_at_cursor(viewer, false);
                    return true;
                }
                KeyCode::Char('c') => {
                    set_fold_at_cursor(viewer, true);
                    return true;
                }
                KeyCode::Char('M') => {
                    fold_all(viewer);
                    return true;
                }
                KeyCode::Char('R') => {
                    unfold_all(viewer);
                    return true;
                }
                _ => {
                    // Absorb unknown z-sequences
                    return true;
                }
            }
        }
        if key.code == KeyCode::Char('z') {
            viewer.pending_z = true;
            return true;
        }

        // --- Normal mode `:` — enter command mode ---
        if key.code == KeyCode::Char(':') {
            if let Some(s) = app.sessions.get_mut(&session_id) {
                if let Some(v) = s.file_viewer.as_mut() {
                    v.command.active = true;
                    v.command.buffer.clear();
                    v.command.cursor = 0;
                }
            }
            return true;
        }

        if handle_viewer_scroll_key(viewer, key) {
            return true;
        }

        // --- Markdown preview mode: block editing keys ---
        if viewer.markdown_preview && viewer.is_markdown() {
            match key.code {
                // Allow navigation-related keys
                KeyCode::Char('j' | 'k' | 'g' | 'G' | '/' | '?' | 'n' | 'N' | 'z')
                | KeyCode::Up
                | KeyCode::Down
                | KeyCode::PageUp
                | KeyCode::PageDown => {
                    // Fall through to normal handling below
                }
                KeyCode::Esc => {} // Allow Esc
                // Block everything else in preview mode
                _ => return true,
            }
        }

        // --- Normal mode search triggers ---
        match key.code {
            KeyCode::Char('/') => {
                viewer.search.input_active = true;
                viewer.search.forward = true;
                viewer.search.input_buffer.clear();
                return true;
            }
            KeyCode::Char('?') if key.modifiers.is_empty() => {
                viewer.search.input_active = true;
                viewer.search.forward = false;
                viewer.search.input_buffer.clear();
                return true;
            }
            KeyCode::Char('n') => {
                if viewer.search.pattern.is_some() {
                    let forward = viewer.search.forward;
                    advance_search_match(viewer, forward);
                    return true;
                }
            }
            KeyCode::Char('N') => {
                if viewer.search.pattern.is_some() {
                    let forward = !viewer.search.forward;
                    advance_search_match(viewer, forward);
                    return true;
                }
            }
            KeyCode::Char('*') => {
                handle_star_search(viewer, true);
                return true;
            }
            KeyCode::Char('#') => {
                handle_star_search(viewer, false);
                return true;
            }
            _ => {}
        }
    }

    // --- Backspace (normal mode): close viewer or pass through ---
    if viewer.surface.mode == PopupMode::Normal && key.code == KeyCode::Backspace {
        if on_session_list {
            // On SessionList, close the viewer directly (BackToList is a no-op here)
            close_viewer(app, session_id);
            return true;
        }
        // On SessionDetail, pass through to trigger BackToList navigation
        return false;
    }

    // --- Delegate to InputSurface ---
    // For normal mode, we intercept to provide FileEditorCtx for auto-indent on o/O.
    // For insert mode, we delegate directly to input_surface::handle_key.
    let content_before = viewer.surface.content();

    if viewer.surface.mode == PopupMode::Normal {
        // Ctrl+Enter — ignore in file viewer context
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Enter {
            return true;
        }

        // q in normal mode — close file viewer
        if key.code == KeyCode::Char('q') {
            close_viewer(app, session_id);
            return true;
        }

        // Build FileEditorCtx snapshot for auto-indent
        let row_before = viewer.surface.textarea.cursor().0;
        let lines_snapshot: Vec<String> = viewer.surface.textarea.lines().to_vec();
        let ctx = FileEditorCtx {
            indent_style: viewer.indent_style,
            lines: &lines_snapshot,
            wrap_width: Some(viewer.surface.wrap_width.get()),
        };
        let action = crate::vim_textarea::handle_vim_normal(
            &mut viewer.surface.textarea,
            &mut viewer.surface.vim_state,
            key,
            Some(&ctx),
        );

        match action {
            crate::vim_textarea::VimAction::EnteredInsert => {
                viewer.surface.mode = PopupMode::Insert;
                let content = viewer.surface.content();
                viewer.surface.vim_state.snapshot_for_insert(&content);
                // Check dirty after o/O (content changed by newline + indent)
                if viewer.surface.content() != content_before {
                    viewer.mark_dirty();
                    recompute_folds_from_content(viewer);
                }
            }
            crate::vim_textarea::VimAction::Consumed => {
                if viewer.surface.content() != content_before {
                    viewer.mark_dirty();
                    recompute_folds_from_content(viewer);
                }
                // Skip cursor over folded ranges after vertical movement
                let row_after = viewer.surface.textarea.cursor().0;
                if row_after != row_before && !viewer.folds.folded_ranges.is_empty() {
                    let forward = row_after > row_before;
                    skip_folded_cursor(viewer, forward);
                    ensure_cursor_visible(viewer);
                }
            }
            crate::vim_textarea::VimAction::Unhandled => {
                // Swallow unhandled keys in file viewer
            }
        }
    } else {
        // Insert mode

        // --- Auto-pair: intercept bracket keys before delegation ---
        if viewer.auto_pair {
            if let Some(_consumed) = handle_auto_pair(viewer, key) {
                if viewer.surface.content() != content_before {
                    viewer.mark_dirty();
                    recompute_folds_from_content(viewer);
                }
                return true;
            }
        }

        // Delegate to input_surface
        let config = InputSurfaceConfig {
            pass_through_unhandled: false,
            available_commands: &[],
            working_dir: None,
            // This surface edits a document; Enter is always a line break.
            submit_on_enter: false,
        };
        let action = input_surface::handle_key(&mut viewer.surface, key, &config);

        match action {
            InputAction::Consumed => {
                if viewer.surface.content() != content_before {
                    viewer.mark_dirty();
                    recompute_folds_from_content(viewer);
                }
            }
            InputAction::Close => {
                // Should not happen in insert mode
            }
            InputAction::Submit(_) => {
                // Ctrl+Enter — ignore
            }
            InputAction::Passthrough(_) => {
                // Should not happen
            }
            InputAction::CompileDecision { .. } => {
                // File viewer doesn't support prompt compilation
            }
        }
    }

    true
}

// =============================================================================
// Command Mode
// =============================================================================

/// Handle keystroke while command mode is active.
fn handle_file_command_key(app: &mut crate::app::App, session_id: Uuid, key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Esc => {
            if let Some(s) = app.sessions.get_mut(&session_id) {
                if let Some(v) = s.file_viewer.as_mut() {
                    v.command.cancel();
                }
            }
        }
        KeyCode::Enter => {
            let buffer = {
                let s = app.sessions.get(&session_id).unwrap();
                let v = s.file_viewer.as_ref().unwrap();
                v.command.buffer.clone()
            };
            execute_file_command(app, session_id, &buffer);
        }
        KeyCode::Backspace => {
            if let Some(s) = app.sessions.get_mut(&session_id) {
                if let Some(v) = s.file_viewer.as_mut() {
                    if v.command.buffer.is_empty() {
                        v.command.cancel();
                    } else {
                        v.command.backspace();
                    }
                }
            }
        }
        KeyCode::Char(c) => {
            if let Some(s) = app.sessions.get_mut(&session_id) {
                if let Some(v) = s.file_viewer.as_mut() {
                    v.command.insert_char(c);
                }
            }
        }
        _ => {}
    }
    true
}

/// Execute a parsed file viewer command.
fn execute_file_command(app: &mut crate::app::App, session_id: Uuid, buffer: &str) {
    let cmd = parse_file_command(buffer);

    // Cancel command mode first
    if let Some(s) = app.sessions.get_mut(&session_id) {
        if let Some(v) = s.file_viewer.as_mut() {
            v.command.cancel();
        }
    }

    match cmd {
        FileViewerCommand::Save => {
            save_file(app, session_id, false);
        }
        FileViewerCommand::ForceSave => {
            save_file(app, session_id, true);
        }
        FileViewerCommand::SaveAndClose => {
            // A clean buffer can still fail to write (for example, if its
            // directory became read-only). Close only on an actual success.
            if save_file(app, session_id, false) {
                close_viewer(app, session_id);
            }
        }
        FileViewerCommand::Close => {
            let dirty = app
                .sessions
                .get(&session_id)
                .and_then(|s| s.file_viewer.as_ref())
                .map(|v| v.dirty)
                .unwrap_or(false);
            if dirty {
                app.notify_error("No write since last change (use :q! to override)".to_string());
            } else {
                close_viewer(app, session_id);
            }
        }
        FileViewerCommand::ForceClose => {
            close_viewer(app, session_id);
        }
        FileViewerCommand::Revert => {
            if let Some(s) = app.sessions.get_mut(&session_id) {
                if let Some(v) = s.file_viewer.as_mut() {
                    let path = v.file_path.clone();
                    match std::fs::read_to_string(&path) {
                        Ok(content) => {
                            let lines: Vec<String> = content.lines().map(str::to_string).collect();
                            let lines = if lines.is_empty() {
                                vec![String::new()]
                            } else {
                                lines
                            };
                            v.surface.textarea.select_all();
                            v.surface.textarea.cut();
                            v.surface.textarea.insert_str(&lines.join("\n"));
                            v.dirty = false;
                            v.content_version = v.content_version.wrapping_add(1);
                            v.markdown_cache = None;
                            v.disk_content = content.clone();
                            v.external_conflict = None;
                            let line_count = v.surface.textarea.lines().len();
                            v.git_gutter.line_states =
                                crate::git_gutter::compute_git_gutter(&v.file_path, line_count);
                            let display_path = path.display().to_string();
                            app.notify_success(format!("Reverted: {display_path}"));
                        }
                        Err(e) => {
                            let display_path = path.display().to_string();
                            app.notify_error(format!("Revert failed: {display_path}: {e}"));
                        }
                    }
                }
            }
        }
        FileViewerCommand::GoToLine(n) => {
            if let Some(s) = app.sessions.get_mut(&session_id) {
                if let Some(v) = s.file_viewer.as_mut() {
                    let total = v.surface.textarea.lines().len();
                    let target_row = n.saturating_sub(1).min(total.saturating_sub(1));
                    vim_textarea::move_cursor_to(&mut v.surface.textarea, target_row, 0);
                    ensure_cursor_visible(v);
                }
            }
        }
        FileViewerCommand::SetAutoPair(enabled) => {
            if let Some(s) = app.sessions.get_mut(&session_id) {
                if let Some(v) = s.file_viewer.as_mut() {
                    v.auto_pair = enabled;
                }
            }
            let label = if enabled { "on" } else { "off" };
            app.notify_success(format!("Auto-pair: {label}"));
        }
        FileViewerCommand::Unknown(s) => {
            if !s.is_empty() {
                app.notify_error(format!("Unknown command: :{s}"));
            }
        }
    }
}

// =============================================================================
// Bracket Auto-pairing
// =============================================================================

/// Returns `Some(true)` if auto-pair insert was performed.
/// Returns `Some(false)` if skip-over was performed.
/// Returns `None` if key was not an auto-pair key (caller falls through).
fn handle_auto_pair(viewer: &mut FileViewerState, key: KeyEvent) -> Option<bool> {
    use tui_textarea::CursorMove;

    let textarea = &mut viewer.surface.textarea;
    let (row, col) = textarea.cursor();

    // `col` is a char index from the textarea cursor — peek by char position
    // rather than byte-slicing (which panics mid-char on multibyte lines).
    let next_char = textarea
        .lines()
        .get(row)
        .and_then(|line| line.chars().nth(col));

    match key.code {
        // Opening brackets: insert pair and step back
        KeyCode::Char(c @ ('{' | '(' | '[')) => {
            let close = match c {
                '{' => '}',
                '(' => ')',
                '[' => ']',
                _ => unreachable!(),
            };
            if next_char == Some(close) {
                return None;
            }
            let pair = format!("{c}{close}");
            textarea.insert_str(&pair);
            textarea.move_cursor(CursorMove::Back);
            Some(true)
        }
        // Closing brackets: skip-over if already present
        KeyCode::Char(c @ ('}' | ')' | ']')) => {
            if next_char == Some(c) {
                textarea.move_cursor(CursorMove::Forward);
                return Some(false);
            }
            None
        }
        // Backspace: delete pair if cursor is between empty brackets
        KeyCode::Backspace => {
            let prev_char = if col > 0 {
                textarea
                    .lines()
                    .get(row)
                    .and_then(|line| line.chars().nth(col - 1))
            } else {
                None
            };
            let is_matching_close = match prev_char {
                Some('{') => next_char == Some('}'),
                Some('(') => next_char == Some(')'),
                Some('[') => next_char == Some(']'),
                _ => false,
            };
            if matches!(prev_char, Some('{' | '(' | '[')) && is_matching_close {
                textarea.delete_next_char();
                textarea.delete_char();
                return Some(true);
            }
            None
        }
        _ => None,
    }
}

// =============================================================================
// Search
// =============================================================================

/// Handle keystroke while search input is active.
fn handle_search_input(viewer: &mut FileViewerState, key: KeyEvent) {
    match key.code {
        KeyCode::Char(c) => {
            viewer.search.input_buffer.push(c);
            recompute_search_matches(viewer);
            let forward = viewer.search.forward;
            jump_to_first_match_from_cursor(viewer, forward);
        }
        KeyCode::Backspace => {
            viewer.search.input_buffer.pop();
            recompute_search_matches(viewer);
            let forward = viewer.search.forward;
            jump_to_first_match_from_cursor(viewer, forward);
        }
        KeyCode::Enter => {
            // Finalize: freeze pattern, close input bar
            viewer.search.input_active = false;
            viewer.search.pattern = if viewer.search.input_buffer.is_empty() {
                None
            } else {
                Some(viewer.search.input_buffer.clone())
            };
            recompute_search_matches(viewer);
            let forward = viewer.search.forward;
            jump_to_first_match_from_cursor(viewer, forward);
        }
        KeyCode::Esc => {
            // Cancel: clear search entirely
            viewer.search.input_active = false;
            viewer.search.input_buffer.clear();
            viewer.search.pattern = None;
            viewer.search.matches.clear();
        }
        _ => {}
    }
}

/// Recompute all match positions from the current input buffer.
fn recompute_search_matches(viewer: &mut FileViewerState) {
    let pat = &viewer.search.input_buffer;
    if pat.is_empty() {
        viewer.search.matches.clear();
        return;
    }
    let mut matches = Vec::new();
    let lines: Vec<String> = viewer
        .surface
        .textarea
        .lines()
        .iter()
        .map(|s| s.to_string())
        .collect();
    for (line_idx, line) in lines.iter().enumerate() {
        let mut search_start = 0usize;
        while let Some(offset) = line[search_start..].find(pat.as_str()) {
            let abs_start = search_start + offset;
            let abs_end = abs_start + pat.len();
            matches.push((line_idx, abs_start, abs_end));
            search_start = abs_start + 1; // advance by 1 to allow overlapping matches
        }
    }
    viewer.search.matches = matches;
}

/// Jump to the first match from the current cursor position.
fn jump_to_first_match_from_cursor(viewer: &mut FileViewerState, forward: bool) {
    if viewer.search.matches.is_empty() {
        return;
    }
    let (cur_row, cur_col) = viewer.surface.textarea.cursor();
    let idx = if forward {
        viewer
            .search
            .matches
            .iter()
            .position(|&(r, c, _)| r > cur_row || (r == cur_row && c >= cur_col))
            .unwrap_or(0) // wrap to start
    } else {
        // Last match before cursor (or wrap to end)
        viewer
            .search
            .matches
            .iter()
            .rposition(|&(r, c, _)| r < cur_row || (r == cur_row && c < cur_col))
            .unwrap_or(viewer.search.matches.len() - 1)
    };
    viewer.search.current_match = idx;
    let (row, col, _) = viewer.search.matches[idx];
    // Unfold if the match is inside a folded region
    unfold_line(viewer, row);
    vim_textarea::move_cursor_to(&mut viewer.surface.textarea, row, col);
    ensure_cursor_visible(viewer);
}

/// Advance to the next/previous search match (n/N).
fn advance_search_match(viewer: &mut FileViewerState, forward: bool) {
    if viewer.search.matches.is_empty() {
        return;
    }
    let len = viewer.search.matches.len();
    viewer.search.current_match = if forward {
        (viewer.search.current_match + 1) % len
    } else {
        (viewer.search.current_match + len - 1) % len
    };
    let (row, col, _) = viewer.search.matches[viewer.search.current_match];
    // Unfold if the match is inside a folded region
    unfold_line(viewer, row);
    vim_textarea::move_cursor_to(&mut viewer.surface.textarea, row, col);
    ensure_cursor_visible(viewer);
}

// =============================================================================
// Word-under-cursor search (* / #)
// =============================================================================

/// Extract the word under the cursor (alphanumeric + underscore).
fn word_under_cursor(lines: &[String], row: usize, col: usize) -> Option<String> {
    let line = lines.get(row)?;
    let chars: Vec<char> = line.chars().collect();
    if col >= chars.len() {
        return None;
    }
    let is_word_char = |c: char| c.is_alphanumeric() || c == '_';
    if !is_word_char(chars[col]) {
        return None;
    }
    // Walk backward to word start
    let start = (0..=col)
        .rev()
        .take_while(|&i| is_word_char(chars[i]))
        .last()
        .unwrap_or(col);
    // Walk forward to word end (exclusive)
    let end = (col..chars.len())
        .take_while(|&i| is_word_char(chars[i]))
        .last()
        .map(|i| i + 1)
        .unwrap_or(col + 1);
    Some(chars[start..end].iter().collect())
}

/// Handle * (forward=true) or # (forward=false) word search.
fn handle_star_search(viewer: &mut FileViewerState, forward: bool) {
    let (row, col) = viewer.surface.textarea.cursor();
    let lines: Vec<String> = viewer
        .surface
        .textarea
        .lines()
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Some(word) = word_under_cursor(&lines, row, col) {
        viewer.search.input_buffer = word.clone();
        viewer.search.pattern = Some(word);
        viewer.search.forward = forward;
        recompute_search_matches(viewer);
        if viewer.search.matches.is_empty() {
            return;
        }
        // Jump to next/previous occurrence (not the one under cursor)
        let (cur_row, cur_col) = viewer.surface.textarea.cursor();
        let idx = if forward {
            viewer
                .search
                .matches
                .iter()
                .position(|&(r, c, _)| r > cur_row || (r == cur_row && c > cur_col))
                .unwrap_or(0)
        } else {
            viewer
                .search
                .matches
                .iter()
                .rposition(|&(r, c, _)| r < cur_row || (r == cur_row && c < col))
                .unwrap_or(viewer.search.matches.len().saturating_sub(1))
        };
        viewer.search.current_match = idx;
        let (target_row, target_col, _) = viewer.search.matches[idx];
        unfold_line(viewer, target_row);
        vim_textarea::move_cursor_to(&mut viewer.surface.textarea, target_row, target_col);
        ensure_cursor_visible(viewer);
    }
}

// =============================================================================
// Code Folding
// =============================================================================

/// Foldable tree-sitter node types.
const FOLDABLE_TYPES: &[&str] = &[
    // Rust
    "block",
    "declaration_list",
    "field_declaration_list",
    "enum_variant_list",
    "where_clause",
    // General (works for many languages)
    "body",
    "arguments",
    "parameters",
    "object",
    "array",
    "statement_block",
    // Comments
    "block_comment",
];

/// Recompute foldable_regions from the current tree-sitter parse tree.
pub fn recompute_foldable_regions(folds: &mut FoldState, tree: &tree_sitter::Tree) {
    let root = tree.root_node();
    let mut regions = Vec::new();
    collect_foldable_nodes(root, &mut regions);
    regions.sort_by_key(|&(start, _)| start);
    regions.dedup();
    folds.foldable_regions = regions;

    // Remove any folded_ranges that are no longer in foldable_regions
    folds
        .folded_ranges
        .retain(|r| folds.foldable_regions.contains(r));
}

fn collect_foldable_nodes(node: tree_sitter::Node, out: &mut Vec<(usize, usize)>) {
    let start = node.start_position().row;
    let end = node.end_position().row;
    if end > start && FOLDABLE_TYPES.contains(&node.kind()) {
        out.push((start, end));
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_foldable_nodes(child, out);
    }
}

/// Recompute fold regions from file content using tree-sitter parser.
fn recompute_folds_from_content(viewer: &mut FileViewerState) {
    let ext = viewer.language_ext.as_deref().unwrap_or("");
    let language = match ext {
        "rs" => Some(tree_sitter_rust::LANGUAGE),
        "py" | "pyi" => Some(tree_sitter_python::LANGUAGE),
        "js" | "jsx" | "mjs" | "cjs" => Some(tree_sitter_javascript::LANGUAGE),
        "ts" => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT),
        "tsx" => Some(tree_sitter_typescript::LANGUAGE_TSX),
        "sh" | "bash" | "zsh" => Some(tree_sitter_bash::LANGUAGE),
        "json" | "jsonc" => Some(tree_sitter_json::LANGUAGE),
        "toml" => Some(tree_sitter_toml_ng::LANGUAGE),
        "yaml" | "yml" => Some(tree_sitter_yaml::LANGUAGE),
        "go" => Some(tree_sitter_go::LANGUAGE),
        "c" | "h" => Some(tree_sitter_c::LANGUAGE),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => Some(tree_sitter_cpp::LANGUAGE),
        "md" | "markdown" => Some(tree_sitter_md::LANGUAGE),
        _ => None,
    };
    let Some(lang) = language else {
        return;
    };
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter::Language::from(lang))
        .is_err()
    {
        return;
    }
    let content = viewer.surface.textarea.lines().join("\n");
    if let Some(tree) = parser.parse(&content, None) {
        recompute_foldable_regions(&mut viewer.folds, &tree);
    }
}

/// Find the innermost foldable region whose start_line == cursor row.
fn fold_region_at_cursor(viewer: &FileViewerState) -> Option<(usize, usize)> {
    let (row, _) = viewer.surface.textarea.cursor();
    viewer
        .folds
        .foldable_regions
        .iter()
        .filter(|&&(s, _)| s == row)
        .min_by_key(|&&(s, e)| e - s)
        .copied()
}

fn toggle_fold_at_cursor(viewer: &mut FileViewerState) {
    if let Some(region) = fold_region_at_cursor(viewer) {
        if viewer.folds.folded_ranges.contains(&region) {
            viewer.folds.folded_ranges.remove(&region);
        } else {
            viewer.folds.folded_ranges.insert(region);
        }
    }
}

fn set_fold_at_cursor(viewer: &mut FileViewerState, collapse: bool) {
    if let Some(region) = fold_region_at_cursor(viewer) {
        if collapse {
            viewer.folds.folded_ranges.insert(region);
        } else {
            viewer.folds.folded_ranges.remove(&region);
        }
    }
}

fn fold_all(viewer: &mut FileViewerState) {
    viewer.folds.folded_ranges = viewer.folds.foldable_regions.iter().copied().collect();
}

fn unfold_all(viewer: &mut FileViewerState) {
    viewer.folds.folded_ranges.clear();
}

/// Unfold any fold range that contains the given line as an interior line.
fn unfold_line(viewer: &mut FileViewerState, line: usize) {
    let to_remove: Vec<(usize, usize)> = viewer
        .folds
        .folded_ranges
        .iter()
        .filter(|&&(start, end)| line > start && line <= end)
        .copied()
        .collect();
    for r in to_remove {
        viewer.folds.folded_ranges.remove(&r);
    }
}

// =============================================================================
// Cursor-Fold Interaction
// =============================================================================

/// After any cursor movement, if the cursor landed inside a folded range,
/// skip it in the requested direction.
fn skip_folded_cursor(viewer: &mut FileViewerState, forward: bool) {
    loop {
        let (row, _) = viewer.surface.textarea.cursor();
        let fold = viewer
            .folds
            .folded_ranges
            .iter()
            .find(|&&(start, end)| row > start && row <= end)
            .copied();
        let Some((start, end)) = fold else {
            break; // cursor is not inside a fold
        };
        if forward {
            let target = end + 1;
            let total = viewer.surface.textarea.lines().len();
            if target >= total {
                vim_textarea::move_cursor_to(&mut viewer.surface.textarea, start, 0);
            } else {
                vim_textarea::move_cursor_to(&mut viewer.surface.textarea, target, 0);
            }
        } else {
            vim_textarea::move_cursor_to(&mut viewer.surface.textarea, start, 0);
        }
    }
}

// =============================================================================
// Viewport Helpers
// =============================================================================

/// Returns true if `line` should be hidden because it falls inside a collapsed fold.
pub fn line_is_folded(folds: &FoldState, line: usize) -> bool {
    folds
        .folded_ranges
        .iter()
        .any(|&(start, end)| line > start && line <= end)
}

/// If `line` is the start of a collapsed fold, returns the number of hidden lines.
pub fn fold_summary_at(folds: &FoldState, line: usize) -> Option<usize> {
    folds
        .folded_ranges
        .iter()
        .find(|&&(start, _)| start == line)
        .map(|&(start, end)| end - start)
}

/// Ensure upward cursor jumps do not leave an obviously stale source viewport.
/// The renderer applies the final clamp because it knows wrapped visual rows.
fn ensure_cursor_visible(viewer: &mut FileViewerState) {
    let (cursor_row, _) = viewer.surface.textarea.cursor();
    if cursor_row < viewer.viewport_top {
        viewer.viewport_top = cursor_row;
    }
    // Upper bound is handled by the renderer which knows actual viewport height.
}

fn page_step(viewer: &FileViewerState, half: bool) -> usize {
    let height = viewer.viewport_height.max(1);
    if half { (height / 2).max(1) } else { height }
}

fn handle_viewer_scroll_key(viewer: &mut FileViewerState, key: KeyEvent) -> bool {
    if viewer.surface.mode != PopupMode::Normal {
        return false;
    }

    let page_down = key.code == KeyCode::PageDown
        || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('f'));
    let page_up = key.code == KeyCode::PageUp
        || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('b'));
    let half_down = key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('d');
    let half_up = key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('u');

    let preview = viewer.markdown_preview && viewer.is_markdown();
    if preview {
        match key.code {
            KeyCode::Char('j') | KeyCode::Down if key.modifiers.is_empty() => {
                scroll_preview(viewer, 1);
                return true;
            }
            KeyCode::Char('k') | KeyCode::Up if key.modifiers.is_empty() => {
                scroll_preview(viewer, -1);
                return true;
            }
            _ => {}
        }
    }

    if page_down {
        scroll_viewer_by_page(viewer, page_step(viewer, false), true, preview);
        return true;
    }
    if page_up {
        scroll_viewer_by_page(viewer, page_step(viewer, false), false, preview);
        return true;
    }
    if half_down {
        scroll_viewer_by_page(viewer, page_step(viewer, true), true, preview);
        return true;
    }
    if half_up {
        scroll_viewer_by_page(viewer, page_step(viewer, true), false, preview);
        return true;
    }

    false
}

fn scroll_viewer_by_page(
    viewer: &mut FileViewerState,
    amount: usize,
    forward: bool,
    preview: bool,
) {
    if preview {
        let delta = if forward {
            amount as isize
        } else {
            -(amount as isize)
        };
        scroll_preview(viewer, delta);
    } else {
        scroll_source(viewer, amount, forward);
    }
}

fn scroll_preview(viewer: &mut FileViewerState, delta: isize) {
    if delta >= 0 {
        viewer.viewport_top = viewer.viewport_top.saturating_add(delta as usize);
    } else {
        viewer.viewport_top = viewer.viewport_top.saturating_sub(delta.unsigned_abs());
    }
}

fn scroll_source(viewer: &mut FileViewerState, amount: usize, forward: bool) {
    let total = viewer.surface.textarea.lines().len();
    if total == 0 {
        viewer.viewport_top = 0;
        return;
    }

    let (row, col) = viewer.surface.textarea.cursor();
    let target_row = if forward {
        row.saturating_add(amount).min(total.saturating_sub(1))
    } else {
        row.saturating_sub(amount)
    };
    vim_textarea::move_cursor_to(&mut viewer.surface.textarea, target_row, col);
    if !viewer.folds.folded_ranges.is_empty() {
        skip_folded_cursor(viewer, forward);
    }

    let next_top = if forward {
        viewer.viewport_top.saturating_add(amount)
    } else {
        viewer.viewport_top.saturating_sub(amount)
    };
    viewer.viewport_top = next_top;
}

/// Whether a terminal coordinate falls inside the most recently rendered file
/// viewer body. The title, border, and command/search prompt are deliberately
/// excluded so they keep their existing behavior.
pub fn mouse_targets_viewer(viewer: &FileViewerState, col: u16, row: u16) -> bool {
    viewer.mouse_layout.editor_area.is_some_and(|area| {
        col >= area.x
            && col < area.x.saturating_add(area.width)
            && row >= area.y
            && row < area.y.saturating_add(area.height)
    })
}

/// Set the source cursor from a click in the rendered file editor.
///
/// The renderer records every visible wrapped row, so clicks remain accurate
/// with line wrapping and collapsed folds. Clicking the gutter puts the cursor
/// at the start of that source row; clicking beyond the text clamps to EOL.
pub fn set_cursor_from_mouse(viewer: &mut FileViewerState, col: u16, row: u16) -> bool {
    if !mouse_targets_viewer(viewer, col, row) || (viewer.markdown_preview && viewer.is_markdown())
    {
        return false;
    }

    let Some(content_area) = viewer.mouse_layout.content_area else {
        return false;
    };
    let Some(screen_row) = row.checked_sub(content_area.y).map(usize::from) else {
        return false;
    };
    let Some(hit) = viewer.mouse_layout.rows.get(screen_row).copied() else {
        return true;
    };

    let source_len = viewer
        .surface
        .textarea
        .lines()
        .get(hit.line)
        .map(|line| line.chars().count())
        .unwrap_or(0);
    let clicked_offset = col.saturating_sub(content_area.x) as usize;
    let target_col = if col < content_area.x {
        0
    } else {
        hit.char_start
            .saturating_add(clicked_offset)
            .min(hit.char_end)
            .min(source_len)
    };
    vim_textarea::move_cursor_to(&mut viewer.surface.textarea, hit.line, target_col);
    // A mouse click establishes a new vertical-motion column.
    viewer.surface.vim_state.desired_col = None;
    true
}

/// Scroll the file viewer in response to a mouse wheel event.
pub fn scroll_from_mouse(viewer: &mut FileViewerState, amount: usize, forward: bool) {
    if viewer.markdown_preview && viewer.is_markdown() {
        let delta = if forward {
            amount as isize
        } else {
            -(amount as isize)
        };
        scroll_preview(viewer, delta);
    } else {
        let delta = if forward {
            amount as isize
        } else {
            -(amount as isize)
        };
        vim_textarea::move_vertical_with_curswant(
            &mut viewer.surface.textarea,
            &mut viewer.surface.vim_state,
            delta,
            Some(viewer.surface.wrap_width.get()),
            viewer.surface.mode == PopupMode::Insert,
        );
        if !viewer.folds.folded_ranges.is_empty() {
            skip_folded_cursor(viewer, forward);
        }
        ensure_cursor_visible(viewer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn plain_enter_inserts_newline_in_file_editor_with_submit_setting() {
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        app.settings.submit_on_enter = true;
        let session_id = app.filtered_session_order[0];
        let Some(state) = app.sessions.get_mut(&session_id) else {
            panic!("session")
        };
        let mut viewer =
            FileViewerState::new(PathBuf::from("/tmp/editor-enter.txt"), "first".into());
        viewer.surface.mode = PopupMode::Insert;
        viewer
            .surface
            .textarea
            .move_cursor(tui_textarea::CursorMove::End);
        state.file_viewer = Some(viewer);

        assert!(handle_file_viewer_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
        ));
        let Some(viewer) = app
            .sessions
            .get(&session_id)
            .and_then(|state| state.file_viewer.as_ref())
        else {
            panic!("file viewer")
        };
        assert_eq!(viewer.surface.content(), "first\n");
        assert!(viewer.dirty);
    }

    #[test]
    fn test_search_finds_all_occurrences() {
        let lines = vec![
            "fn foo() {".to_string(),
            "    let foo = 1;".to_string(),
            "    println!(\"{foo}\");".to_string(),
            "}".to_string(),
        ];
        let pat = "foo";
        let mut matches = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let mut start = 0;
            while let Some(off) = line[start..].find(pat) {
                let abs = start + off;
                matches.push((i, abs, abs + pat.len()));
                start = abs + 1;
            }
        }
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0], (0, 3, 6));
        assert_eq!(matches[1], (1, 8, 11));
        assert_eq!(matches[2], (2, 15, 18));
    }

    #[test]
    fn mouse_wheel_moves_through_wrapped_source_rows() {
        let mut viewer = FileViewerState::new(PathBuf::from("/tmp/wrapped.txt"), "x".repeat(80));
        viewer.surface.wrap_width.set(10);

        scroll_from_mouse(&mut viewer, 3, true);

        assert_eq!(viewer.surface.textarea.cursor(), (0, 30));
    }

    #[test]
    fn test_fold_regions_rust_function() {
        let src = "fn foo() {\n    let x = 1;\n}\n";
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter::Language::from(tree_sitter_rust::LANGUAGE))
            .unwrap();
        let tree = parser.parse(src, None).unwrap();

        let mut folds = FoldState::default();
        recompute_foldable_regions(&mut folds, &tree);

        assert!(
            folds
                .foldable_regions
                .iter()
                .any(|&(s, e)| s == 0 && e == 2),
            "expected block fold region (0, 2), got: {:?}",
            folds.foldable_regions
        );
    }

    #[test]
    fn test_cursor_skip_over_fold() {
        let mut folds = FoldState::default();
        folds.folded_ranges.insert((0, 3));

        let row: usize = 2;
        let fold = folds
            .folded_ranges
            .iter()
            .find(|&&(start, end)| row > start && row <= end)
            .copied();
        let (start, end) = fold.unwrap();
        let expected_target = end + 1;
        assert_eq!(expected_target, 4);
        assert_eq!(start, 0);
    }

    #[test]
    fn test_word_under_cursor() {
        let lines = vec!["let my_var = 42;".to_string()];
        // col 4 = 'm' in "my_var"
        assert_eq!(word_under_cursor(&lines, 0, 4), Some("my_var".to_string()));
        // col 0 = 'l' in "let"
        assert_eq!(word_under_cursor(&lines, 0, 0), Some("let".to_string()));
        // col 11 = ' ' (space)
        assert_eq!(word_under_cursor(&lines, 0, 11), None);
    }

    // --- auto-pair multibyte regression tests ---
    // Pre-fix, handle_auto_pair peeked next/prev chars via byte slices indexed
    // with the char-based cursor column, panicking mid-char on multibyte lines.

    fn viewer_with(content: &str) -> FileViewerState {
        FileViewerState::new(
            std::path::PathBuf::from("/nonexistent/rsi_test_auto_pair.txt"),
            content.to_string(),
        )
    }

    #[test]
    fn test_auto_pair_insert_after_multibyte() {
        let mut viewer = viewer_with("é");
        // Cursor after the multibyte char (char col 1; byte offset 2).
        viewer
            .surface
            .textarea
            .move_cursor(tui_textarea::CursorMove::End);

        let key = KeyEvent::new(KeyCode::Char('('), KeyModifiers::NONE);
        let result = handle_auto_pair(&mut viewer, key);

        assert_eq!(result, Some(true));
        assert_eq!(viewer.surface.textarea.lines(), &["é()"]);
        // Cursor stepped back between the pair.
        assert_eq!(viewer.surface.textarea.cursor(), (0, 2));
    }

    #[test]
    fn test_auto_pair_backspace_between_pair_after_multibyte() {
        let mut viewer = viewer_with("✨()");
        // Cursor between the brackets (char col 2; the emoji is 3 bytes, so
        // pre-fix both the next- and prev-char byte slices landed mid-char).
        viewer
            .surface
            .textarea
            .move_cursor(tui_textarea::CursorMove::Jump(0, 2));

        let key = KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE);
        let result = handle_auto_pair(&mut viewer, key);

        assert_eq!(result, Some(true));
        assert_eq!(viewer.surface.textarea.lines(), &["✨"]);
    }

    #[test]
    fn test_auto_pair_close_skip_over_after_multibyte() {
        let mut viewer = viewer_with("é()");
        // Cursor between the brackets; typing `)` should skip over the
        // existing close instead of inserting a new one.
        viewer
            .surface
            .textarea
            .move_cursor(tui_textarea::CursorMove::Jump(0, 2));

        let key = KeyEvent::new(KeyCode::Char(')'), KeyModifiers::NONE);
        let result = handle_auto_pair(&mut viewer, key);

        assert_eq!(result, Some(false));
        assert_eq!(viewer.surface.textarea.lines(), &["é()"]);
        assert_eq!(viewer.surface.textarea.cursor(), (0, 3));
    }

    // --- External-edit detection tests (RSI #387) ---

    fn make_temp_file(content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("rsi_test_{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&path, content).unwrap();
        path
    }

    fn open_viewer(app: &mut crate::app::App, session_id: Uuid, path: std::path::PathBuf) {
        open_path_in_session(app, session_id, path);
    }

    #[test]
    fn reopen_clean_cache_with_external_change_reloads_from_disk() {
        let path = make_temp_file("original content");
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session_id = app.filtered_session_order[0];

        // First open — creates viewer with disk_content = "original content"
        open_viewer(&mut app, session_id, path.clone());
        // Close viewer (caches it, clean state)
        close_viewer(&mut app, session_id);

        // External edit
        std::fs::write(&path, "externally modified content").unwrap();

        // Reopen — should reload from disk since cache is clean
        open_viewer(&mut app, session_id, path.clone());

        let state = app.sessions.get(&session_id).unwrap();
        let viewer = state.file_viewer.as_ref().unwrap();
        assert_eq!(viewer.surface.content(), "externally modified content");
        assert!(viewer.external_conflict.is_none());
        assert!(!viewer.dirty);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn reopen_dirty_cache_with_external_change_preserves_draft() {
        let path = make_temp_file("original content");
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session_id = app.filtered_session_order[0];

        // First open
        open_viewer(&mut app, session_id, path.clone());

        // Make the viewer dirty (simulate unsaved edits)
        {
            let state = app.sessions.get_mut(&session_id).unwrap();
            let viewer = state.file_viewer.as_mut().unwrap();
            viewer.dirty = true;
            viewer
                .surface
                .textarea
                .move_cursor(tui_textarea::CursorMove::End);
            viewer.surface.textarea.insert_str(" [edited]");
        }

        // Close viewer (caches dirty state)
        close_viewer(&mut app, session_id);

        // External edit
        std::fs::write(&path, "externally modified content").unwrap();

        // Reopen — should preserve draft and set conflict
        open_viewer(&mut app, session_id, path.clone());

        let state = app.sessions.get(&session_id).unwrap();
        let viewer = state.file_viewer.as_ref().unwrap();
        // Draft is preserved
        assert_eq!(viewer.surface.content(), "original content [edited]");
        assert!(viewer.dirty);
        // Conflict is set with the external content
        assert_eq!(
            viewer.external_conflict,
            Some("externally modified content".to_string())
        );
        let Some(notice) = app.notifications.back() else {
            panic!("conflict notification expected");
        };
        assert_eq!(notice.kind, crate::types::NotificationKind::OperationFailed);
        assert!(notice.message.contains("External change detected"));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn external_edit_then_reopen_and_save_preserves_both_versions() {
        let path = make_temp_file("original");
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session_id = app.filtered_session_order[0];
        open_viewer(&mut app, session_id, path.clone());
        {
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
        }
        close_viewer(&mut app, session_id);
        std::fs::write(&path, "external").unwrap();
        open_viewer(&mut app, session_id, path.clone());

        assert!(!save_file(&mut app, session_id, false));
        let viewer = app
            .sessions
            .get(&session_id)
            .unwrap()
            .file_viewer
            .as_ref()
            .unwrap();
        assert_eq!(viewer.surface.content(), "original draft");
        assert_eq!(viewer.external_conflict.as_deref(), Some("external"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "external");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn reopen_no_external_change_reuses_cached_viewer() {
        let path = make_temp_file("unchanged content");
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session_id = app.filtered_session_order[0];

        // First open
        open_viewer(&mut app, session_id, path.clone());

        // Make dirty
        {
            let state = app.sessions.get_mut(&session_id).unwrap();
            let viewer = state.file_viewer.as_mut().unwrap();
            viewer.dirty = true;
            viewer
                .surface
                .textarea
                .move_cursor(tui_textarea::CursorMove::End);
            viewer.surface.textarea.insert_str(" [edited]");
        }

        // Close viewer (caches dirty state)
        close_viewer(&mut app, session_id);

        // No external edit — reopen should reuse cached viewer
        open_viewer(&mut app, session_id, path.clone());

        let state = app.sessions.get(&session_id).unwrap();
        let viewer = state.file_viewer.as_ref().unwrap();
        assert_eq!(viewer.surface.content(), "unchanged content [edited]");
        assert!(viewer.dirty);
        assert!(viewer.external_conflict.is_none());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn save_with_no_external_change_succeeds_and_updates_snapshot() {
        let path = make_temp_file("original content");
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session_id = app.filtered_session_order[0];

        // First open
        open_viewer(&mut app, session_id, path.clone());

        // Edit
        {
            let state = app.sessions.get_mut(&session_id).unwrap();
            let viewer = state.file_viewer.as_mut().unwrap();
            viewer.dirty = true;
            viewer
                .surface
                .textarea
                .move_cursor(tui_textarea::CursorMove::End);
            viewer.surface.textarea.insert_str(" [edited]");
        }

        // Save (no external change)
        save_file(&mut app, session_id, false);

        // Verify disk content
        let disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(disk, "original content [edited]");

        // Verify viewer state
        let state = app.sessions.get(&session_id).unwrap();
        let viewer = state.file_viewer.as_ref().unwrap();
        assert!(!viewer.dirty);
        assert!(viewer.external_conflict.is_none());
        assert_eq!(viewer.disk_content, "original content [edited]");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn save_with_external_change_refused_and_sets_conflict() {
        let path = make_temp_file("original content");
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session_id = app.filtered_session_order[0];

        // First open
        open_viewer(&mut app, session_id, path.clone());

        // Edit
        {
            let state = app.sessions.get_mut(&session_id).unwrap();
            let viewer = state.file_viewer.as_mut().unwrap();
            viewer.dirty = true;
            viewer
                .surface
                .textarea
                .move_cursor(tui_textarea::CursorMove::End);
            viewer.surface.textarea.insert_str(" [edited]");
        }

        // External edit
        std::fs::write(&path, "externally modified").unwrap();

        // Save should be refused
        save_file(&mut app, session_id, false);

        // Verify disk content was NOT overwritten
        let disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(disk, "externally modified");

        // Verify viewer state: still dirty, conflict set
        let state = app.sessions.get(&session_id).unwrap();
        let viewer = state.file_viewer.as_ref().unwrap();
        assert!(viewer.dirty);
        assert_eq!(
            viewer.external_conflict,
            Some("externally modified".to_string())
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn force_save_with_external_change_overwrites_disk() {
        let path = make_temp_file("original content");
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session_id = app.filtered_session_order[0];

        // First open
        open_viewer(&mut app, session_id, path.clone());

        // Edit
        {
            let state = app.sessions.get_mut(&session_id).unwrap();
            let viewer = state.file_viewer.as_mut().unwrap();
            viewer.dirty = true;
            viewer
                .surface
                .textarea
                .move_cursor(tui_textarea::CursorMove::End);
            viewer.surface.textarea.insert_str(" [edited]");
        }

        // External edit
        std::fs::write(&path, "externally modified").unwrap();

        // Force save should overwrite
        save_file(&mut app, session_id, true);

        // Verify disk content was overwritten
        let disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(disk, "original content [edited]");

        // Verify viewer state
        let state = app.sessions.get(&session_id).unwrap();
        let viewer = state.file_viewer.as_ref().unwrap();
        assert!(!viewer.dirty);
        assert!(viewer.external_conflict.is_none());
        assert_eq!(viewer.disk_content, "original content [edited]");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn save_buffer_matching_disk_is_noop_not_conflict() {
        let path = make_temp_file("original content");
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session_id = app.filtered_session_order[0];

        // First open — disk_content = "original content", buffer = "original content"
        open_viewer(&mut app, session_id, path.clone());

        // Mark dirty and edit the buffer to match the disk content exactly.
        {
            let state = app.sessions.get_mut(&session_id).unwrap();
            let viewer = state.file_viewer.as_mut().unwrap();
            viewer.dirty = true;
            *viewer.surface.textarea = tui_textarea::TextArea::from(["original content"]);
        }

        // Now change the disk content to something different — the external
        // change will be detected, but the buffer already matches the new
        // disk content, so the noop branch fires (no data loss, no conflict).
        std::fs::write(&path, "external change").unwrap();

        // Set buffer to match the NEW disk content to trigger the noop branch.
        {
            let state = app.sessions.get_mut(&session_id).unwrap();
            let viewer = state.file_viewer.as_mut().unwrap();
            *viewer.surface.textarea = tui_textarea::TextArea::from(["external change"]);
        }

        save_file(&mut app, session_id, false);

        let state = app.sessions.get(&session_id).unwrap();
        let viewer = state.file_viewer.as_ref().unwrap();
        // The noop branch should have cleared dirty and set disk_content to
        // the new content without setting a conflict.
        assert!(!viewer.dirty);
        assert!(viewer.external_conflict.is_none());
        assert_eq!(viewer.disk_content, "external change");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn save_and_close_success_closes_viewer() {
        let path = make_temp_file("saved content");
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session_id = app.filtered_session_order[0];

        open_viewer(&mut app, session_id, path.clone());
        // Make a real edit so dirty = true
        {
            let state = app.sessions.get_mut(&session_id).unwrap();
            let viewer = state.file_viewer.as_mut().unwrap();
            viewer.surface.textarea.insert_str(" edited");
            viewer.dirty = true;
        }

        execute_file_command(&mut app, session_id, "wq");

        let state = app.sessions.get(&session_id).unwrap();
        assert!(
            state.file_viewer.is_none(),
            "viewer should be closed after successful :wq"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            " editedsaved content",
            "disk content should match edited buffer"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn save_and_close_refused_keeps_viewer_open() {
        let path = make_temp_file("disk content");
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session_id = app.filtered_session_order[0];

        open_viewer(&mut app, session_id, path.clone());
        // Simulate external change while viewer has dirty draft
        std::fs::write(&path, "externally changed").unwrap();
        {
            let state = app.sessions.get_mut(&session_id).unwrap();
            let viewer = state.file_viewer.as_mut().unwrap();
            viewer.dirty = true;
        }

        execute_file_command(&mut app, session_id, "wq");

        let state = app.sessions.get(&session_id).unwrap();
        assert!(
            state.file_viewer.is_some(),
            "viewer should remain open after refused :wq (conflict)"
        );
        let viewer = state.file_viewer.as_ref().unwrap();
        assert!(
            viewer.external_conflict.is_some(),
            "conflict should be set after refused :wq"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "externally changed",
            "disk content should be preserved on refusal"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn save_and_close_with_unreadable_disk_keeps_clean_viewer_open() {
        let path = make_temp_file("original");
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session_id = app.filtered_session_order[0];
        open_viewer(&mut app, session_id, path.clone());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();

        execute_file_command(&mut app, session_id, "wq");

        let viewer = app
            .sessions
            .get(&session_id)
            .unwrap()
            .file_viewer
            .as_ref()
            .unwrap();
        assert_eq!(viewer.surface.content(), "original");
        assert!(viewer.external_conflict.is_some());
        assert!(path.is_dir());
        std::fs::remove_dir(&path).unwrap();
    }

    #[test]
    fn save_refused_unreadable_file_preserves_disk_content() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("rsi_test_unreadable_{}.txt", uuid::Uuid::new_v4()));
        std::fs::write(&path, [0xFF, 0xFE, 0x00, 0x01]).unwrap();

        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session_id = app.filtered_session_order[0];

        // Open the viewer with a placeholder (viewer can be open without
        // being able to read the raw bytes — e.g. restored from cache).
        {
            let state = app.sessions.get_mut(&session_id).unwrap();
            let mut viewer =
                crate::types::FileViewerState::new(path.clone(), "my draft".to_string());
            viewer.disk_content = String::from("placeholder");
            viewer.dirty = true;
            state.file_viewer = Some(viewer);
        }

        // Attempt save — read_to_string fails (non-UTF-8), so the save must
        // be refused and the disk content preserved.
        save_file(&mut app, session_id, false);

        let state = app.sessions.get(&session_id).unwrap();
        let viewer = state.file_viewer.as_ref().unwrap();
        assert!(
            viewer.external_conflict.is_some(),
            "non-UTF-8 file should set external_conflict on save attempt"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            [0xFF, 0xFE, 0x00, 0x01],
            "non-UTF-8 disk content must be preserved when save is refused"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn open_different_file_preserves_dirty_draft_in_cache() {
        let path_a = make_temp_file("file A content");
        let path_b = make_temp_file("file B content");
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session_id = app.filtered_session_order[0];

        // Open file A and make a dirty edit
        open_viewer(&mut app, session_id, path_a.clone());
        {
            let state = app.sessions.get_mut(&session_id).unwrap();
            let viewer = state.file_viewer.as_mut().unwrap();
            viewer.surface.textarea.insert_str(" my draft edit");
            viewer.dirty = true;
        }

        // Open file B — the dirty draft for A must be cached, not discarded
        open_viewer(&mut app, session_id, path_b.clone());

        let state = app.sessions.get(&session_id).unwrap();
        assert!(
            state
                .file_viewer
                .as_ref()
                .map(|v| v.file_path == path_b)
                .unwrap_or(false),
            "viewer should now show file B"
        );

        // Switch back to file A — the dirty draft must be restored from cache
        let restored = {
            let state = app.sessions.get_mut(&session_id).unwrap();
            let cached = state
                .file_viewer_cache
                .remove(&path_a)
                .expect("dirty draft for A must be preserved in cache");
            assert!(cached.dirty, "restored draft must remain dirty");
            assert!(
                cached
                    .surface
                    .textarea
                    .lines()
                    .iter()
                    .any(|l| l.contains("my draft edit")),
                "restored draft must contain the user's edit"
            );
            state.file_viewer = Some(cached);
            true
        };
        assert!(restored);

        std::fs::remove_file(&path_a).ok();
        std::fs::remove_file(&path_b).ok();
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn activate_cached_viewer_preserves_dirty_draft_and_disk_conflict() {
        let path = std::path::PathBuf::from("helper-test.rs");
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session_id = app.filtered_session_order[0];
        let state = app.sessions.get_mut(&session_id).unwrap();
        assert!(!activate_cached_viewer(
            state,
            path.clone(),
            "original".into()
        ));
        let viewer = state.file_viewer.as_mut().unwrap();
        viewer
            .surface
            .textarea
            .move_cursor(tui_textarea::CursorMove::End);
        viewer.surface.textarea.insert_str(" draft");
        viewer.dirty = true;
        assert!(activate_cached_viewer(state, path, "external".into()));
        let viewer = state.file_viewer.as_ref().unwrap();
        assert_eq!(viewer.surface.content(), "original draft");
        assert_eq!(viewer.external_conflict.as_deref(), Some("external"));
    }

    #[test]
    fn revert_clears_conflict_and_reloads_from_disk() {
        let path = make_temp_file("original content");
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session_id = app.filtered_session_order[0];

        // First open
        open_viewer(&mut app, session_id, path.clone());

        // Make dirty
        {
            let state = app.sessions.get_mut(&session_id).unwrap();
            let viewer = state.file_viewer.as_mut().unwrap();
            viewer.dirty = true;
            viewer
                .surface
                .textarea
                .move_cursor(tui_textarea::CursorMove::End);
            viewer.surface.textarea.insert_str(" [edited]");
        }

        // Close, external edit, reopen to create conflict
        close_viewer(&mut app, session_id);
        std::fs::write(&path, "externally modified").unwrap();
        open_viewer(&mut app, session_id, path.clone());

        // Verify conflict is set
        {
            let state = app.sessions.get(&session_id).unwrap();
            let viewer = state.file_viewer.as_ref().unwrap();
            assert!(viewer.external_conflict.is_some());
        }

        // Revert via command
        execute_file_command(&mut app, session_id, "e!");

        // Verify conflict cleared and content reloaded from disk
        let state = app.sessions.get(&session_id).unwrap();
        let viewer = state.file_viewer.as_ref().unwrap();
        assert_eq!(viewer.surface.content(), "externally modified");
        assert!(!viewer.dirty);
        assert!(viewer.external_conflict.is_none());
        assert_eq!(viewer.disk_content, "externally modified");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn save_refused_then_force_save_clears_conflict() {
        let path = make_temp_file("original content");
        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let session_id = app.filtered_session_order[0];

        // First open
        open_viewer(&mut app, session_id, path.clone());

        // Edit
        {
            let state = app.sessions.get_mut(&session_id).unwrap();
            let viewer = state.file_viewer.as_mut().unwrap();
            viewer.dirty = true;
            viewer
                .surface
                .textarea
                .move_cursor(tui_textarea::CursorMove::End);
            viewer.surface.textarea.insert_str(" [edited]");
        }

        // External edit
        std::fs::write(&path, "externally modified").unwrap();

        // First save attempt refused
        save_file(&mut app, session_id, false);
        {
            let state = app.sessions.get(&session_id).unwrap();
            let viewer = state.file_viewer.as_ref().unwrap();
            assert!(viewer.external_conflict.is_some());
        }

        // Force save clears conflict
        save_file(&mut app, session_id, true);
        {
            let state = app.sessions.get(&session_id).unwrap();
            let viewer = state.file_viewer.as_ref().unwrap();
            assert!(viewer.external_conflict.is_none());
            assert!(!viewer.dirty);
        }

        std::fs::remove_file(&path).ok();
    }
}
