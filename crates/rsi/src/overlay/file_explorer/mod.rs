//! File explorer drawer: key, mouse and paste handling plus its wiring to the
//! per-session file viewer.
//!
//! State lives in [`state::FileExplorerState`]; filesystem mutations live in
//! [`fs_ops`]. Keys follow LazyVim's neo-tree where it does not clash with
//! rsi conventions: `a` add (trailing `/` = directory), `A` add directory,
//! `r` rename, `m` move, `c` copy, `d` delete (confirmed, recoverable with
//! `u`), `h`/`l` close/open nodes, `P` follow preview, `z` collapse all.

pub(crate) mod fs_ops;
pub mod state;

#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Position;
use uuid::Uuid;

use crate::app::App;
use crate::types::{FileExplorerEntry, FileViewerState, OverlayState};
use fs_ops::UndoOp;
pub use state::{
    ExplorerFinder, ExplorerPrompt, FileExplorerState, PromptKind, RESIZE_STEP, drawer_width,
};

/// Trash batches older than this are pruned when the explorer first opens.
const TRASH_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Rows moved per mouse-wheel notch.
const MOUSE_SCROLL_ROWS: isize = 3;
/// Finder results kept after scoring.
const MAX_FINDER_RESULTS: usize = 100;
/// Largest file the viewer opens.
const MAX_VIEWER_BYTES: u64 = 1_048_576;

fn explorer(app: &App) -> Option<&FileExplorerState> {
    match &app.overlay {
        OverlayState::FileExplorer(state) => Some(state),
        _ => None,
    }
}

fn explorer_mut(app: &mut App) -> Option<&mut FileExplorerState> {
    match &mut app.overlay {
        OverlayState::FileExplorer(state) => Some(state),
        _ => None,
    }
}

#[cfg(not(test))]
fn default_trash_dir() -> PathBuf {
    rsi_common::identity::data_path("explorer-trash", "explorer-trash")
}

#[cfg(test)]
fn default_trash_dir() -> PathBuf {
    std::env::temp_dir().join(format!("rsi-explorer-trash-test-{}", std::process::id()))
}

/// Prune old trash batches once per process, off the UI thread.
#[cfg(not(test))]
fn prune_trash_once(dir: PathBuf) {
    static PRUNED: std::sync::Once = std::sync::Once::new();
    PRUNED.call_once(move || {
        std::thread::spawn(move || fs_ops::prune_trash(&dir, TRASH_MAX_AGE));
    });
}

#[cfg(test)]
fn prune_trash_once(_dir: PathBuf) {
    let _ = TRASH_MAX_AGE;
}

// =============================================================================
// Session / viewer targeting
// =============================================================================

/// Session whose file viewer the explorer drives (and new files open in):
/// the focused detail pane's session, or the session selected in the
/// focused list — the same session `file_viewer` routes keys to.
fn viewer_session_id(app: &App) -> Option<Uuid> {
    app.selected_session_id()
}

/// Whether a file viewer is showing beside the explorer.
pub fn viewer_is_active(app: &App) -> bool {
    viewer_session_id(app)
        .and_then(|id| app.sessions.get(&id))
        .is_some_and(|state| state.file_viewer.is_some())
}

fn active_viewer_path(app: &App) -> Option<PathBuf> {
    let id = viewer_session_id(app)?;
    app.sessions
        .get(&id)?
        .file_viewer
        .as_ref()
        .map(|viewer| viewer.file_path.clone())
}

/// Files the tree decorates: the one open in the viewer and unsaved drafts.
#[derive(Debug, Default)]
pub(crate) struct ViewerMarks {
    pub active: Option<PathBuf>,
    pub dirty: HashSet<PathBuf>,
}

pub(crate) fn viewer_marks(app: &App) -> ViewerMarks {
    let Some(session) = viewer_session_id(app).and_then(|id| app.sessions.get(&id)) else {
        return ViewerMarks::default();
    };
    ViewerMarks {
        active: session.file_viewer.as_ref().map(|v| v.file_path.clone()),
        dirty: session
            .file_viewer
            .iter()
            .chain(session.file_viewer_cache.values())
            .filter(|viewer| viewer.dirty)
            .map(|viewer| viewer.file_path.clone())
            .collect(),
    }
}

/// Point open viewers (and cached drafts) at `to` after `from` moved, so a
/// later save writes to the new location instead of resurrecting the old one.
fn retarget_viewers(app: &mut App, from: &Path, to: &Path) {
    for session in app.sessions.values_mut() {
        if let Some(viewer) = session.file_viewer.as_mut() {
            retarget_viewer(viewer, from, to);
        }
        let moved: Vec<PathBuf> = session
            .file_viewer_cache
            .keys()
            .filter(|path| path.starts_with(from))
            .cloned()
            .collect();
        for key in moved {
            if let Some(mut viewer) = session.file_viewer_cache.remove(&key) {
                retarget_viewer(&mut viewer, from, to);
                session
                    .file_viewer_cache
                    .insert(viewer.file_path.clone(), viewer);
            }
        }
    }
}

fn retarget_viewer(viewer: &mut FileViewerState, from: &Path, to: &Path) {
    let Ok(rel) = viewer.file_path.strip_prefix(from) else {
        return;
    };
    viewer.file_path = if rel.as_os_str().is_empty() {
        to.to_path_buf()
    } else {
        to.join(rel)
    };
    viewer.language_ext = viewer
        .file_path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase);
    viewer.highlight_cache = None;
    viewer.cached_content_version = u64::MAX;
    viewer.markdown_cache = None;
}

// =============================================================================
// Opening / closing
// =============================================================================

/// Open the file explorer overlay rooted at the current project's directory,
/// revealing the file currently open in the viewer.
pub fn open_file_explorer(app: &mut App) {
    let Some(root) = app.current_project().and_then(|p| p.path.clone()) else {
        app.notify("No project selected — file explorer requires an active project");
        return;
    };
    let trash_dir = default_trash_dir();
    prune_trash_once(trash_dir.clone());
    let mut state = FileExplorerState::new(root, trash_dir);
    state.width = app.file_explorer_width;
    if let Some(path) = active_viewer_path(app) {
        state.reveal(&path);
    }
    app.overlay = OverlayState::FileExplorer(Box::new(state));
}

/// `Space e` from the file viewer: close the explorer when it is open,
/// otherwise open it on the viewer's file.
pub fn toggle_from_viewer(app: &mut App) {
    if matches!(app.overlay, OverlayState::FileExplorer(_)) {
        app.overlay = OverlayState::None;
    } else if matches!(app.overlay, OverlayState::None) {
        open_file_explorer(app);
    }
}

/// The viewer closed: hand keyboard focus back to the tree.
pub fn on_viewer_closed(app: &mut App) {
    if let Some(state) = explorer_mut(app) {
        state.explorer_focused = true;
    }
}

fn notify_open_failure(app: &mut App, quiet: bool, message: String) -> bool {
    if !quiet {
        app.notify_error(message);
    }
    false
}

/// Open `path` in the session file viewer. `focus_viewer` moves keyboard
/// focus to the viewer; `quiet` suppresses failure notices (follow preview).
fn open_file(app: &mut App, path: &Path, focus_viewer: bool, quiet: bool) -> bool {
    let Some(session_id) = viewer_session_id(app) else {
        if !quiet {
            app.notify("Select a session first — the file viewer opens inside a session");
        }
        return false;
    };

    if let Some(project_path) = app.current_project().and_then(|p| p.path.clone())
        && !crate::file_utils::is_within_project_scope(&project_path, path)
    {
        return notify_open_failure(app, quiet, "File is outside project scope".to_string());
    }
    if !path.is_file() {
        return notify_open_failure(app, quiet, "Not a file".to_string());
    }
    let size = match std::fs::metadata(path) {
        Ok(meta) => meta.len(),
        Err(e) => return notify_open_failure(app, quiet, format!("Cannot read: {e}")),
    };
    if size > MAX_VIEWER_BYTES {
        return notify_open_failure(
            app,
            quiet,
            format!(
                "File too large: {} (max {})",
                fs_ops::human_size(size),
                fs_ops::human_size(MAX_VIEWER_BYTES)
            ),
        );
    }
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            return notify_open_failure(
                app,
                quiet,
                format!("Binary or non-UTF-8 file: {}", fs_ops::name_of(path)),
            );
        }
        Err(e) => return notify_open_failure(app, quiet, format!("Cannot read: {e}")),
    };

    let Some(session) = app.sessions.get_mut(&session_id) else {
        return false;
    };
    let conflict = crate::file_viewer::activate_cached_viewer(session, path.to_path_buf(), content);
    if conflict {
        app.notify_error(format!(
            "External change detected: {} (use :e! to reload or :w! to overwrite)",
            path.display()
        ));
    }

    if let Some(state) = explorer_mut(app) {
        state.reveal(path);
        if focus_viewer {
            state.explorer_focused = false;
        }
    }
    true
}

/// Open a file by absolute path in the file viewer and focus it.
pub fn open_file_by_path(app: &mut App, path: &Path) {
    open_file(app, path, true, false);
}

// =============================================================================
// Key routing
// =============================================================================

fn is_ctrl_char(key: &KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(c)
}

/// Route a key while the explorer overlay is open. Returns false when the key
/// belongs to the file viewer (the viewer has focus and is still open).
pub(super) fn route_key(app: &mut App, key: KeyEvent) -> bool {
    let Some(state) = explorer(app) else {
        return false;
    };
    let owns_input = state.explorer_focused || state.finder.active || state.prompt.is_some();
    if !owns_input {
        if is_ctrl_char(&key, 'h') {
            if let Some(state) = explorer_mut(app) {
                state.explorer_focused = true;
            }
            return true;
        }
        if viewer_is_active(app) {
            return false;
        }
        // The viewer closed underneath the explorer: the tree takes over.
        if let Some(state) = explorer_mut(app) {
            state.explorer_focused = true;
        }
    }
    handle_file_explorer_key(app, key);
    true
}

pub(super) fn handle_file_explorer_key(app: &mut App, key: KeyEvent) {
    let Some(state) = explorer(app) else {
        return;
    };
    if state.prompt.is_some() {
        handle_prompt_key(app, key);
    } else if state.finder.active {
        handle_finder_key(app, key);
    } else {
        handle_tree_key(app, key);
    }
}

#[derive(Clone, Copy)]
enum YankKind {
    Absolute,
    Name,
    Relative,
}

#[derive(Clone, Copy)]
enum PromptAction {
    Add,
    AddDirectory,
    Rename,
    Move,
    Copy,
    Delete,
}

fn handle_tree_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    if key.modifiers.contains(KeyModifiers::ALT) {
        return;
    }

    // Second key of a `y` chord; any other key cancels the chord and then
    // acts normally.
    let pending = explorer_mut(app).and_then(|state| state.pending_key.take());
    if pending == Some('y') && !ctrl {
        match key.code {
            KeyCode::Char('y') => return yank(app, YankKind::Absolute),
            KeyCode::Char('n') => return yank(app, YankKind::Name),
            KeyCode::Char('r') => return yank(app, YankKind::Relative),
            _ => {}
        }
    }

    if ctrl {
        match key.code {
            KeyCode::Left if shift => resize(app, -RESIZE_STEP),
            KeyCode::Right if shift => resize(app, RESIZE_STEP),
            KeyCode::Char('l') => focus_viewer(app),
            KeyCode::Char('0') => reset_width(app),
            KeyCode::Char('d') => move_by_half_page(app, 1),
            KeyCode::Char('u') => move_by_half_page(app, -1),
            KeyCode::Char('n') => move_selection(app, 1),
            KeyCode::Char('p') => move_selection(app, -1),
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q' | ' ') => app.overlay = OverlayState::None,
        KeyCode::Char('j') | KeyCode::Down => move_selection(app, 1),
        KeyCode::Char('k') | KeyCode::Up => move_selection(app, -1),
        KeyCode::Char('g') | KeyCode::Home => move_selection(app, isize::MIN),
        KeyCode::Char('G') | KeyCode::End => move_selection(app, isize::MAX),
        KeyCode::PageDown => move_by_half_page(app, 2),
        KeyCode::PageUp => move_by_half_page(app, -2),
        KeyCode::Enter | KeyCode::Char('o') => activate_selected(app, true),
        KeyCode::Char('l') | KeyCode::Right => {
            let handled_dir = explorer_mut(app).is_some_and(FileExplorerState::open_node);
            if !handled_dir {
                activate_selected(app, true);
            }
        }
        KeyCode::Char('h') | KeyCode::Left => {
            if let Some(state) = explorer_mut(app) {
                state.close_node();
            }
        }
        KeyCode::Tab => activate_selected(app, false),
        KeyCode::Char('P') => toggle_follow_preview(app),
        KeyCode::Char('z') => {
            if let Some(state) = explorer_mut(app) {
                state.collapse_all();
            }
        }
        KeyCode::Char('R') => {
            if let Some(state) = explorer_mut(app) {
                state.finder.cache.clear();
                state.refresh();
            }
            app.notify("Explorer refreshed");
        }
        KeyCode::Char('a') => start_prompt(app, PromptAction::Add),
        KeyCode::Char('A') => start_prompt(app, PromptAction::AddDirectory),
        KeyCode::Char('r') => start_prompt(app, PromptAction::Rename),
        KeyCode::Char('m') => start_prompt(app, PromptAction::Move),
        KeyCode::Char('c') => start_prompt(app, PromptAction::Copy),
        KeyCode::Char('d') => start_prompt(app, PromptAction::Delete),
        KeyCode::Char('u') => undo_last(app),
        KeyCode::Char('y') => {
            if let Some(state) = explorer_mut(app) {
                state.pending_key = Some('y');
            }
        }
        KeyCode::Char('i') => show_info(app),
        KeyCode::Char('.' | 'H') => {
            if let Some(state) = explorer_mut(app) {
                state.toggle_hidden();
            }
        }
        KeyCode::Char('/') => activate_finder(app),
        KeyCode::Char('?') => crate::overlay::open_keybindings_help(app),
        KeyCode::Char('<') => resize(app, -RESIZE_STEP),
        KeyCode::Char('>') => resize(app, RESIZE_STEP),
        KeyCode::Char('=') => reset_width(app),
        _ => {}
    }
}

fn move_selection(app: &mut App, delta: isize) {
    if let Some(state) = explorer_mut(app) {
        state.move_selection(delta);
    }
    after_move(app);
}

fn move_by_half_page(app: &mut App, halves: isize) {
    let delta = explorer(app).map_or(0, |state| state.half_page() * halves);
    move_selection(app, delta);
}

/// Follow preview: show the newly selected file without leaving the tree.
fn after_move(app: &mut App) {
    let Some(state) = explorer(app) else {
        return;
    };
    if !state.follow_preview {
        return;
    }
    if let Some(FileExplorerEntry::File { path, .. }) = state.selected_entry() {
        let path = path.clone();
        open_file(app, &path, false, true);
    }
}

fn toggle_follow_preview(app: &mut App) {
    let Some(state) = explorer_mut(app) else {
        return;
    };
    state.follow_preview = !state.follow_preview;
    let on = state.follow_preview;
    app.notify(if on {
        "Follow preview on: moving opens files in the viewer"
    } else {
        "Follow preview off"
    });
    if on {
        after_move(app);
    }
}

/// Enter/o/l/Tab: toggle a directory, or open the selected file (moving
/// focus to the viewer when `focus_viewer`).
fn activate_selected(app: &mut App, focus_viewer: bool) {
    let Some(state) = explorer_mut(app) else {
        return;
    };
    let idx = state.selected_index;
    match state.entries.get(idx) {
        Some(FileExplorerEntry::Directory { .. }) => state.toggle_at(idx),
        Some(FileExplorerEntry::File { path, .. }) => {
            let path = path.clone();
            open_file(app, &path, focus_viewer, false);
        }
        None => {}
    }
}

fn focus_viewer(app: &mut App) {
    if !viewer_is_active(app) {
        app.notify("No file open — Enter opens the selected file");
        return;
    }
    if let Some(state) = explorer_mut(app) {
        state.explorer_focused = false;
    }
}

// =============================================================================
// Drawer width
// =============================================================================

fn terminal_width(app: &App) -> u16 {
    match app.last_terminal_size.0 {
        0 => 120,
        width => width,
    }
}

fn store_width(app: &mut App, width: Option<u16>) {
    if let Some(state) = explorer_mut(app) {
        state.width = width;
    }
    app.file_explorer_width = width;
    // The surface beside the drawer shifts; repaint every cell.
    app.pane_switch_clear = true;
    crate::state::PersistedState::capture(app).save();
}

/// Widen (positive `delta`) or narrow the drawer by `delta` columns.
fn resize(app: &mut App, delta: i32) {
    let term_width = terminal_width(app);
    let Some(state) = explorer(app) else {
        return;
    };
    let current = i32::from(drawer_width(term_width, state.width));
    let requested = (current + delta).clamp(0, i32::from(u16::MAX)) as u16;
    let width = drawer_width(term_width, Some(requested));
    store_width(app, Some(width));
}

fn reset_width(app: &mut App) {
    store_width(app, None);
}

// =============================================================================
// Clipboard and info
// =============================================================================

fn yank(app: &mut App, kind: YankKind) {
    let Some(state) = explorer(app) else {
        return;
    };
    let Some(path) = state.selected_path() else {
        return;
    };
    let text = match kind {
        YankKind::Absolute => path.display().to_string(),
        YankKind::Name => fs_ops::name_of(&path),
        YankKind::Relative => state.display_relative(&path),
    };
    crate::clipboard::osc52_copy(&text);
    app.notify_success(format!("Copied: {text}"));
}

fn show_info(app: &mut App) {
    let Some(path) = explorer(app).and_then(FileExplorerState::selected_path) else {
        return;
    };
    match fs_ops::describe(&path) {
        Ok(summary) => app.notify(summary),
        Err(e) => app.notify_error(e),
    }
}

// =============================================================================
// Prompts: add / rename / move / copy / delete
// =============================================================================

/// Char index of the extension dot in a file name (cursor lands before it).
fn stem_cursor(name: &str) -> Option<usize> {
    let dot = name.rfind('.').filter(|&i| i > 0)?;
    Some(name[..dot].chars().count())
}

fn start_prompt(app: &mut App, action: PromptAction) {
    let Some(state) = explorer_mut(app) else {
        return;
    };
    let selected = state.selected_path();
    let prompt = match action {
        PromptAction::Add => ExplorerPrompt::new(
            PromptKind::Add {
                dir: state.target_dir_for_add(),
            },
            "",
        ),
        PromptAction::AddDirectory => ExplorerPrompt::new(
            PromptKind::AddDirectory {
                dir: state.target_dir_for_add(),
            },
            "",
        ),
        _ => {
            let Some(target) = selected else {
                app.notify("Nothing selected");
                return;
            };
            match action {
                PromptAction::Rename => {
                    let name = fs_ops::name_of(&target);
                    let is_file = !target.is_dir();
                    let mut prompt = ExplorerPrompt::new(PromptKind::Rename { target }, name);
                    if is_file && let Some(cursor) = stem_cursor(&prompt.input) {
                        prompt.cursor = cursor;
                    }
                    prompt
                }
                PromptAction::Move => {
                    let initial = state.display_relative(&target);
                    ExplorerPrompt::new(PromptKind::Move { target }, initial)
                }
                PromptAction::Copy => {
                    let initial = state.display_relative(&target);
                    ExplorerPrompt::new(PromptKind::Copy { source: target }, initial)
                }
                _ => ExplorerPrompt::new(PromptKind::ConfirmDelete { target }, ""),
            }
        }
    };
    state.prompt = Some(prompt);
}

/// Directory a prompt's relative input is resolved against.
fn prompt_base(root: &Path, kind: &PromptKind) -> PathBuf {
    match kind {
        PromptKind::Add { dir } | PromptKind::AddDirectory { dir } => dir.clone(),
        PromptKind::Rename { target } => target
            .parent()
            .map_or_else(|| root.to_path_buf(), Path::to_path_buf),
        PromptKind::Move { .. } | PromptKind::Copy { .. } | PromptKind::ConfirmDelete { .. } => {
            root.to_path_buf()
        }
    }
}

fn handle_prompt_key(app: &mut App, key: KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let Some(state) = explorer_mut(app) else {
        return;
    };
    let root = state.root.clone();
    let show_hidden = state.show_hidden;
    let Some(prompt) = state.prompt.as_mut() else {
        return;
    };

    if prompt.is_confirmation() {
        match key.code {
            KeyCode::Enter | KeyCode::Char('y' | 'Y' | 'd') if !ctrl => confirm_delete(app),
            KeyCode::Esc | KeyCode::Char('n' | 'N' | 'q') => state.prompt = None,
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Esc => state.prompt = None,
        KeyCode::Enter => submit_prompt(app),
        KeyCode::Tab => {
            let base = prompt_base(&root, &prompt.kind);
            if let Some(completed) = complete_path(&base, &prompt.input, show_hidden) {
                prompt.input = completed;
                prompt.end();
            }
        }
        KeyCode::Backspace => prompt.backspace(),
        KeyCode::Delete => prompt.delete(),
        KeyCode::Left => prompt.left(),
        KeyCode::Right => prompt.right(),
        KeyCode::Home => prompt.home(),
        KeyCode::End => prompt.end(),
        KeyCode::Char(c) if ctrl => match c {
            'a' => prompt.home(),
            'e' => prompt.end(),
            'b' => prompt.left(),
            'f' => prompt.right(),
            'h' => prompt.backspace(),
            'u' => prompt.clear_to_start(),
            'w' => prompt.delete_word_back(),
            _ => {}
        },
        KeyCode::Char(c) if !alt => prompt.insert_char(c),
        _ => {}
    }
}

/// Complete the last path segment of `input` against the directory it names
/// (relative to `base`): a unique match completes fully (directories gain a
/// trailing `/`), several matches complete their longest common prefix.
pub(crate) fn complete_path(base: &Path, input: &str, show_hidden: bool) -> Option<String> {
    let (dir_part, partial) = match input.rfind('/') {
        Some(i) => (&input[..=i], &input[i + 1..]),
        None => ("", input),
    };
    let dir = if Path::new(dir_part).is_absolute() {
        PathBuf::from(dir_part)
    } else {
        base.join(dir_part)
    };
    let mut matches: Vec<(String, bool)> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let hidden_ok = show_hidden || partial.starts_with('.') || !name.starts_with('.');
            (hidden_ok && name.starts_with(partial)).then(|| (name, entry.path().is_dir()))
        })
        .collect();
    matches.sort();
    match matches.as_slice() {
        [] => None,
        [(name, is_dir)] => Some(format!(
            "{dir_part}{name}{}",
            if *is_dir { "/" } else { "" }
        )),
        [(first, _), rest @ ..] => {
            let mut prefix: Vec<char> = first.chars().collect();
            for (name, _) in rest {
                let common = prefix
                    .iter()
                    .zip(name.chars())
                    .take_while(|(a, b)| **a == *b)
                    .count();
                prefix.truncate(common);
            }
            let prefix: String = prefix.into_iter().collect();
            (prefix.chars().count() > partial.chars().count())
                .then(|| format!("{dir_part}{prefix}"))
        }
    }
}

/// A filesystem operation that succeeded.
struct OpResult {
    undo: UndoOp,
    select: PathBuf,
    message: String,
    moved: Option<(PathBuf, PathBuf)>,
}

fn relative_label(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).map_or_else(
        |_| path.display().to_string(),
        |rel| rel.display().to_string(),
    )
}

/// Execute a text prompt. `Ok(None)` means nothing to do (unchanged name).
fn run_prompt(root: &Path, prompt: &ExplorerPrompt) -> Result<Option<OpResult>, String> {
    let input = prompt.input.as_str();
    let base = prompt_base(root, &prompt.kind);
    match &prompt.kind {
        PromptKind::Add { .. } => create(root, &base, input, fs_ops::wants_directory(input)),
        PromptKind::AddDirectory { .. } => create(root, &base, input, true),
        PromptKind::Rename { target } => {
            let dest = fs_ops::resolve_destination(root, &base, input)?;
            transfer(root, target, dest, false)
        }
        PromptKind::Move { target } => {
            let dest = into_existing_dir(fs_ops::resolve_destination(root, &base, input)?, target);
            transfer(root, target, dest, false)
        }
        PromptKind::Copy { source } => {
            let dest = into_existing_dir(fs_ops::resolve_destination(root, &base, input)?, source);
            transfer(root, source, dest, true)
        }
        PromptKind::ConfirmDelete { .. } => Ok(None),
    }
}

/// `mv a dir/` semantics: a destination that is an existing directory
/// receives the source under its own name.
fn into_existing_dir(dest: PathBuf, source: &Path) -> PathBuf {
    if dest != source
        && dest.is_dir()
        && let Some(name) = source.file_name()
    {
        dest.join(name)
    } else {
        dest
    }
}

fn create(root: &Path, base: &Path, input: &str, as_dir: bool) -> Result<Option<OpResult>, String> {
    let path = fs_ops::resolve_destination(root, base, input)?;
    let top = fs_ops::create_path(&path, as_dir)?;
    let label = relative_label(root, &path);
    Ok(Some(OpResult {
        undo: UndoOp::Created { path: top },
        message: format!("Created {label}{}", if as_dir { "/" } else { "" }),
        select: path,
        moved: None,
    }))
}

fn transfer(root: &Path, from: &Path, to: PathBuf, copy: bool) -> Result<Option<OpResult>, String> {
    if to == from {
        return Ok(None);
    }
    let label = relative_label(root, &to);
    if copy {
        fs_ops::copy_path(from, &to)?;
        return Ok(Some(OpResult {
            undo: UndoOp::Copied { path: to.clone() },
            message: format!("Copied to {label}"),
            select: to,
            moved: None,
        }));
    }
    fs_ops::move_path(from, &to)?;
    let message = if from.parent() == to.parent() {
        format!("Renamed to {}", fs_ops::name_of(&to))
    } else {
        format!("Moved to {label}")
    };
    Ok(Some(OpResult {
        undo: UndoOp::Moved {
            from: from.to_path_buf(),
            to: to.clone(),
        },
        message,
        select: to.clone(),
        moved: Some((from.to_path_buf(), to)),
    }))
}

fn submit_prompt(app: &mut App) {
    let Some(state) = explorer_mut(app) else {
        return;
    };
    let Some(prompt) = state.prompt.take() else {
        return;
    };
    let root = state.root.clone();
    match run_prompt(&root, &prompt) {
        Ok(Some(done)) => apply_result(app, done),
        Ok(None) => {}
        Err(message) => {
            // Keep the prompt open so the input can be corrected.
            if let Some(state) = explorer_mut(app) {
                state.prompt = Some(prompt);
            }
            app.notify_error(message);
        }
    }
}

fn apply_result(app: &mut App, done: OpResult) {
    if let Some((from, to)) = &done.moved {
        retarget_viewers(app, from, to);
    }
    if let Some(state) = explorer_mut(app) {
        state.push_undo(done.undo);
        state.finder.cache.clear();
        state.refresh();
        state.reveal(&done.select);
    }
    app.notify_success(done.message);
}

/// Whether `path` is a real entry below `root` (its parent resolves inside
/// the root, so deleting a symlink only removes the link).
fn entry_in_root(root: &Path, path: &Path) -> bool {
    if path == root || !path.starts_with(root) {
        return false;
    }
    let canon_root = root.canonicalize();
    let canon_parent = path.parent().map(Path::canonicalize);
    matches!((canon_root, canon_parent), (Ok(r), Some(Ok(p))) if p.starts_with(&r))
}

fn confirm_delete(app: &mut App) {
    let Some(state) = explorer_mut(app) else {
        return;
    };
    let Some(ExplorerPrompt {
        kind: PromptKind::ConfirmDelete { target },
        ..
    }) = state.prompt.take()
    else {
        return;
    };
    if !entry_in_root(&state.root, &target) {
        app.notify_error("Cannot delete: file is outside project scope");
        return;
    }
    let name = fs_ops::name_of(&target);
    match fs_ops::move_to_trash(&state.trash_dir.clone(), &target) {
        Ok(trashed) => {
            state.push_undo(UndoOp::Trashed {
                original: target,
                trashed,
            });
            state.finder.cache.clear();
            state.refresh();
            app.notify_success(format!("Deleted: {name} (u to undo)"));
        }
        Err(e) => app.notify_error(format!("Delete failed: {e}")),
    }
}

fn undo_last(app: &mut App) {
    let Some(state) = explorer_mut(app) else {
        return;
    };
    let Some(op) = state.undo.pop() else {
        app.notify("Nothing to undo");
        return;
    };
    let trash_dir = state.trash_dir.clone();
    match fs_ops::undo(&op, &trash_dir) {
        Ok(outcome) => {
            if let Some((from, to)) = &outcome.moved {
                retarget_viewers(app, from, to);
            }
            if let Some(state) = explorer_mut(app) {
                state.finder.cache.clear();
                state.refresh();
                if let Some(path) = &outcome.select {
                    state.reveal(path);
                }
            }
            app.notify_success(outcome.message);
        }
        Err(e) => app.notify_error(format!("Undo failed: {e}")),
    }
}

// =============================================================================
// Fuzzy finder
// =============================================================================

/// Activate the fuzzy finder sub-mode. Walks the project on first use.
fn activate_finder(app: &mut App) {
    let Some(state) = explorer_mut(app) else {
        return;
    };
    let finder = &mut state.finder;
    finder.active = true;
    finder.query.clear();
    finder.selected = 0;
    if finder.cache.is_empty() {
        finder.cache = crate::file_utils::walk_files_scoped(&state.root, state.show_hidden, None);
    }
    finder.results = (0..finder.cache.len().min(MAX_FINDER_RESULTS)).collect();
}

fn close_finder(state: &mut FileExplorerState) {
    state.finder.active = false;
    state.finder.query.clear();
    state.finder.selected = 0;
}

fn move_finder_selection(finder: &mut ExplorerFinder, delta: isize) {
    if finder.results.is_empty() {
        finder.selected = 0;
        return;
    }
    let last = finder.results.len() - 1;
    finder.selected = finder.selected.saturating_add_signed(delta).min(last);
}

/// Keys while the finder is active. Every printable key (including `j`,
/// `k`, `q` and Space) types into the query; arrows and Ctrl-J/K/N/P move.
fn handle_finder_key(app: &mut App, key: KeyEvent) {
    // Standard editing: a real cursor and selection in the query.
    match app.edit_field(key, |overlay| match overlay {
        OverlayState::FileExplorer(state) => Some(&mut state.finder.query),
        _ => None,
    }) {
        crate::field_edit::FieldKey::Edited => {
            if let Some(state) = explorer_mut(app) {
                rescore_finder(state);
            }
            return;
        }
        crate::field_edit::FieldKey::Moved => return,
        crate::field_edit::FieldKey::Ignored => {}
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let Some(state) = explorer_mut(app) else {
        return;
    };
    let mut rescore = false;
    match key.code {
        KeyCode::Esc => close_finder(state),
        KeyCode::Enter => return handle_finder_enter(app),
        KeyCode::Down => move_finder_selection(&mut state.finder, 1),
        KeyCode::Up => move_finder_selection(&mut state.finder, -1),
        KeyCode::Char('j' | 'n') if ctrl => move_finder_selection(&mut state.finder, 1),
        KeyCode::Char('k' | 'p') if ctrl => move_finder_selection(&mut state.finder, -1),
        KeyCode::Char('u') if ctrl => {
            state.finder.query.clear();
            rescore = true;
        }
        KeyCode::Char('w') if ctrl => {
            let trimmed = state.finder.query.trim_end_matches(['/', ' ']);
            let cut = trimmed.rfind(['/', ' ']).map_or(0, |i| i + 1);
            state.finder.query.truncate(cut);
            rescore = true;
        }
        KeyCode::Backspace => {
            state.finder.query.pop();
            rescore = true;
        }
        KeyCode::Char(c) if !ctrl && !alt => {
            state.finder.query.push(c);
            rescore = true;
        }
        _ => {}
    }
    if rescore {
        rescore_finder(state);
    }
}

/// Re-score the finder cache against the query.
fn rescore_finder(state: &mut FileExplorerState) {
    let finder = &mut state.finder;
    finder.selected = 0;
    finder.results = crate::overlay::telescope::rank_finder(
        &finder.query,
        finder.cache.iter().map(|path| path.to_string_lossy()),
        MAX_FINDER_RESULTS,
    );
}

/// Open the selected finder result and reveal it in the tree.
fn handle_finder_enter(app: &mut App) {
    let Some(state) = explorer_mut(app) else {
        return;
    };
    let path = state
        .finder
        .results
        .get(state.finder.selected)
        .and_then(|&idx| state.finder.cache.get(idx))
        .map(|rel| state.root.join(rel));
    let Some(path) = path else {
        return;
    };
    close_finder(state);
    open_file(app, &path, true, false);
}

// =============================================================================
// Paste and mouse
// =============================================================================

/// Paste text into the focused explorer's prompt or fuzzy finder.
///
/// The tree has no text field, so it owns and acknowledges the paste instead
/// of allowing it to fall through to an obscured input surface.
pub(super) fn paste_text(app: &mut App, text: &str) -> bool {
    let Some(state) = explorer_mut(app) else {
        return false;
    };
    if !state.explorer_focused {
        return false;
    }
    if let Some(prompt) = state.prompt.as_mut() {
        if !prompt.is_confirmation() {
            prompt.insert_str(text);
        }
        return true;
    }
    if !state.finder.active {
        app.notify("Open finder with / before pasting");
        return true;
    }

    // Finder queries are single-line, matching ordinary typed query behavior.
    let pasted: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
    if pasted.is_empty() {
        return true;
    }
    state.finder.query.push_str(&pasted);
    rescore_finder(state);
    true
}

/// Paste system clipboard content into the explorer's prompt or finder.
pub(super) fn paste_clipboard(app: &mut App) -> bool {
    if !explorer(app).is_some_and(|state| state.explorer_focused) {
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

/// Left click: inside the drawer, focus the tree and select the row (a
/// second click on the selected row opens it). Outside the drawer, a click
/// hands focus to an open viewer. Returns whether the drawer consumed it.
pub fn handle_mouse_click(app: &mut App, col: u16, row: u16) -> bool {
    let viewer_active = viewer_is_active(app);
    let Some(state) = explorer_mut(app) else {
        return false;
    };
    let pos = Position::new(col, row);
    if !state.drawer_area.contains(pos) {
        if viewer_active && state.prompt.is_none() && !state.finder.active {
            state.explorer_focused = false;
        }
        return false;
    }
    state.explorer_focused = true;
    if state.prompt.is_some() || state.finder.active || !state.list_area.contains(pos) {
        return true;
    }
    let idx = state.scroll_offset + usize::from(row - state.list_area.y);
    if idx >= state.entries.len() {
        return true;
    }
    if idx == state.selected_index {
        activate_selected(app, true);
    } else {
        state.selected_index = idx;
        after_move(app);
    }
    true
}

/// Mouse wheel over the drawer moves the selection.
pub fn handle_mouse_scroll(app: &mut App, col: u16, row: u16, up: bool) -> bool {
    let Some(state) = explorer_mut(app) else {
        return false;
    };
    if !state.drawer_area.contains(Position::new(col, row)) {
        return false;
    }
    let delta = if up {
        -MOUSE_SCROLL_ROWS
    } else {
        MOUSE_SCROLL_ROWS
    };
    if state.finder.active {
        move_finder_selection(&mut state.finder, delta);
    } else {
        state.move_selection(delta);
    }
    true
}
