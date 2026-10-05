//! Creation records (#1140): the in-tree half of scratch provenance.
//!
//! A scratch directory RSI made carries `.rsi-scratch-record`, which names its
//! allocation entry in the private registry (`registry.rs`) by nonce. The
//! record alone proves nothing; the registry entry it names must bind this very
//! directory (device, inode, birth time, owner, kind and parent). Allocation is
//! the only way to create one: a fresh, empty directory made just now by this
//! user. Adopting an existing directory is deliberately not possible.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::fsys::{self, Budget, Pinned};
use super::registry::{self, Entry, Registry};

pub const RECORD_FILE: &str = ".rsi-scratch-record";
const MAGIC: &str = "rsi-scratch-record v2";
const RECORD_LIMIT: usize = 1024;
/// A directory is "fresh" when it was born within this many seconds.
const FRESH_WINDOW_SECS: u64 = 120;

/// What the in-tree record says; verified against the registry before use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RecordRef {
    pub nonce: String,
    pub kind: String,
}

pub(super) fn record_text(nonce: &str, kind: &str) -> String {
    format!("{MAGIC}\nnonce={nonce}\nkind={kind}\n")
}

fn parse(text: &str) -> Option<RecordRef> {
    let mut lines = text.lines();
    if lines.next()? != MAGIC {
        return None;
    }
    let (mut nonce, mut kind) = (None, None);
    for line in lines {
        let (key, value) = line.split_once('=')?;
        match key {
            "nonce" => nonce = Some(value.to_string()),
            "kind" => kind = Some(value.to_string()),
            _ => {}
        }
    }
    Some(RecordRef {
        nonce: nonce.filter(|n| registry::valid_nonce(n))?,
        kind: kind?,
    })
}

/// The directory's record, or `None` when it has none or it is malformed.
pub(super) fn read_record(dir: &Pinned) -> Option<RecordRef> {
    let text = fsys::read_small_file(dir, RECORD_FILE, RECORD_LIMIT)
        .ok()
        .flatten()?;
    parse(&text)
}

/// Open `dir` itself without following a symlink at its last component.
pub(super) fn pin_dir(dir: &Path) -> io::Result<Pinned> {
    let parent = dir
        .parent()
        .ok_or_else(|| io::Error::other("scratch directory has no parent"))?;
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::other("non-utf8 scratch name"))?;
    let parent = fsys::pin(open_parent(parent)?)?;
    fsys::pin_child(&parent, name)
}

fn open_parent(parent: &Path) -> io::Result<std::os::fd::OwnedFd> {
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::Mode;
    use std::os::fd::FromRawFd;
    let raw = openat(
        None,
        parent,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| io::Error::from_raw_os_error(e as i32))?;
    // SAFETY: `raw` is a freshly opened descriptor owned by nobody else.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) })
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Allocate: register the fresh, empty directory `dir` of `kind` in the
/// registry and write its record. Refuses anything that is not just now
/// created by this user (a populated, old or foreign directory), and any
/// filesystem that keeps no birth time.
pub(super) fn allocate(
    registry_path: &Path,
    dir: &Path,
    kind: super::RootKind,
    source: Option<&Path>,
) -> io::Result<(Pinned, Entry)> {
    let uid = nix::unistd::geteuid().as_raw();
    let pinned = pin_dir(dir)?;
    let parent = dir
        .parent()
        .ok_or_else(|| io::Error::other("scratch directory has no parent"))?;
    let parent_ident = fsys::pin(open_parent(parent)?)?.ident;
    ensure_fresh(&pinned, uid)?;
    let btime = pinned
        .btime
        .ok_or_else(|| io::Error::other("filesystem keeps no birth time"))?;
    let registry = Registry::open_or_create(registry_path, uid)?;
    let entry = Entry {
        nonce: registry::new_nonce(),
        kind: kind.tag().to_string(),
        uid,
        ident: pinned.ident,
        btime,
        parent: parent_ident,
        created_unix: unix_now(),
        path: dir.display().to_string(),
        source: source.map(|s| s.display().to_string()),
        clone: None,
    };
    registry.put_new(&entry)?;
    if let Err(error) = fsys::create_file(
        &pinned,
        RECORD_FILE,
        record_text(&entry.nonce, &entry.kind).as_bytes(),
    ) {
        registry.remove_entry(&entry.nonce);
        return Err(error);
    }
    Ok((pinned, entry))
}

/// `dir` was created by this user within the last few minutes and is empty.
pub(super) fn ensure_fresh(pinned: &Pinned, uid: u32) -> io::Result<()> {
    let btime = pinned
        .btime
        .ok_or_else(|| io::Error::other("filesystem keeps no birth time"))?;
    let now = i128::from(unix_now()) * 1_000_000_000;
    let window = i128::from(FRESH_WINDOW_SECS) * 1_000_000_000;
    if pinned.uid != uid || now.saturating_sub(btime) > window || btime > now + 5_000_000_000 {
        return Err(io::Error::other("not a freshly created directory of ours"));
    }
    let mut budget = Budget::new(
        std::time::Instant::now() + std::time::Duration::from_secs(5),
        1024,
    );
    if !fsys::entry_names(pinned.raw(), false, &mut budget)?.is_empty() {
        return Err(io::Error::other("directory is not empty"));
    }
    Ok(())
}

/// Create `parent/name` (mode 0700) and register it in the default registry.
///
/// # Errors
/// The directory exists, cannot be created or registered, or the filesystem
/// keeps no birth time.
pub fn create_scratch_dir(parent: &Path, name: &str, kind: super::RootKind) -> io::Result<PathBuf> {
    create_scratch_dir_in(&registry::default_registry_path(), parent, name, kind)
}

/// [`create_scratch_dir`] against an explicit registry directory.
///
/// # Errors
/// As [`create_scratch_dir`].
pub fn create_scratch_dir_in(
    registry_path: &Path,
    parent: &Path,
    name: &str,
    kind: super::RootKind,
) -> io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = parent.join(name);
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    if let Err(error) = allocate(registry_path, &dir, kind, None) {
        let _ = std::fs::remove_dir(&dir);
        return Err(error);
    }
    Ok(dir)
}

/// The allocation an adoption would register for the legacy directory `pinned`
/// (a child of `root`), built only from what the open descriptors show. `None`
/// when the filesystem keeps no birth time. Nothing is persisted.
pub(super) fn legacy_entry(
    pinned: &Pinned,
    root: &Pinned,
    kind: super::RootKind,
    path: &Path,
    uid: u32,
) -> Option<Entry> {
    Some(Entry {
        nonce: registry::new_nonce(),
        kind: kind.tag().to_string(),
        uid,
        ident: pinned.ident,
        btime: pinned.btime?,
        parent: root.ident,
        created_unix: unix_now(),
        path: path.display().to_string(),
        source: None,
        clone: None,
    })
}

/// Register `entry` and write the directory's record, so a later reclaim pass
/// can bind it (#1147). Deletes nothing.
///
/// Writing the record bumps the directory's mtime, which the reclaim pass reads
/// as "last written". The timestamps are put back to their pre-record values so
/// adoption does not restart the age clock, but only on proof that nothing else
/// changed: afterwards the whole tree's manifest must equal `before` (the
/// manifest of the final proof) plus exactly the record. If anything else
/// appeared, vanished or changed (for example an old-mtime file renamed in
/// while the record was written), or the walk cannot be completed, the
/// directory is marked written just now, so it waits out the full minimum age
/// again. `between` runs after the record exists, before the timestamps are
/// restored (a test seam for exactly that race).
pub(super) fn adopt(
    registry: &Registry,
    pinned: &Pinned,
    entry: &Entry,
    before: &[u64],
    budget: &mut Budget,
    between: impl FnOnce(),
) -> io::Result<()> {
    let stat_before = fsys::fstat_fd(pinned.raw())?;
    registry.put_new(entry)?;
    if let Err(error) = fsys::create_file(
        pinned,
        RECORD_FILE,
        record_text(&entry.nonce, &entry.kind).as_bytes(),
    ) {
        registry.remove_entry(&entry.nonce);
        return Err(error);
    }
    between();
    fsys::restore_times(pinned, RECORD_FILE, &stat_before);
    if !only_the_record_was_added(pinned, before, budget) {
        fsys::touch_now(pinned);
    }
    Ok(())
}

/// Whether the tree now is exactly `before` plus the record.
fn only_the_record_was_added(pinned: &Pinned, before: &[u64], budget: &mut Budget) -> bool {
    let Ok(stat) = fsys::stat_at(pinned.raw(), RECORD_FILE) else {
        return false;
    };
    let record_hash = fsys::entry_hash(Path::new(RECORD_FILE), &stat);
    let Some(after) = fsys::census(pinned, budget) else {
        return false;
    };
    after.manifest.len() == before.len() + 1
        && fsys::manifest_contains(&after.manifest, record_hash)
        && before
            .iter()
            .all(|hash| fsys::manifest_contains(&after.manifest, *hash))
}
