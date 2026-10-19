//! Portable descriptor-relative containment for hosts without Linux
//! `openat2(2)` (#89, hub #3055).
//!
//! The reclaim proves containment with `openat2(RESOLVE_BENEATH |
//! RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_XDEV)` and moves
//! staged trees with `renameat2(RENAME_NOREPLACE)`. On macOS this module
//! provides the same names with the same guarantees, so the reclaim code reads
//! identically on both platforms and Linux keeps calling the kernel directly:
//!
//! - **Beneath.** [`open_beneath`] walks the relative path one component at a
//!   time, each `openat` starting from the descriptor the previous step
//!   returned. An absolute path or a `..` component is refused with `EXDEV`
//!   (what `RESOLVE_BENEATH` answers for an escape), so the walk can never
//!   leave the starting directory.
//! - **No symlinks.** Every component is opened with `O_NOFOLLOW_ANY` on
//!   macOS 11+ (`O_NOFOLLOW` elsewhere), so a symlink at any step fails
//!   `ELOOP`/`ENOTDIR` instead of being followed.
//! - **No mount or device crossing.** Every descriptor the walk opens is
//!   `fstat`ed and must report the starting directory's `st_dev`; otherwise the
//!   walk closes it and refuses with `EXDEV` (`RESOLVE_NO_XDEV`).
//! - **No magic links.** macOS has no `/proc`-style magic links; `/dev/fd`
//!   lives on devfs, which the device check already refuses.
//!
//! [`renameat2`] maps `RENAME_NOREPLACE` to macOS `renameatx_np(RENAME_EXCL)`,
//! which refuses an existing destination with `EEXIST` atomically, as Linux
//! does. The walk itself is plain POSIX and also builds on Linux, where only
//! its unit tests use it.

use nix::NixPath;
use nix::errno::Errno;
use nix::fcntl::{OFlag, openat};
use nix::sys::stat::{Mode, fstat};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// The no-symlink open flag of each step. macOS 11+ has `O_NOFOLLOW_ANY`,
/// which refuses a symlink anywhere in the path, the last component included;
/// it subsumes `O_NOFOLLOW`, and the kernel refuses the two together with
/// `EINVAL`, so the walk passes exactly one of them.
#[cfg(target_os = "macos")]
const NOFOLLOW: OFlag = OFlag::from_bits_retain(nix::libc::O_NOFOLLOW_ANY);
#[cfg(not(target_os = "macos"))]
const NOFOLLOW: OFlag = OFlag::O_NOFOLLOW;

/// The `openat2` resolve flags the reclaim uses. The portable walk enforces
/// all four at once and refuses any other set with `EINVAL`, so a caller can
/// never believe it got a weaker guarantee than it asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolveFlag(u8);

impl ResolveFlag {
    pub const RESOLVE_BENEATH: Self = Self(1);
    pub const RESOLVE_NO_SYMLINKS: Self = Self(1 << 1);
    pub const RESOLVE_NO_MAGICLINKS: Self = Self(1 << 2);
    pub const RESOLVE_NO_XDEV: Self = Self(1 << 3);
    const ALL: Self = Self(0b1111);

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// The `open_how` argument of [`openat2`]: open flags and resolve flags.
#[derive(Clone, Copy, Debug)]
pub struct OpenHow {
    flags: OFlag,
    resolve: ResolveFlag,
}

impl OpenHow {
    #[must_use]
    pub fn new() -> Self {
        Self {
            flags: OFlag::empty(),
            resolve: ResolveFlag(0),
        }
    }

    #[must_use]
    pub fn flags(mut self, flags: OFlag) -> Self {
        self.flags = flags;
        self
    }

    #[must_use]
    pub fn resolve(mut self, resolve: ResolveFlag) -> Self {
        self.resolve = resolve;
        self
    }
}

/// `openat2(2)` with the full reclaim resolve set, by [`open_beneath`].
/// Returns a new owned raw descriptor, as the Linux call does.
pub fn openat2<P: ?Sized + NixPath>(dirfd: RawFd, path: &P, how: OpenHow) -> nix::Result<RawFd> {
    if how.resolve != ResolveFlag::ALL {
        return Err(Errno::EINVAL);
    }
    let fd = path.with_nix_path(|path| open_beneath(dirfd, path.to_bytes(), how.flags))??;
    Ok(into_raw(fd))
}

fn into_raw(fd: OwnedFd) -> RawFd {
    use std::os::fd::IntoRawFd;
    fd.into_raw_fd()
}

/// Open `path` relative to `dirfd` without leaving it, following no symlink
/// and crossing no mount (see the module docs). `flags` apply to the last
/// component; the no-symlink flag and `O_CLOEXEC` are always added.
pub fn open_beneath(dirfd: RawFd, path: &[u8], flags: OFlag) -> nix::Result<OwnedFd> {
    if path.is_empty() {
        return Err(Errno::ENOENT);
    }
    if path[0] == b'/' {
        return Err(Errno::EXDEV);
    }
    let mut components: Vec<&[u8]> = Vec::new();
    for component in path.split(|byte| *byte == b'/') {
        match component {
            b"" | b"." => {}
            b".." => return Err(Errno::EXDEV),
            component => components.push(component),
        }
    }
    if components.is_empty() {
        components.push(b".");
    }
    let start_dev = fstat(dirfd)?.st_dev;
    let last = components.len() - 1;
    let mut current: Option<OwnedFd> = None;
    for (index, component) in components.into_iter().enumerate() {
        let at = current.as_ref().map_or(dirfd, AsRawFd::as_raw_fd);
        let step = if index == last {
            (flags - OFlag::O_NOFOLLOW) | NOFOLLOW | OFlag::O_CLOEXEC
        } else {
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | NOFOLLOW | OFlag::O_CLOEXEC
        };
        let raw = openat(Some(at), component, step, Mode::empty())?;
        // SAFETY: `openat` returned a new descriptor that nothing else owns.
        let opened = unsafe { OwnedFd::from_raw_fd(raw) };
        if fstat(opened.as_raw_fd())?.st_dev != start_dev {
            return Err(Errno::EXDEV);
        }
        current = Some(opened);
    }
    current.ok_or(Errno::ENOENT)
}

/// The `renameat2` flag the reclaim uses.
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenameFlags(nix::libc::c_uint);

#[cfg(target_os = "macos")]
impl RenameFlags {
    /// Fail with `EEXIST` instead of replacing an existing destination.
    pub const RENAME_NOREPLACE: Self = Self(nix::libc::RENAME_EXCL);
}

/// `renameat2(2)` by macOS `renameatx_np(2)`; `RENAME_NOREPLACE` becomes the
/// atomic `RENAME_EXCL`.
#[cfg(target_os = "macos")]
pub fn renameat2<P1: ?Sized + NixPath, P2: ?Sized + NixPath>(
    old_dirfd: Option<RawFd>,
    old_path: &P1,
    new_dirfd: Option<RawFd>,
    new_path: &P2,
    flags: RenameFlags,
) -> nix::Result<()> {
    let result = old_path.with_nix_path(|old| {
        new_path.with_nix_path(|new| {
            // SAFETY: both paths are NUL-terminated C strings that outlive the
            // call, and the descriptors are the caller's (or AT_FDCWD).
            unsafe {
                nix::libc::renameatx_np(
                    old_dirfd.unwrap_or(nix::libc::AT_FDCWD),
                    old.as_ptr(),
                    new_dirfd.unwrap_or(nix::libc::AT_FDCWD),
                    new.as_ptr(),
                    flags.0,
                )
            }
        })
    })??;
    Errno::result(result).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::fcntl::open;
    use std::os::unix::fs::symlink;

    const DIR: OFlag = OFlag::O_RDONLY.union(OFlag::O_DIRECTORY);

    fn open_dir(path: &std::path::Path) -> OwnedFd {
        let raw = open(path, DIR | OFlag::O_CLOEXEC, Mode::empty()).unwrap();
        // SAFETY: `open` returned a new owned descriptor.
        unsafe { OwnedFd::from_raw_fd(raw) }
    }

    fn inode(fd: &OwnedFd) -> u64 {
        fstat(fd.as_raw_fd()).unwrap().st_ino as u64
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn portable_walk_opens_nested_directories_beneath_the_root() {
        use std::os::unix::fs::MetadataExt;
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("a/b")).unwrap();
        let root = open_dir(temp.path());

        let nested = open_beneath(root.as_raw_fd(), b"a/./b", DIR).unwrap();
        assert_eq!(
            inode(&nested),
            std::fs::metadata(temp.path().join("a/b")).unwrap().ino()
        );
        let how = OpenHow::new().flags(DIR).resolve(
            ResolveFlag::RESOLVE_BENEATH
                .union(ResolveFlag::RESOLVE_NO_SYMLINKS)
                .union(ResolveFlag::RESOLVE_NO_MAGICLINKS)
                .union(ResolveFlag::RESOLVE_NO_XDEV),
        );
        let raw = openat2(root.as_raw_fd(), "a", how).unwrap();
        // SAFETY: `openat2` returned a new owned descriptor.
        let child = unsafe { OwnedFd::from_raw_fd(raw) };
        assert_eq!(
            inode(&child),
            std::fs::metadata(temp.path().join("a")).unwrap().ino()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn portable_walk_refuses_a_symlink_escape_at_any_component() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(root_path.join("inside")).unwrap();
        std::fs::create_dir_all(outside.join("target")).unwrap();
        symlink(&outside, root_path.join("escape")).unwrap();
        symlink(root_path.join("inside"), root_path.join("loop_inside")).unwrap();
        let root = open_dir(&root_path);

        // A final symlink, even one that points back inside, is never followed.
        for path in [&b"escape"[..], b"loop_inside"] {
            let error = open_beneath(root.as_raw_fd(), path, DIR).unwrap_err();
            assert!(
                matches!(error, Errno::ELOOP | Errno::ENOTDIR),
                "{path:?}: {error}"
            );
        }
        // An intermediate symlink to a directory outside the root.
        let error = open_beneath(root.as_raw_fd(), b"escape/target", DIR).unwrap_err();
        assert!(matches!(error, Errno::ELOOP | Errno::ENOTDIR), "{error}");
        // `..` and absolute paths cannot leave the root.
        assert_eq!(
            open_beneath(root.as_raw_fd(), b"inside/../../outside", DIR).unwrap_err(),
            Errno::EXDEV
        );
        assert_eq!(
            open_beneath(
                root.as_raw_fd(),
                outside.as_os_str().as_encoded_bytes(),
                DIR
            )
            .unwrap_err(),
            Errno::EXDEV
        );
        // A weaker resolve set than the reclaim's is refused, not emulated.
        let how = OpenHow::new()
            .flags(DIR)
            .resolve(ResolveFlag::RESOLVE_BENEATH);
        assert_eq!(
            openat2(root.as_raw_fd(), "inside", how).unwrap_err(),
            Errno::EINVAL
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn portable_walk_refuses_a_mount_or_device_crossing() {
        use std::os::unix::fs::MetadataExt;
        // `/dev` is its own filesystem (devfs on macOS, devtmpfs or tmpfs on
        // Linux), so it is a mount point directly beneath `/`.
        let root_dev = std::fs::metadata("/").unwrap().dev();
        let dev_dev = std::fs::metadata("/dev").unwrap().dev();
        assert_ne!(root_dev, dev_dev, "fixture: /dev must be a separate mount");
        let root = open_dir(std::path::Path::new("/"));

        assert_eq!(
            open_beneath(root.as_raw_fd(), b"dev", DIR).unwrap_err(),
            Errno::EXDEV
        );
        // The same directory opened as the walk's root is fine.
        let dev = open_dir(std::path::Path::new("/dev"));
        assert!(open_beneath(dev.as_raw_fd(), b".", DIR).is_ok());
    }

    #[cfg(target_os = "macos")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-05"))]
    #[test]
    fn portable_rename_noreplace_refuses_an_existing_destination() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("from")).unwrap();
        std::fs::create_dir(temp.path().join("taken")).unwrap();
        let root = open_dir(temp.path());
        let fd = Some(root.as_raw_fd());

        assert_eq!(
            renameat2(fd, "from", fd, "taken", RenameFlags::RENAME_NOREPLACE).unwrap_err(),
            Errno::EEXIST
        );
        assert!(temp.path().join("from").is_dir());
        renameat2(fd, "from", fd, "moved", RenameFlags::RENAME_NOREPLACE).unwrap();
        assert!(temp.path().join("moved").is_dir());
        assert!(!temp.path().join("from").exists());
    }
}
