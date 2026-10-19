//! Deterministic swaps at the boundary between resolution and handle I/O.

std::thread_local! {
    static BEFORE_OPEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const {
        std::cell::RefCell::new(None)
    };
}

pub(super) fn before_open() {
    let hook = BEFORE_OPEN.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(unix)]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
mod unix {
    use super::BEFORE_OPEN;
    use crate::project_files::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    enum Operation {
        Read,
        GutterInspect,
        Count,
    }

    fn swap_is_refused(operation: Operation, ancestor: bool) {
        let fixture = TempDir::new().unwrap();
        let base = fixture.path().canonicalize().unwrap();
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(root.join("dir")).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(root.join("dir/file"), b"inside\n").unwrap();
        fs::write(outside.join("file"), b"OUTSIDE SECRET\nsecond\nthird\n").unwrap();
        let swap_root = root.clone();
        let swap = move || {
            if ancestor {
                fs::rename(swap_root.join("dir"), swap_root.join("original")).unwrap();
                symlink(&outside, swap_root.join("dir")).unwrap();
            } else {
                fs::remove_file(swap_root.join("dir/file")).unwrap();
                symlink(outside.join("file"), swap_root.join("dir/file")).unwrap();
            }
        };
        let result = match operation {
            Operation::Read => {
                BEFORE_OPEN.with(|slot| *slot.borrow_mut() = Some(Box::new(swap)));
                read_file(&root, "dir/file", None, None, None).map(|_| ())
            }
            Operation::GutterInspect => {
                BEFORE_OPEN.with(|slot| *slot.borrow_mut() = Some(Box::new(swap)));
                inspect_gutter_file(&root, "dir/file").map(|_| ())
            }
            Operation::Count => {
                // This is the second open in the untracked gutter path,
                // after the initial inspection has already succeeded.
                let (scope, _, _) = inspect_gutter_file(&root, "dir/file").unwrap();
                BEFORE_OPEN.with(|slot| *slot.borrow_mut() = Some(Box::new(swap)));
                count_lines(&scope).map(|_| ())
            }
        };
        // An error contains neither content nor a count derived from outside.
        let error = result.expect_err("a replacement link must refuse the read");
        assert!(error.to_string().contains(write::IO_FAILED), "{error}");
        assert!(BEFORE_OPEN.with(|slot| slot.borrow().is_none()));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn read_refuses_file_swap() {
        swap_is_refused(Operation::Read, false);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn read_refuses_ancestor_swap() {
        swap_is_refused(Operation::Read, true);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn gutter_inspection_refuses_file_swap() {
        swap_is_refused(Operation::GutterInspect, false);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn gutter_inspection_refuses_ancestor_swap() {
        swap_is_refused(Operation::GutterInspect, true);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn count_refuses_file_swap() {
        swap_is_refused(Operation::Count, false);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn count_refuses_ancestor_swap() {
        swap_is_refused(Operation::Count, true);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn contained_file_and_directory_symlinks_still_read() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        fs::create_dir(root.join("real")).unwrap();
        fs::write(root.join("real/file"), b"inside\nsecond\n").unwrap();
        symlink(root.join("real"), root.join("absolute_dir")).unwrap();
        symlink("real", root.join("relative_dir")).unwrap();
        symlink("real/file", root.join("file_link")).unwrap();
        for path in ["absolute_dir/file", "relative_dir/file", "file_link"] {
            let read = read_file(&root, path, None, None, None).unwrap();
            assert_eq!(read.content.as_deref(), Some("inside\nsecond\n"));
            assert_eq!(read.path, path);
            let (scope, size, binary) = inspect_gutter_file(&root, path).unwrap();
            assert_eq!(size, 14);
            assert!(!binary);
            assert_eq!(count_lines(&scope).unwrap(), 2);
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn replacement_of_root_stays_on_the_pinned_directory() {
        let fixture = TempDir::new().unwrap();
        let base = fixture.path().canonicalize().unwrap();
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(root.join("file"), b"inside\n").unwrap();
        fs::write(outside.join("file"), b"OUTSIDE SECRET\n").unwrap();
        let scope = FileReadScope::resolve(&root, "file").unwrap();
        fs::rename(&root, base.join("original")).unwrap();
        symlink(outside, &root).unwrap();
        let mut bytes = String::new();
        scope.open().unwrap().read_to_string(&mut bytes).unwrap();
        assert_eq!(bytes, "inside\n");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn fifo_swap_is_refused_without_blocking() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        fs::write(root.join("file"), b"inside\n").unwrap();
        let swap_root = root.clone();
        BEFORE_OPEN.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                fs::remove_file(swap_root.join("file")).unwrap();
                nix::unistd::mkfifo(&swap_root.join("file"), nix::sys::stat::Mode::S_IRWXU)
                    .unwrap();
            }))
        });
        let error = read_file(&root, "file", None, None, None).unwrap_err();
        assert!(error.to_string().contains(NOT_A_FILE), "{error}");
    }
}

#[cfg(not(unix))]
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
#[test]
fn safe_reads_are_explicitly_unsupported() {
    let fixture = tempfile::TempDir::new().unwrap();
    std::fs::write(fixture.path().join("file"), b"inside").unwrap();
    let root = fixture.path().canonicalize().unwrap();
    let error = super::read_file(&root, "file", None, None, None).unwrap_err();
    assert!(error.to_string().contains(super::write::UNSUPPORTED));
}
