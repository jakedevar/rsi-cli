//! File explorer state: the flattened tree, selection, finder, inline prompt
//! and drawer geometry. Everything here is pure state manipulation plus
//! directory reads, so it is testable without an `App`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ratatui::layout::Rect;

use super::fs_ops::UndoOp;
use crate::types::FileExplorerEntry;

/// Narrowest drawer the resize keys allow.
pub const MIN_DRAWER_WIDTH: u16 = 20;
/// Columns always left for the viewer/session surface beside the drawer.
pub const MIN_SURFACE_WIDTH: u16 = 20;
/// Columns added or removed per resize keypress (matches overlay geometry).
pub const RESIZE_STEP: i32 = 2;
/// Undo history kept per explorer lifetime.
const MAX_UNDO: usize = 100;

/// Default drawer width: ~40% of the terminal, clamped to 30..=50 columns.
///
/// Drawer geometry must be independent of centered transcript gutters. On
/// compact terminals, the gutter can be only a few columns wide, leaving the
/// explorer unable to show paths or its own key hints.
pub fn default_drawer_width(area_width: u16) -> u16 {
    let preferred = (u32::from(area_width) * 2 / 5).clamp(30, 50) as u16;
    preferred.min(area_width)
}

/// Effective drawer width for a terminal `area_width` columns wide, honoring
/// an operator-chosen width (`preferred`) when one was set with the resize
/// keys. The result always leaves `MIN_SURFACE_WIDTH` columns for the surface
/// beside the drawer when the terminal is wide enough to do so.
pub fn drawer_width(area_width: u16, preferred: Option<u16>) -> u16 {
    let Some(preferred) = preferred else {
        return default_drawer_width(area_width);
    };
    let max = area_width
        .saturating_sub(MIN_SURFACE_WIDTH)
        .max(MIN_DRAWER_WIDTH)
        .min(area_width);
    let min = MIN_DRAWER_WIDTH.min(max);
    preferred.clamp(min, max)
}

/// Read a single directory level and return sorted entries (dirs first, then
/// files, case-insensitive alphabetical). Hidden entries (leading `.`) are
/// excluded unless `show_hidden` is true. Entries ignored by git (or `.git`
/// itself) are kept but flagged so the drawer can dim them.
pub(crate) fn read_directory(
    dir: &Path,
    depth: usize,
    show_hidden: bool,
) -> Vec<FileExplorerEntry> {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let not_ignored = non_ignored_children(dir);

    let mut dirs = Vec::new();
    let mut files = Vec::new();

    for entry in read_dir.flatten() {
        let path = entry.path();
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();

        if !show_hidden && name.starts_with('.') {
            continue;
        }
        let ignored = name == ".git" || !not_ignored.contains(&path);

        if path.is_dir() {
            dirs.push(FileExplorerEntry::Directory {
                path,
                depth,
                expanded: false,
                ignored,
            });
        } else {
            files.push(FileExplorerEntry::File {
                path,
                depth,
                ignored,
            });
        }
    }

    let sort_key = |e: &FileExplorerEntry| {
        e.path()
            .file_name()
            .unwrap_or_default()
            .to_ascii_lowercase()
    };
    dirs.sort_by_key(sort_key);
    files.sort_by_key(sort_key);

    dirs.extend(files);
    dirs
}

/// Direct children of `dir` that git (and `.ignore` files) do not ignore.
fn non_ignored_children(dir: &Path) -> HashSet<PathBuf> {
    ignore::WalkBuilder::new(dir)
        .max_depth(Some(1))
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build()
        .flatten()
        .filter(|entry| entry.depth() == 1)
        .map(ignore::DirEntry::into_path)
        .collect()
}

/// Read `dir` and recursively re-expand every directory listed in `expanded`.
fn build_tree(
    dir: &Path,
    depth: usize,
    show_hidden: bool,
    expanded: &HashSet<PathBuf>,
    out: &mut Vec<FileExplorerEntry>,
) {
    for mut entry in read_directory(dir, depth, show_hidden) {
        let expand = match &mut entry {
            FileExplorerEntry::Directory {
                path,
                expanded: is_expanded,
                ..
            } if expanded.contains(path.as_path()) => {
                *is_expanded = true;
                true
            }
            _ => false,
        };
        let path = entry.path().to_path_buf();
        out.push(entry);
        if expand {
            build_tree(&path, depth + 1, show_hidden, expanded, out);
        }
    }
}

/// Fuzzy finder sub-mode state.
#[derive(Debug, Default)]
pub struct ExplorerFinder {
    /// Whether the finder replaces the tree.
    pub active: bool,
    /// Current query.
    pub query: String,
    /// File paths relative to the explorer root. Populated on first `/`.
    pub cache: Vec<PathBuf>,
    /// Scored and sorted indices into `cache`.
    pub results: Vec<usize>,
    /// Selected index within `results`.
    pub selected: usize,
}

/// What an inline explorer prompt will do when submitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptKind {
    /// Create a file inside `dir` (a trailing `/` creates a directory).
    Add { dir: PathBuf },
    /// Create a directory inside `dir`.
    AddDirectory { dir: PathBuf },
    /// Rename `target`; the input is relative to its parent directory.
    Rename { target: PathBuf },
    /// Move `target`; the input is relative to the explorer root.
    Move { target: PathBuf },
    /// Copy `source`; the input is relative to the explorer root.
    Copy { source: PathBuf },
    /// Confirm moving `target` to the explorer trash.
    ConfirmDelete { target: PathBuf },
}

/// Single-line inline prompt shown at the bottom of the drawer.
#[derive(Debug, Clone)]
pub struct ExplorerPrompt {
    pub kind: PromptKind,
    pub input: String,
    /// Cursor position as a char index into `input`.
    pub cursor: usize,
}

impl ExplorerPrompt {
    pub fn new(kind: PromptKind, initial: impl Into<String>) -> Self {
        let input = initial.into();
        let cursor = input.chars().count();
        Self {
            kind,
            input,
            cursor,
        }
    }

    pub fn is_confirmation(&self) -> bool {
        matches!(self.kind, PromptKind::ConfirmDelete { .. })
    }

    fn byte_index(&self, char_index: usize) -> usize {
        self.input
            .char_indices()
            .nth(char_index)
            .map_or(self.input.len(), |(i, _)| i)
    }

    fn len_chars(&self) -> usize {
        self.input.chars().count()
    }

    pub fn insert_char(&mut self, c: char) {
        let at = self.byte_index(self.cursor);
        self.input.insert(at, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, text: &str) {
        for c in text.chars().filter(|c| *c != '\n' && *c != '\r') {
            self.insert_char(c);
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let at = self.byte_index(self.cursor - 1);
        self.input.remove(at);
        self.cursor -= 1;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.len_chars() {
            return;
        }
        let at = self.byte_index(self.cursor);
        self.input.remove(at);
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.len_chars());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.len_chars();
    }

    /// Ctrl-U: delete everything before the cursor.
    pub fn clear_to_start(&mut self) {
        let at = self.byte_index(self.cursor);
        self.input.replace_range(..at, "");
        self.cursor = 0;
    }

    /// Ctrl-W: delete the path segment or word before the cursor.
    pub fn delete_word_back(&mut self) {
        let chars: Vec<char> = self.input.chars().collect();
        let mut start = self.cursor.min(chars.len());
        // Skip separators immediately before the cursor, then the word.
        while start > 0 && matches!(chars[start - 1], '/' | ' ' | '.' | '-' | '_') {
            start -= 1;
        }
        while start > 0 && !matches!(chars[start - 1], '/' | ' ' | '.' | '-' | '_') {
            start -= 1;
        }
        let from = self.byte_index(start);
        let to = self.byte_index(self.cursor);
        self.input.replace_range(from..to, "");
        self.cursor = start;
    }
}

/// Complete state of the file explorer drawer.
#[derive(Debug)]
pub struct FileExplorerState {
    /// Root directory being explored (the current project's path).
    pub root: PathBuf,
    /// Flattened tree entries (directories + files, depth-tracked).
    pub entries: Vec<FileExplorerEntry>,
    /// Index into `entries` of the highlighted row.
    pub selected_index: usize,
    /// First visible tree row; kept in view by the renderer.
    pub scroll_offset: usize,
    /// Whether dotfiles are listed.
    pub show_hidden: bool,
    /// Whether the drawer has keyboard focus (vs the file viewer).
    /// Ctrl+L shifts focus to the viewer, Ctrl+H shifts it back here.
    pub explorer_focused: bool,
    /// Fuzzy finder sub-mode.
    pub finder: ExplorerFinder,
    /// Inline add/rename/move/copy/delete prompt.
    pub prompt: Option<ExplorerPrompt>,
    /// First key of a pending chord (`y` for yy / yn / yr).
    pub pending_key: Option<char>,
    /// Undo history of filesystem operations (most recent last).
    pub undo: Vec<UndoOp>,
    /// Directory deleted entries are moved into.
    pub trash_dir: PathBuf,
    /// Operator-chosen drawer width in columns; `None` uses the default.
    pub width: Option<u16>,
    /// Open files in the viewer as the selection moves (keeps tree focus).
    pub follow_preview: bool,
    /// Last rendered drawer rectangle, for mouse hit testing.
    pub drawer_area: Rect,
    /// Last rendered tree-row rectangle, for mouse hit testing and paging.
    pub list_area: Rect,
}

impl FileExplorerState {
    /// Build an explorer rooted at `root` with its first level read.
    pub fn new(root: PathBuf, trash_dir: PathBuf) -> Self {
        let entries = read_directory(&root, 0, false);
        Self::with_entries(root, entries, trash_dir)
    }

    /// Build an explorer from pre-computed entries (no disk reads).
    pub fn with_entries(
        root: PathBuf,
        entries: Vec<FileExplorerEntry>,
        trash_dir: PathBuf,
    ) -> Self {
        Self {
            root,
            entries,
            selected_index: 0,
            scroll_offset: 0,
            show_hidden: false,
            explorer_focused: true,
            finder: ExplorerFinder::default(),
            prompt: None,
            pending_key: None,
            undo: Vec::new(),
            trash_dir,
            width: None,
            follow_preview: false,
            drawer_area: Rect::default(),
            list_area: Rect::default(),
        }
    }

    pub fn selected_entry(&self) -> Option<&FileExplorerEntry> {
        self.entries.get(self.selected_index)
    }

    pub fn selected_path(&self) -> Option<PathBuf> {
        self.selected_entry().map(|e| e.path().to_path_buf())
    }

    /// Path relative to the root for display (`.` for the root itself).
    pub fn display_relative(&self, path: &Path) -> String {
        match path.strip_prefix(&self.root) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
            Ok(rel) => rel.display().to_string(),
            Err(_) => path.display().to_string(),
        }
    }

    /// Directory a new entry is created in: the selected directory itself, or
    /// the parent of the selected file, or the root when the tree is empty.
    pub fn target_dir_for_add(&self) -> PathBuf {
        match self.selected_entry() {
            Some(FileExplorerEntry::Directory { path, .. }) => path.clone(),
            Some(FileExplorerEntry::File { path, .. }) => path
                .parent()
                .map_or_else(|| self.root.clone(), Path::to_path_buf),
            None => self.root.clone(),
        }
    }

    /// Directories currently expanded in the tree.
    pub fn expanded_dirs(&self) -> HashSet<PathBuf> {
        self.entries
            .iter()
            .filter_map(|e| match e {
                FileExplorerEntry::Directory {
                    path,
                    expanded: true,
                    ..
                } => Some(path.clone()),
                _ => None,
            })
            .collect()
    }

    /// Re-read the tree from disk, keeping expanded directories expanded and
    /// the selection on the same path when it still exists.
    pub fn refresh(&mut self) {
        let expanded = self.expanded_dirs();
        let selected = self.selected_path();
        let previous_index = self.selected_index;
        let mut entries = Vec::new();
        build_tree(&self.root, 0, self.show_hidden, &expanded, &mut entries);
        self.entries = entries;
        let reselected = selected.is_some_and(|path| self.select_path(&path));
        if !reselected {
            self.selected_index = previous_index.min(self.entries.len().saturating_sub(1));
        }
    }

    /// Select the entry at `path` if it is visible. Returns whether it was found.
    pub fn select_path(&mut self, path: &Path) -> bool {
        match self.entries.iter().position(|e| e.path() == path) {
            Some(idx) => {
                self.selected_index = idx;
                true
            }
            None => false,
        }
    }

    fn index_of(&self, path: &Path) -> Option<usize> {
        self.entries.iter().position(|e| e.path() == path)
    }

    /// Map `path` onto the root's spelling (the tree is built from `root`,
    /// while viewers may hold canonical paths).
    fn root_relative(&self, path: &Path) -> Option<PathBuf> {
        if let Ok(rel) = path.strip_prefix(&self.root) {
            return Some(rel.to_path_buf());
        }
        let canon_root = self.root.canonicalize().ok()?;
        let canon_path = path.canonicalize().ok()?;
        canon_path
            .strip_prefix(&canon_root)
            .ok()
            .map(Path::to_path_buf)
    }

    /// Expand every ancestor of `path` and select it. Returns whether the
    /// path is now selected.
    pub fn reveal(&mut self, path: &Path) -> bool {
        let Some(rel) = self.root_relative(path) else {
            return false;
        };
        let mut current = self.root.clone();
        let components: Vec<_> = rel.components().collect();
        let Some((last, ancestors)) = components.split_last() else {
            return false;
        };
        for component in ancestors {
            current.push(component);
            let Some(idx) = self.index_of(&current) else {
                return false;
            };
            self.expand_at(idx);
        }
        current.push(last);
        self.select_path(&current)
    }

    /// Expand the directory at `idx` (no-op for files or expanded dirs).
    pub fn expand_at(&mut self, idx: usize) {
        let show_hidden = self.show_hidden;
        let Some(FileExplorerEntry::Directory {
            path,
            depth,
            expanded,
            ..
        }) = self.entries.get_mut(idx)
        else {
            return;
        };
        if *expanded {
            return;
        }
        *expanded = true;
        let children = read_directory(path, *depth + 1, show_hidden);
        self.entries.splice(idx + 1..idx + 1, children);
    }

    /// Collapse the directory at `idx`, removing its visible descendants.
    pub fn collapse_at(&mut self, idx: usize) {
        let Some(FileExplorerEntry::Directory {
            depth, expanded, ..
        }) = self.entries.get_mut(idx)
        else {
            return;
        };
        if !*expanded {
            return;
        }
        *expanded = false;
        let depth = *depth;
        let count = self.entries[idx + 1..]
            .iter()
            .take_while(|e| e.depth() > depth)
            .count();
        self.entries.drain(idx + 1..idx + 1 + count);
        if self.selected_index > idx && self.selected_index <= idx + count {
            self.selected_index = idx;
        } else if self.selected_index > idx + count {
            self.selected_index -= count;
        }
    }

    /// Toggle the directory at `idx`.
    pub fn toggle_at(&mut self, idx: usize) {
        match self.entries.get(idx) {
            Some(FileExplorerEntry::Directory { expanded: true, .. }) => self.collapse_at(idx),
            Some(FileExplorerEntry::Directory { .. }) => self.expand_at(idx),
            _ => {}
        }
    }

    /// Index of the directory containing the entry at `idx`.
    pub fn parent_index(&self, idx: usize) -> Option<usize> {
        let depth = self.entries.get(idx)?.depth();
        if depth == 0 {
            return None;
        }
        (0..idx).rev().find(|&i| self.entries[i].depth() < depth)
    }

    /// `h`: collapse the selected directory, or collapse and select its parent.
    pub fn close_node(&mut self) {
        let idx = self.selected_index;
        if matches!(
            self.entries.get(idx),
            Some(FileExplorerEntry::Directory { expanded: true, .. })
        ) {
            self.collapse_at(idx);
            return;
        }
        if let Some(parent) = self.parent_index(idx) {
            self.collapse_at(parent);
            self.selected_index = parent;
        }
    }

    /// `l` on a directory: expand it, or step into its first child when it
    /// is already expanded. Returns false when the selection is a file.
    pub fn open_node(&mut self) -> bool {
        let idx = self.selected_index;
        match self.entries.get(idx) {
            Some(FileExplorerEntry::Directory {
                expanded: false, ..
            }) => {
                self.expand_at(idx);
                true
            }
            Some(FileExplorerEntry::Directory { depth, .. }) => {
                let depth = *depth;
                if self
                    .entries
                    .get(idx + 1)
                    .is_some_and(|child| child.depth() > depth)
                {
                    self.selected_index = idx + 1;
                }
                true
            }
            _ => false,
        }
    }

    /// Collapse every directory, keeping the selection on the top-level
    /// ancestor of the previously selected entry.
    pub fn collapse_all(&mut self) {
        let mut top = self.selected_index;
        while let Some(parent) = self.parent_index(top) {
            top = parent;
        }
        let top_path = self.entries.get(top).map(|e| e.path().to_path_buf());
        self.entries = read_directory(&self.root, 0, self.show_hidden);
        self.selected_index = 0;
        if let Some(path) = top_path {
            self.select_path(&path);
        }
        self.scroll_offset = 0;
    }

    /// Flip dotfile visibility and re-read the tree, keeping expansion.
    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.finder.cache.clear(); // re-walk on the next finder activation
        self.refresh();
    }

    /// Move the selection by `delta` rows, clamped to the list.
    pub fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            self.selected_index = 0;
            return;
        }
        let last = self.entries.len() - 1;
        self.selected_index = self.selected_index.saturating_add_signed(delta).min(last);
    }

    /// Rows moved by a half-page command.
    pub fn half_page(&self) -> isize {
        let rows = if self.list_area.height == 0 {
            20
        } else {
            self.list_area.height
        };
        (rows / 2).max(1) as isize
    }

    /// Keep the selection inside a viewport of `height` rows.
    pub fn ensure_visible(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.selected_index < self.scroll_offset {
            self.scroll_offset = self.selected_index;
        } else if self.selected_index >= self.scroll_offset + height {
            self.scroll_offset = self.selected_index + 1 - height;
        }
        let max_offset = self.entries.len().saturating_sub(height);
        self.scroll_offset = self.scroll_offset.min(max_offset);
    }

    /// Record a completed filesystem operation for `u`.
    pub fn push_undo(&mut self, op: UndoOp) {
        self.undo.push(op);
        if self.undo.len() > MAX_UNDO {
            self.undo.remove(0);
        }
    }
}
