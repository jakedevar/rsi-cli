//! The file-explorer trash rule both frontends share (hub #3103, local #120).
//!
//! The TUI explorer (`<data dir>/explorer-trash`) and the daemon's project-file
//! writes (`<data dir>/project-file-trash`, used by the desktop) keep a deleted
//! entry in a batch directory `<UTC %Y%m%dT%H%M%S>-<8 hex>/<name>` with a
//! `<batch>.origin` sidecar naming where it came from. A batch is kept
//! [`TRASH_MAX_AGE`] (7 days) after the time its name records, then
//! [`prune_trash`] removes it and its sidecar.
//!
//! Pruning acts only inside the trash root and never follows a symlink:
//! - the root is opened with `O_NOFOLLOW | O_DIRECTORY` (a symlinked trash
//!   root is not pruned at all);
//! - every top-level entry is `lstat`ed through the root handle; a symlink,
//!   a special file or a name that is not a batch or a sidecar is kept;
//! - an expired batch is removed one level at a time through handles opened
//!   `O_NOFOLLOW`, checked against the `lstat` identity, so a link inside a
//!   batch is unlinked as the link and its target is never touched.
//!
//! The recorded time is the batch name, not the mtime, so touching or
//! restoring into a batch never extends or shortens its life. A batch dated in
//! the future (clock skew) is kept. Every pass is bounded
//! ([`MAX_PRUNE_SCAN`] top-level names, [`MAX_PRUNE_REMOVALS`] removed
//! entries, [`MAX_PRUNE_DEPTH`] levels); what a pass does not finish waits for
//! the next pass.

use chrono::{DateTime, NaiveDateTime, Utc};
use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::time::Duration;

/// How long a trash batch is kept after its recorded time.
pub const TRASH_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Extension of the sidecar that names a batch's original path.
pub const ORIGIN_EXTENSION: &str = "origin";
/// Most top-level trash names one pass looks at.
pub const MAX_PRUNE_SCAN: usize = 10_000;
/// Most entries one pass removes.
pub const MAX_PRUNE_REMOVALS: usize = 200_000;
/// Deepest nesting one pass descends into a batch.
pub const MAX_PRUNE_DEPTH: usize = 256;

const STAMP_FORMAT: &str = "%Y%m%dT%H%M%S";
const STAMP_LEN: usize = 15;
const ID_LEN: usize = 8;

/// A fresh batch name recording `now`: `<UTC %Y%m%dT%H%M%S>-<8 hex>`.
#[must_use]
pub fn new_batch_name(now: DateTime<Utc>) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("{}-{}", now.format(STAMP_FORMAT), &id[..ID_LEN])
}

/// The time a batch name records, `None` for any other name.
#[must_use]
pub fn batch_recorded_at(name: &str) -> Option<DateTime<Utc>> {
    let (stamp, id) = name.split_once('-')?;
    if stamp.len() != STAMP_LEN || id.len() != ID_LEN || !id.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    NaiveDateTime::parse_from_str(stamp, STAMP_FORMAT)
        .ok()
        .map(|naive| naive.and_utc())
}

/// Whether a batch recorded at `recorded` is past `max_age` at `now`.
#[must_use]
pub fn is_expired(recorded: DateTime<Utc>, now: DateTime<Utc>, max_age: Duration) -> bool {
    now.signed_duration_since(recorded)
        .to_std()
        .is_ok_and(|age| age > max_age)
}

/// What a top-level trash name is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    Batch(DateTime<Utc>),
    Sidecar(DateTime<Utc>),
}

fn classify(name: &OsStr) -> Option<Entry> {
    let name = name.to_str()?;
    if let Some(stem) = name
        .strip_suffix(ORIGIN_EXTENSION)
        .and_then(|rest| rest.strip_suffix('.'))
    {
        return batch_recorded_at(stem).map(Entry::Sidecar);
    }
    batch_recorded_at(name).map(Entry::Batch)
}

/// What one prune pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Expired batch directories removed.
    pub batches_removed: usize,
    /// Expired `.origin` sidecars removed.
    pub sidecars_removed: usize,
    /// Top-level names kept: young, not a batch, a symlink or special file.
    pub kept: usize,
    /// Expired entries that could not be (fully) removed this pass.
    pub failed: usize,
    /// A bound stopped the pass early; the rest waits for the next pass.
    pub truncated: bool,
}

/// Remove the batches (and sidecars) under `trash_root` older than
/// `max_age` at `now`. A missing root is an empty pass; a root that is a
/// symlink or not a directory is refused.
///
/// # Errors
///
/// Opening or listing the trash root failed; a failure on one entry is
/// counted in [`PruneReport::failed`] instead.
pub fn prune_trash(
    trash_root: &Path,
    max_age: Duration,
    now: DateTime<Utc>,
) -> io::Result<PruneReport> {
    imp::prune(trash_root, &|name| match classify(name) {
        Some(Entry::Batch(at)) if is_expired(at, now, max_age) => Verdict::Batch,
        Some(Entry::Sidecar(at)) if is_expired(at, now, max_age) => Verdict::Sidecar,
        _ => Verdict::Keep,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Keep,
    Batch,
    Sidecar,
}

#[cfg(unix)]
mod imp {
    use super::{MAX_PRUNE_DEPTH, MAX_PRUNE_REMOVALS, MAX_PRUNE_SCAN, PruneReport, Verdict};
    use rustix::fd::OwnedFd;
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    struct Budget {
        removals: usize,
        truncated: bool,
    }

    /// Device and inode of a `stat`.
    #[allow(clippy::unnecessary_cast)]
    fn ident(st: &rustix::fs::Stat) -> (u64, u64) {
        (st.st_dev as u64, st.st_ino as u64)
    }

    fn open_dir_at(dir: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
        Ok(rustix::fs::openat(
            dir,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?)
    }

    /// The names in `dir` (no `.`/`..`), at most `limit + 1`.
    fn names(dir: &OwnedFd, limit: usize) -> io::Result<Vec<OsString>> {
        let mut out = Vec::new();
        for entry in rustix::fs::Dir::read_from(dir)? {
            let entry = entry?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes());
            if name == "." || name == ".." {
                continue;
            }
            out.push(name.to_os_string());
            if out.len() > limit {
                break;
            }
        }
        out.sort();
        Ok(out)
    }

    pub(super) fn prune(
        root: &Path,
        verdict: &dyn Fn(&OsStr) -> Verdict,
    ) -> io::Result<PruneReport> {
        let root = match rustix::fs::open(
            root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(PruneReport::default()),
            Err(error) => return Err(error.into()),
        };
        let mut report = PruneReport::default();
        let mut listed = names(&root, MAX_PRUNE_SCAN)?;
        if listed.len() > MAX_PRUNE_SCAN {
            listed.truncate(MAX_PRUNE_SCAN);
            report.truncated = true;
        }
        let mut budget = Budget {
            removals: MAX_PRUNE_REMOVALS,
            truncated: false,
        };
        for name in listed {
            let want = verdict(&name);
            let st = match rustix::fs::statat(&root, &*name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) => st,
                Err(rustix::io::Errno::NOENT) => continue,
                Err(_) => {
                    report.failed += 1;
                    continue;
                }
            };
            let kind = FileType::from_raw_mode(st.st_mode as _);
            match (want, kind) {
                (Verdict::Batch, FileType::Directory) => {
                    if budget.removals == 0 {
                        budget.truncated = true;
                        break;
                    }
                    match remove_tree(&root, &name, ident(&st), 0, &mut budget) {
                        Ok(true) => report.batches_removed += 1,
                        Ok(false) | Err(_) => report.failed += 1,
                    }
                }
                (Verdict::Sidecar, FileType::RegularFile) => {
                    if budget.removals == 0 {
                        budget.truncated = true;
                        break;
                    }
                    match rustix::fs::unlinkat(&root, &*name, AtFlags::empty()) {
                        Ok(()) => {
                            budget.removals -= 1;
                            report.sidecars_removed += 1;
                        }
                        Err(_) => report.failed += 1,
                    }
                }
                // A symlink, special file or young / foreign name stays.
                _ => report.kept += 1,
            }
        }
        report.truncated |= budget.truncated;
        Ok(report)
    }

    /// Remove `name` in `parent`, which was `lstat`ed as the directory `id`,
    /// one level at a time without following anything. `Ok(false)` when a
    /// bound or a swapped entry left part of it in place.
    fn remove_tree(
        parent: &OwnedFd,
        name: &OsStr,
        id: (u64, u64),
        depth: usize,
        budget: &mut Budget,
    ) -> io::Result<bool> {
        if depth >= MAX_PRUNE_DEPTH {
            return Ok(false);
        }
        let dir = open_dir_at(parent, name)?;
        let st = rustix::fs::fstat(&dir)?;
        if ident(&st) != id {
            // Swapped since the lstat: leave it.
            return Ok(false);
        }
        let mut complete = true;
        for child in names(&dir, budget.removals)? {
            if budget.removals == 0 {
                budget.truncated = true;
                return Ok(false);
            }
            let cst = match rustix::fs::statat(&dir, &*child, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(cst) => cst,
                Err(rustix::io::Errno::NOENT) => continue,
                Err(error) => return Err(error.into()),
            };
            if FileType::from_raw_mode(cst.st_mode as _) == FileType::Directory {
                complete &= remove_tree(&dir, &child, ident(&cst), depth + 1, budget)?;
            } else {
                // A file, a special file or a symlink: the entry itself.
                rustix::fs::unlinkat(&dir, &*child, AtFlags::empty())?;
                budget.removals -= 1;
            }
        }
        if !complete || budget.removals == 0 {
            budget.truncated |= budget.removals == 0;
            return Ok(false);
        }
        rustix::fs::unlinkat(parent, name, AtFlags::REMOVEDIR)?;
        budget.removals -= 1;
        Ok(true)
    }
}

#[cfg(not(unix))]
mod imp {
    //! Portable fallback: `symlink_metadata` never follows, and
    //! `std::fs::remove_dir_all` removes links as links on every platform.
    use super::{MAX_PRUNE_SCAN, PruneReport, Verdict};
    use std::ffi::OsStr;
    use std::fs;
    use std::io;
    use std::path::Path;

    pub(super) fn prune(
        root: &Path,
        verdict: &dyn Fn(&OsStr) -> Verdict,
    ) -> io::Result<PruneReport> {
        let meta = match fs::symlink_metadata(root) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(PruneReport::default());
            }
            Err(error) => return Err(error),
        };
        if !meta.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the trash root is not a directory",
            ));
        }
        let mut report = PruneReport::default();
        for (seen, entry) in fs::read_dir(root)?.enumerate() {
            if seen >= MAX_PRUNE_SCAN {
                report.truncated = true;
                break;
            }
            let Ok(entry) = entry else {
                report.failed += 1;
                continue;
            };
            let name = entry.file_name();
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                report.failed += 1;
                continue;
            };
            match verdict(&name) {
                Verdict::Batch if meta.is_dir() => match fs::remove_dir_all(entry.path()) {
                    Ok(()) => report.batches_removed += 1,
                    Err(_) => report.failed += 1,
                },
                Verdict::Sidecar if meta.is_file() => match fs::remove_file(entry.path()) {
                    Ok(()) => report.sidecars_removed += 1,
                    Err(_) => report.failed += 1,
                },
                _ => report.kept += 1,
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests;
