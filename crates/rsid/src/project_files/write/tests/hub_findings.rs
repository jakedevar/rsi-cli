//! One regression test (or more) per finding of the post-land review of hub
//! #3103 (hub #3143-#3150), each a synthetic reproducer in a temporary root.
//! A test hook plays the concurrent writer at the exact point the review
//! named (`Hook`), and `cross_device` makes every transfer rename fail as
//! if it crossed filesystems, so the cross-filesystem paths run on one disk.

use super::*;
use std::os::unix::fs::symlink;
use std::sync::atomic::Ordering;

/// Run `action` the first time the operation reaches `point`.
fn at(ops: &FileOps, point: Hook, action: impl FnOnce() + Send + 'static) {
    let mut action = Some(action);
    *ops.hook.lock().unwrap() = Some(Box::new(move |reached| {
        if reached == point
            && let Some(action) = action.take()
        {
            action();
        }
    }));
}

fn clear(ops: &FileOps) {
    *ops.hook.lock().unwrap() = None;
}

/// Replace `root/dir` by a symlink to `target`, keeping the real directory
/// as `root/<dir>-aside`.
fn swap_for_link(root: &Path, dir: &str, target: &Path) {
    fs::rename(root.join(dir), root.join(format!("{dir}-aside"))).unwrap();
    symlink(target, root.join(dir)).unwrap();
}

fn sentinel(f: &Fixture) {
    put(&f.outside, "sentinel.txt", "outside\n");
}

fn outside_intact(f: &Fixture) {
    assert_eq!(tree(&f.outside), vec!["sentinel.txt"]);
    assert_eq!(text(&f.outside, "sentinel.txt"), "outside\n");
}

/// The only trash batch of the fixture.
fn batch(f: &Fixture) -> String {
    let names = tree(&f.trash);
    let origin = names
        .iter()
        .find(|name| name.ends_with(".origin"))
        .expect("one batch");
    origin.trim_end_matches(".origin").to_string()
}

// --- hub #3143: a checked parent replaced by a symlink -----------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3143_a_parent_swapped_for_an_outside_symlink_after_the_check_keeps_every_write_inside() {
    type Op = fn(&Fixture, &str) -> Result<()>;
    let cases: [(&str, Op, &[&str]); 5] = [
        (
            "save",
            |f, observed| {
                f.ops
                    .save(&f.root, "dir/file.txt", "saved\n", Some(observed), false)
                    .map(drop)
            },
            &["dir", "dir-aside/", "dir-aside/file.txt", "top.txt"],
        ),
        (
            "create",
            |f, _| {
                f.ops
                    .create(&f.root, "dir/new.txt", ProjectEntryKindV1::File)
                    .map(drop)
            },
            &[
                "dir",
                "dir-aside/",
                "dir-aside/file.txt",
                "dir-aside/new.txt",
                "top.txt",
            ],
        ),
        (
            "move",
            |f, _| f.ops.rename(&f.root, "top.txt", "dir/top.txt").map(drop),
            &[
                "dir",
                "dir-aside/",
                "dir-aside/file.txt",
                "dir-aside/top.txt",
            ],
        ),
        (
            "copy",
            |f, _| f.ops.copy(&f.root, "top.txt", "dir/copy.txt").map(drop),
            &[
                "dir",
                "dir-aside/",
                "dir-aside/copy.txt",
                "dir-aside/file.txt",
                "top.txt",
            ],
        ),
        (
            "trash",
            |f, _| f.ops.trash(&f.root, "dir/file.txt").map(drop),
            &["dir", "dir-aside/", "top.txt"],
        ),
    ];
    for (name, op, expected) in cases {
        let f = fixture();
        sentinel(&f);
        put(&f.root, "dir/file.txt", "inside\n");
        put(&f.root, "top.txt", "top\n");
        let observed = version(&f.root, "dir/file.txt");
        let (root, outside) = (f.root.clone(), f.outside.clone());
        at(&f.ops, Hook::Checked, move || {
            swap_for_link(&root, "dir", &outside)
        });
        op(&f, &observed).unwrap_or_else(|error| panic!("{name}: {error}"));
        outside_intact(&f);
        assert_eq!(tree(&f.root), expected, "{name}");
        if name == "save" {
            assert_eq!(text(&f.root, "dir-aside/file.txt"), "saved\n");
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3143_a_subtree_swapped_for_a_symlink_during_a_copy_is_copied_as_the_link_only() {
    let f = fixture();
    sentinel(&f);
    put(&f.root, "pkg/a.txt", "a\n");
    put(&f.root, "pkg/sub/b.txt", "b\n");
    let (root, outside) = (f.root.clone(), f.outside.clone());
    // After the first entry (pkg/a.txt) is copied, pkg/sub becomes a link out.
    at(&f.ops, Hook::CopiedEntry, move || {
        swap_for_link(&root.join("pkg"), "sub", &outside)
    });
    f.ops.copy(&f.root, "pkg", "pkg2").unwrap();
    outside_intact(&f);
    let copy = f.root.join("pkg2/sub");
    assert!(copy.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(fs::read_link(&copy).unwrap(), f.outside);
    // No byte of the outside tree was copied in.
    let copied: Vec<String> = tree(&f.root)
        .into_iter()
        .filter(|path| path.starts_with("pkg2"))
        .collect();
    assert_eq!(copied, vec!["pkg2/", "pkg2/a.txt", "pkg2/sub"]);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3143_a_parent_swapped_during_an_undo_restore_keeps_the_restore_inside() {
    let f = fixture();
    sentinel(&f);
    put(&f.root, "dir/a.txt", "a\n");
    f.ops.trash(&f.root, "dir/a.txt").unwrap();
    let (root, outside) = (f.root.clone(), f.outside.clone());
    at(&f.ops, Hook::Checked, move || {
        swap_for_link(&root, "dir", &outside)
    });
    f.ops.undo(&f.root).unwrap();
    outside_intact(&f);
    assert_eq!(text(&f.root, "dir-aside/a.txt"), "a\n");
}

// --- hub #3144: a failed copy never deletes a destination it did not make ----

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3144_a_destination_created_during_a_copy_is_kept_and_the_copy_refused() {
    for as_dir in [false, true] {
        let f = fixture();
        put(&f.root, "pkg/a.txt", "a\n");
        let root = f.root.clone();
        at(&f.ops, Hook::Checked, move || {
            if as_dir {
                put(&root, "pkg2/theirs.txt", "other writer\n");
            } else {
                put(&root, "pkg2", "other writer\n");
            }
        });
        let refused = f.ops.copy(&f.root, "pkg", "pkg2").unwrap_err();
        assert_eq!(code(refused), EXISTS, "dir={as_dir}");
        let theirs = if as_dir { "pkg2/theirs.txt" } else { "pkg2" };
        assert_eq!(text(&f.root, theirs), "other writer\n");
        // Only the source and the other writer's entry: no staging left.
        let mut want = vec!["pkg/", "pkg/a.txt"];
        if as_dir {
            want.extend(["pkg2/", "pkg2/theirs.txt"]);
        } else {
            want.push("pkg2");
        }
        assert_eq!(tree(&f.root), want, "dir={as_dir}");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3144_a_copy_that_fails_midway_removes_only_its_private_copy() {
    let f = fixture();
    put(&f.root, "pkg/a.txt", "a\n");
    nix::unistd::mkfifo(&f.root.join("pkg/z-pipe"), nix::sys::stat::Mode::S_IRWXU).unwrap();
    put(&f.root, "keep/other.txt", "other\n");
    let refused = f.ops.copy(&f.root, "pkg", "keep/pkg").unwrap_err();
    assert_eq!(code(refused), NOT_A_FILE);
    assert_eq!(
        tree(&f.root),
        vec!["keep/", "keep/other.txt", "pkg/", "pkg/a.txt", "pkg/z-pipe"]
    );
    assert_eq!(text(&f.root, "keep/other.txt"), "other\n");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3144_hub_3145_a_cross_device_move_into_a_concurrently_created_destination_keeps_both() {
    let f = fixture();
    f.ops.cross_device.store(true, Ordering::SeqCst);
    put(&f.root, "src/a.txt", "source\n");
    let root = f.root.clone();
    at(&f.ops, Hook::CrossDeviceCopied, move || {
        put(&root, "dst.txt", "other writer\n")
    });
    let refused = f.ops.rename(&f.root, "src/a.txt", "dst.txt").unwrap_err();
    assert_eq!(code(refused), EXISTS);
    assert_eq!(text(&f.root, "src/a.txt"), "source\n");
    assert_eq!(text(&f.root, "dst.txt"), "other writer\n");
    assert_eq!(tree(&f.root), vec!["dst.txt", "src/", "src/a.txt"]);
    assert_eq!(tree(&f.trash), Vec::<String>::new());
}

// --- hub #3145: move and undo never overwrite a concurrent destination -------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3145_a_destination_created_during_a_move_is_never_replaced() {
    for as_dir in [false, true] {
        let f = fixture();
        put(&f.root, "a.txt", "mine\n");
        let root = f.root.clone();
        at(&f.ops, Hook::Checked, move || {
            if as_dir {
                fs::create_dir(root.join("b.txt")).unwrap();
            } else {
                put(&root, "b.txt", "theirs\n");
            }
        });
        let refused = f.ops.rename(&f.root, "a.txt", "b.txt").unwrap_err();
        assert_eq!(code(refused), EXISTS, "dir={as_dir}");
        assert_eq!(text(&f.root, "a.txt"), "mine\n");
        if as_dir {
            assert_eq!(tree(&f.root), vec!["a.txt", "b.txt/"]);
        } else {
            assert_eq!(text(&f.root, "b.txt"), "theirs\n");
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3145_an_undo_never_replaces_an_original_path_created_concurrently() {
    let f = fixture();
    put(&f.root, "a.txt", "moved\n");
    f.ops.rename(&f.root, "a.txt", "b.txt").unwrap();
    let root = f.root.clone();
    at(&f.ops, Hook::Checked, move || {
        put(&root, "a.txt", "theirs\n")
    });
    assert_eq!(code(f.ops.undo(&f.root).unwrap_err()), UNDO_CONFLICT);
    assert_eq!(text(&f.root, "a.txt"), "theirs\n");
    assert_eq!(text(&f.root, "b.txt"), "moved\n");
    // Kept for a retry once the conflict is gone.
    clear(&f.ops);
    fs::remove_file(f.root.join("a.txt")).unwrap();
    assert_eq!(
        f.ops.undo(&f.root).unwrap().effect,
        ProjectFileUndoEffectV1::MovedBack
    );
    assert_eq!(tree(&f.root), vec!["a.txt"]);

    // The same for a trash restore.
    f.ops.trash(&f.root, "a.txt").unwrap();
    let root = f.root.clone();
    at(&f.ops, Hook::Checked, move || {
        put(&root, "a.txt", "theirs again\n")
    });
    assert_eq!(code(f.ops.undo(&f.root).unwrap_err()), UNDO_CONFLICT);
    assert_eq!(text(&f.root, "a.txt"), "theirs again\n");
    assert_eq!(text(&f.trash, &format!("{}/a.txt", batch(&f))), "moved\n");
}

// --- hub #3146: a save keeps external edits and answers its own version ------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3146_an_edit_after_the_version_check_is_kept_and_the_save_refused() {
    for replace in [false, true] {
        let f = fixture();
        put(&f.root, "notes.md", "v1\n");
        let observed = version(&f.root, "notes.md");
        let root = f.root.clone();
        at(&f.ops, Hook::BeforePublish, move || {
            if replace {
                // An editor that saves by rename.
                put(&root, "theirs.tmp", "external\n");
                fs::rename(root.join("theirs.tmp"), root.join("notes.md")).unwrap();
            } else {
                // An in-place write that keeps the size.
                fs::write(root.join("notes.md"), "v2\n").unwrap();
            }
        });
        let refused = f
            .ops
            .save(&f.root, "notes.md", "mine\n", Some(&observed), false)
            .unwrap_err();
        assert_eq!(code(refused), STALE, "replace={replace}");
        let theirs = if replace { "external\n" } else { "v2\n" };
        assert_eq!(text(&f.root, "notes.md"), theirs);
        // The temporary file is gone too.
        assert_eq!(tree(&f.root), vec!["notes.md"]);
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3146_the_answered_version_is_the_saved_bytes_not_a_later_replacement() {
    let f = fixture();
    put(&f.root, "notes.md", "v1\n");
    let observed = version(&f.root, "notes.md");
    let root = f.root.clone();
    at(&f.ops, Hook::AfterPublish, move || {
        put(&root, "theirs.tmp", "theirs\n");
        fs::rename(root.join("theirs.tmp"), root.join("notes.md")).unwrap();
    });
    let saved = f
        .ops
        .save(&f.root, "notes.md", "mine\n", Some(&observed), false)
        .unwrap();
    let digest = hex::encode(Sha256::digest(b"mine\n"));
    assert!(saved.version.starts_with("v1:5:"), "{}", saved.version);
    assert!(saved.version.ends_with(&digest), "{}", saved.version);
    assert_ne!(saved.version, version(&f.root, "notes.md"));
    // Its next save is refused: the replacement survives.
    clear(&f.ops);
    let refused = f
        .ops
        .save(
            &f.root,
            "notes.md",
            "mine again\n",
            Some(&saved.version),
            false,
        )
        .unwrap_err();
    assert_eq!(code(refused), STALE);
    assert_eq!(text(&f.root, "notes.md"), "theirs\n");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3146_a_file_created_while_a_forced_create_was_saving_is_kept() {
    let f = fixture();
    let root = f.root.clone();
    at(&f.ops, Hook::BeforePublish, move || {
        put(&root, "new.md", "theirs\n")
    });
    let refused = f
        .ops
        .save(&f.root, "new.md", "mine\n", None, true)
        .unwrap_err();
    assert_eq!(code(refused), STALE);
    assert_eq!(text(&f.root, "new.md"), "theirs\n");
    assert_eq!(tree(&f.root), vec!["new.md"]);
}

// --- hub #3147: undo acts only on what the operation left ---------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3147_undo_refuses_to_move_back_a_replaced_destination() {
    let f = fixture();
    put(&f.root, "a", "original\n");
    f.ops.rename(&f.root, "a", "b").unwrap();
    fs::rename(f.root.join("b"), f.root.join("preserved-original")).unwrap();
    put(&f.root, "b", "external replacement\n");
    for _ in 0..2 {
        assert_eq!(code(f.ops.undo(&f.root).unwrap_err()), UNDO_CONFLICT);
    }
    assert_eq!(tree(&f.root), vec!["b", "preserved-original"]);
    assert_eq!(text(&f.root, "b"), "external replacement\n");
    assert_eq!(text(&f.root, "preserved-original"), "original\n");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3147_a_destination_replaced_after_the_undo_check_is_never_moved_back() {
    let f = fixture();
    put(&f.root, "a", "original\n");
    f.ops.rename(&f.root, "a", "b").unwrap();
    let root = f.root.clone();
    at(&f.ops, Hook::Checked, move || {
        fs::rename(root.join("b"), root.join("preserved-original")).unwrap();
        put(&root, "b", "external replacement\n");
    });
    assert_eq!(code(f.ops.undo(&f.root).unwrap_err()), UNDO_CONFLICT);
    assert_eq!(tree(&f.root), vec!["b", "preserved-original"]);
    assert_eq!(text(&f.root, "b"), "external replacement\n");
    assert_eq!(text(&f.root, "preserved-original"), "original\n");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3147_an_added_file_replaced_after_the_undo_check_is_never_removed() {
    let f = fixture();
    f.ops
        .create(&f.root, "created/requested", ProjectEntryKindV1::File)
        .unwrap();
    let root = f.root.clone();
    at(&f.ops, Hook::Checked, move || {
        fs::rename(
            root.join("created/requested"),
            root.join("preserved-original"),
        )
        .unwrap();
        put(&root, "created/requested", "external replacement\n");
    });
    assert_eq!(code(f.ops.undo(&f.root).unwrap_err()), UNDO_CONFLICT);
    assert_eq!(
        tree(&f.root),
        vec!["created/", "created/requested", "preserved-original"]
    );
    assert_eq!(text(&f.root, "created/requested"), "external replacement\n");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3147_undo_of_an_add_keeps_an_externally_added_empty_file() {
    let f = fixture();
    let added = f
        .ops
        .create(&f.root, "created/requested", ProjectEntryKindV1::File)
        .unwrap();
    assert_eq!(added.created_top.as_deref(), Some("created"));
    put(&f.root, "created/external-marker", "");
    assert_eq!(code(f.ops.undo(&f.root).unwrap_err()), UNDO_CONFLICT);
    assert_eq!(
        tree(&f.root),
        vec!["created/", "created/external-marker", "created/requested"]
    );
    // An edit of the added file itself is a conflict too.
    fs::remove_file(f.root.join("created/external-marker")).unwrap();
    put(&f.root, "created/requested", "typed\n");
    assert_eq!(code(f.ops.undo(&f.root).unwrap_err()), UNDO_CONFLICT);
    assert_eq!(text(&f.root, "created/requested"), "typed\n");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3147_undo_of_a_copy_refuses_an_edited_or_extended_copy() {
    for extend in [false, true] {
        let f = fixture();
        put(&f.root, "pkg/a.txt", "a\n");
        f.ops.copy(&f.root, "pkg", "pkg2").unwrap();
        if extend {
            put(&f.root, "pkg2/extra.txt", "");
        } else {
            put(&f.root, "pkg2/a.txt", "edited\n");
        }
        assert_eq!(code(f.ops.undo(&f.root).unwrap_err()), UNDO_CONFLICT);
        assert!(f.root.join("pkg2/a.txt").exists(), "extend={extend}");
        assert_eq!(tree(&f.trash), Vec::<String>::new());
    }
}

// --- hub #3148: across filesystems no live tree is unlinked -------------------

/// Trash `pkg` (a.txt, sub/b.txt) as if the trash were on another
/// filesystem, with `interfere` run once the copy is done.
fn cross_device_trash(interfere: impl FnOnce(&Path) + Send + 'static) -> (Fixture, String) {
    let f = fixture();
    f.ops.cross_device.store(true, Ordering::SeqCst);
    put(&f.root, "pkg/a.txt", "a\n");
    put(&f.root, "pkg/sub/b.txt", "b\n");
    let root = f.root.clone();
    at(&f.ops, Hook::CrossDeviceCopied, move || interfere(&root));
    let refused = f.ops.trash(&f.root, "pkg").unwrap_err();
    let refused = code(refused);
    (f, refused)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3148_a_cross_device_trash_keeps_an_append_made_after_the_copy() {
    let (f, refused) = cross_device_trash(|root| {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(root.join("pkg/a.txt"))
            .unwrap();
        file.write_all(b"more\n").unwrap();
    });
    assert_eq!(refused, CONFLICT);
    let batch = batch(&f);
    // The appended bytes stay at the source, the copied ones are in trash.
    assert_eq!(text(&f.root, "pkg/a.txt"), "a\nmore\n");
    assert_eq!(tree(&f.root), vec!["pkg/", "pkg/a.txt"]);
    assert_eq!(text(&f.trash, &format!("{batch}/pkg/a.txt")), "a\n");
    assert_eq!(text(&f.trash, &format!("{batch}/pkg/sub/b.txt")), "b\n");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3148_a_cross_device_trash_keeps_a_child_added_after_the_copy() {
    let (f, refused) = cross_device_trash(|root| put(root, "pkg/sub/new.txt", "new\n"));
    assert_eq!(refused, CONFLICT);
    let batch = batch(&f);
    assert_eq!(tree(&f.root), vec!["pkg/", "pkg/sub/", "pkg/sub/new.txt"]);
    assert_eq!(text(&f.root, "pkg/sub/new.txt"), "new\n");
    assert_eq!(text(&f.trash, &format!("{batch}/pkg/a.txt")), "a\n");
    assert_eq!(text(&f.trash, &format!("{batch}/pkg/sub/b.txt")), "b\n");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3148_a_cross_device_trash_leaves_a_replaced_source_alone() {
    let (f, refused) = cross_device_trash(|root| {
        fs::rename(root.join("pkg"), root.join("pkg-old")).unwrap();
        put(root, "pkg/c.txt", "replacement\n");
    });
    assert_eq!(refused, CONFLICT);
    let batch = batch(&f);
    assert_eq!(text(&f.root, "pkg/c.txt"), "replacement\n");
    assert_eq!(text(&f.root, "pkg-old/a.txt"), "a\n");
    assert_eq!(text(&f.root, "pkg-old/sub/b.txt"), "b\n");
    assert_eq!(text(&f.trash, &format!("{batch}/pkg/a.txt")), "a\n");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3148_a_cross_device_move_of_a_changing_source_moves_nothing() {
    let f = fixture();
    f.ops.cross_device.store(true, Ordering::SeqCst);
    put(&f.root, "pkg/a.txt", "a\n");
    let root = f.root.clone();
    at(&f.ops, Hook::CrossDeviceCopied, move || {
        fs::write(root.join("pkg/a.txt"), "b\n").unwrap()
    });
    assert_eq!(
        code(f.ops.rename(&f.root, "pkg", "moved").unwrap_err()),
        CONFLICT
    );
    assert_eq!(tree(&f.root), vec!["pkg/", "pkg/a.txt"]);
    assert_eq!(text(&f.root, "pkg/a.txt"), "b\n");
    assert_eq!(tree(&f.trash), Vec::<String>::new());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3148_an_undisturbed_cross_device_move_and_trash_complete_and_undo() {
    let f = fixture();
    f.ops.cross_device.store(true, Ordering::SeqCst);
    put(&f.root, "pkg/a.txt", "a\n");
    put(&f.root, "pkg/sub/b.txt", "b\n");

    f.ops.rename(&f.root, "pkg", "lib/pkg").unwrap();
    assert_eq!(
        tree(&f.root),
        vec![
            "lib/",
            "lib/pkg/",
            "lib/pkg/a.txt",
            "lib/pkg/sub/",
            "lib/pkg/sub/b.txt"
        ]
    );
    // The source went to the trash, never unlinked.
    assert_eq!(
        text(&f.trash, &format!("{}/pkg/sub/b.txt", batch(&f))),
        "b\n"
    );
    f.ops.trash(&f.root, "lib/pkg/a.txt").unwrap();
    assert_eq!(
        tree(&f.root),
        vec!["lib/", "lib/pkg/", "lib/pkg/sub/", "lib/pkg/sub/b.txt"]
    );

    let restored = f.ops.undo(&f.root).unwrap();
    assert_eq!(restored.effect, ProjectFileUndoEffectV1::Restored);
    assert_eq!(text(&f.root, "lib/pkg/a.txt"), "a\n");
    let moved_back = f.ops.undo(&f.root).unwrap();
    assert_eq!(moved_back.effect, ProjectFileUndoEffectV1::MovedBack);
    assert_eq!(
        tree(&f.root),
        vec!["lib/", "pkg/", "pkg/a.txt", "pkg/sub/", "pkg/sub/b.txt"]
    );
    assert_eq!(text(&f.root, "pkg/sub/b.txt"), "b\n");
}

// --- hub #3149: the trash is protected at resolved targets and undo ends ------

/// A root that holds the daemon trash at `.trash`, with `a.txt` trashed.
fn trash_in_root() -> (TempDir, PathBuf, FileOps, String) {
    let dir = TempDir::new().unwrap();
    let root = canonical_root(dir.path()).unwrap();
    let ops = FileOps::new(root.join(".trash"));
    put(&root, "a.txt", "trashed\n");
    ops.trash(&root, "a.txt").unwrap();
    let batch = tree(&root.join(".trash"))[0]
        .trim_end_matches(".origin")
        .to_string();
    (dir, root, ops, batch)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3149_a_save_through_an_alias_into_the_trash_is_refused() {
    let (_dir, root, ops, batch) = trash_in_root();
    symlink(format!(".trash/{batch}/a.txt"), root.join("alias")).unwrap();
    for force in [false, true] {
        let observed = version(&root, "alias");
        let refused = ops
            .save(&root, "alias", "pwned\n", Some(&observed), force)
            .unwrap_err();
        assert_eq!(code(refused), INVALID_REQUEST, "force={force}");
    }
    assert_eq!(text(&root, &format!(".trash/{batch}/a.txt")), "trashed\n");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3149_an_undo_endpoint_resolved_into_the_trash_is_refused() {
    // The restore side: the original parent now leads into the trash.
    let (_dir, root, ops, batch) = trash_in_root();
    put(&root, "sub/m.txt", "m\n");
    ops.rename(&root, "sub/m.txt", "n.txt").unwrap();
    fs::rename(root.join("sub"), root.join("sub-real")).unwrap();
    symlink(format!(".trash/{batch}"), root.join("sub")).unwrap();
    assert_eq!(code(ops.undo(&root).unwrap_err()), INVALID_REQUEST);
    assert_eq!(text(&root, "n.txt"), "m\n");
    assert_eq!(tree(&root.join(format!(".trash/{batch}"))), vec!["a.txt"]);

    // The source side: the moved entry itself now sits in the trash.
    let (_dir, root, ops, batch) = trash_in_root();
    put(&root, "x.txt", "x\n");
    ops.rename(&root, "x.txt", "d/x.txt").unwrap();
    fs::rename(root.join("d"), root.join("d-real")).unwrap();
    symlink(format!(".trash/{batch}"), root.join("d")).unwrap();
    fs::rename(
        root.join("d-real/x.txt"),
        root.join(format!(".trash/{batch}/x.txt")),
    )
    .unwrap();
    assert_eq!(code(ops.undo(&root).unwrap_err()), INVALID_REQUEST);
    assert_eq!(text(&root, &format!(".trash/{batch}/x.txt")), "x\n");
    assert!(root.join("x.txt").symlink_metadata().is_err());
}

// --- hub #3150: a FIFO never blocks the write lock -----------------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn hub_3150_a_fifo_is_refused_without_blocking_and_the_lock_stays_free() {
    let f = std::sync::Arc::new(fixture());
    nix::unistd::mkfifo(&f.root.join("pipe"), nix::sys::stat::Mode::S_IRWXU).unwrap();
    put(&f.root, "dir/ok.txt", "ok\n");
    nix::unistd::mkfifo(&f.root.join("dir/pipe"), nix::sys::stat::Mode::S_IRWXU).unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let worker = std::sync::Arc::clone(&f);
    std::thread::spawn(move || {
        let codes = vec![
            code(
                worker
                    .ops
                    .save(&worker.root, "pipe", "x", None, false)
                    .unwrap_err(),
            ),
            code(
                worker
                    .ops
                    .save(&worker.root, "pipe", "x", None, true)
                    .unwrap_err(),
            ),
            code(read_file(&worker.root, "pipe", None, None, None).unwrap_err()),
            code(worker.ops.copy(&worker.root, "dir", "dir2").unwrap_err()),
        ];
        let _ = send.send(codes);
    });
    let codes = receive
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("a FIFO blocked a project-file call");
    assert_eq!(codes, vec![NOT_A_FILE; 4]);
    // The lock is free: a normal save on the same FileOps goes through.
    let observed = version(&f.root, "dir/ok.txt");
    f.ops
        .save(&f.root, "dir/ok.txt", "fine\n", Some(&observed), false)
        .unwrap();
    assert_eq!(text(&f.root, "dir/ok.txt"), "fine\n");
    assert!(!f.root.join("dir2").exists());
}

// --- #403: root and trash acquisition must never reopen ambient paths --------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn issue_403_root_and_ancestor_swaps_refuse_every_write_class() {
    for ancestor in [false, true] {
        for operation in ["save", "add", "mkdir", "move", "copy", "trash", "undo"] {
            let mut f = fixture();
            sentinel(&f);
            put(&f.outside, "file.txt", "outside file\n");
            put(&f.outside, "project/file.txt", "outside nested file\n");
            let holder = f.root.parent().unwrap().join("ancestor");
            fs::create_dir(&holder).unwrap();
            let root = holder.join("project");
            fs::rename(&f.root, &root).unwrap();
            f.root = root;
            put(&f.root, "file.txt", "original\n");
            let observed = version(&f.root, "file.txt");
            if operation == "undo" {
                f.ops.trash(&f.root, "file.txt").unwrap();
            }
            let swapped = if ancestor {
                holder.clone()
            } else {
                f.root.clone()
            };
            let aside = swapped.with_extension("aside");
            let original_root = if ancestor {
                aside.join("project")
            } else {
                aside.clone()
            };
            let outside = f.outside.clone();
            at(&f.ops, Hook::BeforeRootOpen, move || {
                fs::rename(&swapped, &aside).unwrap();
                symlink(outside, swapped).unwrap();
            });
            let result = match operation {
                "save" => f
                    .ops
                    .save(&f.root, "file.txt", "changed\n", Some(&observed), false)
                    .map(drop),
                "add" => f
                    .ops
                    .create(&f.root, "new.txt", ProjectEntryKindV1::File)
                    .map(drop),
                "mkdir" => f
                    .ops
                    .create(&f.root, "newdir", ProjectEntryKindV1::Dir)
                    .map(drop),
                "move" => f.ops.rename(&f.root, "file.txt", "moved.txt").map(drop),
                "copy" => f.ops.copy(&f.root, "file.txt", "copied.txt").map(drop),
                "trash" => f.ops.trash(&f.root, "file.txt").map(drop),
                "undo" => f.ops.undo(&f.root).map(drop),
                _ => unreachable!(),
            };
            assert_eq!(
                code(result.unwrap_err()),
                super::super::super::UNKNOWN_ROOT,
                "{operation}, ancestor={ancestor}"
            );
            assert_eq!(
                tree(&f.outside),
                vec!["file.txt", "project/", "project/file.txt", "sentinel.txt"]
            );
            assert_eq!(text(&f.outside, "file.txt"), "outside file\n");
            assert_eq!(
                text(&f.outside, "project/file.txt"),
                "outside nested file\n"
            );
            assert_eq!(text(&f.outside, "sentinel.txt"), "outside\n");
            if operation == "undo" {
                assert_eq!(tree(&original_root), Vec::<String>::new());
                assert_eq!(
                    text(&f.trash, &format!("{}/file.txt", batch(&f))),
                    "original\n"
                );
            } else {
                assert_eq!(tree(&original_root), vec!["file.txt"]);
                assert_eq!(text(&original_root, "file.txt"), "original\n");
            }
        }
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn issue_403_trash_identity_probe_refuses_root_and_ancestor_symlink_swaps() {
    for ancestor in [false, true] {
        let mut f = fixture();
        sentinel(&f);
        f.trash = f.trash.join("nested");
        fs::create_dir_all(&f.trash).unwrap();
        f.ops = FileOps::new(f.trash.clone());
        put(&f.root, "file.txt", "original\n");
        let swapped = if ancestor {
            f.trash.parent().unwrap().to_path_buf()
        } else {
            f.trash.clone()
        };
        let outside = f.outside.clone();
        at(&f.ops, Hook::TrashResolved, move || {
            fs::rename(&swapped, swapped.with_extension("aside")).unwrap();
            symlink(outside, swapped).unwrap();
        });
        assert_eq!(
            code(f.ops.trash(&f.root, "file.txt").unwrap_err()),
            IO_FAILED
        );
        outside_intact(&f);
        assert_eq!(text(&f.root, "file.txt"), "original\n");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn issue_403_trash_refuses_a_swap_after_the_identity_probe_even_when_parents_are_missing() {
    for missing in [false, true] {
        let mut f = fixture();
        sentinel(&f);
        let anchor = f.trash.clone();
        fs::create_dir(&anchor).unwrap();
        if missing {
            f.trash = anchor.join("missing/leaf");
            f.ops = FileOps::new(f.trash.clone());
        }
        put(&f.root, "file.txt", "original\n");
        let outside = f.outside.clone();
        at(&f.ops, Hook::Checked, move || {
            fs::rename(&anchor, anchor.with_extension("aside")).unwrap();
            symlink(outside, anchor).unwrap();
        });
        assert_eq!(
            code(f.ops.trash(&f.root, "file.txt").unwrap_err()),
            IO_FAILED
        );
        outside_intact(&f);
        assert_eq!(text(&f.root, "file.txt"), "original\n");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn issue_403_trash_refuses_batch_and_ancestor_swaps_before_sidecar_or_transfer() {
    for level in ["batch", "trash", "ancestor"] {
        let mut f = fixture();
        sentinel(&f);
        f.trash = f.trash.join("nested");
        f.ops = FileOps::new(f.trash.clone());
        put(&f.root, "file.txt", "original\n");
        let (trash, outside) = (f.trash.clone(), f.outside.clone());
        at(&f.ops, Hook::TrashBatchCreated, move || {
            let batch = fs::read_dir(&trash)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            let swapped = match level {
                "batch" => batch,
                "trash" => trash.clone(),
                "ancestor" => trash.parent().unwrap().to_path_buf(),
                _ => unreachable!(),
            };
            fs::rename(&swapped, swapped.with_extension("aside")).unwrap();
            symlink(outside, swapped).unwrap();
        });
        assert_eq!(
            code(f.ops.trash(&f.root, "file.txt").unwrap_err()),
            IO_FAILED,
            "{level}"
        );
        outside_intact(&f);
        assert_eq!(text(&f.root, "file.txt"), "original\n");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn issue_403_origin_sidecar_symlink_is_refused_without_overwriting_its_target() {
    let f = fixture();
    sentinel(&f);
    put(&f.root, "file.txt", "original\n");
    let (trash, outside) = (f.trash.clone(), f.outside.clone());
    at(&f.ops, Hook::TrashBatchCreated, move || {
        let batch = fs::read_dir(&trash)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        symlink(outside.join("sentinel.txt"), batch.with_extension("origin")).unwrap();
    });
    assert_eq!(
        code(f.ops.trash(&f.root, "file.txt").unwrap_err()),
        IO_FAILED
    );
    outside_intact(&f);
    assert_eq!(text(&f.root, "file.txt"), "original\n");
    assert_eq!(fs::read_dir(&f.trash).unwrap().count(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn issue_403_undo_refuses_batch_trash_root_and_ancestor_swaps_and_preserves_the_trashed_inode() {
    for level in ["batch", "trash", "ancestor"] {
        let mut f = fixture();
        sentinel(&f);
        f.trash = f.trash.join("nested");
        f.ops = FileOps::new(f.trash.clone());
        put(&f.root, "file.txt", "original\n");
        f.ops.trash(&f.root, "file.txt").unwrap();
        let batch_name = batch(&f);
        let (swapped, entry) = match level {
            "batch" => (f.trash.join(&batch_name), PathBuf::from("file.txt")),
            "trash" => (f.trash.clone(), PathBuf::from(&batch_name).join("file.txt")),
            "ancestor" => (
                f.trash.parent().unwrap().to_path_buf(),
                PathBuf::from("nested").join(&batch_name).join("file.txt"),
            ),
            _ => unreachable!(),
        };
        let escaped = f.outside.join("escaped");
        let preserved = escaped.join(entry);
        at(&f.ops, Hook::BeforeTrashOpen, move || {
            // The inode and stamp still match the undo record. An ambient
            // reopen would really remove it from outside and restore it.
            fs::rename(&swapped, &escaped).unwrap();
            symlink(&escaped, swapped).unwrap();
        });
        assert_eq!(code(f.ops.undo(&f.root).unwrap_err()), IO_FAILED, "{level}");
        assert_eq!(fs::read_to_string(preserved).unwrap(), "original\n");
        assert_eq!(text(&f.outside, "sentinel.txt"), "outside\n");
        assert_eq!(tree(&f.root), Vec::<String>::new());
        // The refusal retains the undo entry for a safe retry.
        assert_eq!(f.ops.log().get(&f.root).unwrap().len(), 1);
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn issue_403_pruning_uses_the_pinned_parent_after_a_trash_ancestor_swap() {
    let f = fixture();
    sentinel(&f);
    put(&f.root, "file.txt", "original\n");
    f.ops.trash(&f.root, "file.txt").unwrap();
    let name = batch(&f);
    fs::create_dir(f.outside.join(&name)).unwrap();
    put(&f.outside, &format!("{name}.origin"), "outside origin\n");
    let (trash, outside) = (f.trash.clone(), f.outside.clone());
    at(&f.ops, Hook::BeforeTrashPrune, move || {
        fs::rename(&trash, trash.with_extension("aside")).unwrap();
        symlink(outside, trash).unwrap();
    });
    f.ops.undo(&f.root).unwrap();
    assert_eq!(text(&f.root, "file.txt"), "original\n");
    assert_eq!(text(&f.outside, "sentinel.txt"), "outside\n");
    assert_eq!(
        text(&f.outside, &format!("{name}.origin")),
        "outside origin\n"
    );
    assert!(f.outside.join(&name).is_dir());
    assert_eq!(tree(&f.trash.with_extension("aside")), Vec::<String>::new());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn issue_403_normal_save_trash_undo_preserves_configured_trash_links_and_contained_project_links() {
    for cross_device in [false, true] {
        let f = fixture();
        fs::create_dir(f.outside.join("data")).unwrap();
        symlink(f.outside.join("data"), &f.trash).unwrap();
        let ops = FileOps::new(f.trash.join("missing/trash"));
        ops.cross_device.store(cross_device, Ordering::SeqCst);
        put(&f.root, "real/file.txt", "original\n");
        symlink("real", f.root.join("alias")).unwrap();
        symlink("real/file.txt", f.root.join("file-link")).unwrap();
        let observed = version(&f.root, "file-link");
        ops.save(&f.root, "file-link", "saved\n", Some(&observed), false)
            .unwrap();
        ops.trash(&f.root, "alias/file.txt").unwrap();
        assert_eq!(tree(&f.root.join("real")), Vec::<String>::new());
        ops.undo(&f.root).unwrap();
        assert_eq!(text(&f.root, "real/file.txt"), "saved\n");
        assert_eq!(tree(&f.trash.join("missing/trash")), Vec::<String>::new());
        assert!(f.trash.symlink_metadata().unwrap().file_type().is_symlink());
    }
}
