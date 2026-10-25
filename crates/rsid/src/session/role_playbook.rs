//! #656: references belong to the daemon's selected harness, never a caller's
//! project or a provider's ambient skill roots. Discovery stays in preamble.

use rsi_common::daemon_handoff::{
    CoordinatorSeatV1, PlaybookLocationV1, PlaybookUnavailableReasonV1 as Reason,
    valid_playbook_path_text,
};
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

const MAX_PLAYBOOK_BYTES: u64 = 256 * 1024;

fn unavailable(reason: Reason) -> PlaybookLocationV1 {
    PlaybookLocationV1::Unavailable { reason }
}

/// Recheck on every emission: the cached discovery root can be reclaimed.
pub(super) fn resolve(root: Option<&Path>, seat: CoordinatorSeatV1) -> PlaybookLocationV1 {
    resolve_with_read(root, seat, check_readable)
}

fn resolve_with_read(
    root: Option<&Path>,
    seat: CoordinatorSeatV1,
    read: impl FnOnce(&Path) -> Result<(), Reason>,
) -> PlaybookLocationV1 {
    let Some(root) = root
        .and_then(|p| p.canonicalize().ok())
        .filter(|p| p.is_dir())
    else {
        return unavailable(Reason::HarnessUnavailable);
    };
    let Ok(file) = root.join(seat.playbook()).canonicalize() else {
        return unavailable(Reason::FileUnavailable);
    };
    if !file.starts_with(&root) {
        return unavailable(Reason::OutsideHarness);
    }
    let Some(path) = file.to_str().filter(|p| valid_playbook_path_text(p)) else {
        return unavailable(Reason::UnsafePathText);
    };
    // Check before opening too: do not open a FIFO/device as a playbook.
    if !file.is_file() {
        return unavailable(Reason::NotRegularFile);
    }
    if let Err(reason) = read(&file) {
        return unavailable(reason);
    }
    PlaybookLocationV1::Resolved { path: path.into() }
}

fn check_readable(path: &Path) -> Result<(), Reason> {
    check_readable_with_open(path, |path| File::open(path))
}

fn check_readable_with_open(
    path: &Path,
    open: impl FnOnce(&Path) -> io::Result<File>,
) -> Result<(), Reason> {
    let file = open(path).map_err(|_| Reason::Unreadable)?;
    let metadata = file.metadata().map_err(|_| Reason::Unreadable)?;
    if !metadata.is_file() {
        return Err(Reason::NotRegularFile);
    }
    if metadata.len() > MAX_PLAYBOOK_BYTES {
        return Err(Reason::TooLarge);
    }
    let bytes = io::copy(&mut file.take(MAX_PLAYBOOK_BYTES + 1), &mut io::sink())
        .map_err(|_| Reason::Unreadable)?;
    if bytes > MAX_PLAYBOOK_BYTES {
        return Err(Reason::TooLarge);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    fn write(root: &Path, seat: CoordinatorSeatV1) -> std::path::PathBuf {
        let path = root.join(seat.playbook());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"selected harness playbook").unwrap();
        path.canonicalize().unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn resolves_all_seats_from_the_selected_harness_and_preserves_resources() {
        let temp = crate::test_support::disk_backed_tempdir("role-playbooks");
        let root = temp.path().join("harness space 雪");
        let other_project = temp.path().join("other-project");
        for seat in [
            CoordinatorSeatV1::ProjectManager,
            CoordinatorSeatV1::AreaManager {
                node_id: uuid::Uuid::from_u128(1),
            },
            CoordinatorSeatV1::EpicLead {
                epic_id: uuid::Uuid::from_u128(2),
            },
            CoordinatorSeatV1::GlobalManager,
        ] {
            let path = write(&root, seat);
            let lookalike = write(&other_project, seat);
            std::fs::write(lookalike, b"caller project lookalike").unwrap();
            let location = resolve(Some(&root), seat);
            assert_eq!(
                location,
                PlaybookLocationV1::Resolved {
                    path: path.to_str().unwrap().into()
                }
            );
            assert_eq!(std::fs::read(&path).unwrap(), b"selected harness playbook");
            location.validate().unwrap();
        }
        let pm = write(&root, CoordinatorSeatV1::ProjectManager);
        let resources = pm.parent().unwrap().join("procedures.md");
        std::fs::write(&resources, b"adjacent procedures").unwrap();
        let PlaybookLocationV1::Resolved { path } = resolve(
            Some(&root.join("docs/..")),
            CoordinatorSeatV1::ProjectManager,
        ) else {
            panic!("canonical harness reference expected");
        };
        assert_eq!(
            std::fs::read(Path::new(&path).parent().unwrap().join("procedures.md")).unwrap(),
            b"adjacent procedures"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn missing_reclaimed_nonregular_read_errors_and_size_return_unavailable() {
        let temp = crate::test_support::disk_backed_tempdir("role-unavailable");
        let root = temp.path();
        let seat = CoordinatorSeatV1::ProjectManager;
        assert_eq!(resolve(None, seat), unavailable(Reason::HarnessUnavailable));
        assert_eq!(
            resolve(Some(&root.join("missing")), seat),
            unavailable(Reason::HarnessUnavailable)
        );
        assert_eq!(
            resolve(Some(root), seat),
            unavailable(Reason::FileUnavailable)
        );
        let path = write(root, seat);
        // Inject an actual PermissionDenied read result, independent of chmod,
        // privilege level and OS. Production maps every read error this way.
        assert_eq!(
            resolve_with_read(Some(root), seat, |path| {
                check_readable_with_open(path, |_| {
                    Err(io::Error::from(io::ErrorKind::PermissionDenied))
                })
            }),
            unavailable(Reason::Unreadable)
        );
        assert_eq!(
            resolve_with_read(Some(root), seat, |path| {
                std::fs::remove_file(path).unwrap();
                check_readable(path)
            }),
            unavailable(Reason::Unreadable)
        );
        assert_eq!(
            resolve(Some(root), seat),
            unavailable(Reason::FileUnavailable)
        );
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            resolve(Some(root), seat),
            unavailable(Reason::NotRegularFile)
        );
        std::fs::remove_dir(&path).unwrap();
        let file = File::create(&path).unwrap();
        file.set_len(MAX_PLAYBOOK_BYTES + 1).unwrap();
        assert_eq!(resolve(Some(root), seat), unavailable(Reason::TooLarge));
        file.set_len(MAX_PLAYBOOK_BYTES).unwrap();
        assert_eq!(
            resolve(Some(root), seat),
            PlaybookLocationV1::Resolved {
                path: path.to_str().unwrap().into(),
            }
        );
        assert!(
            resolve(None, seat)
                .reference()
                .contains("binary-shipped role guidance")
        );
    }

    #[cfg(unix)]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-01"))]
    #[test]
    fn escaping_links_and_unsafe_native_text_return_unavailable() {
        #[cfg(target_os = "linux")]
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;
        let temp = crate::test_support::disk_backed_tempdir("role-path-safety");
        let seat = CoordinatorSeatV1::ProjectManager;
        let root = temp.path().join("harness");
        let path = write(&root, seat);
        let outside = temp.path().join("outside.md");
        std::fs::write(&outside, b"outside tree").unwrap();
        std::fs::remove_file(&path).unwrap();
        symlink(outside, &path).unwrap();
        assert_eq!(
            resolve(Some(&root), seat),
            unavailable(Reason::OutsideHarness)
        );
        let names = [std::ffi::OsString::from("unsafe`<\n")];
        // Linux filesystems allow non-UTF-8 names; macOS filesystems may
        // reject their creation. The portable unsafe-text case runs on both.
        #[cfg(target_os = "linux")]
        let names = [
            names[0].clone(),
            std::ffi::OsString::from_vec(vec![b'x', 0xff]),
        ];
        for name in names {
            let unsafe_root = temp.path().join(name);
            write(&unsafe_root, seat);
            assert_eq!(
                resolve(Some(&unsafe_root), seat),
                unavailable(Reason::UnsafePathText)
            );
        }
    }
}
