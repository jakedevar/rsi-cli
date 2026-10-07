//! Disk-pressure reclaim of the shared cargo `debug/` cache (#1607).
//!
//! The hub builds every worker, shard and lander against one shared cargo
//! target. Its `debug/` directory grew to 102 GB while idle for hours and the
//! sandbox target reclaim (#1575) never considered it. Under pressure the whole
//! `debug/` directory is now removed, under the same kinds of rules sandbox
//! targets follow:
//!
//! * **Idle age.** The newest write seen in `debug/` (its own mtime, its
//!   top-level entries, and the children of `.fingerprint`, `build` and
//!   `incremental`) must be older than [`MIN_IDLE`]. As with sandbox targets,
//!   pressure bypasses the longer target-cache TTL; without pressure nothing
//!   is removed and the TTL only labels a younger cache `Fresh`. A cache that
//!   cannot be walked within the entry budget is treated as unreadable and
//!   kept.
//! * **Live consumer.** A build holding `debug/.cargo-lock`, or a same-user
//!   process whose cwd, executable or open file lies in `debug/`, or a cargo
//!   tool whose `CARGO_TARGET_DIR` covers it, keeps the cache. Platforms
//!   without procfs refuse (the cache is kept), never guess.
//! * **Never `release/`.** Only `<target>/debug` is touched; `~/.local/bin`
//!   links into `release/` (#666).
//!
//! Removal first renames `debug/` to a sibling stage while the cargo lock is
//! held, so `debug/` is either whole or absent, never half-deleted. The stage
//! is then removed within a time budget; a later pass finishes any remainder.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use rsi_common::sandbox_storage::{
    SandboxSharedTargetReclaimOutcome, SandboxSharedTargetReclaimV1,
};

use crate::shared_target_prune::try_lock_exclusive;

/// A shared cache written within this window may belong to a build about to be
/// repeated; the configured TTL can raise but never lower it.
pub const MIN_IDLE: Duration = Duration::from_secs(30 * 60);
/// Directory entries one inspection or measurement may visit.
pub const MAX_SCAN_ENTRIES: u64 = 500_000;
/// Wall-clock ceiling for deleting staged state in one pass.
pub const MAX_REMOVE_DURATION: Duration = Duration::from_secs(30);
/// Sibling name prefix of a `debug/` directory staged for deletion.
pub const STAGE_PREFIX: &str = "debug.rsi-reclaim-";

type Outcome = SandboxSharedTargetReclaimOutcome;

#[derive(Clone, Copy, Debug)]
pub struct ReclaimParams {
    pub now: SystemTime,
    /// The target-cache TTL. Like sandbox targets, disk pressure bypasses it;
    /// without pressure it only decides whether the status calls the cache
    /// `Fresh` or merely `PressureInactive`. [`MIN_IDLE`] always applies.
    pub min_idle: Duration,
    /// Disk-pressure hysteresis is active and reclaim is enabled.
    pub pressure: bool,
    pub dry_run: bool,
    pub max_remove_duration: Duration,
    pub max_scan_entries: u64,
}

impl ReclaimParams {
    pub fn new(min_idle: Duration, pressure: bool, dry_run: bool) -> Self {
        Self {
            now: SystemTime::now(),
            min_idle,
            pressure,
            dry_run,
            max_remove_duration: MAX_REMOVE_DURATION,
            max_scan_entries: MAX_SCAN_ENTRIES,
        }
    }

    fn effective_min_idle(&self) -> Duration {
        if self.pressure {
            MIN_IDLE
        } else {
            self.min_idle.max(MIN_IDLE)
        }
    }
}

fn report(
    params: &ReclaimParams,
    outcome: Outcome,
    check: Option<String>,
    idle: Option<Duration>,
) -> SandboxSharedTargetReclaimV1 {
    SandboxSharedTargetReclaimV1 {
        outcome,
        check,
        idle_secs: idle.map(|idle| idle.as_secs()),
        min_idle_secs: params.effective_min_idle().as_secs(),
        measured_bytes: 0,
        measured_truncated: false,
        reclaimed_bytes: 0,
        staged_pending: false,
    }
}

/// Inspect, and under pressure reclaim, `<target_dir>/debug`.
pub fn reclaim_shared_debug(
    target_dir: &Path,
    params: &ReclaimParams,
) -> SandboxSharedTargetReclaimV1 {
    let deadline = Instant::now() + params.max_remove_duration;
    let (mut staged_bytes, mut staged_pending) = (0, false);
    if !params.dry_run {
        (staged_bytes, staged_pending) = finish_staged(target_dir, deadline);
    } else {
        staged_pending = has_staged(target_dir);
    }
    let mut result = inspect_and_reclaim(target_dir, params, deadline);
    result.reclaimed_bytes = result.reclaimed_bytes.saturating_add(staged_bytes);
    result.staged_pending |= staged_pending;
    result
}

fn inspect_and_reclaim(
    target_dir: &Path,
    params: &ReclaimParams,
    deadline: Instant,
) -> SandboxSharedTargetReclaimV1 {
    let debug = target_dir.join("debug");
    match fs::symlink_metadata(&debug) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return report(params, Outcome::Unsafe, Some("debug_symlink".into()), None);
        }
        Ok(meta) if !meta.is_dir() => {
            return report(
                params,
                Outcome::Unsafe,
                Some("debug_not_directory".into()),
                None,
            );
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return report(params, Outcome::Absent, None, None);
        }
        Err(_) => {
            return report(
                params,
                Outcome::Unreadable,
                Some("debug_unreadable".into()),
                None,
            );
        }
    }

    let newest = match newest_write(&debug, params.max_scan_entries) {
        Ok(newest) => newest,
        Err(check) => return report(params, Outcome::Unreadable, Some(check.into()), None),
    };
    let idle = params.now.duration_since(newest).unwrap_or(Duration::ZERO);
    if idle < params.effective_min_idle() {
        return report(
            params,
            Outcome::Fresh,
            Some("idle_below_ttl".into()),
            Some(idle),
        );
    }

    // Take the build lock first: a cargo that starts after this point waits
    // for it, so the consumer scan below cannot be raced by a new build.
    let lock = match File::open(debug.join(".cargo-lock")) {
        Ok(file) => match try_lock_exclusive(&file) {
            Ok(true) => Some(file),
            Ok(false) => {
                return report(
                    params,
                    Outcome::LockHeld,
                    Some("cargo_lock_held".into()),
                    Some(idle),
                );
            }
            Err(_) => {
                return report(
                    params,
                    Outcome::Unreadable,
                    Some("cargo_lock_unavailable".into()),
                    Some(idle),
                );
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => {
            return report(
                params,
                Outcome::Unreadable,
                Some("cargo_lock_unavailable".into()),
                Some(idle),
            );
        }
    };

    match find_live_consumer(&debug) {
        Ok(None) => {}
        Ok(Some(pid)) => {
            return report(
                params,
                Outcome::LiveConsumer,
                Some(format!("process_uses_shared_target:{pid}")),
                Some(idle),
            );
        }
        Err(ConsumerScanError::Unsupported) => {
            return report(
                params,
                Outcome::Unsupported,
                Some("process_scan_unsupported".into()),
                Some(idle),
            );
        }
        Err(ConsumerScanError::Unreadable(pid)) => {
            return report(
                params,
                Outcome::Unreadable,
                Some(format!("process_scan_unreadable:{pid}")),
                Some(idle),
            );
        }
    }

    if params.dry_run {
        let (bytes, truncated) = measure(&debug, params.max_scan_entries);
        let mut result = report(
            params,
            if params.pressure {
                Outcome::WouldReclaim
            } else {
                Outcome::PressureInactive
            },
            None,
            Some(idle),
        );
        result.measured_bytes = bytes;
        result.measured_truncated = truncated;
        return result;
    }
    if !params.pressure {
        return report(params, Outcome::PressureInactive, None, Some(idle));
    }

    // Atomic cut: `debug/` is whole or absent, never half-deleted.
    let stage = target_dir.join(format!(
        "{STAGE_PREFIX}{}",
        params
            .now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    ));
    if fs::rename(&debug, &stage).is_err() {
        return report(
            params,
            Outcome::Unreadable,
            Some("stage_rename_failed".into()),
            Some(idle),
        );
    }
    drop(lock);
    let (bytes, complete) = remove_tree_budgeted(&stage, deadline);
    let mut result = report(
        params,
        if complete {
            Outcome::Reclaimed
        } else {
            Outcome::Partial
        },
        None,
        Some(idle),
    );
    result.reclaimed_bytes = bytes;
    result.staged_pending = !complete;
    result
}

/// Newest mtime across `debug/` and its cheap-to-list structure. File contents
/// of `deps/` are not listed: a rewrite there replaces a hard link and moves
/// the directory's mtime.
fn newest_write(debug: &Path, budget: u64) -> Result<SystemTime, &'static str> {
    let mut seen = 0u64;
    let mut newest = fs::symlink_metadata(debug)
        .and_then(|meta| meta.modified())
        .map_err(|_| "debug_unreadable")?;
    let entries = fs::read_dir(debug).map_err(|_| "debug_unreadable")?;
    for entry in entries {
        let entry = entry.map_err(|_| "debug_unreadable")?;
        seen += 1;
        if seen > budget {
            return Err("scan_budget");
        }
        let meta = fs::symlink_metadata(entry.path()).map_err(|_| "debug_unreadable")?;
        newest = newest.max(meta.modified().map_err(|_| "debug_unreadable")?);
        let name = entry.file_name();
        let deep = matches!(
            name.to_str(),
            Some(".fingerprint") | Some("build") | Some("incremental")
        );
        if deep && meta.is_dir() {
            for child in fs::read_dir(entry.path()).map_err(|_| "debug_unreadable")? {
                let child = child.map_err(|_| "debug_unreadable")?;
                seen += 1;
                if seen > budget {
                    return Err("scan_budget");
                }
                let meta = fs::symlink_metadata(child.path()).map_err(|_| "debug_unreadable")?;
                newest = newest.max(meta.modified().map_err(|_| "debug_unreadable")?);
            }
        }
    }
    Ok(newest)
}

/// Allocated bytes under `path`, stopping after `budget` entries.
fn measure(path: &Path, budget: u64) -> (u64, bool) {
    fn walk(path: &Path, remaining: &mut u64, total: &mut u64) -> bool {
        use std::os::unix::fs::MetadataExt;
        let Ok(meta) = fs::symlink_metadata(path) else {
            return true;
        };
        *total = total.saturating_add(meta.blocks().saturating_mul(512));
        if !meta.is_dir() {
            return true;
        }
        let Ok(children) = fs::read_dir(path) else {
            return true;
        };
        for child in children.flatten() {
            if *remaining == 0 {
                return false;
            }
            *remaining -= 1;
            if !walk(&child.path(), remaining, total) {
                return false;
            }
        }
        true
    }
    let (mut remaining, mut total) = (budget, 0);
    let complete = walk(path, &mut remaining, &mut total);
    (total, !complete)
}

fn has_staged(target_dir: &Path) -> bool {
    fs::read_dir(target_dir).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(STAGE_PREFIX)
        })
    })
}

/// Finish removing stages left by earlier passes. Returns bytes freed and
/// whether any stage remains.
fn finish_staged(target_dir: &Path, deadline: Instant) -> (u64, bool) {
    let Ok(entries) = fs::read_dir(target_dir) else {
        return (0, false);
    };
    let (mut bytes, mut pending) = (0u64, false);
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(STAGE_PREFIX)
        {
            continue;
        }
        let (freed, complete) = remove_tree_budgeted(&entry.path(), deadline);
        bytes = bytes.saturating_add(freed);
        pending |= !complete;
    }
    (bytes, pending)
}

/// Remove `path` without following symlinks, summing allocated bytes of what
/// was unlinked. Returns `(bytes, complete)`; `complete` is false when the
/// deadline or an error stopped the removal, leaving the remainder in place.
fn remove_tree_budgeted(path: &Path, deadline: Instant) -> (u64, bool) {
    use std::os::unix::fs::MetadataExt;
    fn remove(path: &Path, deadline: Instant, bytes: &mut u64, depth: u32) -> bool {
        let Ok(meta) = fs::symlink_metadata(path) else {
            return true;
        };
        if !meta.is_dir() {
            let blocks = meta.blocks().saturating_mul(512);
            if fs::remove_file(path).is_ok() {
                *bytes = bytes.saturating_add(blocks);
                return true;
            }
            return false;
        }
        if depth > 64 {
            return false;
        }
        let Ok(children) = fs::read_dir(path) else {
            return false;
        };
        let mut complete = true;
        for child in children.flatten() {
            if Instant::now() >= deadline {
                return false;
            }
            complete &= remove(&child.path(), deadline, bytes, depth + 1);
        }
        complete && fs::remove_dir(path).is_ok()
    }
    let mut bytes = 0;
    let complete = remove(path, deadline, &mut bytes, 0);
    (bytes, complete)
}

#[derive(Debug, PartialEq, Eq)]
enum ConsumerScanError {
    Unsupported,
    Unreadable(u32),
}

#[cfg(not(target_os = "linux"))]
fn find_live_consumer(_debug: &Path) -> Result<Option<u32>, ConsumerScanError> {
    Err(ConsumerScanError::Unsupported)
}

#[cfg(target_os = "linux")]
fn find_live_consumer(debug: &Path) -> Result<Option<u32>, ConsumerScanError> {
    scan_processes(Path::new("/proc"), debug)
}

#[cfg(target_os = "linux")]
fn is_cargo_tool(comm: &str) -> bool {
    let comm = comm.trim();
    comm.starts_with("cargo")
        || matches!(
            comm,
            "rustc" | "rustdoc" | "clippy-driver" | "rustfmt" | "rust-analyzer"
        )
}

#[cfg(target_os = "linux")]
fn process_exited(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::NotFound)
        || error.raw_os_error() == Some(nix::libc::ESRCH)
}

/// Lexical `.`/`..` collapse so a relative `CARGO_TARGET_DIR` compares with the
/// absolute cache path without touching the filesystem.
#[cfg(target_os = "linux")]
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The first live same-user consumer of `debug`, found through `proc_root`.
#[cfg(target_os = "linux")]
fn scan_processes(proc_root: &Path, debug: &Path) -> Result<Option<u32>, ConsumerScanError> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let uid = nix::unistd::getuid().as_raw();
    let entries = fs::read_dir(proc_root).map_err(|_| ConsumerScanError::Unreadable(0))?;
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let path = entry.path();
        match fs::metadata(&path) {
            Ok(meta) if meta.uid() != uid => continue,
            Ok(_) => {}
            Err(error) if process_exited(&error) => continue,
            Err(_) => return Err(ConsumerScanError::Unreadable(pid)),
        }
        // A process whose links we may not read is outside RSI's own tree
        // (non-dumpable service); the cargo lock covers real builds.
        let readable = |result: io::Result<PathBuf>| match result {
            Ok(value) => Ok(Some(value)),
            Err(error) if process_exited(&error) => Ok(None),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(None),
            Err(_) => Err(ConsumerScanError::Unreadable(pid)),
        };
        let cwd = readable(fs::read_link(path.join("cwd")))?;
        for link in [cwd.clone(), readable(fs::read_link(path.join("exe")))?]
            .into_iter()
            .flatten()
        {
            if link.starts_with(debug) {
                return Ok(Some(pid));
            }
        }
        if let Ok(descriptors) = fs::read_dir(path.join("fd")) {
            for descriptor in descriptors.flatten() {
                if let Ok(Some(value)) = readable(fs::read_link(descriptor.path())) {
                    if value.starts_with(debug) {
                        return Ok(Some(pid));
                    }
                }
            }
        }
        let is_tool = fs::read_to_string(path.join("comm")).is_ok_and(|comm| is_cargo_tool(&comm));
        if !is_tool {
            continue;
        }
        let Ok(environment) = fs::read(path.join("environ")) else {
            continue;
        };
        for variable in environment.split(|byte| *byte == 0) {
            let Some(value) = variable.strip_prefix(b"CARGO_TARGET_DIR=") else {
                continue;
            };
            let configured = Path::new(OsStr::from_bytes(value));
            let configured = if configured.is_absolute() {
                normalize(configured)
            } else if let Some(cwd) = &cwd {
                normalize(&cwd.join(configured))
            } else {
                // Cannot resolve a relative setting: fail closed.
                return Ok(Some(pid));
            };
            // `CARGO_TARGET_DIR` names the directory that holds `debug/`.
            if debug.parent() == Some(configured.as_path()) || configured.starts_with(debug) {
                return Ok(Some(pid));
            }
        }
    }
    Ok(None)
}

/// Daemon entry point for a non-dry pass: reclaim, then log the result.
pub fn run_and_log(
    trigger: &'static str,
    min_idle: Duration,
    pressure: bool,
) -> Option<SandboxSharedTargetReclaimV1> {
    let target_dir = crate::shared_target_prune::shared_target_dir()?;
    let result = reclaim_shared_debug(&target_dir, &ReclaimParams::new(min_idle, pressure, false));
    if result.outcome != Outcome::Absent || result.reclaimed_bytes > 0 {
        tracing::info!(
            trigger,
            target_dir = %target_dir.display(),
            outcome = ?result.outcome,
            check = result.check.as_deref().unwrap_or(""),
            idle_secs = result.idle_secs.unwrap_or(0),
            reclaimed_bytes = result.reclaimed_bytes,
            staged_pending = result.staged_pending,
            "Shared target debug reclaim pass completed"
        );
    }
    Some(result)
}

/// Daemon entry point for a dry-run: inspect and size, change nothing.
pub fn preview(min_idle: Duration, pressure: bool) -> Option<SandboxSharedTargetReclaimV1> {
    let target_dir = crate::shared_target_prune::shared_target_dir()?;
    Some(reclaim_shared_debug(
        &target_dir,
        &ReclaimParams::new(min_idle, pressure, true),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(3600);

    fn target_with_debug() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["deps", ".fingerprint", "build", "incremental"] {
            fs::create_dir_all(dir.path().join("debug").join(sub)).unwrap();
        }
        fs::create_dir_all(dir.path().join("release/deps")).unwrap();
        fs::write(dir.path().join("debug/.cargo-lock"), b"").unwrap();
        fs::write(dir.path().join("debug/deps/libx.rlib"), vec![7u8; 16384]).unwrap();
        fs::write(dir.path().join("debug/.fingerprint/x"), b"f").unwrap();
        fs::write(dir.path().join("release/deps/rsi"), vec![9u8; 4096]).unwrap();
        dir
    }

    fn params(hours_later: u64, pressure: bool, dry_run: bool) -> ReclaimParams {
        let mut params = ReclaimParams::new(Duration::from_secs(900), pressure, dry_run);
        params.now = SystemTime::now() + HOUR * hours_later as u32;
        params
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn idle_cache_under_pressure_is_removed_and_release_is_untouched() {
        let dir = target_with_debug();
        let result = reclaim_shared_debug(dir.path(), &params(5, true, false));
        assert_eq!(result.outcome, Outcome::Reclaimed, "{result:?}");
        assert!(result.reclaimed_bytes >= 16384, "{result:?}");
        assert!(!result.staged_pending);
        assert!(!dir.path().join("debug").exists());
        assert!(!has_staged(dir.path()));
        assert!(dir.path().join("release/deps/rsi").is_file());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn without_pressure_the_idle_cache_is_kept() {
        let dir = target_with_debug();
        let result = reclaim_shared_debug(dir.path(), &params(5, false, false));
        assert_eq!(result.outcome, Outcome::PressureInactive, "{result:?}");
        assert!(dir.path().join("debug/deps/libx.rlib").is_file());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_recently_written_cache_is_kept_even_under_pressure() {
        let dir = target_with_debug();
        let result = reclaim_shared_debug(dir.path(), &params(0, true, false));
        assert_eq!(result.outcome, Outcome::Fresh, "{result:?}");
        assert_eq!(result.min_idle_secs, MIN_IDLE.as_secs());
        assert!(dir.path().join("debug/deps/libx.rlib").is_file());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn pressure_bypasses_the_ttl_but_without_pressure_it_labels_the_cache_fresh() {
        let dir = target_with_debug();
        let mut p = params(5, false, true);
        p.min_idle = HOUR * 8;
        let result = reclaim_shared_debug(dir.path(), &p);
        assert_eq!(result.outcome, Outcome::Fresh, "{result:?}");
        assert_eq!(result.min_idle_secs, 8 * 3600);
        let mut p = params(5, true, false);
        p.min_idle = HOUR * 8;
        let result = reclaim_shared_debug(dir.path(), &p);
        assert_eq!(result.outcome, Outcome::Reclaimed, "{result:?}");
        assert_eq!(result.min_idle_secs, MIN_IDLE.as_secs());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_held_cargo_lock_keeps_the_cache() {
        let dir = target_with_debug();
        let holder = File::open(dir.path().join("debug/.cargo-lock")).unwrap();
        assert!(try_lock_exclusive(&holder).unwrap());
        let result = reclaim_shared_debug(dir.path(), &params(5, true, false));
        assert_eq!(result.outcome, Outcome::LockHeld, "{result:?}");
        assert!(dir.path().join("debug/deps/libx.rlib").is_file());
        drop(holder);
        let result = reclaim_shared_debug(dir.path(), &params(5, true, false));
        assert_eq!(result.outcome, Outcome::Reclaimed, "{result:?}");
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_process_running_inside_the_cache_keeps_it() {
        let dir = target_with_debug();
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .current_dir(dir.path().join("debug/deps"))
            .spawn()
            .unwrap();
        let result = reclaim_shared_debug(dir.path(), &params(5, true, false));
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(result.outcome, Outcome::LiveConsumer, "{result:?}");
        assert_eq!(
            result.check,
            Some(format!("process_uses_shared_target:{}", child.id()))
        );
        assert!(dir.path().join("debug/deps/libx.rlib").is_file());
        let result = reclaim_shared_debug(dir.path(), &params(5, true, false));
        assert_eq!(result.outcome, Outcome::Reclaimed, "{result:?}");
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_cargo_tool_pointed_at_the_target_dir_keeps_the_cache() {
        let dir = target_with_debug();
        let tools = tempfile::tempdir().unwrap();
        let cargo = tools.path().join("cargo");
        let sleep = std::process::Command::new("which")
            .arg("sleep")
            .output()
            .unwrap();
        let sleep = String::from_utf8(sleep.stdout).unwrap();
        std::os::unix::fs::symlink(sleep.trim(), &cargo).unwrap();
        let mut child = std::process::Command::new(&cargo)
            .arg("60")
            .env("CARGO_TARGET_DIR", dir.path())
            .current_dir(tools.path())
            .spawn()
            .unwrap();
        let result = reclaim_shared_debug(dir.path(), &params(5, true, false));
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(result.outcome, Outcome::LiveConsumer, "{result:?}");
        assert!(dir.path().join("debug/deps/libx.rlib").is_file());
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_unrelated_process_with_the_setting_does_not_block_reclaim() {
        let dir = target_with_debug();
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .env("CARGO_TARGET_DIR", dir.path())
            .current_dir(std::env::temp_dir())
            .spawn()
            .unwrap();
        let result = reclaim_shared_debug(dir.path(), &params(5, true, false));
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(result.outcome, Outcome::Reclaimed, "{result:?}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_symlinked_debug_is_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        fs::write(elsewhere.path().join("keep"), b"x").unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), dir.path().join("debug")).unwrap();
        let result = reclaim_shared_debug(dir.path(), &params(5, true, false));
        assert_eq!(result.outcome, Outcome::Unsafe, "{result:?}");
        assert!(elsewhere.path().join("keep").is_file());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn dry_run_measures_without_changing_anything() {
        let dir = target_with_debug();
        let result = reclaim_shared_debug(dir.path(), &params(5, true, true));
        assert_eq!(result.outcome, Outcome::WouldReclaim, "{result:?}");
        assert!(result.measured_bytes >= 16384 && !result.measured_truncated);
        assert_eq!(result.reclaimed_bytes, 0);
        assert!(dir.path().join("debug/deps/libx.rlib").is_file());
        let quiet = reclaim_shared_debug(dir.path(), &params(5, false, true));
        assert_eq!(quiet.outcome, Outcome::PressureInactive, "{quiet:?}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_absent_cache_is_reported_absent() {
        let dir = tempfile::tempdir().unwrap();
        let result = reclaim_shared_debug(dir.path(), &params(5, true, false));
        assert_eq!(result.outcome, Outcome::Absent);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_exhausted_removal_budget_leaves_a_stage_the_next_pass_finishes() {
        let dir = target_with_debug();
        let mut p = params(5, true, false);
        p.max_remove_duration = Duration::ZERO;
        let result = reclaim_shared_debug(dir.path(), &p);
        assert_eq!(result.outcome, Outcome::Partial, "{result:?}");
        assert!(result.staged_pending);
        assert!(!dir.path().join("debug").exists());
        assert!(has_staged(dir.path()));
        // The next pass finishes the stage even with nothing left to reclaim.
        let result = reclaim_shared_debug(dir.path(), &params(5, true, false));
        assert_eq!(result.outcome, Outcome::Absent, "{result:?}");
        assert!(result.reclaimed_bytes >= 16384, "{result:?}");
        assert!(!result.staged_pending);
        assert!(!has_staged(dir.path()));
        assert!(dir.path().join("release/deps/rsi").is_file());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_scan_over_budget_keeps_the_cache() {
        let dir = target_with_debug();
        let mut p = params(5, true, false);
        p.max_scan_entries = 2;
        let result = reclaim_shared_debug(dir.path(), &p);
        assert_eq!(result.outcome, Outcome::Unreadable, "{result:?}");
        assert!(dir.path().join("debug/deps/libx.rlib").is_file());
    }
}
