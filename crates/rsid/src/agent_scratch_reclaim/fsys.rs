//! Descriptor-pinned filesystem primitives for scratch reclaim (#1140).
//!
//! Everything here works on opened directory descriptors and single path
//! components, never on a path that could be re-resolved through a symlink:
//! roots are opened one component at a time with `O_NOFOLLOW`, children are
//! opened relative to their parent, and identity (device, inode) is compared
//! after every step that could have raced. Directory enumeration charges the
//! pass budget as each entry is observed, before anything is allocated for it.

use std::collections::HashSet;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use nix::dir::Dir;
use nix::fcntl::{AtFlags, OFlag, RenameFlags, openat, renameat, renameat2};
use nix::sys::stat::{FileStat, Mode, SFlag, fstat, fstatat};
use nix::unistd::{UnlinkatFlags, fsync, unlinkat};

/// Directory entries a single candidate may contain before the walk gives up
/// (the candidate is then retained as unproven).
pub(super) const MAX_WALK_ENTRIES: usize = 2_000_000;
/// Directory nesting a walk or removal follows (also bounds open descriptors).
const MAX_WALK_DEPTH: usize = 96;
/// Largest mount table read; a bigger one is not proven complete.
const MOUNTINFO_LIMIT: usize = 8 << 20;

/// Device and inode of a filesystem object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Ident {
    pub dev: u64,
    pub ino: u64,
}

#[allow(clippy::unnecessary_cast)] // st_dev/st_ino widths differ per target
pub(super) fn ident_of(stat: &FileStat) -> Ident {
    Ident {
        dev: stat.st_dev as u64,
        ino: stat.st_ino as u64,
    }
}

#[allow(clippy::unnecessary_cast)]
fn mode_bits(stat: &FileStat) -> u32 {
    stat.st_mode as u32
}

#[allow(clippy::unnecessary_cast)]
fn file_type_bits(stat: &FileStat) -> u32 {
    mode_bits(stat) & SFlag::S_IFMT.bits() as u32
}

#[allow(clippy::unnecessary_cast)]
pub(super) fn is_dir(stat: &FileStat) -> bool {
    file_type_bits(stat) == SFlag::S_IFDIR.bits() as u32
}

#[allow(clippy::unnecessary_cast)]
pub(super) fn is_symlink(stat: &FileStat) -> bool {
    file_type_bits(stat) == SFlag::S_IFLNK.bits() as u32
}

#[allow(clippy::unnecessary_cast)]
fn is_regular(stat: &FileStat) -> bool {
    file_type_bits(stat) == SFlag::S_IFREG.bits() as u32
}

#[allow(clippy::unnecessary_cast)]
fn uid_of(stat: &FileStat) -> u32 {
    stat.st_uid as u32
}

/// One bounded pass worth of time and directory-entry budget, shared by every
/// scan, proof and removal so no phase can outrun the pass.
pub(super) struct Budget {
    deadline: Instant,
    scan_left: usize,
    exhausted: bool,
}

impl Budget {
    pub(super) const fn new(deadline: Instant, scan_entries: usize) -> Self {
        Self {
            deadline,
            scan_left: scan_entries,
            exhausted: false,
        }
    }

    pub(super) fn expired(&mut self) -> bool {
        if Instant::now() >= self.deadline {
            self.exhausted = true;
        }
        self.exhausted
    }

    /// Charge `n` observations; false when the pass is out of time or entries.
    pub(super) fn spend(&mut self, n: usize) -> bool {
        if self.scan_left < n {
            self.scan_left = 0;
            self.exhausted = true;
            return false;
        }
        self.scan_left -= n;
        !self.expired()
    }

    pub(super) const fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    pub(super) const fn deadline(&self) -> Instant {
        self.deadline
    }
}

fn budget_error() -> io::Error {
    io::Error::other("scan budget exhausted")
}

/// What the kernel says about a descriptor beyond `fstat`: the mount it is on
/// (tells a same-device bind mount from its parent) and its birth time (a
/// per-inode-generation value a copied or replayed record cannot reproduce).
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Statx {
    pub mount: Option<u64>,
    /// Birth time in nanoseconds since the epoch.
    pub btime: Option<i128>,
}

pub(super) fn statx_of(fd: RawFd) -> Statx {
    let mut out = std::mem::MaybeUninit::<nix::libc::statx>::zeroed();
    // SAFETY: `fd` is a valid open descriptor, the path is the empty string
    // with AT_EMPTY_PATH, and `out` is a writable statx buffer.
    let rc = unsafe {
        nix::libc::statx(
            fd,
            c"".as_ptr(),
            nix::libc::AT_EMPTY_PATH,
            nix::libc::STATX_MNT_ID | nix::libc::STATX_BTIME,
            out.as_mut_ptr(),
        )
    };
    if rc != 0 {
        return Statx::default();
    }
    // SAFETY: statx succeeded and filled the buffer.
    let out = unsafe { out.assume_init() };
    Statx {
        mount: (out.stx_mask & nix::libc::STATX_MNT_ID != 0).then_some(out.stx_mnt_id),
        btime: (out.stx_mask & nix::libc::STATX_BTIME != 0).then(|| {
            i128::from(out.stx_btime.tv_sec) * 1_000_000_000 + i128::from(out.stx_btime.tv_nsec)
        }),
    }
}

/// An opened directory with the identity it had when opened.
pub(super) struct Pinned {
    pub fd: OwnedFd,
    pub ident: Ident,
    pub uid: u32,
    pub mode: u32,
    /// Mount the directory is on, when the kernel reports it.
    pub mount: Option<u64>,
    /// Birth time (nanoseconds), when the filesystem keeps one.
    pub btime: Option<i128>,
}

impl Pinned {
    pub(super) fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

fn nix_io(error: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

fn wrap_fd(raw: RawFd) -> OwnedFd {
    // SAFETY: `raw` was just returned by a successful open and is owned here.
    unsafe { OwnedFd::from_raw_fd(raw) }
}

/// Open `name` (a single component) under `dir` as a directory without
/// following a symlink.
pub(super) fn open_dir_at(dir: RawFd, name: &str) -> io::Result<OwnedFd> {
    openat(
        Some(dir),
        name,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map(wrap_fd)
    .map_err(nix_io)
}

pub(super) fn fstat_fd(fd: RawFd) -> io::Result<FileStat> {
    fstat(fd).map_err(nix_io)
}

/// Put `dir` and its top-level file `name` back to `before`'s timestamps, so a
/// record written into a legacy directory does not look like a recent write.
/// Best effort: a failure is ignored (the age gate then errs toward retaining).
pub(super) fn restore_times(dir: &Pinned, name: &str, before: &FileStat) {
    use nix::sys::stat::{UtimensatFlags, futimens, utimensat};
    use nix::sys::time::TimeSpec;
    let atime = TimeSpec::new(before.st_atime as _, before.st_atime_nsec as _);
    let mtime = TimeSpec::new(before.st_mtime as _, before.st_mtime_nsec as _);
    let _ = utimensat(
        Some(dir.raw()),
        name,
        &atime,
        &mtime,
        UtimensatFlags::NoFollowSymlink,
    );
    let _ = futimens(dir.raw(), &atime, &mtime);
}

/// Mark `dir` as written just now (fail toward keeping data: a directory whose
/// activity cannot be accounted for must look fresh).
pub(super) fn touch_now(dir: &Pinned) {
    use nix::sys::stat::futimens;
    use nix::sys::time::TimeSpec;
    let _ = futimens(dir.raw(), &TimeSpec::UTIME_NOW, &TimeSpec::UTIME_NOW);
}

pub(super) fn stat_at(dir: RawFd, name: &str) -> io::Result<FileStat> {
    fstatat(Some(dir), name, AtFlags::AT_SYMLINK_NOFOLLOW).map_err(nix_io)
}

pub(super) fn pin(fd: OwnedFd) -> io::Result<Pinned> {
    let stat = fstat_fd(fd.as_raw_fd())?;
    let extra = statx_of(fd.as_raw_fd());
    Ok(Pinned {
        ident: ident_of(&stat),
        uid: uid_of(&stat),
        mode: mode_bits(&stat),
        mount: extra.mount,
        btime: extra.btime,
        fd,
    })
}

/// Open a pinned child directory of `parent`.
pub(super) fn pin_child(parent: &Pinned, name: &str) -> io::Result<Pinned> {
    pin(open_dir_at(parent.raw(), name)?)
}

/// Whether an ancestor (or the root itself) can be trusted not to have its
/// entries renamed or swapped by another user: owned by root or the daemon
/// user, and either closed to group and other writes or sticky (so only an
/// entry's owner can rename it). Ownership by the daemon user does not excuse
/// an open mode.
const fn ancestor_authentic(pinned: &Pinned, uid: u32) -> bool {
    let owner_ok = pinned.uid == 0 || pinned.uid == uid;
    let open_write = pinned.mode & 0o022 != 0;
    let sticky = pinned.mode & 0o1000 != 0;
    owner_ok && (!open_write || sticky)
}

/// Open `root` one component at a time with `O_NOFOLLOW`, authenticating every
/// ancestor. A symlink anywhere on the path, a relative or non-normal path, or
/// an ancestor another user could rename in refuses the root.
pub(super) fn open_root(root: &Path, uid: u32) -> io::Result<Pinned> {
    if !root.is_absolute() {
        return Err(io::Error::other("scratch root must be absolute"));
    }
    let start = openat(
        None,
        "/",
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map(wrap_fd)
    .map_err(nix_io)?;
    let mut current = pin(start)?;
    for component in root.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                let name = name
                    .to_str()
                    .ok_or_else(|| io::Error::other("non-utf8 scratch root component"))?;
                // Authenticate the directory we are about to descend from.
                if !ancestor_authentic(&current, uid) {
                    return Err(io::Error::other("scratch root ancestor not authentic"));
                }
                current = pin_child(&current, name)?;
            }
            _ => return Err(io::Error::other("scratch root is not a normal path")),
        }
    }
    if !ancestor_authentic(&current, uid) {
        return Err(io::Error::other("scratch root not authentic"));
    }
    Ok(current)
}

/// Re-resolve `root` and confirm it is still the directory `pinned` holds.
pub(super) fn root_unchanged(root: &Path, uid: u32, pinned: &Pinned) -> bool {
    open_root(root, uid).is_ok_and(|again| again.ident == pinned.ident)
}

// ---- mounts -------------------------------------------------------------

fn unescape_mount_field(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1..i + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let value = (u32::from(bytes[i + 1] - b'0') << 6)
                | (u32::from(bytes[i + 2] - b'0') << 3)
                | u32::from(bytes[i + 3] - b'0');
            #[allow(clippy::cast_possible_truncation)]
            out.push(value as u8);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Mount points from `mountinfo`. `None` when it is unreadable, larger than
/// the read limit, empty, or has a line this parser does not understand: a
/// table that is not wholly understood is not an inventory.
pub(super) fn read_mounts(mountinfo: &Path) -> Option<Vec<PathBuf>> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(mountinfo)
        .ok()?
        .take(MOUNTINFO_LIMIT as u64 + 1)
        .read_to_string(&mut text)
        .ok()?;
    if text.len() > MOUNTINFO_LIMIT {
        return None;
    }
    let mut mounts = Vec::new();
    for line in text.lines() {
        mounts.push(PathBuf::from(unescape_mount_field(line.split(' ').nth(4)?)));
    }
    (!mounts.is_empty()).then_some(mounts)
}

/// A mount at the candidate or anywhere below it (bind mounts included).
pub(super) fn mount_at_or_under(mounts: &[PathBuf], candidate: &Path) -> bool {
    mounts.iter().any(|mount| mount.starts_with(candidate))
}

/// Both mounts are known and equal. When the kernel cannot report mount ids
/// the boundary cannot be established, which retains.
pub(super) const fn same_known_mount(a: Option<u64>, b: Option<u64>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if a == b)
}

fn child_on_mount(top: Option<u64>, child: &OwnedFd) -> bool {
    same_known_mount(top, statx_of(child.as_raw_fd()).mount)
}

/// Whether `candidate` is on `root`'s device and mount.
pub(super) fn on_root_mount(root: &Pinned, candidate: &Pinned) -> bool {
    root.ident.dev == candidate.ident.dev && same_known_mount(root.mount, candidate.mount)
}

// ---- enumeration ----------------------------------------------------------

/// Names in a directory, each charged to `budget` as it is read (before it is
/// stored). A name that is not UTF-8 is an error unless `skip_non_utf8` (a root
/// listing, where such a name is not allowlisted). Budget exhaustion is an
/// error with `budget.is_exhausted()` set.
pub(super) fn entry_names(
    dir: RawFd,
    skip_non_utf8: bool,
    budget: &mut Budget,
) -> io::Result<Vec<String>> {
    let mut handle = Dir::openat(
        Some(dir),
        ".",
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(nix_io)?;
    let mut names = Vec::new();
    for entry in handle.iter() {
        let entry = entry.map_err(nix_io)?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if !budget.spend(1) {
            return Err(budget_error());
        }
        let Ok(name) = std::str::from_utf8(name) else {
            if skip_non_utf8 {
                continue;
            }
            // A name we cannot address as UTF-8 cannot be proven either.
            return Err(io::Error::other("non-utf8 entry name"));
        };
        names.push(name.to_string());
    }
    Ok(names)
}

// ---- census ---------------------------------------------------------------

/// Where git state must be proven inside a candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct GitRoot {
    /// Path relative to the candidate (empty for the candidate itself).
    pub rel: PathBuf,
    /// Identity of the directory holding the repository (`.git` entry's
    /// parent, or the bare repository itself).
    pub ident: Ident,
    /// Birth time (ns) of that directory, when the filesystem keeps one.
    pub btime: Option<i128>,
    /// A bare repository (or git dir): no working tree to inspect.
    pub bare: bool,
}

/// A complete walk of one candidate: sizes, every inode, git roots, and the
/// per-entry manifest the deletion must stay inside.
#[derive(Debug, Default)]
pub(super) struct Census {
    pub entries: usize,
    pub bytes: u64,
    pub idents: HashSet<Ident>,
    pub gits: Vec<GitRoot>,
    /// Sorted hashes of every entry below the walked directory: relative path,
    /// inode, type and (for non-directories) size and mtime. Directory
    /// timestamps are excluded because deleting a child changes them.
    pub manifest: Vec<u64>,
    /// Newest mtime anywhere below the walked directory, ns since the epoch.
    pub newest_mtime_ns: i128,
}

fn blocks_bytes(stat: &FileStat) -> u64 {
    u64::try_from(stat.st_blocks)
        .unwrap_or(0)
        .saturating_mul(512)
}

#[allow(clippy::unnecessary_cast)]
pub(super) fn mtime_ns(stat: &FileStat) -> i128 {
    i128::from(stat.st_mtime as i64) * 1_000_000_000 + i128::from(stat.st_mtime_nsec as i64)
}

/// Stable 64-bit identity of one entry for the deletion manifest.
pub(super) fn entry_hash(rel: &Path, stat: &FileStat) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(rel.as_os_str().as_bytes());
    hasher.update(&[0]);
    hasher.update(&ident_of(stat).ino.to_le_bytes());
    if is_dir(stat) {
        hasher.update(b"d");
    } else {
        hasher.update(if is_symlink(stat) { b"l" } else { b"f" });
        hasher.update(&stat.st_size.to_le_bytes());
        hasher.update(&mtime_ns(stat).to_le_bytes());
    }
    let digest = hasher.finalize();
    let mut first = [0u8; 8];
    first.copy_from_slice(&digest.as_bytes()[..8]);
    u64::from_le_bytes(first)
}

pub(super) fn manifest_contains(manifest: &[u64], hash: u64) -> bool {
    manifest.binary_search(&hash).is_ok()
}

struct WalkDir<'a> {
    fd: &'a OwnedFd,
    rel: &'a Path,
    self_ident: Ident,
    self_btime: Option<i128>,
    /// The directory is named `.git`: its own `HEAD`/`objects`/`refs` are the
    /// admin directory of the repository already found in its parent, not an
    /// independent bare repository.
    git_admin: bool,
    dev: u64,
    mount: Option<u64>,
    depth: usize,
}

fn walk_dir(dir: &WalkDir<'_>, census: &mut Census, budget: &mut Budget) -> bool {
    if dir.depth > MAX_WALK_DEPTH {
        return false;
    }
    let Ok(names) = entry_names(dir.fd.as_raw_fd(), false, budget) else {
        return false;
    };
    let mut has_head = false;
    let mut has_objects = false;
    let mut has_refs = false;
    let mut has_dot_git = false;
    for name in &names {
        census.entries += 1;
        if census.entries > MAX_WALK_ENTRIES {
            return false;
        }
        let Ok(stat) = stat_at(dir.fd.as_raw_fd(), name) else {
            return false;
        };
        if ident_of(&stat).dev != dir.dev {
            // A different device below the candidate is a mount boundary.
            return false;
        }
        let child_rel = dir.rel.join(name);
        census.idents.insert(ident_of(&stat));
        census.bytes = census.bytes.saturating_add(blocks_bytes(&stat));
        census.newest_mtime_ns = census.newest_mtime_ns.max(mtime_ns(&stat));
        census.manifest.push(entry_hash(&child_rel, &stat));
        match name.as_str() {
            "HEAD" => has_head = true,
            "objects" => has_objects = is_dir(&stat),
            "refs" => has_refs = is_dir(&stat),
            ".git" => has_dot_git = true,
            _ => {}
        }
        if is_symlink(&stat) || !is_dir(&stat) {
            continue;
        }
        let Ok(child) = open_dir_at(dir.fd.as_raw_fd(), name) else {
            return false;
        };
        let Ok(child_stat) = fstat_fd(child.as_raw_fd()) else {
            return false;
        };
        let child_statx = statx_of(child.as_raw_fd());
        if ident_of(&child_stat) != ident_of(&stat)
            || !same_known_mount(dir.mount, child_statx.mount)
        {
            return false;
        }
        let next = WalkDir {
            fd: &child,
            rel: &child_rel,
            self_ident: ident_of(&child_stat),
            self_btime: child_statx.btime,
            git_admin: name == ".git",
            dev: dir.dev,
            mount: dir.mount,
            depth: dir.depth + 1,
        };
        if !walk_dir(&next, census, budget) {
            return false;
        }
    }
    if has_dot_git {
        census.gits.push(GitRoot {
            rel: dir.rel.to_path_buf(),
            ident: dir.self_ident,
            btime: dir.self_btime,
            bare: false,
        });
    } else if !dir.git_admin && has_head && has_objects && has_refs {
        census.gits.push(GitRoot {
            rel: dir.rel.to_path_buf(),
            ident: dir.self_ident,
            btime: dir.self_btime,
            bare: true,
        });
    }
    true
}

/// Walk every entry below `top` (never following symlinks, never leaving its
/// device or mount). `None` means the walk could not be completed: unreadable
/// directory, depth or entry limit, mount boundary, or the pass budget.
/// Repositories are discovered everywhere, including under `target/`,
/// `node_modules/` and a repository's own `.git` (a saved nested checkout).
pub(super) fn census(top: &Pinned, budget: &mut Budget) -> Option<Census> {
    let mut census = Census::default();
    let start = WalkDir {
        fd: &top.fd,
        rel: Path::new(""),
        self_ident: top.ident,
        self_btime: top.btime,
        git_admin: false,
        dev: top.ident.dev,
        mount: top.mount,
        depth: 0,
    };
    walk_dir(&start, &mut census, budget).then(|| {
        census.manifest.sort_unstable();
        census.manifest.dedup();
        census
    })
}

/// Newest mtime of `top` and its immediate children (the cheap age pre-gate).
pub(super) fn newest_mtime_ns(top: &Pinned, budget: &mut Budget) -> Option<i128> {
    let stat = fstat_fd(top.raw()).ok()?;
    let mut newest = mtime_ns(&stat);
    for name in entry_names(top.raw(), false, budget).ok()? {
        let child = stat_at(top.raw(), &name).ok()?;
        newest = newest.max(mtime_ns(&child));
    }
    Some(newest)
}

// ---- removal --------------------------------------------------------------

/// How a bounded removal ended.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Removal {
    Done,
    /// The pass ran out of time or entries; what remains is finished later.
    OutOfBudget,
    /// An entry the final proof did not authorize (new, replaced or edited):
    /// nothing more is deleted.
    Drift,
    /// A mount boundary, replaced directory or I/O error: nothing more is
    /// deleted here.
    Stopped,
}

struct RemoveCtx<'a> {
    dev: u64,
    mount: Option<u64>,
    keep: &'a [&'a str],
    manifest: &'a [u64],
}

fn remove_children(
    dir: &OwnedFd,
    rel: &Path,
    depth: usize,
    ctx: &RemoveCtx<'_>,
    budget: &mut Budget,
) -> Removal {
    if depth > MAX_WALK_DEPTH {
        return Removal::Stopped;
    }
    let names = match entry_names(dir.as_raw_fd(), false, budget) {
        Ok(names) => names,
        Err(_) if budget.is_exhausted() => return Removal::OutOfBudget,
        Err(_) => return Removal::Stopped,
    };
    for name in names {
        if depth == 0 && ctx.keep.contains(&name.as_str()) {
            continue;
        }
        let Ok(stat) = stat_at(dir.as_raw_fd(), &name) else {
            return Removal::Stopped;
        };
        if ident_of(&stat).dev != ctx.dev {
            return Removal::Stopped;
        }
        let entry_rel = rel.join(&name);
        // Only entries the final proof saw, unchanged, may be deleted.
        if !manifest_contains(ctx.manifest, entry_hash(&entry_rel, &stat)) {
            return Removal::Drift;
        }
        if is_dir(&stat) && !is_symlink(&stat) {
            let Ok(child) = open_dir_at(dir.as_raw_fd(), &name) else {
                return Removal::Stopped;
            };
            let Ok(child_stat) = fstat_fd(child.as_raw_fd()) else {
                return Removal::Stopped;
            };
            if ident_of(&child_stat) != ident_of(&stat) || !child_on_mount(ctx.mount, &child) {
                return Removal::Stopped;
            }
            match remove_children(&child, &entry_rel, depth + 1, ctx, budget) {
                Removal::Done => {}
                other => return other,
            }
            #[cfg(test)]
            if let Some(hook) = BEFORE_DIR_UNLINK.with(std::cell::Cell::get) {
                hook(dir.as_raw_fd(), &name);
            }
            // The name must still be the directory just emptied: a concurrent
            // rename may have put something else there.
            match stat_at(dir.as_raw_fd(), &name) {
                Ok(again) if ident_of(&again) == ident_of(&child_stat) => {}
                Ok(_) => return Removal::Drift,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Removal::Drift,
                Err(_) => return Removal::Stopped,
            }
            match unlinkat(
                Some(dir.as_raw_fd()),
                name.as_str(),
                UnlinkatFlags::RemoveDir,
            ) {
                Ok(()) => {}
                // Something appeared inside after the last listing.
                Err(nix::errno::Errno::ENOTEMPTY | nix::errno::Errno::EEXIST) => {
                    return Removal::Drift;
                }
                Err(_) => return Removal::Stopped,
            }
        } else if unlinkat(
            Some(dir.as_raw_fd()),
            name.as_str(),
            UnlinkatFlags::NoRemoveDir,
        )
        .is_err()
        {
            return Removal::Stopped;
        }
    }
    Removal::Done
}

/// Delete everything below `top` except the top-level `keep` entries, staying
/// on `top`'s device and mount, never following a symlink, and only deleting
/// entries whose hash is in `manifest` (the final proof).
pub(super) fn remove_contents(
    top: &Pinned,
    keep: &[&str],
    manifest: &[u64],
    budget: &mut Budget,
) -> Removal {
    let ctx = RemoveCtx {
        dev: top.ident.dev,
        mount: top.mount,
        keep,
        manifest,
    };
    remove_children(&top.fd, Path::new(""), 0, &ctx, budget)
}

/// Remove one top-level non-directory entry that the final proof saw.
pub(super) fn unlink_proved(dir: &Pinned, name: &str, manifest: &[u64]) -> io::Result<Removal> {
    let stat = match stat_at(dir.raw(), name) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Removal::Done),
        Err(error) => return Err(error),
    };
    if !manifest_contains(manifest, entry_hash(Path::new(name), &stat)) {
        return Ok(Removal::Drift);
    }
    unlinkat(Some(dir.raw()), name, UnlinkatFlags::NoRemoveDir).map_err(nix_io)?;
    Ok(Removal::Done)
}

/// Remove the (empty) directory entry `name` of `parent`, but only while it is
/// still the directory with identity `expected` that the proof was about.
pub(super) fn rmdir_proved(parent: &Pinned, name: &str, expected: Ident) -> io::Result<Removal> {
    match stat_at(parent.raw(), name) {
        Ok(stat) if ident_of(&stat) == expected => {}
        Ok(_) => return Ok(Removal::Drift),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Removal::Drift),
        Err(error) => return Err(error),
    }
    match unlinkat(Some(parent.raw()), name, UnlinkatFlags::RemoveDir) {
        Ok(()) => Ok(Removal::Done),
        Err(nix::errno::Errno::ENOTEMPTY | nix::errno::Errno::EEXIST) => Ok(Removal::Drift),
        Err(error) => Err(nix_io(error)),
    }
}

#[cfg(test)]
thread_local! {
    /// Test seam: runs just before a just-emptied directory's name is unlinked
    /// (`dir` descriptor, entry name), to simulate a concurrent replacement.
    pub(super) static BEFORE_DIR_UNLINK: std::cell::Cell<Option<fn(RawFd, &str)>> =
        const { std::cell::Cell::new(None) };
}

/// Rename within one directory without replacing an existing entry.
pub(super) fn rename_noreplace(dir: &Pinned, from: &str, to: &str) -> io::Result<()> {
    renameat2(
        Some(dir.raw()),
        from,
        Some(dir.raw()),
        to,
        RenameFlags::RENAME_NOREPLACE,
    )
    .map_err(nix_io)
}

// ---- files ----------------------------------------------------------------

/// A small regular file of a directory (no symlink, owned by `owner` when
/// given) with its stat. Missing is `Ok(None)`. A file over `limit` bytes is
/// an error, not a truncation.
pub(super) fn read_file_at(
    dir: RawFd,
    name: &str,
    limit: usize,
    owner: Option<u32>,
) -> io::Result<Option<(Vec<u8>, FileStat)>> {
    use std::io::Read;
    let raw = match openat(
        Some(dir),
        name,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
        Mode::empty(),
    ) {
        Ok(raw) => raw,
        Err(nix::errno::Errno::ENOENT) => return Ok(None),
        Err(error) => return Err(nix_io(error)),
    };
    let mut file = std::fs::File::from(wrap_fd(raw));
    let stat = fstat_fd(file.as_raw_fd())?;
    if !is_regular(&stat) {
        return Err(io::Error::other("not a regular file"));
    }
    if owner.is_some_and(|uid| uid_of(&stat) != uid) {
        return Err(io::Error::other("file has the wrong owner"));
    }
    let mut data = Vec::new();
    file.by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut data)?;
    if data.len() > limit {
        return Err(io::Error::other("file larger than its limit"));
    }
    Ok(Some((data, stat)))
}

/// Read a small text file of a pinned directory. Missing is `Ok(None)`.
pub(super) fn read_small_file(
    dir: &Pinned,
    name: &str,
    limit: usize,
) -> io::Result<Option<String>> {
    read_file_at(dir.raw(), name, limit, None)?
        .map(|(data, _)| String::from_utf8(data).map_err(|_| io::Error::other("file is not UTF-8")))
        .transpose()
}

/// Read a small text file at `components` below a pinned directory, opening
/// every directory on the way without following a symlink.
pub(super) fn read_small_file_below(
    top: &Pinned,
    components: &[&str],
    limit: usize,
) -> io::Result<Option<String>> {
    let Some((file, dirs)) = components.split_last() else {
        return Ok(None);
    };
    let mut held: Vec<OwnedFd> = Vec::new();
    for name in dirs {
        let parent = held.last().map_or(top.raw(), AsRawFd::as_raw_fd);
        match open_dir_at(parent, name) {
            Ok(fd) => held.push(fd),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
    }
    let parent = held.last().map_or(top.raw(), AsRawFd::as_raw_fd);
    read_file_at(parent, file, limit, None)?
        .map(|(data, _)| String::from_utf8(data).map_err(|_| io::Error::other("file is not UTF-8")))
        .transpose()
}

/// Create a new regular file `name` in a pinned directory (`O_EXCL`, no
/// symlink) holding `content`, and make it durable.
pub(super) fn create_file(dir: &Pinned, name: &str, content: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let raw = openat(
        Some(dir.raw()),
        name,
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(nix_io)?;
    let mut file = std::fs::File::from(wrap_fd(raw));
    file.write_all(content)?;
    file.sync_all()
}

/// Atomically replace (or create) `name` in a pinned directory with `content`:
/// write a temporary file, make it durable, rename it over the name, then make
/// the directory entry durable.
pub(super) fn replace_file(dir: &Pinned, name: &str, content: &[u8]) -> io::Result<()> {
    let temp = format!(".tmp-{}-{}", std::process::id(), name);
    match unlinkat(Some(dir.raw()), temp.as_str(), UnlinkatFlags::NoRemoveDir) {
        Ok(()) | Err(nix::errno::Errno::ENOENT) => {}
        Err(error) => return Err(nix_io(error)),
    }
    create_file(dir, &temp, content)?;
    if let Err(error) = renameat(Some(dir.raw()), temp.as_str(), Some(dir.raw()), name) {
        let _ = unlinkat(Some(dir.raw()), temp.as_str(), UnlinkatFlags::NoRemoveDir);
        return Err(nix_io(error));
    }
    fsync(dir.raw()).map_err(nix_io)
}

/// Remove a top-level file by name; missing is fine.
pub(super) fn remove_file_quiet(dir: &Pinned, name: &str) {
    let _ = unlinkat(Some(dir.raw()), name, UnlinkatFlags::NoRemoveDir);
}

/// Stat a top-level entry (no symlink following); missing is `Ok(None)`.
pub(super) fn stat_opt(dir: &Pinned, name: &str) -> io::Result<Option<FileStat>> {
    match stat_at(dir.raw(), name) {
        Ok(stat) => Ok(Some(stat)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// A fresh open file description of the pinned directory itself, to `flock`:
/// the lock lives on the directory (a lock file would be one more name in it)
/// and two descriptions conflict even inside one process.
pub(super) fn open_dir_lock(dir: &Pinned) -> io::Result<std::fs::File> {
    open_dir_at(dir.raw(), ".").map(std::fs::File::from)
}

/// Try to `flock` the file without waiting: `Ok(false)` when another open file
/// description holds a conflicting lock. A scratch pass is bounded, so nothing
/// here ever blocks on a lock.
pub(super) fn try_flock(file: &std::fs::File, exclusive: bool) -> io::Result<bool> {
    let op = nix::libc::LOCK_NB
        | if exclusive {
            nix::libc::LOCK_EX
        } else {
            nix::libc::LOCK_SH
        };
    loop {
        // SAFETY: the descriptor is open for the duration of the call.
        let rc = unsafe { nix::libc::flock(file.as_raw_fd(), op) };
        if rc == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.kind() {
            io::ErrorKind::Interrupted => {}
            io::ErrorKind::WouldBlock => return Ok(false),
            _ => return Err(error),
        }
    }
}

/// One name in a directory, as observed in a single stat of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Named {
    pub name: String,
    pub ident: Ident,
    /// Birth time (ns), when the filesystem keeps one.
    pub btime: Option<i128>,
    /// Mount the name resolves into, when the kernel reports it. A name on a
    /// different mount than its directory is a mount point covering whatever
    /// the directory itself holds there.
    pub mount: Option<u64>,
    /// The name is a directory (no symlink followed).
    pub is_dir: bool,
}

/// Why a directory inventory is not usable.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum InventoryError {
    /// A name changed or vanished while it was read: the directory was being
    /// modified, so the picture is not a snapshot. Worth retrying.
    Mutated,
    /// The listing or a stat could not be completed, the budget ran out, or a
    /// name is not UTF-8.
    Unproven,
}

#[cfg(test)]
thread_local! {
    /// Called with the call number after every listing, before its names are
    /// examined (a deterministic point for a test to modify the directory).
    pub(super) static AFTER_LISTING: std::cell::Cell<Option<fn(usize)>> =
        const { std::cell::Cell::new(None) };
    pub(super) static LISTINGS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Called with the name before each name is stat'ed.
    pub(super) static BEFORE_STAT: std::cell::Cell<Option<fn(&str)>> =
        const { std::cell::Cell::new(None) };
    /// May alter the identity the first stat reported (a mixed observation).
    pub(super) static AFTER_FSTATAT: std::cell::Cell<Option<fn(&mut Ident)>> =
        const { std::cell::Cell::new(None) };
}

/// Stat `name` (no symlink followed) once for identity, birth time and mount,
/// confirming both calls saw the same inode.
fn named_stat(dir: RawFd, name: &str) -> Result<Named, InventoryError> {
    let stat = match stat_at(dir, name) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(InventoryError::Mutated);
        }
        Err(_) => return Err(InventoryError::Unproven),
    };
    #[allow(unused_mut)]
    let mut ident = ident_of(&stat);
    #[cfg(test)]
    if let Some(hook) = AFTER_FSTATAT.with(std::cell::Cell::get) {
        hook(&mut ident);
    }
    let cname = std::ffi::CString::new(name).map_err(|_| InventoryError::Unproven)?;
    let mut out = std::mem::MaybeUninit::<nix::libc::statx>::zeroed();
    // SAFETY: `dir` is a valid open directory, `cname` is NUL-terminated and
    // `out` is a writable statx buffer.
    let rc = unsafe {
        nix::libc::statx(
            dir,
            cname.as_ptr(),
            nix::libc::AT_SYMLINK_NOFOLLOW,
            nix::libc::STATX_BASIC_STATS | nix::libc::STATX_MNT_ID | nix::libc::STATX_BTIME,
            out.as_mut_ptr(),
        )
    };
    if rc != 0 {
        let error = io::Error::last_os_error();
        return Err(if error.kind() == io::ErrorKind::NotFound {
            InventoryError::Mutated
        } else {
            InventoryError::Unproven
        });
    }
    // SAFETY: statx succeeded and filled the buffer.
    let out = unsafe { out.assume_init() };
    // The name was replaced between the two stats: the complete device and
    // inode identity must agree (an inode number alone is only unique within
    // one device, and a covering mount can change the device between calls).
    let basic = nix::libc::STATX_INO | nix::libc::STATX_TYPE;
    let statx_ident = Ident {
        dev: nix::libc::makedev(out.stx_dev_major, out.stx_dev_minor),
        ino: out.stx_ino,
    };
    if out.stx_mask & basic != basic || statx_ident != ident {
        return Err(InventoryError::Mutated);
    }
    Ok(Named {
        name: name.to_string(),
        ident,
        btime: (out.stx_mask & nix::libc::STATX_BTIME != 0).then(|| {
            i128::from(out.stx_btime.tv_sec) * 1_000_000_000 + i128::from(out.stx_btime.tv_nsec)
        }),
        mount: (out.stx_mask & nix::libc::STATX_MNT_ID != 0).then_some(out.stx_mnt_id),
        is_dir: u32::from(out.stx_mode) & nix::libc::S_IFMT == nix::libc::S_IFDIR,
    })
}

/// Every name in `dir` with what it names, sorted by name. A name that is not
/// UTF-8, a failed stat or an exhausted budget is `Unproven`; a name that
/// vanishes or changes between the listing and its stat is `Mutated` (a
/// directory being modified is not inventoried, never skipped over).
pub(super) fn named_inventory(
    dir: &Pinned,
    budget: &mut Budget,
) -> Result<Vec<Named>, InventoryError> {
    let names = entry_names(dir.raw(), false, budget).map_err(|_| InventoryError::Unproven)?;
    #[cfg(test)]
    {
        let call = LISTINGS.with(|c| {
            c.set(c.get() + 1);
            c.get()
        });
        if let Some(hook) = AFTER_LISTING.with(std::cell::Cell::get) {
            hook(call);
        }
    }
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        // The stat phase is as bounded as the listing: stop once time is up.
        if budget.expired() {
            return Err(InventoryError::Unproven);
        }
        #[cfg(test)]
        if let Some(hook) = BEFORE_STAT.with(std::cell::Cell::get) {
            hook(&name);
        }
        out.push(named_stat(dir.raw(), &name)?);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    // Two names cannot be the same directory: a repeated directory identity
    // means a name was exchanged under the scan and the picture is torn.
    let mut dirs: Vec<Ident> = out.iter().filter(|n| n.is_dir).map(|n| n.ident).collect();
    dirs.sort_by_key(|i| (i.dev, i.ino));
    if dirs.windows(2).any(|w| w[0] == w[1]) {
        return Err(InventoryError::Mutated);
    }
    Ok(out)
}

/// A directory's identity and modification stamps: changes whenever a name is
/// created, removed or renamed in it (to the filesystem's timestamp grain).
pub(super) fn dir_stamp(dir: &Pinned) -> io::Result<(Ident, i128, i128)> {
    let stat = fstat_fd(dir.raw())?;
    #[allow(clippy::unnecessary_cast)]
    let ctime =
        i128::from(stat.st_ctime as i64) * 1_000_000_000 + i128::from(stat.st_ctime_nsec as i64);
    Ok((ident_of(&stat), mtime_ns(&stat), ctime))
}

/// Permissions of a regular file in a pinned directory.
pub(super) fn file_mode(stat: &FileStat) -> u32 {
    mode_bits(stat) & 0o7777
}
