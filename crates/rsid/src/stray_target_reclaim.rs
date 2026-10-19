//! Disk-pressure reclaim of sandbox `target/` directories no live session owns
//! (#2682).
//!
//! The terminal-sandbox sweep (#1575) only visits finished sessions that still
//! hold a live, verified custody root. On a hub at 98% disk, 40 GB of cargo
//! output sat in directories outside that set: sandboxes with no session row at
//! all (**orphans**) and sandboxes only finished sessions without live custody
//! name (**detached**: a failed or quarantined cleanup, or a purged row whose
//! directory survived). This module finds them and removes **only their
//! `target/`**: the source worktree, branch and rows are never touched.
//!
//! The rules follow the shared `debug/` reclaim (#1607):
//!
//! * **Ownership.** A directory any unfinished session, any live custody root or
//!   any `Live` session row names is not stray (`Store::stray_target_owner`);
//!   the ordinary sweep or the owner handles it.
//! * **Idle age.** The newest write seen in `target/` must be older than
//!   [`MIN_IDLE`]; pressure bypasses the longer target-cache TTL. Without
//!   pressure nothing is removed.
//! * **Live consumer.** A held cargo lock, or a same-user process whose cwd,
//!   executable or open file lies in the sandbox directory, keeps it. Platforms
//!   without procfs refuse, never guess.
//!
//! Removal first renames `target/` to a sibling stage, so `target/` is whole or
//! absent; the stage is removed within a time budget and a later pass finishes
//! any remainder.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};

use rsi_common::sandbox_storage::{
    SANDBOX_STRAY_TARGETS_SAMPLES_MAX, SandboxSharedTargetReclaimOutcome as Outcome,
    SandboxStrayTargetKind, SandboxStrayTargetV1, SandboxStrayTargetsV1,
};
use uuid::Uuid;

use crate::shared_target_prune::try_lock_exclusive;
use crate::shared_target_reclaim::{
    ConsumerScanError, MAX_REMOVE_DURATION, MAX_SCAN_ENTRIES, MIN_IDLE, find_live_consumer,
    measure, remove_tree_budgeted,
};
use crate::store::Store;
use crate::store::runaway_process::RunawayNoticeRoute;
use crate::store::sandbox_stray::{RunawayDiskReport, StrayTargetOwner};

/// Sibling name prefix of a `target/` directory staged for deletion.
pub const STAGE_PREFIX: &str = "target.rsi-reclaim-";
/// Wall-clock ceiling for sizing strays in one dry run.
const MAX_MEASURE_DURATION: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug)]
pub struct StrayParams {
    pub now: SystemTime,
    /// The target-cache TTL; pressure bypasses it, [`MIN_IDLE`] always applies.
    pub min_idle: Duration,
    /// Disk-pressure hysteresis is active and reclaim is enabled.
    pub pressure: bool,
    pub dry_run: bool,
    pub max_remove_duration: Duration,
    pub max_scan_entries: u64,
}

impl StrayParams {
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

/// A sandbox directory under the base that holds a `target/` (or an unfinished
/// stage of one) and the owner verdict the store gave it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StrayCandidate {
    pub name: String,
    pub path: PathBuf,
    pub kind: SandboxStrayTargetKind,
    pub session_id: Option<Uuid>,
}

/// Sandbox directories (UUID-named, never a symlink) under `base` that hold a
/// `target/` or a staged remainder, sorted by name. Directories other things
/// own, such as job scratch, are not UUID-named and never listed.
pub fn directories_with_target(base: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(base) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if Uuid::parse_str(&name).is_err() {
            continue;
        }
        let path = entry.path();
        if !fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
            continue;
        }
        if has_target_or_stage(&path) {
            found.push((name, path));
        }
    }
    found.sort();
    found
}

fn has_target_or_stage(sandbox: &Path) -> bool {
    if fs::symlink_metadata(sandbox.join("target")).is_ok() {
        return true;
    }
    staged_entries(sandbox).next().is_some()
}

fn staged_entries(sandbox: &Path) -> impl Iterator<Item = PathBuf> {
    fs::read_dir(sandbox)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(STAGE_PREFIX)
        })
        .map(|entry| entry.path())
}

/// Turn the store's verdicts into candidates; claimed directories drop out.
pub fn classify(
    directories: Vec<(String, PathBuf)>,
    mut owner_of: impl FnMut(&Path) -> Option<StrayTargetOwner>,
) -> Vec<StrayCandidate> {
    directories
        .into_iter()
        .filter_map(|(name, path)| {
            let (kind, session_id) = match owner_of(&path)? {
                StrayTargetOwner::Claimed => return None,
                StrayTargetOwner::Orphan => (SandboxStrayTargetKind::Orphan, None),
                StrayTargetOwner::Detached { session_id } => {
                    (SandboxStrayTargetKind::Detached, session_id)
                }
            };
            Some(StrayCandidate {
                name,
                path,
                kind,
                session_id,
            })
        })
        .collect()
}

struct Verdict {
    outcome: Outcome,
    check: Option<String>,
    idle: Option<Duration>,
    measured_bytes: u64,
    measured_truncated: bool,
    reclaimed_bytes: u64,
    staged_pending: bool,
}

impl Verdict {
    fn new(outcome: Outcome, check: Option<&str>, idle: Option<Duration>) -> Self {
        Self {
            outcome,
            check: check.map(str::to_owned),
            idle,
            measured_bytes: 0,
            measured_truncated: false,
            reclaimed_bytes: 0,
            staged_pending: false,
        }
    }
}

/// Inspect, and under pressure reclaim, `<sandbox>/target` of one stray
/// directory. `measure_until` bounds the dry-run sizing across the whole pass.
fn reclaim_one(
    sandbox: &Path,
    params: &StrayParams,
    deadline: Instant,
    measure_until: Instant,
) -> Verdict {
    // Finish what an earlier pass staged, even when the target is gone.
    let (mut finished_bytes, mut pending) = (0u64, false);
    if params.dry_run {
        pending = staged_entries(sandbox).next().is_some();
    } else {
        for stage in staged_entries(sandbox) {
            let (freed, complete) = remove_tree_budgeted(&stage, deadline);
            finished_bytes = finished_bytes.saturating_add(freed);
            pending |= !complete;
        }
    }
    let mut verdict = inspect_and_reclaim(sandbox, params, deadline, measure_until);
    verdict.reclaimed_bytes = verdict.reclaimed_bytes.saturating_add(finished_bytes);
    verdict.staged_pending |= pending;
    verdict
}

fn inspect_and_reclaim(
    sandbox: &Path,
    params: &StrayParams,
    deadline: Instant,
    measure_until: Instant,
) -> Verdict {
    let target = sandbox.join("target");
    match fs::symlink_metadata(&target) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Verdict::new(Outcome::Unsafe, Some("target_symlink"), None);
        }
        Ok(meta) if !meta.is_dir() => {
            return Verdict::new(Outcome::Unsafe, Some("target_not_directory"), None);
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Verdict::new(Outcome::Absent, None, None);
        }
        Err(_) => return Verdict::new(Outcome::Unreadable, Some("target_unreadable"), None),
    }

    let newest = match newest_write(&target, params.max_scan_entries) {
        Ok(newest) => newest,
        Err(check) => return Verdict::new(Outcome::Unreadable, Some(check), None),
    };
    let idle = params.now.duration_since(newest).unwrap_or(Duration::ZERO);
    if idle < params.effective_min_idle() {
        return Verdict::new(Outcome::Fresh, Some("idle_below_ttl"), Some(idle));
    }

    // Take every cargo build lock first: a cargo that starts after this point
    // waits for it, so the consumer scan cannot be raced by a new build.
    let mut locks = Vec::new();
    for lock in [
        target.join(".cargo-lock"),
        target.join("debug/.cargo-lock"),
        target.join("release/.cargo-lock"),
    ] {
        match File::open(&lock) {
            Ok(file) => match try_lock_exclusive(&file) {
                Ok(true) => locks.push(file),
                Ok(false) => {
                    return Verdict::new(Outcome::LockHeld, Some("cargo_lock_held"), Some(idle));
                }
                Err(_) => {
                    return Verdict::new(
                        Outcome::Unreadable,
                        Some("cargo_lock_unavailable"),
                        Some(idle),
                    );
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Verdict::new(
                    Outcome::Unreadable,
                    Some("cargo_lock_unavailable"),
                    Some(idle),
                );
            }
        }
    }

    // The whole sandbox directory: a shell or editor in the source worktree
    // means someone is still there.
    match find_live_consumer(sandbox) {
        Ok(None) => {}
        Ok(Some(pid)) => {
            return Verdict::new(
                Outcome::LiveConsumer,
                Some(&format!("process_uses_sandbox:{pid}")),
                Some(idle),
            );
        }
        #[cfg(not(target_os = "linux"))]
        Err(ConsumerScanError::Unsupported) => {
            return Verdict::new(
                Outcome::Unsupported,
                Some("process_scan_unsupported"),
                Some(idle),
            );
        }
        Err(ConsumerScanError::Unreadable(pid)) => {
            return Verdict::new(
                Outcome::Unreadable,
                Some(&format!("process_scan_unreadable:{pid}")),
                Some(idle),
            );
        }
    }

    if params.dry_run {
        let mut verdict = Verdict::new(
            if params.pressure {
                Outcome::WouldReclaim
            } else {
                Outcome::PressureInactive
            },
            None,
            Some(idle),
        );
        if Instant::now() < measure_until {
            let (bytes, truncated) = measure(&target, params.max_scan_entries);
            verdict.measured_bytes = bytes;
            verdict.measured_truncated = truncated;
        } else {
            verdict.measured_truncated = true;
        }
        return verdict;
    }
    if !params.pressure {
        return Verdict::new(Outcome::PressureInactive, None, Some(idle));
    }

    // Atomic cut: `target/` is whole or absent, never half-deleted.
    let stage = sandbox.join(format!(
        "{STAGE_PREFIX}{}",
        params
            .now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    ));
    if fs::rename(&target, &stage).is_err() {
        return Verdict::new(Outcome::Unreadable, Some("stage_rename_failed"), Some(idle));
    }
    drop(locks);
    let (bytes, complete) = remove_tree_budgeted(&stage, deadline);
    let mut verdict = Verdict::new(
        if complete {
            Outcome::Reclaimed
        } else {
            Outcome::Partial
        },
        None,
        Some(idle),
    );
    verdict.reclaimed_bytes = bytes;
    verdict.staged_pending = !complete;
    verdict
}

/// Newest mtime across `target/`, its profile directories and their cheap
/// listings; `deps/` contents are not listed (a rewrite there moves the
/// directory's own mtime), but the per-crate fingerprint, build-script and
/// incremental directories are.
fn newest_write(target: &Path, budget: u64) -> Result<SystemTime, &'static str> {
    const UNREADABLE: &str = "target_unreadable";
    let modified = |path: &Path| -> Result<(SystemTime, bool), &'static str> {
        let meta = fs::symlink_metadata(path).map_err(|_| UNREADABLE)?;
        Ok((meta.modified().map_err(|_| UNREADABLE)?, meta.is_dir()))
    };
    let mut seen = 0u64;
    let mut visit = |path: &Path, newest: &mut SystemTime| -> Result<bool, &'static str> {
        seen += 1;
        if seen > budget {
            return Err("scan_budget");
        }
        let (time, is_dir) = modified(path)?;
        *newest = (*newest).max(time);
        Ok(is_dir)
    };
    let mut newest = SystemTime::UNIX_EPOCH;
    visit(target, &mut newest)?;
    for profile in fs::read_dir(target).map_err(|_| UNREADABLE)? {
        let profile = profile.map_err(|_| UNREADABLE)?;
        if !visit(&profile.path(), &mut newest)? {
            continue;
        }
        for entry in fs::read_dir(profile.path()).map_err(|_| UNREADABLE)? {
            let entry = entry.map_err(|_| UNREADABLE)?;
            let deep = matches!(
                entry.file_name().to_str(),
                Some(".fingerprint") | Some("build") | Some("incremental")
            );
            let is_dir = visit(&entry.path(), &mut newest)?;
            if deep && is_dir {
                for child in fs::read_dir(entry.path()).map_err(|_| UNREADABLE)? {
                    let child = child.map_err(|_| UNREADABLE)?;
                    visit(&child.path(), &mut newest)?;
                }
            }
        }
    }
    Ok(newest)
}

/// Reclaim (or, in a dry run, size) every candidate and total the result.
/// Largest entries are listed first and the sample is bounded; the totals
/// count every stray.
pub fn reclaim_strays(
    candidates: &[StrayCandidate],
    params: &StrayParams,
) -> SandboxStrayTargetsV1 {
    let deadline = Instant::now() + params.max_remove_duration;
    let measure_until = Instant::now() + MAX_MEASURE_DURATION;
    let mut entries = Vec::new();
    let mut result = SandboxStrayTargetsV1::default();
    for candidate in candidates {
        let verdict = reclaim_one(&candidate.path, params, deadline, measure_until);
        if verdict.outcome == Outcome::Absent && verdict.reclaimed_bytes == 0 {
            // A stage-only directory that finished, or nothing at all.
            if !verdict.staged_pending {
                continue;
            }
        }
        result.total_count += 1;
        result.total_measured_bytes = result
            .total_measured_bytes
            .saturating_add(verdict.measured_bytes);
        result.reclaimed_bytes = result
            .reclaimed_bytes
            .saturating_add(verdict.reclaimed_bytes);
        entries.push(SandboxStrayTargetV1 {
            name: candidate.name.clone(),
            kind: candidate.kind,
            session_id: candidate.session_id.map(|id| id.to_string()),
            outcome: verdict.outcome,
            check: verdict.check,
            idle_secs: verdict.idle.map(|idle| idle.as_secs()),
            measured_bytes: verdict.measured_bytes,
            measured_truncated: verdict.measured_truncated,
            reclaimed_bytes: verdict.reclaimed_bytes,
            staged_pending: verdict.staged_pending,
        });
    }
    entries.sort_by(|left, right| {
        let size = |entry: &SandboxStrayTargetV1| entry.measured_bytes.max(entry.reclaimed_bytes);
        size(right)
            .cmp(&size(left))
            .then_with(|| left.name.cmp(&right.name))
    });
    entries.truncate(SANDBOX_STRAY_TARGETS_SAMPLES_MAX);
    result.entries = entries;
    result
}

/// How long a pass waits for the Store before skipping the store-backed work.
const STORE_WAIT: Duration = Duration::from_secs(5);
/// Entries one runaway-size measurement may visit. A cut-short walk is a lower
/// bound, so a very large target can read low but never high.
const RUNAWAY_MEASURE_ENTRIES: u64 = 2_000_000;
/// Wall clock one runaway-size pass may spend; the rest waits for the next.
const RUNAWAY_PASS_BUDGET: Duration = Duration::from_secs(30);
/// Quiet period between two runaway-size passes.
pub const RUNAWAY_DISK_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// One daemon pass: list the sandbox directories holding a `target/`, ask the
/// Store who owns each, and reclaim (or in a dry run size) the stray ones.
/// `None` when no sandbox directory holds a `target/` or the Store is busy.
pub fn pass(
    runtime: &tokio::runtime::Runtime,
    store: &Arc<tokio::sync::Mutex<Store>>,
    base: &Path,
    trigger: &'static str,
    params: &StrayParams,
) -> Option<SandboxStrayTargetsV1> {
    let directories = directories_with_target(base);
    if directories.is_empty() {
        return None;
    }
    // A store error fails closed: the directory is treated as owned.
    let candidates = runtime.block_on(async {
        let store = tokio::time::timeout(STORE_WAIT, store.lock()).await.ok()?;
        Some(classify(directories, |path| {
            store.stray_target_owner(&path.to_string_lossy()).ok()
        }))
    })?;
    if candidates.is_empty() {
        return None;
    }
    let report = reclaim_strays(&candidates, params);
    if !params.dry_run
        && (report.reclaimed_bytes > 0 || report.entries.iter().any(|e| e.staged_pending))
    {
        tracing::info!(
            trigger,
            strays = report.total_count,
            reclaimed_bytes = report.reclaimed_bytes,
            "Stray sandbox target reclaim pass completed"
        );
    }
    Some(report)
}

/// The `target/` subdirectory `scripts/scoped-test` builds its baseline in
/// (#1963); removed when the run settles.
const SCOPED_BASELINES_DIR: &str = "scoped-baselines";

/// Sizes of one `target/`, the transient scoped-test baseline apart (#2782).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TargetSizes {
    /// Everything except `scoped-baselines`: what the threshold judges.
    own_bytes: u64,
    /// `scoped-baselines`: the worker's own `scoped_test` baseline build.
    baseline_bytes: u64,
    truncated: bool,
}

/// Allocated bytes of `target`, with `scoped-baselines` measured separately.
fn measure_target(target: &Path, budget: u64) -> TargetSizes {
    let (mut own_bytes, mut truncated) = measure_node(target);
    let mut baseline_bytes = 0;
    if let Ok(children) = fs::read_dir(target) {
        for child in children.flatten() {
            let (bytes, cut) = measure(&child.path(), budget);
            if child.file_name() == SCOPED_BASELINES_DIR {
                baseline_bytes += bytes;
            } else {
                own_bytes += bytes;
            }
            truncated |= cut;
        }
    }
    TargetSizes {
        own_bytes,
        baseline_bytes,
        truncated,
    }
}

/// Allocated bytes of the directory entry itself, children excluded.
fn measure_node(path: &Path) -> (u64, bool) {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path).map_or((0, true), |meta| (meta.blocks().saturating_mul(512), false))
}

/// How many distinct `rsid-<hash>` binaries `target/debug/deps` holds. Cargo
/// names each by a hash of its feature set, so more than one means the target
/// carries several builds of the daemon (a per-shard feature build).
fn rsid_test_binary_sets(target: &Path) -> usize {
    let Ok(entries) = fs::read_dir(target.join("debug/deps")) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix("rsid-"))
                .is_some_and(|hash| hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        })
        .count()
}

/// Tell the owning manager about each running worker whose `target/` passed
/// `threshold_bytes` (#2682, #121). Measures only sandboxes an unfinished
/// session owns, rotates the starting directory so a slow measurement cannot
/// starve the rest, and runs at most once per `min_interval` (`last_run` holds
/// the previous start). Returns each session told and where the record went.
pub fn runaway_disk_pass(
    runtime: &tokio::runtime::Runtime,
    store: &Arc<tokio::sync::Mutex<Store>>,
    base: &Path,
    last_run: &std::sync::Mutex<Option<Instant>>,
    min_interval: Duration,
    threshold_bytes: u64,
) -> Vec<(Uuid, RunawayNoticeRoute)> {
    static ROTATION: AtomicUsize = AtomicUsize::new(0);
    {
        let mut last = last_run
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if last.is_some_and(|started| started.elapsed() < min_interval) {
            return Vec::new();
        }
        *last = Some(Instant::now());
    }
    let mut directories = directories_with_target(base);
    if directories.is_empty() {
        return Vec::new();
    }
    let offset = ROTATION.fetch_add(1, Ordering::Relaxed) % directories.len();
    directories.rotate_left(offset);
    let deadline = Instant::now() + RUNAWAY_PASS_BUDGET;
    let mut told = Vec::new();
    for (name, path) in directories {
        if Instant::now() >= deadline {
            break;
        }
        let owner = runtime.block_on(async {
            let store = tokio::time::timeout(STORE_WAIT, store.lock()).await.ok()?;
            store
                .running_sandbox_owner(&path.to_string_lossy())
                .ok()
                .flatten()
        });
        let Some(session_id) = owner else {
            continue;
        };
        let target = path.join("target");
        let sizes = measure_target(&target, RUNAWAY_MEASURE_ENTRIES);
        let bytes = sizes.own_bytes;
        let truncated = sizes.truncated;
        if bytes < threshold_bytes {
            continue;
        }
        let scoped_test_live = runtime
            .block_on(async {
                let store = tokio::time::timeout(STORE_WAIT, store.lock()).await.ok()?;
                store
                    .sandbox_has_live_scoped_test(&path.to_string_lossy())
                    .ok()
            })
            .unwrap_or(false);
        let report = RunawayDiskReport {
            session_id,
            sandbox: name.clone(),
            target_bytes: bytes,
            baseline_bytes: sizes.baseline_bytes,
            truncated,
            threshold_bytes,
            scoped_test_live,
            shard_copies: rsid_test_binary_sets(&target) > 1,
            observed_at: chrono::Utc::now(),
        };
        let route = runtime.block_on(async {
            let store = tokio::time::timeout(STORE_WAIT, store.lock()).await.ok()?;
            store
                .record_runaway_disk_notice(&report)
                .map_err(|error| {
                    tracing::warn!(%session_id, %error, "runaway disk notice deferred");
                })
                .ok()
        });
        tracing::warn!(
            %session_id,
            sandbox = %name,
            target_gib = bytes >> 30,
            scoped_baseline_gib = sizes.baseline_bytes >> 30,
            scoped_test_live,
            threshold_gib = threshold_bytes >> 30,
            lower_bound = truncated,
            route = ?route,
            "Disk andon: a running worker's target/ passed the per-sandbox threshold"
        );
        if let Some(route) = route {
            told.push((session_id, route));
        }
    }
    told
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(3600);

    fn sandbox_with_target() -> (tempfile::TempDir, PathBuf) {
        let base = tempfile::tempdir().unwrap();
        let sandbox = base.path().join(Uuid::from_u128(7).to_string());
        for sub in [
            "debug/deps",
            "debug/.fingerprint",
            "debug/build",
            "release/deps",
        ] {
            fs::create_dir_all(sandbox.join("target").join(sub)).unwrap();
        }
        fs::write(sandbox.join("target/debug/deps/rsid"), vec![1u8; 8192]).unwrap();
        fs::create_dir_all(sandbox.join("src")).unwrap();
        fs::write(sandbox.join("src/lib.rs"), b"fn main() {}").unwrap();
        (base, sandbox)
    }

    fn params(hours_later: u32, pressure: bool, dry_run: bool) -> StrayParams {
        let mut params = StrayParams::new(Duration::from_secs(900), pressure, dry_run);
        params.now = SystemTime::now() + HOUR * hours_later;
        params
    }

    fn candidate(sandbox: &Path, kind: SandboxStrayTargetKind) -> StrayCandidate {
        StrayCandidate {
            name: sandbox.file_name().unwrap().to_string_lossy().into_owned(),
            path: sandbox.to_path_buf(),
            kind,
            session_id: None,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_idle_orphan_target_is_removed_under_pressure_and_its_source_is_kept() {
        let (_base, sandbox) = sandbox_with_target();
        let report = reclaim_strays(
            &[candidate(&sandbox, SandboxStrayTargetKind::Orphan)],
            &params(5, true, false),
        );
        assert_eq!(report.total_count, 1);
        assert_eq!(report.entries[0].outcome, Outcome::Reclaimed, "{report:?}");
        assert!(report.reclaimed_bytes >= 8192, "{report:?}");
        assert!(!sandbox.join("target").exists());
        assert!(
            sandbox.join("src/lib.rs").is_file(),
            "only build output is removed"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_dry_run_sizes_the_target_and_changes_nothing() {
        let (_base, sandbox) = sandbox_with_target();
        let report = reclaim_strays(
            &[candidate(&sandbox, SandboxStrayTargetKind::Detached)],
            &params(5, true, true),
        );
        assert_eq!(
            report.entries[0].outcome,
            Outcome::WouldReclaim,
            "{report:?}"
        );
        assert!(report.entries[0].measured_bytes >= 8192, "{report:?}");
        assert_eq!(report.reclaimed_bytes, 0);
        assert!(sandbox.join("target/debug/deps/rsid").is_file());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn without_pressure_nothing_is_removed() {
        let (_base, sandbox) = sandbox_with_target();
        let report = reclaim_strays(
            &[candidate(&sandbox, SandboxStrayTargetKind::Orphan)],
            &params(5, false, false),
        );
        assert_eq!(report.entries[0].outcome, Outcome::PressureInactive);
        assert!(sandbox.join("target").is_dir());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_recently_written_target_is_kept() {
        let (_base, sandbox) = sandbox_with_target();
        let report = reclaim_strays(
            &[candidate(&sandbox, SandboxStrayTargetKind::Orphan)],
            &params(0, true, false),
        );
        assert_eq!(report.entries[0].outcome, Outcome::Fresh, "{report:?}");
        assert!(sandbox.join("target").is_dir());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_held_cargo_lock_keeps_the_target() {
        let (_base, sandbox) = sandbox_with_target();
        let lock = sandbox.join("target/debug/.cargo-lock");
        fs::write(&lock, b"").unwrap();
        let held = File::open(&lock).unwrap();
        assert!(try_lock_exclusive(&held).unwrap());
        let report = reclaim_strays(
            &[candidate(&sandbox, SandboxStrayTargetKind::Orphan)],
            &params(5, true, false),
        );
        assert_eq!(report.entries[0].outcome, Outcome::LockHeld, "{report:?}");
        assert!(sandbox.join("target/debug/deps/rsid").is_file());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_symlinked_target_is_never_followed() {
        let base = tempfile::tempdir().unwrap();
        let sandbox = base.path().join(Uuid::from_u128(9).to_string());
        let elsewhere = base.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::write(elsewhere.join("keep"), b"x").unwrap();
        fs::create_dir_all(&sandbox).unwrap();
        std::os::unix::fs::symlink(&elsewhere, sandbox.join("target")).unwrap();
        let report = reclaim_strays(
            &[candidate(&sandbox, SandboxStrayTargetKind::Orphan)],
            &params(5, true, false),
        );
        assert_eq!(report.entries[0].outcome, Outcome::Unsafe, "{report:?}");
        assert!(elsewhere.join("keep").is_file());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn listing_skips_non_sandbox_names_and_claimed_directories() {
        let (base, sandbox) = sandbox_with_target();
        fs::create_dir_all(base.path().join("jobs/target")).unwrap();
        let claimed = base.path().join(Uuid::from_u128(8).to_string());
        fs::create_dir_all(claimed.join("target")).unwrap();
        let no_target = base.path().join(Uuid::from_u128(10).to_string());
        fs::create_dir_all(&no_target).unwrap();

        let directories = directories_with_target(base.path());
        assert_eq!(directories.len(), 2, "{directories:?}");
        let found = classify(directories, |path| {
            Some(if path == claimed {
                StrayTargetOwner::Claimed
            } else {
                StrayTargetOwner::Orphan
            })
        });
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, sandbox);
        assert_eq!(found[0].kind, SandboxStrayTargetKind::Orphan);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_staged_remainder_is_finished_by_the_next_pass() {
        let (_base, sandbox) = sandbox_with_target();
        let stage = sandbox.join(format!("{STAGE_PREFIX}1"));
        fs::rename(sandbox.join("target"), &stage).unwrap();
        assert_eq!(directories_with_target(sandbox.parent().unwrap()).len(), 1);
        let report = reclaim_strays(
            &[candidate(&sandbox, SandboxStrayTargetKind::Detached)],
            &params(5, true, false),
        );
        assert!(!stage.exists(), "{report:?}");
        assert!(report.reclaimed_bytes >= 8192, "{report:?}");
        assert!(sandbox.join("src/lib.rs").is_file());
    }

    fn store_with_session(
        root: &Path,
        status: rsi_common::types::SessionStatus,
        cleanup: rsi_common::types::SandboxCleanupState,
        id: u128,
    ) -> (Arc<tokio::sync::Mutex<Store>>, Uuid) {
        use rsi_common::types::SandboxKind;
        let store = Store::open_in_memory().unwrap();
        let mut session = rsid_store::test_support::make_test_session();
        session.id = Uuid::from_u128(id);
        session.status = status;
        session.sandbox_kind = Some(SandboxKind::GitWorktree);
        session.sandbox_root = Some(root.to_path_buf());
        session.sandbox_branch = Some("rsi/stray-test".into());
        session.sandbox_cleanup_state = Some(cleanup);
        store.insert_session(&session).unwrap();
        (Arc::new(tokio::sync::Mutex::new(store)), session.id)
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_pass_reclaims_a_target_only_finished_sessions_name_and_keeps_a_live_one() {
        use rsi_common::types::{SandboxCleanupState, SessionStatus};
        let (base, detached) = sandbox_with_target();
        let live = base.path().join(Uuid::from_u128(8).to_string());
        fs::create_dir_all(live.join("target/debug")).unwrap();
        fs::write(live.join("target/debug/keep"), b"x").unwrap();
        let orphan = base.path().join(Uuid::from_u128(10).to_string());
        fs::create_dir_all(orphan.join("target")).unwrap();

        let (store, _) = store_with_session(
            &detached,
            SessionStatus::Archived,
            SandboxCleanupState::Failed,
            1,
        );
        {
            let guard = store.try_lock().unwrap();
            let mut running = rsid_store::test_support::make_test_session();
            running.id = Uuid::from_u128(2);
            running.status = SessionStatus::Running;
            running.sandbox_kind = Some(rsi_common::types::SandboxKind::GitWorktree);
            running.sandbox_root = Some(live.clone());
            running.sandbox_branch = Some("rsi/live".into());
            running.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
            guard.insert_session(&running).unwrap();
        }
        let report = pass(
            &runtime(),
            &store,
            base.path(),
            "test",
            &params(5, true, false),
        )
        .unwrap();
        assert_eq!(report.total_count, 2, "{report:?}");
        let kinds = report
            .entries
            .iter()
            .map(|entry| (entry.name.clone(), entry.kind))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            kinds[&detached.file_name().unwrap().to_string_lossy().into_owned()],
            SandboxStrayTargetKind::Detached
        );
        assert_eq!(
            kinds[&orphan.file_name().unwrap().to_string_lossy().into_owned()],
            SandboxStrayTargetKind::Orphan
        );
        assert!(!detached.join("target").exists());
        assert!(!orphan.join("target").exists());
        assert!(
            live.join("target/debug/keep").is_file(),
            "a running session's target is its own"
        );
        assert!(detached.join("src/lib.rs").is_file());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_disk_andon_tells_about_a_running_worker_over_the_threshold_once_per_interval() {
        use rsi_common::types::{SandboxCleanupState, SessionStatus};
        let (base, sandbox) = sandbox_with_target();
        let (store, session) = store_with_session(
            &sandbox,
            SessionStatus::Running,
            SandboxCleanupState::Live,
            3,
        );
        let last = std::sync::Mutex::new(None);
        let runtime = runtime();
        let told = runaway_disk_pass(
            &runtime,
            &store,
            base.path(),
            &last,
            Duration::from_secs(600),
            4096,
        );
        assert_eq!(told.len(), 1, "{told:?}");
        assert_eq!(told[0].0, session);
        // Within the interval nothing is measured again.
        assert!(
            runaway_disk_pass(
                &runtime,
                &store,
                base.path(),
                &last,
                Duration::from_secs(600),
                4096
            )
            .is_empty()
        );
        // Under the threshold, or for a finished session, nothing is said.
        let quiet = std::sync::Mutex::new(None);
        assert!(
            runaway_disk_pass(
                &runtime,
                &store,
                base.path(),
                &quiet,
                Duration::ZERO,
                1 << 40
            )
            .is_empty()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_disk_andon_ignores_a_finished_session() {
        use rsi_common::types::{SandboxCleanupState, SessionStatus};
        let (base, sandbox) = sandbox_with_target();
        let (store, _) = store_with_session(
            &sandbox,
            SessionStatus::Completed,
            SandboxCleanupState::Live,
            4,
        );
        let last = std::sync::Mutex::new(None);
        assert!(
            runaway_disk_pass(&runtime(), &store, base.path(), &last, Duration::ZERO, 1).is_empty()
        );
    }

    /// Fill `dir` with `blocks` 4 KiB blocks of real (non-sparse) data.
    fn fill(dir: &Path, name: &str, blocks: usize) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join(name), vec![1u8; blocks * 4096]).unwrap();
    }

    fn start_scoped_test_job(store: &Arc<tokio::sync::Mutex<Store>>, owner: Uuid) {
        use crate::store::agent_jobs::NewAgentJob;
        use rsi_common::agent_jobs::JobWake;
        let id = Uuid::new_v4();
        let params = serde_json::from_value(
            serde_json::json!({"kind":"test","scoped_test":{"base":"origin/rolling"}}),
        )
        .unwrap();
        store
            .try_lock()
            .unwrap()
            .insert_agent_job(
                &NewAgentJob {
                    id,
                    owner_session_id: owner,
                    project_id: None,
                    name: None,
                    params,
                    cwd: "/x".into(),
                    unit_name: format!("rsi-job-{id}"),
                    log_path: "/x/job.log".into(),
                    status_path: "/x/job.status".into(),
                    idempotency_key: None,
                    wake: JobWake::None,
                },
                chrono::Utc::now(),
            )
            .unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_baseline_tree_is_measured_apart_from_the_target() {
        let (_base, sandbox) = sandbox_with_target();
        let target = sandbox.join("target");
        fill(&target.join("debug"), "big", 40);
        fill(&target.join("scoped-baselines/run-x"), "base", 30);
        let sizes = measure_target(&target, 1000);
        assert!(sizes.baseline_bytes >= 30 * 4096, "{sizes:?}");
        assert!(sizes.baseline_bytes < 40 * 4096, "{sizes:?}");
        assert!(sizes.own_bytes >= 40 * 4096, "{sizes:?}");
        assert!(sizes.own_bytes < 70 * 4096, "{sizes:?}");
        assert!(!sizes.truncated);
        assert_eq!(rsid_test_binary_sets(&target), 0);
        fill(&target.join("debug/deps"), "rsid-0123456789abcdef", 1);
        fill(&target.join("debug/deps"), "rsid-0123456789abcdef.d", 1);
        assert_eq!(rsid_test_binary_sets(&target), 1);
        fill(&target.join("debug/deps"), "rsid-fedcba9876543210", 1);
        assert_eq!(rsid_test_binary_sets(&target), 2);
    }

    /// #2782: a worker's own `scoped_test` baseline build, on top of its debug
    /// output, tells nobody to halt it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_disk_andon_does_not_count_a_live_scoped_test_baseline() {
        use rsi_common::types::{SandboxCleanupState, SessionStatus};
        let (base, sandbox) = sandbox_with_target();
        let target = sandbox.join("target");
        // Scaled from 37 GiB of debug output plus a 6 GiB baseline against 40 GiB.
        fill(&target.join("debug"), "big", 62);
        fill(&target.join("scoped-baselines/run-x"), "base", 12);
        let threshold = 80 * 4096;
        let (store, session) = store_with_session(
            &sandbox,
            SessionStatus::Completed,
            SandboxCleanupState::Live,
            5,
        );
        start_scoped_test_job(&store, session);
        let runtime = runtime();
        let last = std::sync::Mutex::new(None);
        let told = runaway_disk_pass(
            &runtime,
            &store,
            base.path(),
            &last,
            Duration::ZERO,
            threshold,
        );
        assert!(told.is_empty(), "{told:?}");

        // Debug output alone past the threshold is still told, and the record
        // carries no halt while the job is live.
        fill(&target.join("debug"), "more", 20);
        let told = runaway_disk_pass(
            &runtime,
            &store,
            base.path(),
            &last,
            Duration::ZERO,
            threshold,
        );
        assert_eq!(told.len(), 1, "{told:?}");
        assert_eq!(told[0].0, session);
    }
}
