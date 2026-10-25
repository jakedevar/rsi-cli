//! #453: the pool lifecycle of `scripts/target-pool` in Rust: `gc`, `reclaim-idle`,
//! `refresh`, `status` and `summary`, plus what they share with `build` and
//! `seed-from` (markers, events, disk floors, the `.use` / `.build.lock` locks).
//!
//! Every marker, lock file, event record and printed JSON line is byte-compatible
//! with the Python tool: a pool directory is read and written by either tool, the
//! daemon parses `summary`, and `cargo-slot` calls `reclaim-idle`. The invariants
//! are the Python tool's: a pool is never removed while a seed reads it (`gc`
//! takes `<pool>.use` exclusively, without waiting); the `.use` file is never
//! unlinked (its inode is the lock identity); a plan outlived by a rebuilt pool
//! of the same name removes nothing (`same_generation`).
use crate::target_pool::{
    DEFAULT_MAX_DISTANCE, Env, Git, REPO_FILE, choose_pool, dumps, failed_pool, known_repo,
    pool_distance, ready_pools, remember_repo, repo_dir,
};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const DEFAULT_KEEP: i64 = 2;
pub const DEFAULT_REF: &str = "origin/rolling";
/// #2773: a cache build never pushes the host below the floor its own workers need.
pub const SKIP_HEADROOM: &str = "pool_skipped_disk_headroom";
pub const ABORT_FLOOR: &str = "pool_aborted_disk_floor";
pub const RECLAIM_FLOOR: &str = "pool_reclaimed_for_disk_floor";
pub const DEFAULT_EXPECTED_POOL_GIB: f64 = 16.0;
/// cargo-slot's `RSI_CARGO_MIN_FREE_GB` / the daemon's `governor_min_free_disk_gb`.
pub const DEFAULT_WORKER_FLOOR_GIB: f64 = 30.0;
pub const DEFAULT_RECLAIM_INTERVAL: f64 = 600.0;
/// Not `*.json`: `gc` and `status` read every marker of that name.
pub const EVENTS_FILE: &str = "events.state";
/// No free-space floor may demand more than this share of the disk (#1884).
const MAX_FLOOR_PERCENT_OF_DISK: f64 = 25.0;
const GIB: f64 = 1_073_741_824.0;

// ---- small numeric / time helpers ----

pub(crate) fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Python's `round(value, 1)` for the magnitudes the tool reports.
pub(crate) fn round1(value: f64) -> f64 {
    // Rust formats the exact decimal value and rounds ties to even, as Python does.
    format!("{value:.1}").parse().unwrap_or(value)
}

fn float_of(value: &Value) -> f64 {
    value.as_f64().unwrap_or(0.0)
}

/// Pools to keep: the operator's `target_pool_keep` setting, which the daemon
/// passes as `RSI_TARGET_POOL_KEEP`.
pub fn default_keep(env: &Env) -> i64 {
    env.get("RSI_TARGET_POOL_KEEP")
        .and_then(|text| text.trim().parse::<i64>().ok())
        .map_or(DEFAULT_KEEP, |n| n.max(1))
}

pub fn default_min_free_gib(env: &Env) -> io::Result<f64> {
    match env.get("RSI_TARGET_POOL_MIN_FREE_GIB") {
        None => Ok(40.0),
        Some(text) => text.trim().parse::<f64>().map_err(|_| {
            io::Error::other(format!(
                "RSI_TARGET_POOL_MIN_FREE_GIB is not a number: {text:?}"
            ))
        }),
    }
}

pub(crate) fn positive_env(env: &Env, name: &str, default: f64) -> f64 {
    match env.get(name).map(|text| text.trim().parse::<f64>()) {
        Some(Ok(value)) if value > 0.0 => value,
        _ => default,
    }
}

// ---- disk ----

/// (free GiB, total GiB) of the filesystem holding `path` (`shutil.disk_usage`).
#[cfg(unix)]
fn disk_gib(path: &Path) -> io::Result<(f64, f64)> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("path contains a NUL byte"))?;
    // SAFETY: a zeroed statvfs is a valid out-parameter; the path is NUL-terminated.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: statvfs fills `stat` for a valid C string.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    #[allow(clippy::unnecessary_cast)]
    let (frsize, avail, blocks) = (
        stat.f_frsize as f64,
        stat.f_bavail as f64,
        stat.f_blocks as f64,
    );
    Ok((avail * frsize / GIB, blocks * frsize / GIB))
}

#[cfg(not(unix))]
fn disk_gib(_path: &Path) -> io::Result<(f64, f64)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "target-pool disk accounting needs a Unix filesystem",
    ))
}

/// Test seam: a test states the free space its code path sees (see `test_disk`);
/// production always asks the filesystem.
#[cfg(test)]
pub(crate) mod test_disk {
    use std::sync::{Mutex, MutexGuard};
    static SERIAL: Mutex<()> = Mutex::new(());
    static STATED: Mutex<Option<f64>> = Mutex::new(None);
    static STATED_TOTAL: Mutex<Option<f64>> = Mutex::new(None);

    /// Serialises the tests that depend on free space and states it for their duration;
    /// `set` changes it mid-test (a build watching the disk).
    pub(crate) struct Disk(#[allow(dead_code)] MutexGuard<'static, ()>);
    impl Disk {
        pub(crate) fn new(stated: Option<f64>) -> Self {
            Self::with_total(stated, None)
        }
        pub(crate) fn with_total(stated: Option<f64>, total: Option<f64>) -> Self {
            let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            *STATED.lock().unwrap_or_else(|e| e.into_inner()) = stated;
            *STATED_TOTAL.lock().unwrap_or_else(|e| e.into_inner()) = total;
            Self(guard)
        }
        pub(crate) fn set(stated: Option<f64>) {
            *STATED.lock().unwrap_or_else(|e| e.into_inner()) = stated;
        }
    }
    impl Drop for Disk {
        fn drop(&mut self) {
            Disk::set(None);
            *STATED_TOTAL.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }
    pub(crate) fn stated() -> Option<f64> {
        *STATED.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub(crate) fn stated_total() -> Option<f64> {
        *STATED_TOTAL.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub(crate) fn free_gib(path: &Path) -> io::Result<f64> {
    #[cfg(test)]
    if let Some(stated) = test_disk::stated() {
        return Ok(stated);
    }
    disk_gib(path).map(|(free, _)| free)
}

/// The configured floor, capped at a quarter of the disk holding `path`.
pub(crate) fn scaled_floor_gib(path: &Path, configured_gib: f64) -> io::Result<f64> {
    fs::create_dir_all(path)?;
    #[cfg(test)]
    if let Some(total) = test_disk::stated_total() {
        return Ok(configured_gib.min(total * MAX_FLOOR_PERCENT_OF_DISK / 100.0));
    }
    let (_, total) = disk_gib(path)?;
    Ok(configured_gib.min(total * MAX_FLOOR_PERCENT_OF_DISK / 100.0))
}

/// The free-space floor `cargo-slot` holds a worker job to. The daemon passes its
/// `governor_min_free_disk_gb` as `RSI_TARGET_POOL_WORKER_FLOOR_GIB`; without it
/// `RSI_CARGO_MIN_FREE_GB` (the script's own default) and then 30. Capped, like the
/// pool's floor, at a quarter of the disk.
pub(crate) fn worker_floor_gib(env: &Env, path: &Path) -> io::Result<f64> {
    for name in ["RSI_TARGET_POOL_WORKER_FLOOR_GIB", "RSI_CARGO_MIN_FREE_GB"] {
        if let Some(Ok(configured)) = env.get(name).map(|t| t.trim().parse::<f64>()) {
            return scaled_floor_gib(path, configured.max(0.0));
        }
    }
    scaled_floor_gib(path, DEFAULT_WORKER_FLOOR_GIB)
}

// ---- markers and events ----

/// Publish a marker atomically (the temp name does not match `*.json`).
pub(crate) fn write_marker(marker: &Path, record: &Value) -> io::Result<()> {
    let name = marker
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temporary = marker.with_file_name(format!("{name}.{}.tmp", std::process::id()));
    fs::write(&temporary, format!("{}\n", dumps(record, None)))?;
    fs::rename(&temporary, marker)
}

pub(crate) fn read_events(directory: &Path) -> Map<String, Value> {
    fs::read_to_string(directory.join(EVENTS_FILE))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| match value {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default()
}

/// Remember the latest `kind` event of this pool directory (`None` forgets it). A
/// status note never fails a build or a gc.
pub(crate) fn record_event(directory: &Path, kind: &str, fields: Option<Map<String, Value>>) {
    let _ = (|| -> io::Result<()> {
        fs::create_dir_all(directory)?;
        let mut events = read_events(directory);
        match fields {
            None => {
                events.remove(kind);
            }
            Some(fields) => {
                let mut event = Map::new();
                event.insert("at".into(), json!(now()));
                event.extend(fields);
                events.insert(kind.to_string(), Value::Object(event));
            }
        }
        write_marker(&directory.join(EVENTS_FILE), &Value::Object(events))
    })();
}

pub(crate) fn fields(value: Value) -> Option<Map<String, Value>> {
    match value {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

// ---- locks ----

/// `<pool>.use` without waiting: shared by readers (seeds and pool builds that copy
/// from the pool), exclusive by gc. `None` when the other side holds it.
pub(crate) fn use_lock(directory: &Path, name: &str, exclusive: bool) -> Option<File> {
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(directory.join(format!("{name}.use")))
        .ok()?;
    flock(&file, exclusive).then_some(file)
}

/// The one-builder-per-host lock `<directory>/.build.lock`; `None` when held.
pub(crate) fn build_lock(directory: &Path) -> io::Result<Option<File>> {
    fs::create_dir_all(directory)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(directory.join(".build.lock"))?;
    Ok(flock(&file, true).then_some(file))
}

#[cfg(unix)]
fn flock(file: &File, exclusive: bool) -> bool {
    use std::os::fd::AsRawFd;
    let mode = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    };
    // SAFETY: flock on a descriptor `file` owns for the call.
    unsafe { libc::flock(file.as_raw_fd(), mode | libc::LOCK_NB) == 0 }
}

/// Non-Unix: best effort, one builder per host is advisory.
#[cfg(not(unix))]
fn flock(_file: &File, _exclusive: bool) -> bool {
    true
}

// ---- sizes ----

/// Bytes on disk, counting each hard-linked inode once (`os.walk` without
/// following symlinked directories).
pub(crate) fn tree_size(path: &Path) -> u64 {
    fn walk(path: &Path, seen: &mut BTreeSet<(u64, u64)>, total: &mut u64) {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let child = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                walk(&child, seen, total);
                continue;
            }
            // A symlink to a directory is listed with the directories and not counted.
            if kind.is_symlink() && fs::metadata(&child).is_ok_and(|m| m.is_dir()) {
                continue;
            }
            let Ok(status) = fs::symlink_metadata(&child) else {
                continue;
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if !seen.insert((status.dev(), status.ino())) {
                    continue;
                }
            }
            #[cfg(not(unix))]
            {
                let _ = &seen;
            }
            *total += status.len();
        }
    }
    let mut seen = BTreeSet::new();
    let mut total = 0;
    walk(path, &mut seen, &mut total);
    total
}

/// The pool's size, measured once and remembered in its marker (a ready pool is
/// immutable).
pub(crate) fn pool_size(directory: &Path, record: &Value) -> u64 {
    if let Some(size) = record["size_bytes"].as_u64() {
        return size;
    }
    let name = record["name"].as_str().unwrap_or_default();
    // Hold the shared use lock for the walk and the cache write: gc cannot remove
    // the pool meanwhile, so the cache cannot resurrect a removed marker.
    let Some(held) = use_lock(directory, name, false) else {
        return 0; // gc is removing it
    };
    let target = PathBuf::from(record["target"].as_str().unwrap_or_default());
    if !target.is_dir() {
        return 0;
    }
    let size = tree_size(&target);
    let marker = directory.join(format!("{name}.json"));
    if let Some(mut current) = fs::read_to_string(&marker)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
    {
        // A pool is immutable once ready.
        if current["state"] == "ready"
            && target.is_dir()
            && current["built_at"] == record["built_at"]
        {
            current["size_bytes"] = json!(size);
            let _ = write_marker(&marker, &current);
        }
    }
    drop(held);
    size
}

// ---- admission ----

/// (bytes, where the number came from): the nearest ready ancestor's measured size,
/// else the newest ready pool's, else the size of the last pool built (remembered
/// past a gc), else `RSI_TARGET_POOL_EXPECTED_GIB` / 16.
pub(crate) fn expected_pool_size(
    git: &Git,
    directory: &Path,
    repo: &Path,
    sha: &str,
    key: &str,
) -> (u64, &'static str) {
    let pools = ready_pools(directory, key);
    let nearest = if pools.is_empty() {
        None
    } else {
        choose_pool(git, repo, sha, &pools, DEFAULT_MAX_DISTANCE)
    };
    let mut candidates: Vec<(Value, &'static str)> = vec![];
    if let Some(nearest) = nearest {
        candidates.push((nearest, "nearest ready ancestor"));
    }
    candidates.extend(pools.into_iter().map(|r| (r, "newest ready pool")));
    for (record, source) in candidates {
        let size = pool_size(directory, &record);
        if size > 0 {
            return (size, source);
        }
    }
    if let Some(last) = read_events(directory)
        .get("last_pool")
        .and_then(|event| event["size_bytes"].as_u64())
        .filter(|size| *size > 0)
    {
        return (last, "last pool built");
    }
    let default = positive_env(
        git.env,
        "RSI_TARGET_POOL_EXPECTED_GIB",
        DEFAULT_EXPECTED_POOL_GIB,
    );
    ((default * GIB) as u64, "default")
}

/// `None` when a pool build may start; else the typed skip. Free space must cover
/// the floor (the larger of the pool floor and the workers' floor) plus the pool's
/// expected size, so finishing the build still leaves the workers their floor. A
/// floor of 0 turns the check off.
pub(crate) fn admission_skip(
    git: &Git,
    directory: &Path,
    repo: &Path,
    sha: &str,
    key: &str,
    configured_floor: f64,
) -> io::Result<Option<Map<String, Value>>> {
    if configured_floor <= 0.0 {
        return Ok(None);
    }
    let expected = expected_pool_size(git, directory, repo, sha, key);
    admission_skip_with_expected(git, directory, sha, configured_floor, expected)
}

fn admission_skip_with_expected(
    git: &Git,
    directory: &Path,
    sha: &str,
    configured_floor: f64,
    (expected, source): (u64, &'static str),
) -> io::Result<Option<Map<String, Value>>> {
    if configured_floor <= 0.0 {
        return Ok(None);
    }
    let floor =
        scaled_floor_gib(directory, configured_floor)?.max(worker_floor_gib(git.env, directory)?);
    let expected_gib = expected as f64 / GIB;
    let free = free_gib(directory)?;
    let needed = floor + expected_gib;
    if free >= needed {
        return Ok(None);
    }
    Ok(fields(json!({
        "reason": SKIP_HEADROOM,
        "sha": sha,
        "free_gib": round1(free),
        "floor_gib": round1(floor),
        "expected_gib": round1(expected_gib),
        "needed_gib": round1(needed),
        "expected_source": source,
        "detail": format!(
            "only {free:.1} GiB free; needs {needed:.1} GiB (floor {floor:.1} + expected pool {expected_gib:.1}, {source})"
        ),
    })))
}

// ---- removal and gc ----

pub(crate) fn remove_pool(
    git: &Git,
    directory: &Path,
    name: &str,
    repo: Option<&Path>,
    keep_log: bool,
) {
    let worktree = directory.join(name);
    if let Some(repo) = repo {
        if worktree.exists() {
            git.lenient(
                repo,
                &["worktree", "remove", "--force", &worktree.to_string_lossy()],
            );
        }
    }
    if worktree.exists() {
        let _ = fs::remove_dir_all(&worktree);
    }
    if let Some(repo) = repo {
        git.lenient(repo, &["worktree", "prune"]);
    }
    let _ = fs::remove_file(directory.join(format!("{name}.json")));
    // `<name>.use` is never unlinked: its inode is the lock identity, and a waiter
    // that opened it before a removal must exclude a seed that locks the rebuilt
    // pool of the same name. It is an empty file.
    if !keep_log {
        let _ = fs::remove_file(directory.join(format!("{name}.log")));
    }
}

/// Commits between the pool's commit and `reference`, read from the pool's own
/// worktree (it shares the repository's refs); `None` when it cannot be counted. A
/// pool imported by `seed-from` has only a target directory, no Git worktree, so
/// its age is read from `repo`, the checkout the pool directory belongs to (#2717).
pub(crate) fn pool_age_commits(
    git: &Git,
    record: &Value,
    reference: &str,
    repo: Option<&Path>,
) -> Option<u64> {
    let mut worktree = PathBuf::from(record["worktree"].as_str().unwrap_or_default());
    if !worktree.join(".git").exists() {
        if let Some(repo) = repo.filter(|r| r.is_dir()) {
            worktree = repo.to_path_buf();
        }
    }
    if !worktree.is_dir() {
        return None;
    }
    let sha = record["sha"].as_str()?;
    for candidate in [reference, "rolling"] {
        let counted = git.lenient(
            &worktree,
            &["rev-list", "--count", &format!("{sha}..{candidate}")],
        );
        if !counted.is_empty() && counted.bytes().all(|b| b.is_ascii_digit()) {
            return counted.parse().ok();
        }
    }
    None
}

/// Ready pools more than `max_distance` commits behind `reference`: the seed refuses
/// them, so they only hold disk. A pool whose age cannot be counted is not stale.
pub(crate) fn stale_pool_names(
    git: &Git,
    records: &[Value],
    reference: &str,
    max_distance: u64,
    repo: Option<&Path>,
) -> BTreeSet<String> {
    records
        .iter()
        .filter(|record| record["state"] == "ready")
        .filter(|record| {
            pool_age_commits(git, record, reference, repo).is_some_and(|age| age > max_distance)
        })
        .filter_map(|record| record["name"].as_str().map(str::to_string))
        .collect()
}

fn built_at(record: &Value) -> f64 {
    float_of(&record["built_at"])
}

/// (kept, doomed) among the ready pools. Keep the newest `keep` (exactly one under
/// disk pressure) plus the newest complete pool, pressure or not, so a run of
/// degraded pools never evicts the last pool that has test artifacts. A `stale`
/// pool is doomed whatever its rank and never counts toward `keep` (#2707).
pub fn gc_plan(
    records: &[Value],
    keep: i64,
    pressure: bool,
    stale: &BTreeSet<String>,
) -> (Vec<Value>, Vec<Value>) {
    let mut ready: Vec<Value> = records
        .iter()
        .filter(|record| record["state"] == "ready")
        .cloned()
        .collect();
    ready.sort_by(|a, b| built_at(b).total_cmp(&built_at(a)));
    // An explicit GC zero is an eviction request, including the otherwise protected
    // last complete pool. Builds and seeds clamp their own retention.
    if keep == 0 {
        return (vec![], ready);
    }
    let name_of = |record: &Value| record["name"].as_str().unwrap_or_default().to_string();
    let usable: Vec<&Value> = ready
        .iter()
        .filter(|record| !stale.contains(&name_of(record)))
        .collect();
    let keep = if pressure { 1 } else { keep.max(1) } as usize;
    let mut kept: Vec<Value> = usable.iter().take(keep).map(|r| (*r).clone()).collect();
    let degraded = |record: &Value| record["degraded"].as_bool().unwrap_or(false);
    if !kept.iter().any(|record| !degraded(record)) {
        if let Some(complete) = usable.iter().find(|record| !degraded(record)) {
            kept.push((*complete).clone());
        }
    }
    let names: BTreeSet<String> = kept.iter().map(name_of).collect();
    let doomed = ready
        .into_iter()
        .filter(|record| !names.contains(&name_of(record)))
        .collect();
    (kept, doomed)
}

/// Whether the marker on disk still describes the pool gc planned to remove;
/// checked under the exclusive use lock, which a plan can outlive.
fn same_generation(directory: &Path, planned: &Value) -> bool {
    let name = planned["name"].as_str().unwrap_or_default();
    let Some(current) = fs::read_to_string(directory.join(format!("{name}.json")))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
    else {
        return false;
    };
    current["state"] == "ready" && current["built_at"] == planned["built_at"]
}

fn same_filesystem(record: &Value, filesystem: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let target = record["target"].as_str().unwrap_or_default();
        match (fs::metadata(target), fs::metadata(filesystem)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (record, filesystem);
        false
    }
}

pub(crate) fn markers(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(directory) else {
        return vec![];
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    paths
}

fn read_record(marker: &Path) -> Option<Value> {
    fs::read_to_string(marker)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

pub struct GcOptions<'a> {
    pub keep: i64,
    pub repo: Option<&'a Path>,
    pub pressure: bool,
    /// Also remove pools more than `max_distance` commits behind this ref.
    pub reference: Option<&'a str>,
    pub max_distance: u64,
    pub filesystem: Option<&'a Path>,
}

/// Remove the pools outside `gc_plan`; a pool a seed is reading is skipped (its name
/// is pushed to `skipped`). Returns the removed names.
pub fn gc(
    git: &Git,
    directory: &Path,
    options: &GcOptions,
    skipped: &mut Vec<String>,
) -> Vec<String> {
    gc_with(git, directory, options, skipped, &mut |_| {})
}

/// `gc` with a call just before it takes a doomed pool's exclusive lock: a test runs the
/// interleavings a plan can outlive (another gc, a rebuild, a seed) at exactly that point.
pub(crate) fn gc_with(
    git: &Git,
    directory: &Path,
    options: &GcOptions,
    skipped: &mut Vec<String>,
    before_lock: &mut dyn FnMut(&str),
) -> Vec<String> {
    let records: Vec<Value> = markers(directory)
        .iter()
        .filter_map(|marker| read_record(marker))
        .collect();
    let mut removed = vec![];
    let stale = match options.reference {
        Some(reference) => stale_pool_names(
            git,
            &records,
            reference,
            options.max_distance,
            options
                .repo
                .map(Path::to_path_buf)
                .or_else(|| known_repo(directory))
                .as_deref(),
        ),
        None => BTreeSet::new(),
    };
    let (_kept, doomed) = gc_plan(&records, options.keep, options.pressure, &stale);
    for record in doomed {
        if let Some(filesystem) = options.filesystem {
            if !same_filesystem(&record, filesystem) {
                continue;
            }
        }
        let name = record["name"].as_str().unwrap_or_default().to_string();
        if stale.contains(&name) {
            // Before the worktree goes: it is the only thing naming the repository.
            remember_repo(
                git,
                directory,
                Path::new(record["worktree"].as_str().unwrap_or_default()),
            );
        }
        before_lock(&name);
        let Some(held) = use_lock(directory, &name, true) else {
            skipped.push(name);
            continue;
        };
        if same_generation(directory, &record) {
            let repo = options
                .repo
                .map(Path::to_path_buf)
                .or_else(|| known_repo(directory));
            remove_pool(git, directory, &name, repo.as_deref(), false);
            removed.push(name);
        }
        drop(held);
    }
    removed
}

// ---- commands ----

fn pool_directories(env: &Env) -> io::Result<Vec<PathBuf>> {
    let root = crate::target_pool::pool_root(env)?;
    let mut directories: Vec<PathBuf> = match fs::read_dir(&root) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect(),
        Err(_) => vec![],
    };
    directories.sort();
    Ok(directories)
}

fn directories_for(git: &Git, env: &Env, repo: Option<&Path>) -> io::Result<Vec<PathBuf>> {
    match repo {
        Some(repo) => Ok(vec![repo_dir(git, env, repo)?]),
        None => pool_directories(env),
    }
}

pub struct GcCommand<'a> {
    pub repo: Option<&'a Path>,
    pub keep: Option<i64>,
    pub pressure: bool,
    pub json: bool,
    pub reference: Option<&'a str>,
    pub max_distance: u64,
}

pub fn gc_command(options: &GcCommand, env: &Env, out: &mut dyn Write) -> io::Result<i32> {
    let git = Git { env };
    let directories = directories_for(&git, env, options.repo)?;
    let keep = options.keep.unwrap_or_else(|| default_keep(env));
    let (mut removed, mut skipped) = (vec![], vec![]);
    for directory in directories {
        if let Some(repo) = options.repo {
            remember_repo(&git, &directory, repo);
        }
        removed.extend(gc(
            &git,
            &directory,
            &GcOptions {
                keep,
                repo: options.repo,
                pressure: options.pressure,
                reference: options.reference.filter(|r| !r.is_empty()),
                max_distance: options.max_distance,
                filesystem: None,
            },
            &mut skipped,
        ));
    }
    if options.json {
        writeln!(
            out,
            "{}",
            dumps(
                &json!({"removed": removed, "skipped_in_use": skipped}),
                None
            )
        )?;
    } else {
        for name in &removed {
            writeln!(out, "removed {name}")?;
        }
        for name in &skipped {
            writeln!(out, "skipped {name}: in use")?;
        }
    }
    Ok(0)
}

/// Evict every ready pool no seed or build is reading, for a worker waiting on the
/// disk floor (#2773): the pool exists to speed workers up, never to stall them.
/// Bounded: a directory reclaimed within `min_interval` seconds is left alone.
/// In-use pools are skipped (the exclusive use lock), never waited on.
pub fn reclaim_idle_command(
    min_interval: f64,
    filesystem: Option<&Path>,
    env: &Env,
    out: &mut dyn Write,
) -> io::Result<i32> {
    let git = Git { env };
    let (mut removed, mut skipped, mut throttled): (Vec<String>, Vec<String>, Vec<String>) =
        (vec![], vec![], vec![]);
    let mut freed: u64 = 0;
    let started = now();
    for directory in pool_directories(env)? {
        let directory_name = directory
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(last) = read_events(&directory).get("reclaim") {
            if started - float_of(&last["at"]) < min_interval {
                throttled.push(directory_name);
                continue;
            }
        }
        let mut sizes = std::collections::BTreeMap::new();
        for marker in markers(&directory) {
            if let Some(record) = read_record(&marker) {
                if record["state"] == "ready" {
                    let name = record["name"].as_str().unwrap_or_default().to_string();
                    sizes.insert(name, pool_size(&directory, &record));
                }
            }
        }
        let names = gc(
            &git,
            &directory,
            &GcOptions {
                keep: 0,
                repo: None,
                pressure: false,
                reference: None,
                max_distance: DEFAULT_MAX_DISTANCE,
                filesystem,
            },
            &mut skipped,
        );
        if !names.is_empty() {
            let bytes_freed: u64 = names
                .iter()
                .map(|n| sizes.get(n).copied().unwrap_or(0))
                .sum();
            freed += bytes_freed;
            record_event(
                &directory,
                "reclaim",
                fields(json!({
                    "reason": RECLAIM_FLOOR,
                    "removed": names.len(),
                    "freed_bytes": bytes_freed,
                    "detail": format!(
                        "removed {} idle pool(s), {:.1} GiB, for a worker waiting on the disk floor",
                        names.len(),
                        bytes_freed as f64 / GIB
                    ),
                })),
            );
        }
        removed.extend(names);
    }
    writeln!(
        out,
        "{}",
        dumps(
            &json!({
                "reason": RECLAIM_FLOOR,
                "removed": removed,
                "skipped_in_use": skipped,
                "throttled": throttled,
                "freed_bytes": freed,
            }),
            None
        )
    )?;
    Ok(0)
}

/// Whether the pools trail far enough to build a fresh one: none within reach, or the
/// nearest is past half the seed distance (so a seed is never one step from the
/// limit when the next build starts).
pub fn refresh_needed(nearest: Option<u64>, max_distance: u64) -> bool {
    nearest.is_none_or(|n| n > max_distance / 2)
}

/// What a refresh asked for; the build launcher is injected so tests start nothing.
pub type Launch<'a> = &'a dyn Fn(&Git, &Path, &str) -> io::Result<String>;

pub struct RefreshOptions<'a> {
    pub repo: Option<&'a Path>,
    pub reference: &'a str,
    pub max_distance: u64,
    pub retry_after: f64,
}

fn refresh_directory(
    git: &Git,
    directory: &Path,
    options: &RefreshOptions,
    launch: Launch,
) -> io::Result<Value> {
    let directory_name = directory
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut outcome = Map::new();
    outcome.insert("directory".into(), json!(directory_name));
    let skipped = |mut outcome: Map<String, Value>, why: String| {
        outcome.insert("result".into(), json!(why));
        Value::Object(outcome)
    };
    let Some(repo) = options
        .repo
        .map(Path::to_path_buf)
        .or_else(|| known_repo(directory))
    else {
        return Ok(skipped(
            outcome,
            "skipped: no checkout is known for these pools".into(),
        ));
    };
    let tip = [options.reference, "rolling"].iter().find_map(|candidate| {
        let sha = git.lenient(
            &repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{candidate}^{{commit}}"),
            ],
        );
        (!sha.is_empty()).then_some(sha)
    });
    let Some(tip) = tip else {
        return Ok(skipped(
            outcome,
            format!("skipped: {} does not resolve", options.reference),
        ));
    };
    let key = crate::target_pool::build_key(&repo, git.env)?;
    let nearest = ready_pools(directory, &key)
        .iter()
        .filter_map(|record| {
            pool_distance(git, &repo, &tip, record["sha"].as_str().unwrap_or_default())
        })
        .min();
    outcome.insert("tip".into(), json!(tip));
    outcome.insert("nearest".into(), json!(nearest));
    if !refresh_needed(nearest, options.max_distance) {
        return Ok(skipped(outcome, "fresh".into()));
    }
    if let Some(failure) = failed_pool(directory, &tip, &key) {
        if now() - float_of(&failure["built_at"]) < options.retry_after {
            return Ok(skipped(
                outcome,
                "skipped: the build of this tip failed recently".into(),
            ));
        }
    }
    let result = launch(git, &repo, &tip)?;
    Ok(skipped(outcome, result))
}

pub fn refresh_command(
    options: &RefreshOptions,
    env: &Env,
    launch: Launch,
    out: &mut dyn Write,
) -> io::Result<i32> {
    let git = Git { env };
    let mut results = vec![];
    for directory in directories_for(&git, env, options.repo)? {
        results.push(refresh_directory(&git, &directory, options, launch)?);
    }
    writeln!(out, "{}", dumps(&json!({"refresh": results}), None))?;
    Ok(0)
}

/// An estimate only: the exclusive use lock proves no seed/build is reading it now.
/// Reclaim takes that lock again; admission must remeasure free disk afterwards
/// (seeded targets can still hold hardlinks to these files).
fn idle_pool_bytes(directory: &Path, record: &Value, filesystem: Option<&Path>) -> u64 {
    if let Some(filesystem) = filesystem {
        if !same_filesystem(record, filesystem) {
            return 0;
        }
    }
    let name = record["name"].as_str().unwrap_or_default();
    let Some(held) = use_lock(directory, name, true) else {
        return 0;
    };
    let size = if same_generation(directory, record) {
        record["size_bytes"].as_u64().unwrap_or(0)
    } else {
        0
    };
    drop(held);
    size
}

/// What the operator sees in Settings: the newest good pool, how many commits it
/// trails `reference`, whether newer builds failed, and the disk the pools hold.
pub fn summarise(
    git: &Git,
    directories: &[PathBuf],
    reference: &str,
    filesystem: Option<&Path>,
) -> Value {
    let (mut ready, mut failed): (Vec<(Value, PathBuf)>, Vec<Value>) = (vec![], vec![]);
    let (mut skips, mut reclaims): (Vec<Value>, Vec<Value>) = (vec![], vec![]);
    let (mut total, mut reclaimable): (u64, u64) = (0, 0);
    for directory in directories {
        for marker in markers(directory) {
            let Some(mut record) = read_record(&marker) else {
                continue;
            };
            if crate::target_pool::is_ready(&record) {
                let size = pool_size(directory, &record);
                record["size_bytes"] = json!(size);
                total += size;
                reclaimable += idle_pool_bytes(directory, &record, filesystem);
                ready.push((record, directory.clone()));
            } else if record["state"] == "failed" {
                failed.push(record);
            }
        }
        let events = read_events(directory);
        if let Some(event) = events.get("skip").filter(|e| e.is_object()) {
            skips.push(event.clone());
        }
        if let Some(event) = events.get("reclaim").filter(|e| e.is_object()) {
            reclaims.push(event.clone());
        }
    }
    ready.sort_by(|a, b| built_at(&b.0).total_cmp(&built_at(&a.0)));
    let newest = ready.first();
    let mut summary = Map::new();
    summary.insert("pools".into(), json!(ready.len()));
    summary.insert("total_bytes".into(), json!(total));
    summary.insert("reclaimable_bytes".into(), json!(reclaimable));
    summary.insert("newest".into(), Value::Null);
    summary.insert("failed_newer".into(), json!(0));
    summary.insert("failed_latest".into(), Value::Null);
    summary.insert("last_skip".into(), Value::Null);
    summary.insert("last_reclaim".into(), Value::Null);
    let newest_built = newest.map_or(0.0, |(record, _)| built_at(record));
    if let Some((record, home)) = newest {
        summary.insert(
            "newest".into(),
            json!({
                "sha": record["sha"],
                "built_at": record["built_at"],
                "degraded": record["degraded"].as_bool().unwrap_or(false),
                "size_bytes": record["size_bytes"],
                "age_commits": pool_age_commits(git, record, reference, known_repo(home).as_deref()),
            }),
        );
        let newer = failed.iter().filter(|r| built_at(r) > newest_built).count();
        summary.insert("failed_newer".into(), json!(newer));
    }
    let latest = failed
        .iter()
        .max_by(|a, b| built_at(a).total_cmp(&built_at(b)));
    if let Some(latest) = latest {
        if newest.is_none() || built_at(latest) > newest_built {
            summary.insert(
                "failed_latest".into(),
                json!({
                    "sha": latest["sha"],
                    "built_at": latest["built_at"],
                    "reason": latest["reason"],
                    "detail": latest["detail"],
                }),
            );
        }
    }
    // Why no pool was built (#2773): a skip older than the newest pool is history, not a cause.
    let by_time = |a: &&Value, b: &&Value| float_of(&a["at"]).total_cmp(&float_of(&b["at"]));
    if let Some(skip) = skips
        .iter()
        .filter(|event| float_of(&event["at"]) > newest_built)
        .max_by(by_time)
    {
        summary.insert("last_skip".into(), skip.clone());
    }
    if let Some(reclaim) = reclaims.iter().max_by(by_time) {
        summary.insert("last_reclaim".into(), reclaim.clone());
    }
    Value::Object(summary)
}

pub fn summary_command(
    repo: Option<&Path>,
    reference: &str,
    filesystem: Option<&Path>,
    env: &Env,
    out: &mut dyn Write,
) -> io::Result<i32> {
    let git = Git { env };
    let directories = directories_for(&git, env, repo)?;
    writeln!(
        out,
        "{}",
        dumps(&summarise(&git, &directories, reference, filesystem), None)
    )?;
    Ok(0)
}

pub fn status_command(repo: Option<&Path>, env: &Env, out: &mut dyn Write) -> io::Result<i32> {
    let git = Git { env };
    for directory in directories_for(&git, env, repo)? {
        for marker in markers(&directory) {
            if let Ok(text) = fs::read_to_string(&marker) {
                writeln!(out, "{}", text.trim())?;
            }
        }
    }
    Ok(0)
}

#[allow(dead_code)]
const _: &str = REPO_FILE;

#[cfg(test)]
#[path = "target_pool_ops_tests.rs"]
mod tests;
