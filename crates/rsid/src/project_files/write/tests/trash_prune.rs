//! local #120 (hub #3103): the daemon trash follows the TUI's 7-day rule
//! through `rsi_common::trash_prune`. Every test builds a synthetic
//! `<tmp home>/.rsi/project-file-trash`; the operator's `~/.rsi` is never
//! touched.

use super::*;
use chrono::{Duration as Age, Utc};
use rsi_common::trash_prune::new_batch_name;
use std::os::unix::fs::symlink;

/// A synthetic home: a project root, data outside the trash and the
/// `FileOps` whose trash is `<home>/.rsi/project-file-trash`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
struct Home {
    _dir: TempDir,
    root: PathBuf,
    outside: PathBuf,
    trash: PathBuf,
    ops: FileOps,
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
fn home() -> Home {
    let dir = TempDir::new().unwrap();
    let base = canonical_root(dir.path()).unwrap();
    let root = base.join("project");
    let outside = base.join("outside");
    let trash = base.join(".rsi").join("project-file-trash");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(&trash).unwrap();
    Home {
        ops: FileOps::new(trash.clone()),
        _dir: dir,
        root,
        outside,
        trash,
    }
}

/// A batch recorded `age` ago holding `<name>` (a file), with its sidecar.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
fn aged_batch(trash: &Path, age: Age, name: &str) -> String {
    let batch = new_batch_name(Utc::now() - age);
    put(trash, &format!("{batch}/{name}"), "trashed\n");
    put(trash, &format!("{batch}.origin"), "/project/x");
    batch
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn a_trash_write_prunes_only_batches_older_than_seven_days_inside_the_trash() {
    let h = home();
    let old = aged_batch(&h.trash, Age::days(8), "old.rs");
    let young = aged_batch(&h.trash, Age::days(6), "young.rs");
    // Outside the trash root: an old-named batch and data a link points at.
    let outside_batch = aged_batch(&h.outside, Age::days(30), "keep.rs");
    put(&h.outside, "sentinel.txt", "outside\n");
    let linked = new_batch_name(Utc::now() - Age::days(30));
    symlink(&h.outside, h.trash.join(&linked)).unwrap();
    symlink(
        h.outside.join("sentinel.txt"),
        h.trash.join(&old).join("link-out"),
    )
    .unwrap();
    put(&h.root, "gone.txt", "now\n");

    // The first trash write runs a pass (none ran yet).
    f_trash(&h, "gone.txt");

    let fresh = batch_of(&h, "gone.txt");
    let mut expected = vec![
        format!("{young}.origin"),
        format!("{young}/"),
        format!("{young}/young.rs"),
        format!("{fresh}.origin"),
        format!("{fresh}/"),
        format!("{fresh}/gone.txt"),
        linked.clone(),
    ];
    expected.sort();
    let mut got = tree(&h.trash);
    got.sort();
    assert_eq!(got, expected);
    assert!(
        fs::symlink_metadata(h.trash.join(&linked))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let mut outside = tree(&h.outside);
    outside.sort();
    assert_eq!(
        outside,
        vec![
            format!("{outside_batch}.origin"),
            format!("{outside_batch}/"),
            format!("{outside_batch}/keep.rs"),
            "sentinel.txt".to_string(),
        ]
    );
    assert_eq!(text(&h.outside, "sentinel.txt"), "outside\n");

    // Bounded: a second trash write inside the interval runs no pass.
    let later = aged_batch(&h.trash, Age::days(9), "later.rs");
    put(&h.root, "second.txt", "2\n");
    f_trash(&h, "second.txt");
    assert!(h.trash.join(&later).join("later.rs").is_file());
    // The start pass (any time) does prune it.
    let report = h.ops.prune_trash().unwrap();
    assert_eq!(report.batches_removed, 1);
    assert_eq!(report.sidecars_removed, 1);
    assert!(!h.trash.join(&later).exists());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
fn f_trash(h: &Home, path: &str) {
    let trashed = h.ops.trash(&h.root, path).unwrap();
    assert_eq!(trashed.op, ProjectFileOperationKindV1::Trashed);
}

/// The batch whose sidecar names `<root>/<path>`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
fn batch_of(h: &Home, path: &str) -> String {
    let origin = h.root.join(path).to_string_lossy().into_owned();
    tree(&h.trash)
        .into_iter()
        .filter_map(|name| name.strip_suffix(".origin").map(str::to_string))
        .find(|batch| text(&h.trash, &format!("{batch}.origin")) == origin)
        .expect("a batch for the trashed path")
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn undo_of_a_pruned_batch_is_refused_no_longer_in_the_trash() {
    let h = home();
    h.ops
        .create(&h.root, "made", ProjectEntryKindV1::Dir)
        .unwrap();
    put(&h.root, "notes.md", "notes\n");
    f_trash(&h, "notes.md");
    let batch = batch_of(&h, "notes.md");

    // Eight days later a pass removes the batch.
    let report = h.ops.prune_locked(Utc::now() + Age::days(8)).unwrap();
    assert_eq!(report.batches_removed, 1);
    assert!(!h.trash.join(&batch).exists());

    let refused = h.ops.undo(&h.root).unwrap_err();
    let message = refused.to_string();
    assert_eq!(code(refused), NO_LONGER_IN_TRASH);
    assert!(
        message.contains("'notes.md' is no longer in the trash"),
        "{message}"
    );
    // The refused entry is dropped: the next undo reverses the add.
    let undone = h.ops.undo(&h.root).unwrap();
    assert_eq!(undone.undone, ProjectFileOperationKindV1::Created);
    assert_eq!(tree(&h.root), Vec::<String>::new());
}
