//! File explorer tests: tree state, filesystem operations, key flows and the
//! hand-off to the session file viewer. All filesystem work happens in
//! per-test temp directories.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::fs_ops::{self, UndoOp};
use super::state::{
    ExplorerPrompt, FileExplorerState, MIN_DRAWER_WIDTH, PromptKind, drawer_width, read_directory,
};
use super::*;
use crate::types::FileExplorerEntry;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tempfile::TempDir;

// =============================================================================
// Helpers
// =============================================================================

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ch(c: char) -> KeyEvent {
    key(KeyCode::Char(c))
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn ctrl_shift(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL | KeyModifiers::SHIFT)
}

/// Create `layout` under `root`: entries ending in `/` are directories,
/// `path=content` writes content, anything else is an empty file.
fn build_layout(root: &Path, layout: &[&str]) {
    for item in layout {
        if let Some(dir) = item.strip_suffix('/') {
            fs::create_dir_all(root.join(dir)).unwrap();
            continue;
        }
        let (rel, content) = item.split_once('=').unwrap_or((item, ""));
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
}

fn names(entries: &[FileExplorerEntry]) -> Vec<String> {
    entries.iter().map(|e| fs_ops::name_of(e.path())).collect()
}

/// An app with one session, a project rooted at a temp dir and the explorer
/// open on it. The trash lives in its own temp dir outside the project.
struct Fixture {
    root: TempDir,
    trash: TempDir,
    app: App,
}

impl Fixture {
    fn new(layout: &[&str]) -> Self {
        let root = tempfile::tempdir().unwrap();
        build_layout(root.path(), layout);
        let trash = tempfile::tempdir().unwrap();

        let mut app = crate::app::app_test_helpers::with_session_list(1);
        let project_id = Uuid::new_v4();
        app.projects = vec![rsi_common::types::Project {
            id: project_id,
            name: "explorer".to_string(),
            path: Some(root.path().to_path_buf()),
            description: None,
            color: rsi_common::types::Project::DEFAULT_COLOR.to_string(),
            context_files: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }];
        app.current_project_id = Some(project_id);
        app.file_explorer_width = None;
        app.last_terminal_size = (120, 40);

        let mut fixture = Self { root, trash, app };
        fixture.open();
        fixture
    }

    fn open(&mut self) {
        open_file_explorer(&mut self.app);
        let trash = self.trash.path().to_path_buf();
        self.state_mut().trash_dir = trash;
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    fn state(&self) -> &FileExplorerState {
        explorer(&self.app).expect("explorer should be open")
    }

    fn state_mut(&mut self) -> &mut FileExplorerState {
        explorer_mut(&mut self.app).expect("explorer should be open")
    }

    fn press(&mut self, key: KeyEvent) {
        handle_file_explorer_key(&mut self.app, key);
    }

    fn type_str(&mut self, text: &str) {
        for c in text.chars() {
            self.press(ch(c));
        }
    }

    fn select(&mut self, rel: &str) {
        let path = self.path(rel);
        assert!(self.state_mut().reveal(&path), "{rel} should be revealable");
    }

    fn selected(&self) -> Option<PathBuf> {
        self.state().selected_path()
    }

    fn session_id(&self) -> Uuid {
        self.app
            .selected_session_id()
            .expect("fixture has a session")
    }

    fn viewer(&self) -> Option<&FileViewerState> {
        self.app.sessions[&self.session_id()].file_viewer.as_ref()
    }

    fn viewer_path(&self) -> Option<PathBuf> {
        self.viewer().map(|viewer| viewer.file_path.clone())
    }

    fn notice(&self) -> String {
        self.app
            .notifications
            .back()
            .map(|n| n.message.clone())
            .unwrap_or_default()
    }

    fn prompt_kind(&self) -> Option<PromptKind> {
        self.state().prompt.as_ref().map(|p| p.kind.clone())
    }
}

// =============================================================================
// Directory reads and tree state
// =============================================================================

#[test]
fn read_directory_lists_dirs_first_and_hides_dotfiles() {
    let root = tempfile::tempdir().unwrap();
    build_layout(root.path(), &["src/", "Zeta/", "b.txt", "A.txt", ".env"]);

    let entries = read_directory(root.path(), 0, false);
    assert_eq!(names(&entries), vec!["src", "Zeta", "A.txt", "b.txt"]);
    assert!(entries[0].is_dir() && entries[1].is_dir());

    let with_hidden = read_directory(root.path(), 0, true);
    assert_eq!(
        names(&with_hidden),
        vec!["src", "Zeta", ".env", "A.txt", "b.txt"]
    );
}

#[test]
fn read_directory_flags_git_ignored_entries() {
    let root = tempfile::tempdir().unwrap();
    build_layout(
        root.path(),
        &[
            ".git/",
            ".gitignore=target/\n*.log\n",
            "target/",
            "src/",
            "run.log",
            "main.rs",
        ],
    );

    let entries = read_directory(root.path(), 0, true);
    let ignored: Vec<String> = entries
        .iter()
        .filter(|e| e.is_ignored())
        .map(|e| fs_ops::name_of(e.path()))
        .collect();
    assert_eq!(ignored, vec![".git", "target", "run.log"]);
    let kept: Vec<String> = entries
        .iter()
        .filter(|e| !e.is_ignored())
        .map(|e| fs_ops::name_of(e.path()))
        .collect();
    assert_eq!(kept, vec!["src", ".gitignore", "main.rs"]);
}

#[test]
fn reveal_expands_ancestors_and_selects_deep_paths() {
    let root = tempfile::tempdir().unwrap();
    build_layout(root.path(), &["a/b/c/deep.rs", "a/top.rs", "z.rs"]);
    let mut state = FileExplorerState::new(root.path().to_path_buf(), std::env::temp_dir());

    assert!(state.reveal(&root.path().join("a/b/c/deep.rs")));
    assert_eq!(
        state.selected_path(),
        Some(root.path().join("a/b/c/deep.rs"))
    );
    assert_eq!(
        names(&state.entries),
        vec!["a", "b", "c", "deep.rs", "top.rs", "z.rs"]
    );
    assert!(!state.reveal(Path::new("/definitely/not/inside")));
}

#[test]
fn refresh_keeps_expansion_and_selection() {
    let root = tempfile::tempdir().unwrap();
    build_layout(root.path(), &["src/lib.rs", "src/main.rs", "README.md"]);
    let mut state = FileExplorerState::new(root.path().to_path_buf(), std::env::temp_dir());
    state.reveal(&root.path().join("src/main.rs"));

    fs::write(root.path().join("src/added.rs"), "").unwrap();
    state.refresh();

    assert_eq!(
        names(&state.entries),
        vec!["src", "added.rs", "lib.rs", "main.rs", "README.md"]
    );
    assert_eq!(state.selected_path(), Some(root.path().join("src/main.rs")));
}

#[test]
fn h_collapses_then_climbs_and_l_expands_then_descends() {
    let root = tempfile::tempdir().unwrap();
    build_layout(root.path(), &["src/app/mod.rs", "src/lib.rs"]);
    let mut state = FileExplorerState::new(root.path().to_path_buf(), std::env::temp_dir());

    // l on a collapsed dir expands it; l again steps into the first child.
    assert!(state.open_node());
    assert_eq!(names(&state.entries), vec!["src", "app", "lib.rs"]);
    assert!(state.open_node());
    assert_eq!(state.selected_path(), Some(root.path().join("src/app")));

    // h on a file collapses its parent and selects it.
    state.reveal(&root.path().join("src/app/mod.rs"));
    state.close_node();
    assert_eq!(state.selected_path(), Some(root.path().join("src/app")));
    assert_eq!(names(&state.entries), vec!["src", "app", "lib.rs"]);

    // h on an expanded dir collapses it in place.
    state.select_path(&root.path().join("src"));
    state.close_node();
    assert_eq!(names(&state.entries), vec!["src"]);
}

#[test]
fn collapse_all_keeps_the_top_level_ancestor_selected() {
    let root = tempfile::tempdir().unwrap();
    build_layout(root.path(), &["a/b/c.rs", "d/e.rs"]);
    let mut state = FileExplorerState::new(root.path().to_path_buf(), std::env::temp_dir());
    state.reveal(&root.path().join("d/e.rs"));
    state.reveal(&root.path().join("a/b/c.rs"));

    state.collapse_all();

    assert_eq!(names(&state.entries), vec!["a", "d"]);
    assert_eq!(state.selected_path(), Some(root.path().join("a")));
}

#[test]
fn collapse_keeps_selection_index_on_the_same_entry() {
    let root = tempfile::tempdir().unwrap();
    build_layout(root.path(), &["a/1.rs", "a/2.rs", "z.rs"]);
    let mut state = FileExplorerState::new(root.path().to_path_buf(), std::env::temp_dir());
    state.expand_at(0);
    state.select_path(&root.path().join("z.rs"));

    state.collapse_at(0);

    assert_eq!(state.selected_path(), Some(root.path().join("z.rs")));
}

#[test]
fn target_dir_for_add_follows_the_selection() {
    let root = tempfile::tempdir().unwrap();
    build_layout(root.path(), &["src/lib.rs", "top.rs"]);
    let mut state = FileExplorerState::new(root.path().to_path_buf(), std::env::temp_dir());

    state.select_path(&root.path().join("src"));
    assert_eq!(state.target_dir_for_add(), root.path().join("src"));
    state.reveal(&root.path().join("src/lib.rs"));
    assert_eq!(state.target_dir_for_add(), root.path().join("src"));
    state.select_path(&root.path().join("top.rs"));
    assert_eq!(state.target_dir_for_add(), root.path().to_path_buf());

    let empty = FileExplorerState::with_entries(
        root.path().to_path_buf(),
        Vec::new(),
        std::env::temp_dir(),
    );
    assert_eq!(empty.target_dir_for_add(), root.path().to_path_buf());
}

#[test]
fn drawer_width_clamps_operator_choice() {
    assert_eq!(drawer_width(120, None), 48);
    assert_eq!(drawer_width(120, Some(10)), MIN_DRAWER_WIDTH);
    assert_eq!(drawer_width(120, Some(64)), 64);
    assert_eq!(drawer_width(120, Some(500)), 100);
}

// =============================================================================
// Prompt line editing and completion
// =============================================================================

fn prompt(initial: &str) -> ExplorerPrompt {
    ExplorerPrompt::new(
        PromptKind::Add {
            dir: PathBuf::from("/tmp"),
        },
        initial,
    )
}

#[test]
fn prompt_edits_at_the_cursor_with_unicode() {
    let mut p = prompt("héllo");
    assert_eq!(p.cursor, 5);
    p.left();
    p.left();
    p.insert_char('X');
    assert_eq!(p.input, "hélXlo");
    p.backspace();
    p.backspace();
    assert_eq!(p.input, "hélo");
    p.home();
    p.delete();
    assert_eq!(p.input, "élo");
    p.end();
    p.insert_str("\nw\r");
    assert_eq!(p.input, "élow");
}

#[test]
fn prompt_word_and_line_deletion() {
    let mut p = prompt("src/app/mod.rs");
    p.delete_word_back();
    assert_eq!(p.input, "src/app/mod.");
    p.delete_word_back();
    assert_eq!(p.input, "src/app/");
    p.delete_word_back();
    assert_eq!(p.input, "src/");

    let mut p = prompt("notes.md");
    p.cursor = 5;
    p.clear_to_start();
    assert_eq!(p.input, ".md");
    assert_eq!(p.cursor, 0);
}

#[test]
fn complete_path_finishes_unique_and_common_prefixes() {
    let root = tempfile::tempdir().unwrap();
    build_layout(
        root.path(),
        &["src/", "scripts/", "settings.json", "docs/guide/"],
    );

    // Unique directory match gains a trailing slash.
    assert_eq!(
        complete_path(root.path(), "do", false).as_deref(),
        Some("docs/")
    );
    // Nested segment completes inside the named directory.
    assert_eq!(
        complete_path(root.path(), "docs/g", false).as_deref(),
        Some("docs/guide/")
    );
    // Several matches complete their common prefix only when it grows.
    assert_eq!(
        complete_path(root.path(), "se", false).as_deref(),
        Some("settings.json")
    );
    assert_eq!(complete_path(root.path(), "s", false), None);
    assert_eq!(
        complete_path(root.path(), "sc", false).as_deref(),
        Some("scripts/")
    );
    assert_eq!(complete_path(root.path(), "nope", false), None);
}

#[test]
fn stem_cursor_lands_before_the_extension() {
    assert_eq!(stem_cursor("notes.md"), Some(5));
    assert_eq!(stem_cursor("archive.tar.gz"), Some(11));
    assert_eq!(stem_cursor(".env"), None);
    assert_eq!(stem_cursor("Makefile"), None);
}

// =============================================================================
// Filesystem operations
// =============================================================================

#[test]
fn normalize_resolves_dots_without_climbing_past_root() {
    assert_eq!(
        fs_ops::normalize(Path::new("/a/./b/../c")),
        PathBuf::from("/a/c")
    );
    assert_eq!(fs_ops::normalize(Path::new("/../x")), PathBuf::from("/x"));
    assert_eq!(
        fs_ops::normalize(Path::new("a/../../b")),
        PathBuf::from("../b")
    );
}

#[test]
fn resolve_destination_stays_inside_the_root() {
    let root = tempfile::tempdir().unwrap();
    build_layout(root.path(), &["src/"]);
    let src = root.path().join("src");

    assert_eq!(
        fs_ops::resolve_destination(root.path(), &src, "app/mod.rs").unwrap(),
        src.join("app/mod.rs")
    );
    assert_eq!(
        fs_ops::resolve_destination(root.path(), &src, "../top.rs").unwrap(),
        root.path().join("top.rs")
    );
    assert_eq!(
        fs_ops::resolve_destination(root.path(), &src, "  spaced/  ").unwrap(),
        src.join("spaced")
    );
    let absolute = root.path().join("abs.rs");
    assert_eq!(
        fs_ops::resolve_destination(root.path(), &src, &absolute.display().to_string()).unwrap(),
        absolute
    );

    for bad in ["", "   ", "/", "../../escape.rs", "..", "/etc/passwd"] {
        assert!(
            fs_ops::resolve_destination(root.path(), &src, bad).is_err(),
            "{bad:?} must be refused"
        );
    }
}

#[cfg(unix)]
#[test]
fn resolve_destination_refuses_symlink_escapes() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();

    let err = fs_ops::resolve_destination(root.path(), root.path(), "link/evil.rs").unwrap_err();
    assert!(err.contains("Path must stay inside"), "{err}");
}

#[test]
fn create_path_makes_parents_and_reports_the_top_new_entry() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("a/b/c.rs");

    let top = fs_ops::create_path(&file, false).unwrap();
    assert_eq!(top, root.path().join("a"));
    assert!(file.is_file());

    let dir = root.path().join("a/b/d");
    assert_eq!(fs_ops::create_path(&dir, true).unwrap(), dir);
    assert!(dir.is_dir());

    let err = fs_ops::create_path(&file, false).unwrap_err();
    assert!(err.contains("already exists"), "{err}");
}

#[test]
fn move_path_never_overwrites_or_nests_into_itself() {
    let root = tempfile::tempdir().unwrap();
    build_layout(root.path(), &["a.txt=A", "b.txt=B", "dir/inner.txt"]);

    fs_ops::move_path(&root.path().join("a.txt"), &root.path().join("new/a.txt")).unwrap();
    assert_eq!(
        fs::read_to_string(root.path().join("new/a.txt")).unwrap(),
        "A"
    );

    let err =
        fs_ops::move_path(&root.path().join("new/a.txt"), &root.path().join("b.txt")).unwrap_err();
    assert!(err.contains("already exists"), "{err}");
    assert_eq!(fs::read_to_string(root.path().join("b.txt")).unwrap(), "B");

    let err =
        fs_ops::move_path(&root.path().join("dir"), &root.path().join("dir/sub")).unwrap_err();
    assert!(err.contains("into itself"), "{err}");
}

#[test]
fn copy_path_copies_directories_recursively() {
    let root = tempfile::tempdir().unwrap();
    build_layout(root.path(), &["src/a.rs=A", "src/inner/b.rs=B"]);

    fs_ops::copy_path(&root.path().join("src"), &root.path().join("copy")).unwrap();

    assert_eq!(
        fs::read_to_string(root.path().join("copy/a.rs")).unwrap(),
        "A"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("copy/inner/b.rs")).unwrap(),
        "B"
    );
    assert!(root.path().join("src/a.rs").is_file());
    assert!(fs_ops::copy_path(&root.path().join("src"), &root.path().join("copy")).is_err());
}

#[test]
fn trash_round_trip_restores_contents() {
    let root = tempfile::tempdir().unwrap();
    let trash = tempfile::tempdir().unwrap();
    build_layout(root.path(), &["dir/a.rs=A", "dir/sub/b.rs=B"]);
    let dir = root.path().join("dir");

    let trashed = fs_ops::move_to_trash(trash.path(), &dir).unwrap();
    assert!(!dir.exists());
    assert!(trashed.starts_with(trash.path()));
    let origin = trashed.parent().unwrap().with_extension("origin");
    assert_eq!(
        fs::read_to_string(&origin).unwrap(),
        dir.display().to_string()
    );

    fs_ops::restore_from_trash(&dir, &trashed).unwrap();
    assert_eq!(fs::read_to_string(dir.join("sub/b.rs")).unwrap(), "B");
    assert!(!origin.exists(), "restored batches clean up their record");
}

#[test]
fn restore_refuses_when_the_original_path_was_reused() {
    let root = tempfile::tempdir().unwrap();
    let trash = tempfile::tempdir().unwrap();
    build_layout(root.path(), &["a.txt=old"]);
    let path = root.path().join("a.txt");

    let trashed = fs_ops::move_to_trash(trash.path(), &path).unwrap();
    fs::write(&path, "new").unwrap();

    assert!(fs_ops::restore_from_trash(&path, &trashed).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), "new");
    assert_eq!(fs::read_to_string(&trashed).unwrap(), "old");
}

#[test]
fn undo_of_a_create_removes_empty_entries_and_trashes_content() {
    let root = tempfile::tempdir().unwrap();
    let trash = tempfile::tempdir().unwrap();
    let empty = root.path().join("empty.rs");
    fs::write(&empty, "").unwrap();
    let edited = root.path().join("edited.rs");
    fs::write(&edited, "work").unwrap();

    let outcome = fs_ops::undo(
        &UndoOp::Created {
            path: empty.clone(),
        },
        trash.path(),
    )
    .unwrap();
    assert!(!empty.exists());
    assert_eq!(outcome.message, "Removed empty.rs");

    let outcome = fs_ops::undo(
        &UndoOp::Created {
            path: edited.clone(),
        },
        trash.path(),
    )
    .unwrap();
    assert!(!edited.exists());
    assert_eq!(outcome.message, "Moved edited.rs to trash");
    assert_eq!(
        fs::read_dir(trash.path()).unwrap().count(),
        2,
        "batch + origin record"
    );
}

#[test]
fn undo_of_a_move_moves_back() {
    let root = tempfile::tempdir().unwrap();
    build_layout(root.path(), &["b.txt=B"]);
    let from = root.path().join("a.txt");
    let to = root.path().join("b.txt");

    let outcome = fs_ops::undo(
        &UndoOp::Moved {
            from: from.clone(),
            to: to.clone(),
        },
        root.path(),
    )
    .unwrap();

    assert_eq!(fs::read_to_string(&from).unwrap(), "B");
    assert_eq!(outcome.moved, Some((to, from.clone())));
    assert_eq!(outcome.select, Some(from));
}

#[test]
fn prune_trash_removes_only_old_batches() {
    let trash = tempfile::tempdir().unwrap();
    let old = trash.path().join("old");
    let fresh = trash.path().join("fresh");
    fs::create_dir_all(old.join("x")).unwrap();
    fs::create_dir_all(&fresh).unwrap();
    fs::File::open(&old)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60))
        .unwrap();

    fs_ops::prune_trash(trash.path(), Duration::from_secs(7 * 24 * 60 * 60));

    assert!(!old.exists());
    assert!(fresh.exists());
}

#[test]
fn human_size_and_describe_summarize_entries() {
    assert_eq!(fs_ops::human_size(12), "12 B");
    assert_eq!(fs_ops::human_size(2048), "2.0 KB");
    assert_eq!(fs_ops::human_size(5 * 1024 * 1024), "5.0 MB");

    let root = tempfile::tempdir().unwrap();
    build_layout(root.path(), &["f.txt=abc", "d/1", "d/2"]);
    let file = fs_ops::describe(&root.path().join("f.txt")).unwrap();
    assert!(file.starts_with("f.txt · 3 B · modified "), "{file}");
    let dir = fs_ops::describe(&root.path().join("d")).unwrap();
    assert!(dir.starts_with("d · 2 items"), "{dir}");
}

// =============================================================================
// Key flows: add / rename / move / copy / delete / undo
// =============================================================================

#[test]
fn a_adds_a_nested_file_inside_the_selected_directory() {
    let mut fx = Fixture::new(&["src/lib.rs"]);
    fx.select("src");

    fx.press(ch('a'));
    assert_eq!(
        fx.prompt_kind(),
        Some(PromptKind::Add {
            dir: fx.path("src")
        })
    );
    fx.type_str("app/mod.rs");
    fx.press(key(KeyCode::Enter));

    let created = fx.path("src/app/mod.rs");
    assert!(created.is_file());
    assert!(fx.state().prompt.is_none());
    assert_eq!(fx.selected(), Some(created));
    assert_eq!(
        fx.state().undo.last(),
        Some(&UndoOp::Created {
            path: fx.path("src/app")
        })
    );
    assert_eq!(fx.notice(), "Created src/app/mod.rs");

    fx.press(ch('u'));
    assert!(!fx.path("src/app").exists());
}

#[test]
fn a_with_trailing_slash_and_capital_a_create_directories() {
    let mut fx = Fixture::new(&[]);
    assert!(fx.state().entries.is_empty());

    fx.press(ch('a'));
    fx.type_str("docs/");
    fx.press(key(KeyCode::Enter));
    assert!(fx.path("docs").is_dir());
    assert_eq!(fx.selected(), Some(fx.path("docs")));

    fx.press(ch('A'));
    assert_eq!(
        fx.prompt_kind(),
        Some(PromptKind::AddDirectory {
            dir: fx.path("docs")
        })
    );
    fx.type_str("guide");
    fx.press(key(KeyCode::Enter));
    assert!(fx.path("docs/guide").is_dir());
    assert_eq!(fx.selected(), Some(fx.path("docs/guide")));
}

#[test]
fn failed_add_keeps_the_prompt_for_correction() {
    let mut fx = Fixture::new(&["taken.rs"]);
    fx.select("taken.rs");

    fx.press(ch('a'));
    fx.type_str("taken.rs");
    fx.press(key(KeyCode::Enter));
    assert!(fx.notice().contains("already exists"), "{}", fx.notice());
    assert_eq!(
        fx.state().prompt.as_ref().map(|p| p.input.as_str()),
        Some("taken.rs")
    );

    fx.press(ctrl('u'));
    fx.type_str("../escape.rs");
    fx.press(key(KeyCode::Enter));
    assert!(
        fx.notice().contains("Path must stay inside"),
        "{}",
        fx.notice()
    );
    assert!(!fx.root.path().parent().unwrap().join("escape.rs").exists());

    fx.press(key(KeyCode::Esc));
    assert!(fx.state().prompt.is_none());
}

#[test]
fn r_renames_and_open_viewers_follow_the_file() {
    let mut fx = Fixture::new(&["notes.md=# Notes"]);
    fx.select("notes.md");
    fx.press(key(KeyCode::Enter));
    assert_eq!(fx.viewer_path(), Some(fx.path("notes.md")));
    fx.state_mut().explorer_focused = true;

    fx.press(ch('r'));
    let prompt = fx.state().prompt.clone().unwrap();
    assert_eq!(prompt.input, "notes.md");
    assert_eq!(prompt.cursor, 5, "cursor lands before the extension");
    fx.press(ctrl('u'));
    fx.type_str("ideas");
    fx.press(key(KeyCode::Enter));

    assert!(fx.path("ideas.md").is_file());
    assert!(!fx.path("notes.md").exists());
    assert_eq!(fx.viewer_path(), Some(fx.path("ideas.md")));
    assert_eq!(fx.selected(), Some(fx.path("ideas.md")));
    assert_eq!(fx.notice(), "Renamed to ideas.md");

    fx.press(ch('u'));
    assert!(fx.path("notes.md").is_file());
    assert_eq!(fx.viewer_path(), Some(fx.path("notes.md")));
}

#[test]
fn renaming_a_directory_retargets_cached_drafts_inside_it() {
    let mut fx = Fixture::new(&["old/a.rs=a", "old/b.rs=b"]);
    fx.select("old/a.rs");
    fx.press(key(KeyCode::Enter));
    fx.state_mut().explorer_focused = true;
    fx.select("old/b.rs");
    fx.press(key(KeyCode::Enter));
    fx.state_mut().explorer_focused = true;

    fx.select("old");
    fx.press(ch('r'));
    fx.press(ctrl('u'));
    fx.type_str("new");
    fx.press(key(KeyCode::Enter));

    let session = &fx.app.sessions[&fx.session_id()];
    assert_eq!(
        session.file_viewer.as_ref().map(|v| v.file_path.clone()),
        Some(fx.path("new/b.rs"))
    );
    let cached = session
        .file_viewer_cache
        .get(&fx.path("new/a.rs"))
        .expect("cached viewer keyed by its new path");
    assert_eq!(cached.file_path, fx.path("new/a.rs"));
}

#[test]
fn d_confirms_then_deletes_to_trash_and_u_restores() {
    let mut fx = Fixture::new(&["keep.txt", "gone.txt=bye"]);
    fx.select("gone.txt");

    fx.press(ch('d'));
    assert_eq!(
        fx.prompt_kind(),
        Some(PromptKind::ConfirmDelete {
            target: fx.path("gone.txt")
        })
    );
    fx.press(ch('y'));
    assert!(!fx.path("gone.txt").exists());
    assert_eq!(fx.notice(), "Deleted: gone.txt (u to undo)");
    assert_eq!(names(&fx.state().entries), vec!["keep.txt"]);

    fx.press(ch('u'));
    assert_eq!(fs::read_to_string(fx.path("gone.txt")).unwrap(), "bye");
    assert_eq!(fx.selected(), Some(fx.path("gone.txt")));
}

#[test]
fn dd_deletes_directories_with_their_contents_recoverably() {
    let mut fx = Fixture::new(&["dir/a.rs=A", "dir/sub/b.rs=B", "z.rs"]);
    fx.select("dir");

    fx.press(ch('d'));
    fx.press(ch('d'));
    assert!(!fx.path("dir").exists());

    fx.press(ch('u'));
    assert_eq!(fs::read_to_string(fx.path("dir/sub/b.rs")).unwrap(), "B");
}

#[test]
fn delete_cancels_with_n_or_escape() {
    let mut fx = Fixture::new(&["a.txt"]);
    fx.select("a.txt");
    for cancel in [ch('n'), key(KeyCode::Esc)] {
        fx.press(ch('d'));
        fx.press(cancel);
        assert!(fx.state().prompt.is_none());
        assert!(fx.path("a.txt").exists());
    }
    // Explorer stays open after cancelling with Esc.
    assert!(explorer(&fx.app).is_some());
}

#[test]
fn m_moves_into_an_existing_directory() {
    let mut fx = Fixture::new(&["a.txt=A", "dest/"]);
    fx.select("a.txt");

    fx.press(ch('m'));
    assert_eq!(
        fx.state().prompt.as_ref().map(|p| p.input.as_str()),
        Some("a.txt")
    );
    fx.press(ctrl('u'));
    fx.type_str("dest");
    fx.press(key(KeyCode::Enter));

    assert_eq!(fs::read_to_string(fx.path("dest/a.txt")).unwrap(), "A");
    assert_eq!(fx.selected(), Some(fx.path("dest/a.txt")));
    assert_eq!(fx.notice(), "Moved to dest/a.txt");
}

#[test]
fn c_copies_and_u_removes_the_copy() {
    let mut fx = Fixture::new(&["src/a.rs=A", "src/inner/b.rs=B"]);
    fx.select("src");

    fx.press(ch('c'));
    fx.press(ctrl('u'));
    fx.type_str("src2");
    fx.press(key(KeyCode::Enter));
    assert_eq!(fs::read_to_string(fx.path("src2/inner/b.rs")).unwrap(), "B");
    assert!(fx.path("src/a.rs").exists());

    fx.press(ch('u'));
    assert!(!fx.path("src2").exists());
    assert!(fx.path("src/a.rs").exists());
}

#[test]
fn tab_completes_prompt_paths() {
    let mut fx = Fixture::new(&["a.txt", "destination/"]);
    fx.select("a.txt");
    fx.press(ch('m'));
    fx.press(ctrl('u'));
    fx.type_str("dest");
    fx.press(key(KeyCode::Tab));
    assert_eq!(
        fx.state().prompt.as_ref().map(|p| p.input.as_str()),
        Some("destination/")
    );
}

#[test]
fn u_with_empty_history_says_so() {
    let mut fx = Fixture::new(&["a.txt"]);
    fx.press(ch('u'));
    assert_eq!(fx.notice(), "Nothing to undo");
}

#[test]
fn paste_goes_into_an_open_prompt() {
    let mut fx = Fixture::new(&[]);
    fx.press(ch('a'));
    assert!(paste_text(&mut fx.app, "new\nfile.rs"));
    assert_eq!(
        fx.state().prompt.as_ref().map(|p| p.input.as_str()),
        Some("newfile.rs")
    );
}

#[test]
fn i_reports_file_details() {
    let mut fx = Fixture::new(&["f.txt=abc"]);
    fx.select("f.txt");
    fx.press(ch('i'));
    assert!(fx.notice().starts_with("f.txt · 3 B"), "{}", fx.notice());
}

#[test]
fn hidden_toggle_keeps_expanded_directories() {
    let mut fx = Fixture::new(&["src/a.rs", "src/.env"]);
    fx.select("src/a.rs");

    fx.press(ch('.'));
    assert!(fx.state().show_hidden);
    assert_eq!(names(&fx.state().entries), vec!["src", ".env", "a.rs"]);
    assert_eq!(fx.selected(), Some(fx.path("src/a.rs")));

    fx.press(ch('H'));
    assert_eq!(names(&fx.state().entries), vec!["src", "a.rs"]);
}

#[test]
fn navigation_keys_move_and_clamp() {
    let mut fx = Fixture::new(&["a", "b", "c", "d"]);
    fx.press(ch('G'));
    assert_eq!(fx.selected(), Some(fx.path("d")));
    fx.press(ch('j'));
    assert_eq!(fx.selected(), Some(fx.path("d")));
    fx.press(ch('g'));
    assert_eq!(fx.selected(), Some(fx.path("a")));
    fx.press(ctrl('d'));
    assert_eq!(
        fx.selected(),
        Some(fx.path("d")),
        "half page clamps to the end"
    );
    fx.press(key(KeyCode::Up));
    assert_eq!(fx.selected(), Some(fx.path("c")));
}

#[test]
fn z_collapses_every_directory() {
    let mut fx = Fixture::new(&["a/b/c.rs", "d/e.rs"]);
    fx.select("a/b/c.rs");
    fx.select("d/e.rs");
    fx.press(ch('z'));
    assert_eq!(names(&fx.state().entries), vec!["a", "d"]);
}

#[test]
fn capital_r_picks_up_external_changes() {
    let mut fx = Fixture::new(&["a.rs"]);
    fs::write(fx.path("b.rs"), "").unwrap();
    fx.press(ch('R'));
    assert_eq!(names(&fx.state().entries), vec!["a.rs", "b.rs"]);
}

// =============================================================================
// Drawer width
// =============================================================================

#[test]
fn ctrl_shift_arrows_resize_and_the_width_persists() {
    let mut fx = Fixture::new(&["a.rs"]);
    let base = drawer_width(120, None);

    fx.press(ctrl_shift(KeyCode::Right));
    assert_eq!(fx.state().width, Some(base + 2));
    assert_eq!(fx.app.file_explorer_width, Some(base + 2));

    fx.press(ctrl_shift(KeyCode::Left));
    fx.press(ctrl_shift(KeyCode::Left));
    assert_eq!(fx.state().width, Some(base - 2));

    fx.press(ch('>'));
    assert_eq!(fx.state().width, Some(base));

    for _ in 0..100 {
        fx.press(ch('<'));
    }
    assert_eq!(fx.state().width, Some(MIN_DRAWER_WIDTH));

    // Closing and reopening keeps the chosen width.
    fx.press(key(KeyCode::Esc));
    assert!(explorer(&fx.app).is_none());
    fx.open();
    assert_eq!(fx.state().width, Some(MIN_DRAWER_WIDTH));

    fx.press(ch('='));
    assert_eq!(fx.state().width, None);
    assert_eq!(fx.app.file_explorer_width, None);
}

#[test]
fn ctrl_zero_resets_the_width() {
    let mut fx = Fixture::new(&["a.rs"]);
    fx.press(ctrl_shift(KeyCode::Right));
    fx.press(ctrl('0'));
    assert_eq!(fx.state().width, None);
}

#[test]
fn plain_arrows_navigate_instead_of_resizing() {
    let mut fx = Fixture::new(&["dir/inner.rs"]);
    fx.press(key(KeyCode::Right));
    assert_eq!(names(&fx.state().entries), vec!["dir", "inner.rs"]);
    assert_eq!(fx.state().width, None);
    fx.press(key(KeyCode::Left));
    assert_eq!(names(&fx.state().entries), vec!["dir"]);
}

// =============================================================================
// Viewer hand-off and focus
// =============================================================================

#[test]
fn enter_opens_deeply_nested_files_in_the_viewer() {
    // More than two path components below the project root.
    let mut fx = Fixture::new(&["crates/rsi/src/main.rs=fn main() {}"]);
    fx.select("crates/rsi/src/main.rs");

    fx.press(key(KeyCode::Enter));

    assert_eq!(fx.viewer_path(), Some(fx.path("crates/rsi/src/main.rs")));
    assert_eq!(fx.viewer().unwrap().surface.content(), "fn main() {}");
    assert!(!fx.state().explorer_focused, "viewer takes focus");
}

#[test]
fn binary_files_are_refused_with_a_clear_message() {
    let mut fx = Fixture::new(&[]);
    fs::write(fx.path("blob.bin"), [0xff, 0xfe, 0x00, 0x80]).unwrap();
    fx.state_mut().refresh();
    fx.select("blob.bin");

    fx.press(key(KeyCode::Enter));

    assert!(fx.viewer().is_none());
    assert_eq!(fx.notice(), "Binary or non-UTF-8 file: blob.bin");
    assert!(fx.state().explorer_focused);
}

#[test]
fn viewer_focus_hands_escape_q_and_space_to_the_viewer() {
    let mut fx = Fixture::new(&["main.rs=x"]);
    fx.select("main.rs");
    fx.press(key(KeyCode::Enter));
    assert!(!fx.state().explorer_focused);

    for code in [KeyCode::Esc, KeyCode::Char('q'), KeyCode::Char(' ')] {
        assert!(
            !route_key(&mut fx.app, key(code)),
            "{code:?} belongs to the viewer"
        );
    }
    assert!(explorer(&fx.app).is_some(), "explorer stays open");

    assert!(route_key(&mut fx.app, ctrl('h')));
    assert!(fx.state().explorer_focused);
}

#[test]
fn closing_the_viewer_returns_focus_to_the_tree() {
    let mut fx = Fixture::new(&["main.rs=x"]);
    fx.select("main.rs");
    fx.press(key(KeyCode::Enter));

    assert!(crate::file_viewer::handle_file_viewer_key(
        &mut fx.app,
        ch('q')
    ));

    assert!(fx.viewer().is_none());
    assert!(fx.state().explorer_focused);
}

#[test]
fn keys_reach_the_tree_when_the_viewer_vanished() {
    let mut fx = Fixture::new(&["a.rs", "b.rs"]);
    fx.state_mut().explorer_focused = false;

    assert!(route_key(&mut fx.app, ch('j')));

    assert!(fx.state().explorer_focused);
    assert_eq!(fx.selected(), Some(fx.path("b.rs")));
}

#[test]
fn ctrl_l_without_an_open_file_keeps_tree_focus() {
    let mut fx = Fixture::new(&["a.rs"]);
    fx.press(ctrl('l'));
    assert!(fx.state().explorer_focused);
    assert!(fx.notice().contains("No file open"), "{}", fx.notice());
}

#[test]
fn space_e_in_the_viewer_toggles_the_explorer_and_reveals_the_file() {
    let mut fx = Fixture::new(&["src/deep/file.rs=x", "other.rs"]);
    fx.select("src/deep/file.rs");
    fx.press(key(KeyCode::Enter));

    assert!(crate::file_viewer::handle_file_viewer_key(
        &mut fx.app,
        ch(' ')
    ));
    assert!(crate::file_viewer::handle_file_viewer_key(
        &mut fx.app,
        ch('e')
    ));
    assert!(
        explorer(&fx.app).is_none(),
        "Space e closes the open explorer"
    );

    assert!(crate::file_viewer::handle_file_viewer_key(
        &mut fx.app,
        ch(' ')
    ));
    assert!(crate::file_viewer::handle_file_viewer_key(
        &mut fx.app,
        ch('e')
    ));
    assert!(fx.state().explorer_focused);
    assert_eq!(fx.selected(), Some(fx.path("src/deep/file.rs")));
}

#[test]
fn tab_previews_without_leaving_the_tree() {
    let mut fx = Fixture::new(&["a.rs=a"]);
    fx.select("a.rs");
    fx.press(key(KeyCode::Tab));
    assert_eq!(fx.viewer_path(), Some(fx.path("a.rs")));
    assert!(fx.state().explorer_focused);
}

#[test]
fn follow_preview_opens_files_as_the_selection_moves() {
    let mut fx = Fixture::new(&["a.rs=a", "b.rs=b"]);

    fx.press(ch('P'));
    assert!(fx.state().follow_preview);
    assert_eq!(fx.viewer_path(), Some(fx.path("a.rs")));

    fx.press(ch('j'));
    assert_eq!(fx.viewer_path(), Some(fx.path("b.rs")));
    assert!(fx.state().explorer_focused);

    fx.press(ch('P'));
    assert!(!fx.state().follow_preview);
}

#[test]
fn unfocused_explorer_lets_paste_reach_the_viewer() {
    let mut fx = Fixture::new(&["a.rs=x"]);
    fx.select("a.rs");
    fx.press(key(KeyCode::Enter));

    assert!(!crate::overlay::try_paste_text_overlay(
        &mut fx.app,
        "pasted"
    ));
    assert!(crate::file_viewer::try_paste_text_file_viewer(
        &mut fx.app,
        "pasted"
    ));

    let viewer = fx.viewer().unwrap();
    assert!(viewer.surface.content().contains("pasted"));
    assert!(viewer.dirty);
}

#[test]
fn viewer_marks_report_active_and_dirty_files() {
    let mut fx = Fixture::new(&["a.rs=x"]);
    fx.select("a.rs");
    fx.press(key(KeyCode::Enter));
    let session_id = fx.session_id();
    fx.app
        .sessions
        .get_mut(&session_id)
        .unwrap()
        .file_viewer
        .as_mut()
        .unwrap()
        .dirty = true;

    let marks = viewer_marks(&fx.app);
    assert_eq!(marks.active, Some(fx.path("a.rs")));
    assert!(marks.dirty.contains(&fx.path("a.rs")));
}

// =============================================================================
// Finder
// =============================================================================

#[test]
fn finder_types_every_letter_and_opens_with_reveal() {
    let mut fx = Fixture::new(&["jk/deep/kj.rs=x", "other.rs"]);

    fx.press(ch('/'));
    assert!(fx.state().finder.active);
    fx.type_str("jkq ");
    assert_eq!(fx.state().finder.query, "jkq ");
    fx.press(ctrl('w'));
    fx.press(ctrl('u'));
    assert_eq!(fx.state().finder.query, "");
    fx.type_str("kj.rs");
    assert_eq!(
        fx.state()
            .finder
            .results
            .first()
            .map(|&i| fx.state().finder.cache[i].clone()),
        Some(PathBuf::from("jk/deep/kj.rs"))
    );

    fx.press(key(KeyCode::Enter));

    assert!(!fx.state().finder.active);
    assert_eq!(fx.viewer_path(), Some(fx.path("jk/deep/kj.rs")));
    assert_eq!(fx.selected(), Some(fx.path("jk/deep/kj.rs")));
}

#[test]
fn finder_moves_with_arrows_and_ctrl_keys() {
    let mut fx = Fixture::new(&["a.rs", "b.rs", "c.rs"]);
    fx.press(ch('/'));
    fx.press(key(KeyCode::Down));
    fx.press(ctrl('j'));
    assert_eq!(fx.state().finder.selected, 2);
    fx.press(ctrl('k'));
    fx.press(key(KeyCode::Up));
    fx.press(key(KeyCode::Up));
    assert_eq!(fx.state().finder.selected, 0);
}

// =============================================================================
// Mouse
// =============================================================================

fn with_layout(fx: &mut Fixture) {
    let state = fx.state_mut();
    state.drawer_area = Rect::new(0, 0, 30, 20);
    state.list_area = Rect::new(2, 1, 26, 18);
    state.scroll_offset = 0;
}

#[test]
fn clicking_selects_then_opens() {
    let mut fx = Fixture::new(&["a.rs=a", "b.rs=b"]);
    with_layout(&mut fx);

    assert!(handle_mouse_click(&mut fx.app, 5, 2));
    assert_eq!(fx.selected(), Some(fx.path("b.rs")));
    assert!(fx.viewer().is_none());

    assert!(handle_mouse_click(&mut fx.app, 5, 2));
    assert_eq!(fx.viewer_path(), Some(fx.path("b.rs")));
    assert!(!fx.state().explorer_focused);

    // Clicking back into the drawer refocuses the tree; clicking outside
    // hands focus to the open viewer.
    assert!(handle_mouse_click(&mut fx.app, 5, 15));
    assert!(fx.state().explorer_focused);
    assert!(!handle_mouse_click(&mut fx.app, 60, 5));
    assert!(!fx.state().explorer_focused);
}

#[test]
fn wheel_over_the_drawer_moves_the_selection() {
    let mut fx = Fixture::new(&["a", "b", "c", "d", "e"]);
    with_layout(&mut fx);

    assert!(handle_mouse_scroll(&mut fx.app, 3, 3, false));
    assert_eq!(fx.selected(), Some(fx.path("d")));
    assert!(handle_mouse_scroll(&mut fx.app, 3, 3, true));
    assert_eq!(fx.selected(), Some(fx.path("a")));
    assert!(!handle_mouse_scroll(&mut fx.app, 80, 3, true));
}

// =============================================================================
// External-change conflicts
// =============================================================================

#[test]
fn explorer_open_preserves_dirty_draft_and_reports_conflict() {
    let path = std::env::temp_dir().join(format!("rsi-explorer-{}.rs", Uuid::new_v4()));
    fs::write(&path, "original").unwrap();
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
    fs::write(&path, "external").unwrap();

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
    fs::remove_file(path).unwrap();
}
