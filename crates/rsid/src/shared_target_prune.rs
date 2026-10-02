//! Bounded prune of stale `debug/incremental` state in the shared cargo target
//! (#1063).
//!
//! The hub builds every worker, shard, lander and sweep job against one shared
//! target. Incremental sessions written by one-off builds (per-shard feature
//! sets, per-worktree paths) are never reused, and left alone they grew to
//! 63 GB. The sandbox build-cache reclaim (#1051) only covers terminal
//! sandboxes' own targets, so this pass owns the shared one.
//!
//! Safety: cargo holds an exclusive `flock` on `debug/.cargo-lock` for the
//! duration of a build. The prune takes the same lock without blocking, so a
//! running build makes it skip, and no build can start while entries are being
//! removed. Only top-level entries whose newest mtime is older than the
//! minimum age are removed, oldest first, within a fixed entry and time budget.

use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Incremental entries younger than this may belong to a build a developer is
/// about to repeat, so they are kept.
pub const MIN_AGE: Duration = Duration::from_secs(30 * 60);
/// Fixed safety ceilings: a pass frees the oldest entries and yields; the next
/// pressure pass continues.
pub const MAX_ENTRIES_PER_PASS: usize = 256;
pub const MAX_PASS_DURATION: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug)]
pub struct PruneConfig {
    pub min_age: Duration,
    pub max_entries: usize,
    pub max_duration: Duration,
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self {
            min_age: MIN_AGE,
            max_entries: MAX_ENTRIES_PER_PASS,
            max_duration: MAX_PASS_DURATION,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PruneOutcome {
    /// The target has no `debug/incremental` directory.
    NothingToPrune,
    /// A cargo build holds `debug/.cargo-lock`; nothing was touched.
    LockHeld,
    /// The lock file could not be opened or locked.
    LockUnavailable,
    /// The pass ran (possibly removing nothing).
    Pruned,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PruneReport {
    pub target_dir: PathBuf,
    pub outcome: PruneOutcome,
    pub considered: usize,
    pub kept_recent: usize,
    pub pruned: usize,
    pub pruned_bytes: u64,
    pub failed: usize,
    /// The entry or time budget stopped the pass before every stale entry
    /// was removed.
    pub budget_exhausted: bool,
}

impl PruneReport {
    fn new(target_dir: &Path, outcome: PruneOutcome) -> Self {
        Self {
            target_dir: target_dir.to_path_buf(),
            outcome,
            considered: 0,
            kept_recent: 0,
            pruned: 0,
            pruned_bytes: 0,
            failed: 0,
            budget_exhausted: false,
        }
    }
}

/// The shared cargo target: `RSI_SHARED_TARGET_DIR`, then `CARGO_TARGET_DIR`,
/// then `~/.cargo/shared-target`. `None` when none can be resolved.
pub fn shared_target_dir() -> Option<PathBuf> {
    for key in ["RSI_SHARED_TARGET_DIR", "CARGO_TARGET_DIR"] {
        if let Some(dir) = std::env::var_os(key).filter(|value| !value.is_empty()) {
            return Some(PathBuf::from(dir));
        }
    }
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(|home| PathBuf::from(home).join(".cargo/shared-target"))
}

/// Non-blocking exclusive flock; the lock is released when the file drops.
fn try_lock_exclusive(file: &File) -> io::Result<bool> {
    // SAFETY: flock on a descriptor owned by `file` for the whole call.
    let rc = unsafe { nix::libc::flock(file.as_raw_fd(), nix::libc::LOCK_EX | nix::libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        Ok(false)
    } else {
        Err(error)
    }
}

/// Newest mtime of the entry directory and its immediate children (the
/// `s-*` session directories), so a session touched recently keeps its
/// crate directory.
fn newest_mtime(path: &Path, meta: &fs::Metadata) -> SystemTime {
    let mut newest = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    if let Ok(children) = fs::read_dir(path) {
        for child in children.flatten() {
            if let Ok(modified) = child.metadata().and_then(|meta| meta.modified()) {
                newest = newest.max(modified);
            }
        }
    }
    newest
}

fn allocated_bytes(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = fs::symlink_metadata(path) else {
        return 0;
    };
    let mut total = meta.blocks().saturating_mul(512);
    if meta.is_dir() {
        if let Ok(children) = fs::read_dir(path) {
            for child in children.flatten() {
                total = total.saturating_add(allocated_bytes(&child.path()));
            }
        }
    }
    total
}

/// Prune stale entries from `<target_dir>/debug/incremental`.
pub fn prune_stale_incremental(
    target_dir: &Path,
    now: SystemTime,
    config: PruneConfig,
) -> PruneReport {
    let debug = target_dir.join("debug");
    let incremental = debug.join("incremental");
    if !incremental.is_dir() {
        return PruneReport::new(target_dir, PruneOutcome::NothingToPrune);
    }
    let lock = match File::open(debug.join(".cargo-lock")) {
        Ok(file) => file,
        Err(_) => return PruneReport::new(target_dir, PruneOutcome::LockUnavailable),
    };
    match try_lock_exclusive(&lock) {
        Ok(true) => {}
        Ok(false) => return PruneReport::new(target_dir, PruneOutcome::LockHeld),
        Err(_) => return PruneReport::new(target_dir, PruneOutcome::LockUnavailable),
    }

    let mut report = PruneReport::new(target_dir, PruneOutcome::Pruned);
    let started = Instant::now();
    let mut stale: Vec<(SystemTime, PathBuf)> = Vec::new();
    if let Ok(entries) = fs::read_dir(&incremental) {
        for entry in entries.flatten() {
            let path = entry.path();
            // Never follow a symlink out of the target.
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.is_dir() {
                continue;
            }
            report.considered += 1;
            let newest = newest_mtime(&path, &meta);
            let age = now.duration_since(newest).unwrap_or(Duration::ZERO);
            if age < config.min_age {
                report.kept_recent += 1;
            } else {
                stale.push((newest, path));
            }
        }
    }
    stale.sort();
    for (index, (_, path)) in stale.iter().enumerate() {
        if index >= config.max_entries || started.elapsed() >= config.max_duration {
            report.budget_exhausted = true;
            break;
        }
        let bytes = allocated_bytes(path);
        match fs::remove_dir_all(path) {
            Ok(()) => {
                report.pruned += 1;
                report.pruned_bytes = report.pruned_bytes.saturating_add(bytes);
            }
            Err(_) => report.failed += 1,
        }
    }
    drop(lock);
    report
}

/// Daemon entry point: prune the resolved shared target and log the report
/// like the sandbox build-cache passes.
pub fn run_and_log(trigger: &'static str) {
    let Some(target_dir) = shared_target_dir() else {
        return;
    };
    let report = prune_stale_incremental(&target_dir, SystemTime::now(), PruneConfig::default());
    if report.outcome == PruneOutcome::NothingToPrune {
        return;
    }
    tracing::info!(
        trigger,
        target_dir = %report.target_dir.display(),
        outcome = ?report.outcome,
        considered = report.considered,
        kept_recent = report.kept_recent,
        pruned = report.pruned,
        pruned_bytes = report.pruned_bytes,
        failed = report.failed,
        budget_exhausted = report.budget_exhausted,
        "Shared target incremental prune pass completed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn target_with_entries(names: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let debug = dir.path().join("debug");
        fs::create_dir_all(debug.join("incremental")).unwrap();
        fs::write(debug.join(".cargo-lock"), b"").unwrap();
        for name in names {
            let entry = debug.join("incremental").join(name);
            fs::create_dir_all(entry.join("s-abc")).unwrap();
            fs::write(entry.join("s-abc/dep-graph.bin"), vec![1u8; 4096]).unwrap();
        }
        dir
    }

    fn later(by: Duration) -> SystemTime {
        SystemTime::now() + by
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn prunes_entries_older_than_min_age_and_keeps_recent() {
        let dir = target_with_entries(&["old-1", "old-2"]);
        // Advance the clock so both entries are stale, then add a fresh one.
        let now = later(Duration::from_secs(3 * 3600));
        let fresh = dir.path().join("debug/incremental/fresh-3");
        fs::create_dir_all(fresh.join("s-new")).unwrap();
        fs::write(fresh.join("s-new/x"), b"y").unwrap();
        let file = File::open(&fresh).unwrap();
        file.set_modified(now).unwrap();
        File::open(fresh.join("s-new"))
            .unwrap()
            .set_modified(now)
            .unwrap();

        let report = prune_stale_incremental(dir.path(), now, PruneConfig::default());
        assert_eq!(report.outcome, PruneOutcome::Pruned);
        assert_eq!(
            (report.considered, report.pruned, report.kept_recent),
            (3, 2, 1)
        );
        assert!(report.pruned_bytes >= 8192, "{report:?}");
        let inc = dir.path().join("debug/incremental");
        assert!(!inc.join("old-1").exists() && !inc.join("old-2").exists());
        assert!(inc.join("fresh-3").exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn nothing_is_pruned_when_every_entry_is_recent() {
        let dir = target_with_entries(&["a", "b"]);
        let report = prune_stale_incremental(dir.path(), SystemTime::now(), PruneConfig::default());
        assert_eq!(
            (report.considered, report.kept_recent, report.pruned),
            (2, 2, 0)
        );
        assert!(dir.path().join("debug/incremental/a").exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn held_cargo_lock_skips_the_prune() {
        let dir = target_with_entries(&["old-1"]);
        let holder = File::open(dir.path().join("debug/.cargo-lock")).unwrap();
        assert!(try_lock_exclusive(&holder).unwrap());
        let now = later(Duration::from_secs(3 * 3600));
        let report = prune_stale_incremental(dir.path(), now, PruneConfig::default());
        assert_eq!(report.outcome, PruneOutcome::LockHeld);
        assert_eq!(report.pruned, 0);
        assert!(dir.path().join("debug/incremental/old-1").exists());
        drop(holder);
        let report = prune_stale_incremental(dir.path(), now, PruneConfig::default());
        assert_eq!((report.outcome, report.pruned), (PruneOutcome::Pruned, 1));
        assert!(!dir.path().join("debug/incremental/old-1").exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn entry_budget_bounds_the_pass_and_removes_oldest_first() {
        let dir = target_with_entries(&["e1", "e2", "e3"]);
        let inc = dir.path().join("debug/incremental");
        let base = SystemTime::now();
        for (index, name) in ["e1", "e2", "e3"].iter().enumerate() {
            let when = base - Duration::from_secs(10_000 - index as u64 * 1_000);
            File::open(inc.join(name).join("s-abc"))
                .unwrap()
                .set_modified(when)
                .unwrap();
            File::open(inc.join(name))
                .unwrap()
                .set_modified(when)
                .unwrap();
        }
        let config = PruneConfig {
            max_entries: 2,
            ..PruneConfig::default()
        };
        let report = prune_stale_incremental(dir.path(), base, config);
        assert_eq!((report.pruned, report.budget_exhausted), (2, true));
        assert!(!inc.join("e1").exists() && !inc.join("e2").exists());
        assert!(inc.join("e3").exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn missing_incremental_dir_is_nothing_to_prune() {
        let dir = tempfile::tempdir().unwrap();
        let report = prune_stale_incremental(dir.path(), SystemTime::now(), PruneConfig::default());
        assert_eq!(report.outcome, PruneOutcome::NothingToPrune);
    }
}
