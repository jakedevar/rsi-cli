//! hub #3103: exact end states of each project-file write in a synthetic
//! root, the stale-save guard, undo, and every confinement refusal. The
//! post-land review findings (hub #3143-#3150) are in `hub_findings`.

use super::super::{
    INVALID_REQUEST, NOT_A_FILE, NOT_FOUND, OUTSIDE_ROOT, canonical_root, read_file, slash_path,
};
use super::*;
use rsi_common::project_files::{
    MAX_WRITE_BYTES, ProjectEntryKindV1, ProjectFileOperationKindV1, ProjectFileUndoEffectV1,
};
use tempfile::TempDir;

mod hub_findings;
mod trash_prune;

/// A synthetic project root, a trash beside it and the `FileOps` under test.
struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    outside: PathBuf,
    trash: PathBuf,
    ops: FileOps,
}

fn fixture() -> Fixture {
    let dir = TempDir::new().unwrap();
    let base = canonical_root(dir.path()).unwrap();
    let root = base.join("project");
    let outside = base.join("outside");
    let trash = base.join("trash");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    Fixture {
        ops: FileOps::new(trash.clone()),
        _dir: dir,
        root,
        outside,
        trash,
    }
}

fn put(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn text(root: &Path, relative: &str) -> String {
    fs::read_to_string(root.join(relative)).unwrap()
}

fn code(error: crate::error::DaemonError) -> String {
    let message = error.to_string();
    let start = message.find("project_file_").expect("typed refusal");
    message[start..].split(':').next().unwrap().to_string()
}

fn version(root: &Path, relative: &str) -> String {
    read_file(root, relative, None, None, None)
        .unwrap()
        .version
        .expect("versioned")
}

/// Names under `dir`, sorted, recursively, `/`-separated ("d/" for a dir).
fn tree(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let rel = slash_path(path.strip_prefix(base).unwrap());
            if entry.file_type().unwrap().is_dir() {
                out.push(format!("{rel}/"));
                walk(base, &path, out);
            } else {
                out.push(rel);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn save_with_the_observed_version_replaces_the_content_and_answers_the_new_version() {
    let f = fixture();
    put(&f.root, "src/main.rs", "fn main() {}\n");
    let observed = version(&f.root, "src/main.rs");
    assert!(observed.starts_with("v1:13:"), "{observed}");

    let saved = f
        .ops
        .save(
            &f.root,
            "src/main.rs",
            "fn main() { run() }\n",
            Some(&observed),
            false,
        )
        .unwrap();

    assert_eq!(text(&f.root, "src/main.rs"), "fn main() { run() }\n");
    assert_eq!(saved.path, "src/main.rs");
    assert_eq!(saved.size, 20);
    assert!(!saved.forced);
    assert!(!saved.created);
    assert_eq!(saved.version, version(&f.root, "src/main.rs"));
    // The temporary file was renamed away: only the saved file remains.
    assert_eq!(tree(&f.root), vec!["src/", "src/main.rs"]);
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn save_keeps_the_file_mode() {
    use std::os::unix::fs::PermissionsExt;
    let f = fixture();
    put(&f.root, "run.sh", "echo a\n");
    fs::set_permissions(f.root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    let observed = version(&f.root, "run.sh");
    f.ops
        .save(&f.root, "run.sh", "echo b\n", Some(&observed), false)
        .unwrap();
    let mode = fs::metadata(f.root.join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    assert_eq!(text(&f.root, "run.sh"), "echo b\n");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn a_stale_save_is_refused_then_reload_or_force_saves() {
    let f = fixture();
    put(&f.root, "notes.md", "draft one\n");
    let observed = version(&f.root, "notes.md");
    // Another writer changes the file after the editor read it.
    put(&f.root, "notes.md", "someone else\n");

    let refused = f
        .ops
        .save(&f.root, "notes.md", "my edit\n", Some(&observed), false)
        .unwrap_err();
    assert_eq!(code(refused), STALE);
    assert_eq!(text(&f.root, "notes.md"), "someone else\n");
    // A save with no observed version is stale too.
    let refused = f
        .ops
        .save(&f.root, "notes.md", "my edit\n", None, false)
        .unwrap_err();
    assert_eq!(code(refused), STALE);

    // Reload: the read answers the on-disk content and its version.
    let reloaded = read_file(&f.root, "notes.md", None, None, None).unwrap();
    assert_eq!(reloaded.content.as_deref(), Some("someone else\n"));
    let fresh = reloaded.version.unwrap();
    assert_ne!(fresh, observed);
    let saved = f
        .ops
        .save(&f.root, "notes.md", "merged\n", Some(&fresh), false)
        .unwrap();
    assert!(!saved.forced);
    assert_eq!(text(&f.root, "notes.md"), "merged\n");

    // Force overwrites a changed file and says so.
    put(&f.root, "notes.md", "changed again\n");
    let forced = f
        .ops
        .save(&f.root, "notes.md", "mine wins\n", Some(&fresh), true)
        .unwrap();
    assert!(forced.forced);
    assert!(!forced.created);
    assert_eq!(text(&f.root, "notes.md"), "mine wins\n");
    assert_eq!(forced.version, version(&f.root, "notes.md"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn a_vanished_file_is_recreated_only_by_force() {
    let f = fixture();
    put(&f.root, "doc/a.txt", "x\n");
    let observed = version(&f.root, "doc/a.txt");
    fs::remove_file(f.root.join("doc/a.txt")).unwrap();

    let refused = f
        .ops
        .save(&f.root, "doc/a.txt", "y\n", Some(&observed), false)
        .unwrap_err();
    assert_eq!(code(refused), STALE);
    assert_eq!(tree(&f.root), vec!["doc/"]);

    let saved = f
        .ops
        .save(&f.root, "doc/a.txt", "y\n", Some(&observed), true)
        .unwrap();
    assert!(saved.created && saved.forced);
    assert_eq!(text(&f.root, "doc/a.txt"), "y\n");
    // Force never creates a missing directory.
    let refused = f
        .ops
        .save(&f.root, "nope/b.txt", "z\n", None, true)
        .unwrap_err();
    assert_eq!(code(refused), NOT_FOUND);
    assert_eq!(tree(&f.root), vec!["doc/", "doc/a.txt"]);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn oversized_content_and_a_directory_target_are_refused() {
    let f = fixture();
    fs::create_dir_all(f.root.join("dir")).unwrap();
    let big = "a".repeat(MAX_WRITE_BYTES + 1);
    assert_eq!(
        code(
            f.ops
                .save(&f.root, "big.txt", &big, None, true)
                .unwrap_err()
        ),
        TOO_LARGE
    );
    assert_eq!(
        code(f.ops.save(&f.root, "dir", "x", None, true).unwrap_err()),
        NOT_A_FILE
    );
    assert_eq!(tree(&f.root), vec!["dir/"]);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn create_adds_a_file_and_a_directory_with_parents_and_never_overwrites() {
    let f = fixture();
    put(&f.root, "keep.txt", "kept\n");

    let file = f
        .ops
        .create(&f.root, "a/b/new.rs", ProjectEntryKindV1::File)
        .unwrap();
    assert_eq!(file.op, ProjectFileOperationKindV1::Created);
    assert_eq!(file.path, "a/b/new.rs");
    assert_eq!(file.created_top.as_deref(), Some("a"));
    assert_eq!(file.undo_depth, 1);
    let dir = f
        .ops
        .create(&f.root, "a/c", ProjectEntryKindV1::Dir)
        .unwrap();
    assert_eq!(dir.created_top.as_deref(), Some("a/c"));
    assert_eq!(dir.undo_depth, 2);
    assert_eq!(
        tree(&f.root),
        vec!["a/", "a/b/", "a/b/new.rs", "a/c/", "keep.txt"]
    );
    assert_eq!(text(&f.root, "a/b/new.rs"), "");

    let refused = f
        .ops
        .create(&f.root, "keep.txt", ProjectEntryKindV1::File)
        .unwrap_err();
    assert_eq!(code(refused), EXISTS);
    assert_eq!(text(&f.root, "keep.txt"), "kept\n");
    let refused = f
        .ops
        .create(&f.root, "x", ProjectEntryKindV1::Symlink)
        .unwrap_err();
    assert_eq!(code(refused), INVALID_REQUEST);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn undo_removes_an_empty_create_and_refuses_an_empty_log() {
    let f = fixture();
    f.ops
        .create(&f.root, "a/b/new.rs", ProjectEntryKindV1::File)
        .unwrap();
    let undone = f.ops.undo(&f.root).unwrap();
    assert_eq!(undone.undone, ProjectFileOperationKindV1::Created);
    assert_eq!(undone.effect, ProjectFileUndoEffectV1::Removed);
    // The top-most created entry goes, so nothing of the add remains.
    assert_eq!(undone.path, "a");
    assert_eq!(undone.undo_depth, 0);
    assert_eq!(tree(&f.root), Vec::<String>::new());
    assert_eq!(code(f.ops.undo(&f.root).unwrap_err()), NOTHING_TO_UNDO);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn rename_moves_an_entry_and_undo_moves_it_back() {
    let f = fixture();
    put(&f.root, "src/old.rs", "body\n");
    put(&f.root, "taken.rs", "other\n");

    let moved = f.ops.rename(&f.root, "src/old.rs", "lib/new.rs").unwrap();
    assert_eq!(moved.op, ProjectFileOperationKindV1::Moved);
    assert_eq!(moved.path, "src/old.rs");
    assert_eq!(moved.to.as_deref(), Some("lib/new.rs"));
    assert_eq!(
        tree(&f.root),
        vec!["lib/", "lib/new.rs", "src/", "taken.rs"]
    );
    assert_eq!(text(&f.root, "lib/new.rs"), "body\n");

    let refused = f.ops.rename(&f.root, "lib/new.rs", "taken.rs").unwrap_err();
    assert_eq!(code(refused), EXISTS);
    assert_eq!(text(&f.root, "taken.rs"), "other\n");
    let refused = f.ops.rename(&f.root, "lib", "lib/inner").unwrap_err();
    assert_eq!(code(refused), INVALID_REQUEST);
    let refused = f.ops.rename(&f.root, "ghost.rs", "x.rs").unwrap_err();
    assert_eq!(code(refused), NOT_FOUND);

    let undone = f.ops.undo(&f.root).unwrap();
    assert_eq!(undone.undone, ProjectFileOperationKindV1::Moved);
    assert_eq!(undone.effect, ProjectFileUndoEffectV1::MovedBack);
    assert_eq!(undone.path, "src/old.rs");
    assert_eq!(undone.from.as_deref(), Some("lib/new.rs"));
    assert_eq!(text(&f.root, "src/old.rs"), "body\n");
    assert_eq!(
        tree(&f.root),
        vec!["lib/", "src/", "src/old.rs", "taken.rs"]
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn copy_duplicates_a_tree_and_undo_trashes_the_non_empty_copy() {
    let f = fixture();
    put(&f.root, "pkg/a.txt", "a\n");
    put(&f.root, "pkg/sub/b.txt", "b\n");

    let copied = f.ops.copy(&f.root, "pkg", "pkg2").unwrap();
    assert_eq!(copied.op, ProjectFileOperationKindV1::Copied);
    assert_eq!(copied.to.as_deref(), Some("pkg2"));
    assert_eq!(
        tree(&f.root),
        vec![
            "pkg/",
            "pkg/a.txt",
            "pkg/sub/",
            "pkg/sub/b.txt",
            "pkg2/",
            "pkg2/a.txt",
            "pkg2/sub/",
            "pkg2/sub/b.txt"
        ]
    );
    assert_eq!(text(&f.root, "pkg2/sub/b.txt"), "b\n");
    let refused = f.ops.copy(&f.root, "pkg/a.txt", "pkg2/a.txt").unwrap_err();
    assert_eq!(code(refused), EXISTS);

    let undone = f.ops.undo(&f.root).unwrap();
    assert_eq!(undone.undone, ProjectFileOperationKindV1::Copied);
    assert_eq!(undone.effect, ProjectFileUndoEffectV1::Trashed);
    assert_eq!(undone.path, "pkg2");
    assert_eq!(
        tree(&f.root),
        vec!["pkg/", "pkg/a.txt", "pkg/sub/", "pkg/sub/b.txt"]
    );
    // Undo never destroys content: the copy is in the trash.
    let trashed: Vec<String> = tree(&f.trash)
        .into_iter()
        .filter(|path| path.ends_with("pkg2/sub/b.txt"))
        .collect();
    assert_eq!(trashed.len(), 1, "{:?}", tree(&f.trash));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn trash_moves_an_entry_into_the_daemon_trash_and_undo_restores_it() {
    let f = fixture();
    put(&f.root, "docs/guide.md", "guide\n");

    let trashed = f.ops.trash(&f.root, "docs/guide.md").unwrap();
    assert_eq!(trashed.op, ProjectFileOperationKindV1::Trashed);
    assert_eq!(trashed.path, "docs/guide.md");
    assert_eq!(tree(&f.root), vec!["docs/"]);
    let in_trash = tree(&f.trash);
    let batch = in_trash[0].trim_end_matches(".origin").to_string();
    assert_eq!(
        in_trash,
        vec![
            format!("{batch}.origin"),
            format!("{batch}/"),
            format!("{batch}/guide.md")
        ]
    );
    assert_eq!(text(&f.trash, &format!("{batch}/guide.md")), "guide\n");
    assert_eq!(
        text(&f.trash, &format!("{batch}.origin")),
        f.root.join("docs/guide.md").to_string_lossy()
    );

    let undone = f.ops.undo(&f.root).unwrap();
    assert_eq!(undone.undone, ProjectFileOperationKindV1::Trashed);
    assert_eq!(undone.effect, ProjectFileUndoEffectV1::Restored);
    assert_eq!(undone.path, "docs/guide.md");
    assert_eq!(text(&f.root, "docs/guide.md"), "guide\n");
    // The emptied batch and its sidecar are gone.
    assert_eq!(tree(&f.trash), Vec::<String>::new());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn an_undo_conflict_keeps_the_operation_for_a_retry() {
    let f = fixture();
    put(&f.root, "a.txt", "original\n");
    f.ops.trash(&f.root, "a.txt").unwrap();
    put(&f.root, "a.txt", "recreated\n");

    let refused = f.ops.undo(&f.root).unwrap_err();
    assert_eq!(code(refused), UNDO_CONFLICT);
    assert_eq!(text(&f.root, "a.txt"), "recreated\n");

    fs::remove_file(f.root.join("a.txt")).unwrap();
    let undone = f.ops.undo(&f.root).unwrap();
    assert_eq!(undone.effect, ProjectFileUndoEffectV1::Restored);
    assert_eq!(text(&f.root, "a.txt"), "original\n");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn the_undo_log_is_per_root() {
    let f = fixture();
    let other = f.outside.clone();
    put(&f.root, "a.txt", "a\n");
    f.ops.trash(&f.root, "a.txt").unwrap();
    assert_eq!(code(f.ops.undo(&other).unwrap_err()), NOTHING_TO_UNDO);
    assert_eq!(
        f.ops.undo(&f.root).unwrap().effect,
        ProjectFileUndoEffectV1::Restored
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn traversal_absolute_and_root_paths_are_refused_by_every_write() {
    let f = fixture();
    put(&f.root, "in.txt", "in\n");
    put(&f.outside, "secret.txt", "secret\n");
    let absolute = f.outside.join("secret.txt").to_string_lossy().into_owned();
    for escape in [
        "../outside/secret.txt",
        "a/../../outside/x",
        absolute.as_str(),
    ] {
        let refusals = [
            f.ops.save(&f.root, escape, "x", None, true).unwrap_err(),
            f.ops
                .create(&f.root, escape, ProjectEntryKindV1::File)
                .unwrap_err(),
            f.ops.rename(&f.root, "in.txt", escape).unwrap_err(),
            f.ops.rename(&f.root, escape, "got.txt").unwrap_err(),
            f.ops.copy(&f.root, "in.txt", escape).unwrap_err(),
            f.ops.copy(&f.root, escape, "got.txt").unwrap_err(),
            f.ops.trash(&f.root, escape).unwrap_err(),
        ];
        for refusal in refusals {
            assert_eq!(code(refusal), OUTSIDE_ROOT, "{escape}");
        }
    }
    for root_itself in ["", ".", "./"] {
        assert_eq!(
            code(f.ops.trash(&f.root, root_itself).unwrap_err()),
            INVALID_REQUEST
        );
        assert_eq!(
            code(f.ops.rename(&f.root, root_itself, "moved").unwrap_err()),
            INVALID_REQUEST
        );
    }
    assert_eq!(tree(&f.root), vec!["in.txt"]);
    assert_eq!(tree(&f.outside), vec!["secret.txt"]);
    assert_eq!(text(&f.outside, "secret.txt"), "secret\n");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn the_daemon_trash_inside_a_root_is_refused() {
    let dir = TempDir::new().unwrap();
    let root = canonical_root(dir.path()).unwrap();
    let ops = FileOps::new(root.join(".trash"));
    put(&root, "a.txt", "a\n");
    ops.trash(&root, "a.txt").unwrap();
    let batch = tree(&root.join(".trash"))[0]
        .trim_end_matches(".origin")
        .to_string();
    assert_eq!(
        code(ops.trash(&root, ".trash").unwrap_err()),
        INVALID_REQUEST
    );
    assert_eq!(
        code(
            ops.rename(&root, &format!(".trash/{batch}/a.txt"), "b.txt")
                .unwrap_err()
        ),
        INVALID_REQUEST
    );
    assert_eq!(
        ops.undo(&root).unwrap().effect,
        ProjectFileUndoEffectV1::Restored
    );
    assert_eq!(text(&root, "a.txt"), "a\n");
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn symlink_escapes_are_refused_and_links_are_acted_on_not_followed() {
    use std::os::unix::fs::symlink;
    let f = fixture();
    put(&f.outside, "secret.txt", "secret\n");
    put(&f.root, "in.txt", "in\n");
    symlink(&f.outside, f.root.join("out")).unwrap();
    symlink(f.outside.join("secret.txt"), f.root.join("secret-link")).unwrap();

    // A symlinked directory leading outside never carries a write out.
    let refusals = [
        f.ops
            .save(&f.root, "out/secret.txt", "pwned", None, true)
            .unwrap_err(),
        f.ops
            .save(&f.root, "out/new.txt", "pwned", None, true)
            .unwrap_err(),
        f.ops
            .create(&f.root, "out/new.txt", ProjectEntryKindV1::File)
            .unwrap_err(),
        f.ops
            .create(&f.root, "out/deeper/x", ProjectEntryKindV1::Dir)
            .unwrap_err(),
        f.ops.rename(&f.root, "in.txt", "out/in.txt").unwrap_err(),
        f.ops.copy(&f.root, "in.txt", "out/in.txt").unwrap_err(),
        f.ops
            .rename(&f.root, "out/secret.txt", "got.txt")
            .unwrap_err(),
        f.ops
            .copy(&f.root, "out/secret.txt", "got.txt")
            .unwrap_err(),
        f.ops.trash(&f.root, "out/secret.txt").unwrap_err(),
        // A file symlink pointing outside is not saved through.
        f.ops
            .save(&f.root, "secret-link", "pwned", None, true)
            .unwrap_err(),
    ];
    for refusal in refusals {
        assert_eq!(code(refusal), OUTSIDE_ROOT);
    }
    assert_eq!(tree(&f.outside), vec!["secret.txt"]);
    assert_eq!(text(&f.outside, "secret.txt"), "secret\n");

    // Trashing a link moves the link itself; its target stays.
    f.ops.trash(&f.root, "out").unwrap();
    assert_eq!(text(&f.outside, "secret.txt"), "secret\n");
    assert!(f.root.join("out").symlink_metadata().is_err());
    f.ops.undo(&f.root).unwrap();
    assert!(
        f.root
            .join("out")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
    // Copying a link copies the link, not the tree behind it.
    f.ops.copy(&f.root, "out", "out-copy").unwrap();
    let copy = f.root.join("out-copy");
    assert!(copy.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(fs::read_link(copy).unwrap(), f.outside);
    assert_eq!(tree(&f.outside), vec!["secret.txt"]);
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn a_symlink_swapped_in_before_undo_cannot_carry_the_restore_outside() {
    use std::os::unix::fs::symlink;
    let f = fixture();
    put(&f.root, "dir/a.txt", "a\n");
    f.ops.trash(&f.root, "dir/a.txt").unwrap();
    fs::remove_dir(f.root.join("dir")).unwrap();
    symlink(&f.outside, f.root.join("dir")).unwrap();

    assert_eq!(code(f.ops.undo(&f.root).unwrap_err()), OUTSIDE_ROOT);
    assert_eq!(tree(&f.outside), Vec::<String>::new());
}
