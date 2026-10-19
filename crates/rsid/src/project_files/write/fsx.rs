//! Handle-level filesystem primitives of the project-file writes (hub
//! #3143-#3151). Every call takes an open directory handle and ONE plain name
//! (no separator), so nothing here resolves a pathname: the named entry is
//! never followed (`O_NOFOLLOW`, `AT_SYMLINK_NOFOLLOW`), a FIFO or device is
//! never opened blocking (`O_NONBLOCK`, regular files only), and a transfer
//! rename never replaces (`renameat2(RENAME_NOREPLACE)` on Linux,
//! `renameatx_np(RENAME_EXCL)` on macOS; any other target, or a filesystem
//! without it, is `Unsupported`).
//!
//! The tree helpers copy, inventory and remove a tree through handles opened
//! one level at a time without following symlinks, and remove only entries
//! whose identity and stamp still match what they copied.

use cap_std::fs::Dir;
use rustix::fs::{AtFlags, FileType, Mode, OFlags, RenameFlags};
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStringExt;

/// Most entries one copied, inventoried or sealed tree may hold.
pub(super) const MAX_TREE_ENTRIES: usize = 100_000;
/// Deepest nesting one tree walk descends.
pub(super) const MAX_TREE_DEPTH: usize = 256;

/// A filesystem object: device and inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Identity {
    pub(super) dev: u64,
    pub(super) ino: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Kind {
    File,
    Dir,
    Symlink,
    /// FIFO, socket or device: never opened, never copied.
    Special,
}

/// What one `lstat`/`fstat` saw of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Stamp {
    pub(super) id: Identity,
    pub(super) kind: Kind,
    /// Permission bits (`& 0o7777`).
    pub(super) mode: u32,
    pub(super) size: u64,
    pub(super) mtime: (i64, i64),
    pub(super) ctime: (i64, i64),
}

impl Stamp {
    /// Same object, kind, size and modification time: what undo
    /// re-validates (a rename moves only the change time).
    pub(super) fn same_object(&self, other: &Stamp) -> bool {
        self.id == other.id
            && self.kind == other.kind
            && self.size == other.size
            && self.mtime == other.mtime
    }

    /// What a move-back or restore re-validates: a file or symlink is
    /// [`Self::same_object`]; a directory is the same object of the same
    /// kind (its size and mtime track its own entries, which a move-back
    /// carries along and never replaces).
    pub(super) fn same_entry(&self, other: &Stamp) -> bool {
        if self.kind == Kind::Dir {
            self.id == other.id && self.kind == other.kind
        } else {
            self.same_object(other)
        }
    }

    /// [`Self::same_object`] and the same change time: nothing wrote to or
    /// changed the metadata of the object since it was stamped.
    pub(super) fn untouched(&self, other: &Stamp) -> bool {
        self.same_object(other) && self.ctime == other.ctime
    }
}

#[allow(clippy::unnecessary_cast, trivial_numeric_casts)]
fn stamp(st: &rustix::fs::Stat) -> Stamp {
    let kind = match FileType::from_raw_mode(st.st_mode as _) {
        FileType::RegularFile => Kind::File,
        FileType::Directory => Kind::Dir,
        FileType::Symlink => Kind::Symlink,
        _ => Kind::Special,
    };
    Stamp {
        id: Identity {
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
        },
        kind,
        mode: (st.st_mode as u32) & 0o7777,
        size: st.st_size as u64,
        mtime: (st.st_mtime as i64, st.st_mtime_nsec as i64),
        ctime: (st.st_ctime as i64, st.st_ctime_nsec as i64),
    }
}

/// `lstat` of `name` in `dir`; `None` when it does not exist.
pub(super) fn lstat(dir: &Dir, name: &OsStr) -> io::Result<Option<Stamp>> {
    match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => Ok(Some(stamp(&st))),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// `fstat` of an open file or directory.
pub(super) fn fstat(fd: impl AsFd) -> io::Result<Stamp> {
    Ok(stamp(&rustix::fs::fstat(fd)?))
}

fn not_regular() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "not a regular file")
}

/// Open the regular file `name` read-only without following it and without
/// blocking (a FIFO swapped in after a check never blocks the caller).
pub(super) fn open_file(dir: &Dir, name: &OsStr) -> io::Result<File> {
    let fd = rustix::fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY,
        Mode::empty(),
    )?;
    let file = File::from(fd);
    if fstat(&file)?.kind != Kind::File {
        return Err(not_regular());
    }
    Ok(file)
}

/// Open the directory `name` without following it.
pub(super) fn open_dir(dir: &Dir, name: &OsStr) -> io::Result<Dir> {
    let fd = rustix::fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(Dir::from_std_file(File::from(fd)))
}

/// Create the file `name`, failing when anything (a symlink included)
/// already has the name. `mode` is subject to the umask.
pub(super) fn create_file(dir: &Dir, name: &OsStr, mode: u32) -> io::Result<File> {
    let fd = rustix::fs::openat(
        dir,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(mode as _),
    )?;
    Ok(File::from(fd))
}

pub(super) fn make_dir(dir: &Dir, name: &OsStr) -> io::Result<()> {
    Ok(rustix::fs::mkdirat(dir, name, Mode::from_raw_mode(0o777))?)
}

pub(super) fn set_mode(file: &File, mode: u32) -> io::Result<()> {
    Ok(rustix::fs::fchmod(file, Mode::from_raw_mode(mode as _))?)
}

/// Rename without ever replacing: an existing destination is
/// `AlreadyExists`, a different filesystem `CrossesDevices`, a filesystem
/// or platform without the primitive `Unsupported`.
pub(super) fn rename_noreplace(
    from_dir: &Dir,
    from: &OsStr,
    to_dir: &Dir,
    to: &OsStr,
) -> io::Result<()> {
    match rustix::fs::renameat_with(from_dir, from, to_dir, to, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        Err(rustix::io::Errno::INVAL | rustix::io::Errno::NOSYS | rustix::io::Errno::NOTSUP) => {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this filesystem has no no-replace rename",
            ))
        }
        Err(error) => Err(error.into()),
    }
}

/// Rename over an existing entry (only the save's final publication).
pub(super) fn rename_replace(
    from_dir: &Dir,
    from: &OsStr,
    to_dir: &Dir,
    to: &OsStr,
) -> io::Result<()> {
    Ok(rustix::fs::renameat(from_dir, from, to_dir, to)?)
}

pub(super) fn unlink(dir: &Dir, name: &OsStr) -> io::Result<()> {
    Ok(rustix::fs::unlinkat(dir, name, AtFlags::empty())?)
}

/// Remove an empty directory (`DirectoryNotEmpty` otherwise).
pub(super) fn rmdir(dir: &Dir, name: &OsStr) -> io::Result<()> {
    Ok(rustix::fs::unlinkat(dir, name, AtFlags::REMOVEDIR)?)
}

/// Unlink `name` only while it is still the object `id` (best effort: the
/// check and the unlink are two calls on a name only this module uses).
pub(super) fn unlink_if(dir: &Dir, name: &OsStr, id: Identity) -> io::Result<bool> {
    match lstat(dir, name)? {
        Some(now) if now.id == id => unlink(dir, name).map(|()| true),
        _ => Ok(false),
    }
}

pub(super) fn read_link(dir: &Dir, name: &OsStr) -> io::Result<OsString> {
    let target = rustix::fs::readlinkat(dir, name, Vec::new())?;
    Ok(OsString::from_vec(target.into_bytes()))
}

pub(super) fn symlink(target: &OsStr, dir: &Dir, name: &OsStr) -> io::Result<()> {
    Ok(rustix::fs::symlinkat(target, dir, name)?)
}

/// The names in `dir`, sorted.
pub(super) fn names(dir: &Dir) -> io::Result<Vec<OsString>> {
    let mut names = Vec::new();
    for entry in dir.entries()? {
        names.push(entry?.file_name());
        if names.len() > MAX_TREE_ENTRIES {
            return Err(io::Error::other("directory has too many entries"));
        }
    }
    names.sort();
    Ok(names)
}

/// The parent (`..`) of an open directory.
pub(super) fn parent(dir: &Dir) -> io::Result<Dir> {
    dir.open_parent_dir(cap_std::ambient_authority())
}

pub(super) fn is_loop(error: &io::Error) -> bool {
    error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error())
}

// ---------------------------------------------------------------------------
// Trees
// ---------------------------------------------------------------------------

/// Why a tree walk stopped.
#[derive(Debug)]
pub(super) enum TreeError {
    /// An entry changed while it was being copied.
    Changed(String),
    /// A FIFO, socket or device: never opened or copied.
    Special(String),
    TooLarge,
    Io(io::Error),
}

impl From<io::Error> for TreeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl std::fmt::Display for TreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Changed(at) => write!(f, "'{at}' changed while it was copied"),
            Self::Special(at) => write!(f, "'{at}' is not a file, directory or symlink"),
            Self::TooLarge => write!(
                f,
                "the tree is larger than {MAX_TREE_ENTRIES} entries or {MAX_TREE_DEPTH} levels"
            ),
            Self::Io(error) => write!(f, "{error}"),
        }
    }
}

/// Every entry of one tree, top first, parents before children, by path
/// relative to the top (`[]` is the top itself).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Inventory {
    pub(super) entries: Vec<(Vec<OsString>, Stamp)>,
}

impl Inventory {
    fn push(&mut self, rel: &[OsString], stamp: Stamp) -> Result<(), TreeError> {
        if self.entries.len() >= MAX_TREE_ENTRIES {
            return Err(TreeError::TooLarge);
        }
        self.entries.push((rel.to_vec(), stamp));
        Ok(())
    }

    /// A digest of every entry's path, kind, identity, size and mtime (no
    /// change time: a rename moves it), so an undo can tell an untouched tree
    /// from a replaced or extended one without keeping the inventory.
    pub(super) fn seal(&self) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        use std::os::unix::ffi::OsStrExt;
        let mut hasher = Sha256::new();
        for (rel, stamp) in &self.entries {
            hasher.update((rel.len() as u64).to_le_bytes());
            for part in rel {
                hasher.update((part.len() as u64).to_le_bytes());
                hasher.update(part.as_bytes());
            }
            hasher.update([stamp.kind as u8]);
            hasher.update(stamp.id.dev.to_le_bytes());
            hasher.update(stamp.id.ino.to_le_bytes());
            hasher.update(stamp.size.to_le_bytes());
            hasher.update(stamp.mtime.0.to_le_bytes());
            hasher.update(stamp.mtime.1.to_le_bytes());
        }
        hasher.finalize().into()
    }

    /// Whether every entry of `self` is in `now`, untouched, and `now` has
    /// nothing else.
    pub(super) fn untouched_in(&self, now: &Inventory) -> bool {
        self.entries.len() == now.entries.len()
            && self
                .entries
                .iter()
                .zip(&now.entries)
                .all(|((a, sa), (b, sb))| a == b && sa.untouched(sb))
    }
}

fn display(rel: &[OsString]) -> String {
    rel.iter()
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Inventory the tree at `name` in `dir` without following any symlink.
pub(super) fn inventory(dir: &Dir, name: &OsStr) -> Result<Inventory, TreeError> {
    fn walk(
        dir: &Dir,
        name: &OsStr,
        rel: &mut Vec<OsString>,
        out: &mut Inventory,
    ) -> Result<(), TreeError> {
        let Some(stamp) = lstat(dir, name)? else {
            return Err(TreeError::Changed(display(rel)));
        };
        out.push(rel, stamp)?;
        if stamp.kind == Kind::Dir {
            if rel.len() >= MAX_TREE_DEPTH {
                return Err(TreeError::TooLarge);
            }
            let sub = open_dir(dir, name)?;
            if fstat(&sub)?.id != stamp.id {
                return Err(TreeError::Changed(display(rel)));
            }
            for child in names(&sub)? {
                rel.push(child.clone());
                walk(&sub, &child, rel, out)?;
                rel.pop();
            }
        }
        Ok(())
    }
    let mut out = Inventory::default();
    walk(dir, name, &mut Vec::new(), &mut out)?;
    Ok(out)
}

/// Copy the tree at `from` in `from_dir` to the new name `to` in `to_dir`
/// (which must not exist): files byte for byte with their mode, directories
/// recursively, symlinks as links, through handles opened one level at a
/// time without following symlinks. A FIFO, socket or device is refused
/// before it is opened. Returns the inventory of the source as copied; a
/// file that changed while it was read fails the copy. `tick` runs after
/// each entry. On error the partial copy at `to` is the caller's to remove.
pub(super) fn copy_tree(
    from_dir: &Dir,
    from: &OsStr,
    to_dir: &Dir,
    to: &OsStr,
    tick: &mut dyn FnMut(),
) -> Result<Inventory, TreeError> {
    #[allow(clippy::too_many_arguments)]
    fn walk(
        from_dir: &Dir,
        from: &OsStr,
        to_dir: &Dir,
        to: &OsStr,
        rel: &mut Vec<OsString>,
        out: &mut Inventory,
        tick: &mut dyn FnMut(),
    ) -> Result<(), TreeError> {
        let Some(stamp) = lstat(from_dir, from)? else {
            return Err(TreeError::Changed(display(rel)));
        };
        match stamp.kind {
            Kind::Special => return Err(TreeError::Special(display(rel))),
            Kind::Symlink => {
                let target = read_link(from_dir, from)?;
                symlink(&target, to_dir, to)?;
                if lstat(from_dir, from)?.is_none_or(|now| !now.untouched(&stamp)) {
                    return Err(TreeError::Changed(display(rel)));
                }
                out.push(rel, stamp)?;
            }
            Kind::File => {
                let mut source = match open_file(from_dir, from) {
                    Ok(source) => source,
                    Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                        return Err(TreeError::Special(display(rel)));
                    }
                    Err(error) => return Err(error.into()),
                };
                if !fstat(&source)?.untouched(&stamp) {
                    return Err(TreeError::Changed(display(rel)));
                }
                let mut dest = create_file(to_dir, to, 0o600)?;
                io::copy(&mut source, &mut dest)?;
                set_mode(&dest, stamp.mode)?;
                if !fstat(&source)?.untouched(&stamp) {
                    return Err(TreeError::Changed(display(rel)));
                }
                out.push(rel, stamp)?;
            }
            Kind::Dir => {
                if rel.len() >= MAX_TREE_DEPTH {
                    return Err(TreeError::TooLarge);
                }
                let source = open_dir(from_dir, from)?;
                if fstat(&source)?.id != stamp.id {
                    return Err(TreeError::Changed(display(rel)));
                }
                make_dir(to_dir, to)?;
                let dest = open_dir(to_dir, to)?;
                out.push(rel, stamp)?;
                for child in names(&source)? {
                    rel.push(child.clone());
                    walk(&source, &child, &dest, &child, rel, out, tick)?;
                    rel.pop();
                }
                set_dir_mode(&dest, stamp.mode)?;
            }
        }
        tick();
        Ok(())
    }
    let mut out = Inventory::default();
    walk(from_dir, from, to_dir, to, &mut Vec::new(), &mut out, tick)?;
    Ok(out)
}

fn set_dir_mode(dir: &Dir, mode: u32) -> io::Result<()> {
    Ok(rustix::fs::fchmod(dir, Mode::from_raw_mode(mode as _))?)
}

/// Remove, bottom up, every entry of the tree at `name` in `dir` that is
/// still exactly what `copied` recorded (same object, untouched), never
/// following a symlink; an entry that changed or appeared since is left in
/// place together with its ancestors. Returns how many entries were left.
pub(super) fn remove_verified(dir: &Dir, name: &OsStr, copied: &Inventory) -> io::Result<usize> {
    let recorded: HashMap<&[OsString], &Stamp> = copied
        .entries
        .iter()
        .map(|(rel, stamp)| (rel.as_slice(), stamp))
        .collect();
    fn walk(
        dir: &Dir,
        name: &OsStr,
        rel: &mut Vec<OsString>,
        recorded: &HashMap<&[OsString], &Stamp>,
    ) -> io::Result<usize> {
        let Some(&&expected) = recorded.get(rel.as_slice()) else {
            // Appeared after the copy: not ours to remove.
            return Ok(1);
        };
        let Some(now) = lstat(dir, name)? else {
            return Ok(0);
        };
        if now.id != expected.id || now.kind != expected.kind {
            return Ok(1);
        }
        if now.kind != Kind::Dir {
            if !now.untouched(&expected) {
                return Ok(1);
            }
            unlink(dir, name)?;
            return Ok(0);
        }
        let sub = open_dir(dir, name)?;
        if fstat(&sub)?.id != expected.id {
            return Ok(1);
        }
        let mut left = 0;
        for child in names(&sub)? {
            rel.push(child.clone());
            left += walk(&sub, &child, rel, recorded)?;
            rel.pop();
        }
        if left > 0 {
            return Ok(left + 1);
        }
        // Still the same directory under this name, and empty.
        if lstat(dir, name)?.is_none_or(|again| again.id != expected.id) {
            return Ok(1);
        }
        match rmdir(dir, name) {
            Ok(()) => Ok(0),
            Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(1),
            Err(error) => Err(error),
        }
    }
    walk(dir, name, &mut Vec::new(), &recorded)
}

/// Remove a tree this module created under a fresh private name (a staging
/// copy), without following symlinks.
pub(super) fn remove_owned(dir: &Dir, name: &OsStr) -> io::Result<()> {
    match lstat(dir, name)? {
        None => Ok(()),
        Some(stamp) if stamp.kind == Kind::Dir => {
            let sub = open_dir(dir, name)?;
            for child in names(&sub)? {
                remove_owned(&sub, &child)?;
            }
            rmdir(dir, name)
        }
        Some(_) => unlink(dir, name),
    }
}
