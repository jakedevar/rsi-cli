//! Root-confined project-file writes (hub #3103): save, add/mkdir,
//! rename/move, copy, delete-to-trash and undo, for the desktop explorer and
//! editor. The operator-only RPCs in `rpc/project_files.rs` resolve the root
//! exactly as the read RPCs do (#48, hub #3033) and call into this module.
//!
//! Handles, not pathnames (hub #3143-#3151, ruling 2): every operation opens
//! the canonical root as a directory handle and resolves a request path
//! beneath it with `cap-std` (an in-root symlinked directory is followed, any
//! escape, an absolute symlink included, is refused). Every mutation is then
//! one call on a parent handle and one plain name (`write/fsx.rs`): the named
//! entry is never followed, so a directory swapped for a symlink after the
//! check leaves the operation on the object it checked. `..`, an absolute
//! path and the root itself are refused lexically first. A save follows the
//! file's own symlink only to a regular file inside the root.
//! Root acquisition uses the shared `AssetDir` no-follow ancestor walk.
//! Configured trash links are resolved once before acquisition; missing
//! directories, batches, origin sidecars and cleanup use pinned parents too.
//!
//! - Nothing is ever replaced except the file a save re-checked. A move,
//!   copy, restore and created save publish with a no-replace rename
//!   (`renameat2 RENAME_NOREPLACE`, `renameatx_np RENAME_EXCL`); a
//!   destination created concurrently is `project_file_exists` (an undo:
//!   `project_file_undo_conflict`) and both sides keep their bytes. A
//!   filesystem without the primitive is `project_file_unsupported_platform`.
//! - A copy is built under a fresh private name beside the destination and
//!   published by that rename; a failed copy removes only its private copy.
//! - A save checks `expected_version`, writes and fsyncs a temporary file
//!   beside the target and, immediately before renaming it over the target,
//!   re-checks that the target is the object it checked and untouched (same
//!   inode, size, mtime and ctime), else `project_file_stale` (unless
//!   `force`). The answered version is computed from the bytes and metadata
//!   of the published inode, never from a reopen. Residual window: an
//!   in-place write by another process between that re-check and the rename
//!   (two system calls) is replaced.
//! - Undo re-validates what it acts on: the object a move or trash recorded
//!   (device, inode, size, mtime), every entry an add made, or the whole
//!   copied tree (a digest of every entry's identity, size and mtime), and
//!   refuses `project_file_undo_conflict` on any mismatch, keeping the
//!   operation for a retry. Undoing an add removes exactly the entries it
//!   made; undoing a copy moves the copy into the trash.
//! - Across filesystems a transfer is never an unlink of a live tree: the
//!   source is copied, the copy re-checked against the source (inode, size,
//!   mtime, ctime of every entry), and only then is the source moved into the
//!   trash (a move) or removed entry by entry where it is still exactly what
//!   was copied (a trash to another filesystem). An entry that changed or
//!   appeared is left at the source and the call answers
//!   `project_file_conflict`; every byte survives at the source or in trash.
//! - A FIFO, socket or device is refused from `lstat` before any open, and
//!   every open is `O_NONBLOCK`, so nothing blocks under the write lock.
//! - The daemon trash (`~/.rsi/project-file-trash/<batch>/<name>`, with a
//!   `<batch>.origin` sidecar naming the original path) is refused as an
//!   endpoint of every operation, a save's resolved target and an undo's
//!   endpoints included, by the identity of the trash and its ancestors.
//!
//! The undo log is in memory, per canonical root, at most [`UNDO_DEPTH`]
//! entries: a daemon restart forgets it, the trash keeps the data. Every
//! operation runs under one lock. Unix only: native Windows is refused
//! (#1229).
//!
//! Trash retention (local #120, hub #3103): the trash follows the TUI
//! explorer's rule through the one shared routine
//! [`rsi_common::trash_prune`]: a batch is removed 7 days after the time its
//! name records, with its sidecar, through handles that never follow a
//! symlink and only inside the trash root. A pass runs once at daemon start
//! (a blocking task, [`prune_project_file_trash_at_start`]) and after a
//! trash write when the last pass is at least [`PRUNE_INTERVAL`] old, always
//! under the write lock, so it never races an operation or an undo. Undo of
//! a pruned batch is refused [`NO_LONGER_IN_TRASH`] and dropped from the log.

// Off unix only the refusing stubs below are compiled.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

use super::{INVALID_REQUEST, OUTSIDE_ROOT, refuse};
use crate::error::Result;
use rsi_common::project_files::MAX_VERSIONED_FILE_BYTES;
#[cfg(not(unix))]
use rsi_common::project_files::{
    ProjectEntryKindV1, ProjectFileOperationResponseV1, UndoProjectFileOperationResponseV1,
    WriteProjectFileResponseV1,
};
use rsi_common::trash_prune::{PruneReport, TRASH_MAX_AGE};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Seek};
use std::path::{Component, Path, PathBuf};
use std::sync::{LazyLock, Mutex};

#[cfg(unix)]
mod fsx;
#[cfg(unix)]
mod ops;
#[cfg(unix)]
use ops::UndoOp;
#[cfg(not(unix))]
type UndoOp = std::convert::Infallible;

pub(crate) const STALE: &str = "project_file_stale";
pub(crate) const EXISTS: &str = "project_file_exists";
pub(crate) const TOO_LARGE: &str = "project_file_too_large";
pub(crate) const NOTHING_TO_UNDO: &str = "project_file_nothing_to_undo";
pub(crate) const UNDO_CONFLICT: &str = "project_file_undo_conflict";
/// The tree changed while the operation ran; nothing was lost (hub #3148).
pub(crate) const CONFLICT: &str = "project_file_conflict";
pub(crate) const IO_FAILED: &str = "project_file_io_failed";
pub(crate) const UNSUPPORTED: &str = "project_file_unsupported_platform";
/// Undo of a trashed entry whose batch was pruned (or removed by hand).
pub(crate) const NO_LONGER_IN_TRASH: &str = "project_file_no_longer_in_trash";
/// Least time between two prune passes started by trash writes.
pub(crate) const PRUNE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// The version token of a file: `v1:<size>:<mtime unix nanos>:<sha256 hex>`.
pub(crate) fn version_token(metadata: &fs::Metadata, digest: &[u8]) -> String {
    let mtime = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos());
    format!("v1:{}:{mtime}:{}", metadata.len(), hex::encode(digest))
}

/// The version token of an open regular file, or `None` above
/// [`MAX_VERSIONED_FILE_BYTES`] or when the file changed while it was hashed.
/// The file position is rewound.
pub(crate) fn file_version(file: &mut fs::File) -> io::Result<Option<String>> {
    let metadata = file.metadata()?;
    let size = metadata.len();
    if size > MAX_VERSIONED_FILE_BYTES {
        return Ok(None);
    }
    file.rewind()?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 << 10];
    let mut reader = (&mut *file).take(MAX_VERSIONED_FILE_BYTES + 1);
    let mut read_total = 0u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        read_total += read as u64;
        hasher.update(&buffer[..read]);
    }
    file.rewind()?;
    // hub #3146: a token is only ever the hash of the bytes its size and
    // mtime describe.
    if read_total != size || !same_snapshot(&metadata, &file.metadata()?) {
        return Ok(None);
    }
    Ok(Some(version_token(&metadata, &hasher.finalize())))
}

/// Whether two stats of one open file describe the same content.
pub(crate) fn same_snapshot(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        before.len() == after.len()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
            && before.ino() == after.ino()
    }
    #[cfg(not(unix))]
    {
        before.len() == after.len() && before.modified().ok() == after.modified().ok()
    }
}

/// Points a test can hook to act as a concurrent writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hook {
    /// The canonical project root was checked; acquire its handle next.
    BeforeRootOpen,
    /// The configured trash location was resolved; pin it next.
    TrashResolved,
    /// A fresh batch was created; acquire its no-follow handle next.
    TrashBatchCreated,
    /// An undo is about to acquire the recorded batch through no-follow handles.
    BeforeTrashOpen,
    /// An undo completed its transfer; clean up the pinned batch next.
    BeforeTrashPrune,
    /// Every check is done; the first mutation is next.
    Checked,
    /// A save's temporary file is written; its re-check and rename are next.
    BeforePublish,
    /// A save is published; its answer is next.
    AfterPublish,
    /// One entry of a tree copy is done.
    CopiedEntry,
    /// A cross-filesystem copy is done; the source check is next.
    CrossDeviceCopied,
}

#[cfg(test)]
type HookFn = Box<dyn FnMut(Hook) + Send>;

/// The daemon-owned trash and the per-root undo log.
pub(crate) struct FileOps {
    trash_dir: PathBuf,
    undo: Mutex<HashMap<PathBuf, VecDeque<UndoOp>>>,
    /// When the last prune pass ran; `None` before the first.
    last_prune: Mutex<Option<std::time::Instant>>,
    #[cfg(test)]
    hook: Mutex<Option<HookFn>>,
    /// Tests: every transfer rename fails as if it crossed filesystems.
    #[cfg(test)]
    cross_device: std::sync::atomic::AtomicBool,
}

#[cfg(not(test))]
fn default_trash_dir() -> PathBuf {
    rsi_common::identity::data_path("project-file-trash", "project-file-trash")
}

/// Tests never touch the operator's `~/.rsi`.
#[cfg(test)]
fn default_trash_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "rsid-project-file-trash-test-{}",
        std::process::id()
    ))
}

static GLOBAL: LazyLock<FileOps> = LazyLock::new(|| FileOps::new(default_trash_dir()));

fn log_prune(trigger: &str, result: std::io::Result<PruneReport>) {
    match result {
        Ok(report) => tracing::info!(
            trigger,
            batches_removed = report.batches_removed,
            sidecars_removed = report.sidecars_removed,
            kept = report.kept,
            failed = report.failed,
            truncated = report.truncated,
            "Project-file trash prune pass finished"
        ),
        Err(error) => tracing::warn!(
            trigger,
            error = %error,
            "Project-file trash prune pass refused"
        ),
    }
}

/// Daemon start: one bounded prune pass over the daemon's project-file trash
/// (run it on a blocking thread).
pub fn prune_project_file_trash_at_start() {
    log_prune("daemon_start", FileOps::global().prune_trash());
}

/// Refuse every write where rsid itself is unsupported (native Windows, #1229).
pub(crate) fn ensure_supported() -> Result<()> {
    if cfg!(not(unix)) {
        return Err(refuse(
            UNSUPPORTED,
            "project-file writes are not supported on this platform (native Windows)",
        ));
    }
    Ok(())
}

/// Lexically checked, `.`-free components of a request path; refuses an
/// absolute path, `..`, a NUL and the root itself.
fn components(requested: &str) -> Result<Vec<OsString>> {
    if requested.contains('\0') {
        return Err(refuse(INVALID_REQUEST, "a path cannot contain a NUL byte"));
    }
    let mut parts = Vec::new();
    for component in Path::new(requested).components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                return Err(refuse(
                    OUTSIDE_ROOT,
                    format!("'{requested}' is absolute; name a path relative to the root"),
                ));
            }
            Component::ParentDir => {
                return Err(refuse(
                    OUTSIDE_ROOT,
                    format!("'{requested}' has a '..' component"),
                ));
            }
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_os_string()),
        }
    }
    if parts.is_empty() {
        return Err(refuse(
            INVALID_REQUEST,
            "name an entry under the root, not the root itself",
        ));
    }
    Ok(parts)
}

impl FileOps {
    pub(crate) fn new(trash_dir: PathBuf) -> Self {
        Self {
            trash_dir,
            undo: Mutex::new(HashMap::new()),
            last_prune: Mutex::new(None),
            #[cfg(test)]
            hook: Mutex::new(None),
            #[cfg(test)]
            cross_device: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The daemon's instance: trash under `~/.rsi/project-file-trash`.
    pub(crate) fn global() -> &'static FileOps {
        &GLOBAL
    }

    /// One prune pass over the trash under the write lock (the TUI's 7-day
    /// rule, [`rsi_common::trash_prune`]).
    pub(crate) fn prune_trash(&self) -> std::io::Result<PruneReport> {
        let _log = self.log();
        self.prune_locked(chrono::Utc::now())
    }

    /// A prune pass; the caller holds the write lock.
    fn prune_locked(&self, now: chrono::DateTime<chrono::Utc>) -> std::io::Result<PruneReport> {
        *self
            .last_prune
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(std::time::Instant::now());
        rsi_common::trash_prune::prune_trash(&self.trash_dir, TRASH_MAX_AGE, now)
    }

    /// After a trash write: a pass when none ran in the last
    /// [`PRUNE_INTERVAL`]; the caller holds the write lock. Best effort: a
    /// failed pass is logged and never fails the write.
    fn prune_if_due(&self) {
        let due = self
            .last_prune
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_none_or(|at| at.elapsed() >= PRUNE_INTERVAL);
        if due {
            log_prune("trash_write", self.prune_locked(chrono::Utc::now()));
        }
    }

    fn log(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, VecDeque<UndoOp>>> {
        self.undo
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Run the test hook at `point` (a no-op outside tests).
    fn at(&self, point: Hook) {
        #[cfg(test)]
        if let Some(hook) = self
            .hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_mut()
        {
            hook(point);
        }
        let _ = point;
    }
}

/// Where rsid is unsupported every write refuses before touching anything.
#[cfg(not(unix))]
impl FileOps {
    pub(crate) fn save(
        &self,
        _root: &Path,
        _requested: &str,
        _content: &str,
        _expected_version: Option<&str>,
        _force: bool,
    ) -> Result<WriteProjectFileResponseV1> {
        ensure_supported().and_then(|()| unreachable!("unix only"))
    }

    pub(crate) fn create(
        &self,
        _root: &Path,
        _requested: &str,
        _kind: ProjectEntryKindV1,
    ) -> Result<ProjectFileOperationResponseV1> {
        ensure_supported().and_then(|()| unreachable!("unix only"))
    }

    pub(crate) fn rename(
        &self,
        _root: &Path,
        _from: &str,
        _to: &str,
    ) -> Result<ProjectFileOperationResponseV1> {
        ensure_supported().and_then(|()| unreachable!("unix only"))
    }

    pub(crate) fn copy(
        &self,
        _root: &Path,
        _from: &str,
        _to: &str,
    ) -> Result<ProjectFileOperationResponseV1> {
        ensure_supported().and_then(|()| unreachable!("unix only"))
    }

    pub(crate) fn trash(
        &self,
        _root: &Path,
        _requested: &str,
    ) -> Result<ProjectFileOperationResponseV1> {
        ensure_supported().and_then(|()| unreachable!("unix only"))
    }

    pub(crate) fn undo(&self, _root: &Path) -> Result<UndoProjectFileOperationResponseV1> {
        ensure_supported().and_then(|()| unreachable!("unix only"))
    }
}

#[cfg(all(test, unix))]
mod tests;

#[cfg(all(test, not(unix)))]
mod unsupported_tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-02"))]
    #[test]
    fn all_writes_explicitly_refuse_without_a_portable_no_follow_implementation() {
        use super::*;
        let root = Path::new("unopened-project-root");
        let ops = FileOps::new(PathBuf::from("unopened-trash"));
        let results = [
            ops.save(root, "file", "bytes", None, true).map(drop),
            ops.create(root, "file", ProjectEntryKindV1::File).map(drop),
            ops.create(root, "dir", ProjectEntryKindV1::Dir).map(drop),
            ops.rename(root, "from", "to").map(drop),
            ops.copy(root, "from", "to").map(drop),
            ops.trash(root, "file").map(drop),
            ops.undo(root).map(drop),
        ];
        for result in results {
            assert!(result.unwrap_err().to_string().contains(UNSUPPORTED));
        }
    }
}
