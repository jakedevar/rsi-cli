//! A fresh, private git directory for one queue reset (#1160).
//!
//! The queue worktree's real registration (`<common>/worktrees/<name>`) holds
//! files git appends to in place (reflogs) and reads configuration and hooks
//! from the operator's repository. Both are unsafe to hand to a destructive
//! command: a hard link planted under the registration aliases an operator
//! file, and the operator's config can name hooks and filters. So each reset
//! runs git against a directory created fresh by the daemon: its own `HEAD`,
//! empty `refs`, a minimal `config`, and a copy of the registration's index.
//! Git reads the shared objects through `GIT_OBJECT_DIRECTORY`.
//!
//! `logs` is a regular file (git can create no reflog under it) and no ref is
//! ever updated, so nothing is appended in place. A split index in the
//! registration is not seeded (its shared half is not copied); the published
//! index is always a full one.
//!
//! Every operation here is relative to an open directory handle and never
//! follows a symlink; files are created exclusively and published by rename, so
//! an alias planted at a destination name is replaced, never written through.

use std::ffi::CString;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// Prefix of the scratch directories created inside a registration.
pub(super) const SCRATCH_PREFIX: &str = "rsi-queue-scratch-";

fn cname(name: &str) -> Result<CString, String> {
    CString::new(name).map_err(|_| format!("name {name:?} has a NUL"))
}

fn os_error(what: &str, name: &str) -> String {
    format!("{what} {name}: {}", std::io::Error::last_os_error())
}

/// Create `name` exclusively (no symlink follow) under `dir` with `contents`.
pub(super) fn create_file_at(
    dir: &File,
    name: &str,
    contents: &[u8],
    mode: u32,
) -> Result<(), String> {
    let cname = cname(name)?;
    // SAFETY: `cname` is NUL-terminated and `dir` stays open for the call.
    let fd = unsafe {
        nix::libc::openat(
            dir.as_raw_fd(),
            cname.as_ptr(),
            nix::libc::O_WRONLY
                | nix::libc::O_CREAT
                | nix::libc::O_EXCL
                | nix::libc::O_NOFOLLOW
                | nix::libc::O_CLOEXEC,
            mode,
        )
    };
    if fd < 0 {
        return Err(os_error("cannot create", name));
    }
    // SAFETY: `fd` is a new descriptor nothing else owns.
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(contents)
        .map_err(|error| format!("cannot write {name}: {error}"))
}

/// Read a regular file `name` under `dir` without following a symlink. `None`
/// when it does not exist.
pub(super) fn read_file_at(dir: &File, name: &str) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read;
    let cname = cname(name)?;
    // `O_NONBLOCK`: a FIFO planted under the name must not block the open; the
    // regular-file check below then refuses it.
    // SAFETY: as in `create_file_at`.
    let fd = unsafe {
        nix::libc::openat(
            dir.as_raw_fd(),
            cname.as_ptr(),
            nix::libc::O_RDONLY
                | nix::libc::O_NOFOLLOW
                | nix::libc::O_NONBLOCK
                | nix::libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(format!("cannot read {name}: {error}"));
    }
    // SAFETY: `fd` is a new descriptor nothing else owns.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let meta = file
        .metadata()
        .map_err(|error| format!("cannot stat {name}: {error}"))?;
    if !meta.is_file() {
        return Err(format!("{name} is not a regular file"));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read {name}: {error}"))?;
    Ok(Some(bytes))
}

/// Replace `name` under `dir` with `contents`: a fresh exclusive temporary
/// renamed over the destination, so whatever the destination was (a symlink, a
/// hard link to another file) is replaced and never written through.
pub(super) fn replace_file_at(
    dir: &File,
    name: &str,
    contents: &[u8],
    mode: u32,
) -> Result<(), String> {
    let temporary = format!("{SCRATCH_PREFIX}{}.tmp", uuid::Uuid::new_v4());
    create_file_at(dir, &temporary, contents, mode)?;
    let from = cname(&temporary)?;
    let to = cname(name)?;
    // SAFETY: both names are NUL-terminated and `dir` stays open for the call.
    let renamed = unsafe {
        nix::libc::renameat(dir.as_raw_fd(), from.as_ptr(), dir.as_raw_fd(), to.as_ptr())
    };
    if renamed == 0 {
        return Ok(());
    }
    let error = os_error("cannot publish", name);
    // SAFETY: as above.
    unsafe { nix::libc::unlinkat(dir.as_raw_fd(), from.as_ptr(), 0) };
    Err(error)
}

/// Remove `name` under `parent` and everything beneath it without following a
/// symlink (`unlinkat` relative to handles, `AT_REMOVEDIR` only for real
/// directories).
pub(super) fn remove_tree_at(parent: &File, name: &str) -> Result<(), String> {
    let cname = cname(name)?;
    // SAFETY: `stat` is plain old data; the names and handle are valid.
    let is_dir = unsafe {
        let mut stat: nix::libc::stat = std::mem::zeroed();
        if nix::libc::fstatat(
            parent.as_raw_fd(),
            cname.as_ptr(),
            &mut stat,
            nix::libc::AT_SYMLINK_NOFOLLOW,
        ) != 0
        {
            return Err(os_error("cannot stat", name));
        }
        stat.st_mode & nix::libc::S_IFMT == nix::libc::S_IFDIR
    };
    if !is_dir {
        // SAFETY: as above.
        return match unsafe { nix::libc::unlinkat(parent.as_raw_fd(), cname.as_ptr(), 0) } {
            0 => Ok(()),
            _ => Err(os_error("cannot remove", name)),
        };
    }
    let child = open_dir_at(parent, name)?;
    let listing = PathBuf::from(format!("/proc/self/fd/{}", child.as_raw_fd()));
    for entry in std::fs::read_dir(&listing)
        .map_err(|error| format!("cannot list {name}: {error}"))?
        .flatten()
    {
        let entry_name = entry.file_name();
        remove_tree_at(&child, &String::from_utf8_lossy(entry_name.as_bytes()))?;
    }
    // SAFETY: as above.
    match unsafe {
        nix::libc::unlinkat(parent.as_raw_fd(), cname.as_ptr(), nix::libc::AT_REMOVEDIR)
    } {
        0 => Ok(()),
        _ => Err(os_error("cannot remove", name)),
    }
}

/// Open the directory `name` under `parent` without following a symlink.
pub(super) fn open_dir_at(parent: &File, name: &str) -> Result<File, String> {
    let cname = cname(name)?;
    // SAFETY: as in `create_file_at`.
    let fd = unsafe {
        nix::libc::openat(
            parent.as_raw_fd(),
            cname.as_ptr(),
            nix::libc::O_RDONLY
                | nix::libc::O_DIRECTORY
                | nix::libc::O_NOFOLLOW
                | nix::libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(os_error("cannot open", name));
    }
    // SAFETY: `fd` is a new descriptor nothing else owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn make_dir_at(parent: &File, name: &str) -> Result<(), String> {
    let cname = cname(name)?;
    // SAFETY: as in `create_file_at`.
    match unsafe { nix::libc::mkdirat(parent.as_raw_fd(), cname.as_ptr(), 0o700) } {
        0 => Ok(()),
        _ => Err(os_error("cannot create", name)),
    }
}

/// A private git directory: created fresh under `parent` (the registration),
/// removed by [`PrivateGitDir::remove`].
pub(super) struct PrivateGitDir {
    pub(super) dir: File,
    name: String,
}

impl PrivateGitDir {
    /// Create the directory, populate it and seed `index` from the
    /// registration's (when it has one). A symlinked registration `index` is an
    /// error, not followed.
    pub(super) fn create(parent: &File) -> Result<Self, String> {
        let name = format!("{SCRATCH_PREFIX}{}", uuid::Uuid::new_v4());
        make_dir_at(parent, &name)?;
        let dir = open_dir_at(parent, &name)?;
        let private = Self { dir, name };
        let populated = private.populate(parent);
        if let Err(error) = populated {
            let _ = private.remove(parent);
            return Err(error);
        }
        Ok(private)
    }

    fn populate(&self, parent: &File) -> Result<(), String> {
        create_file_at(&self.dir, "HEAD", b"ref: refs/heads/rsi-queue\n", 0o644)?;
        create_file_at(
            &self.dir,
            "config",
            b"[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
            0o644,
        )?;
        make_dir_at(&self.dir, "refs")?;
        make_dir_at(&self.dir, "objects")?;
        // `logs` is a regular file: git can create no reflog beneath it, so no
        // reflog (a file git appends to in place, even with reflogs switched
        // off) can be planted or created here.
        create_file_at(&self.dir, "logs", b"", 0o600)?;
        // `info` is a regular file for the same reason: git reads
        // `info/attributes` (filter drivers) and `info/exclude` from it, and a
        // directory of that name could be planted here beside the files this
        // function creates exclusively. As a file it can only be replaced, not
        // added to (#1170).
        create_file_at(&self.dir, "info", b"", 0o600)?;
        // Only something shaped like an index is seeded: a registration entry
        // replaced by another file (a hard link to a config, say) must not
        // wedge every later reset. Git then starts from no index, and `clean`
        // removes what the old one would have. A split index (a
        // `sharedindex.<oid>` beside it) is not seeded either: its shared half
        // would be missing here, and the published index is always a full one.
        match read_file_at(parent, "index")? {
            Some(index) if index.starts_with(b"DIRC") && !has_shared_index(parent) => {
                create_file_at(&self.dir, "index", &index, 0o644)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// The directory git is given as `GIT_DIR`, as seen by a child that
    /// inherits the handle.
    pub(super) fn handle_path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.dir.as_raw_fd()))
    }

    /// The index git left in the directory.
    pub(super) fn index(&self) -> Result<Vec<u8>, String> {
        read_file_at(&self.dir, "index")?
            .ok_or_else(|| "git left no index in the private git directory".to_string())
    }

    pub(super) fn remove(&self, parent: &File) -> Result<(), String> {
        remove_tree_at(parent, &self.name)
    }
}

/// Whether the registration holds a split index's shared half.
fn has_shared_index(parent: &File) -> bool {
    let listing = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
    let Ok(entries) = std::fs::read_dir(&listing) else {
        // Unreadable: treat as split so the copy is not trusted.
        return true;
    };
    entries.flatten().any(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .starts_with("sharedindex.")
    })
}

/// Remove scratch directories a crashed run left under `parent`.
pub(super) fn retire_stale_scratch(parent: &File) {
    let listing = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
    let Ok(entries) = std::fs::read_dir(&listing) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(SCRATCH_PREFIX) {
            let _ = remove_tree_at(parent, &name);
        }
    }
}
