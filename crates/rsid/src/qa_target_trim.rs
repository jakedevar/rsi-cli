//! Trim the shared full-library QA Cargo target after each run (#2509).
//!
//! Every automatic QA run builds in one registered, reused target so the
//! registry dependencies stay warm. Cargo never deletes a superseded artifact,
//! and a run's snapshot lives at a new path (`rolling-library-qa/<id>/source`),
//! which gives each workspace crate a new metadata hash: every run adds a full
//! set of `rsid*` test binaries, fingerprints, build-script output and
//! incremental directories next to the last run's, which nothing will use
//! again. Left alone the target grew to 79 GB on the hub.
//!
//! [`trim_superseded`] removes exactly those, while it holds the target's job
//! admission lock (every build unit holds it for its whole run, so a trim never
//! races a running QA job):
//!
//! * **Cutoff.** A stamp file in the target root records when the previous trim
//!   finished. Anything the runs since then wrote has a later mtime. The first
//!   trim only plants the stamp: it has no cutoff and removes nothing.
//! * **Supersession.** Entries are grouped by crate name (`deps/` files by their
//!   unit hash, `.fingerprint/`, `build/` and `incremental/` by directory). A
//!   group with at least one entry touched since the cutoff keeps those and
//!   loses its older siblings: they were built for a different hash and the
//!   run that touched the group no longer uses them. A group nothing touched
//!   (a registry dependency a run found fresh) is kept whole, so the target
//!   stays warm for dependencies.
//! * **Bounded.** The operator cap `qa_target_max_gib` (`rsid_core::qa_target_cap`)
//!   is the backstop: `agent_jobs` empties a target above it before the next
//!   run starts (`release_target::enforce_size_cap`).
//!
//! The satellite empties the target outright after every settled hub run
//! (`satellite_qa::reclaim_settled_runs`, #2176), so it never accumulates
//! either; its own queue runs go through the same trim.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use crate::error::{DaemonError, Result};
use crate::release_target::{held_by_process, lock_probe, measure, take_admission};
use crate::store::Store;

/// The stamp file in the target root. Its mtime is when the last trim ended.
pub const TRIM_STAMP: &str = ".rsi-qa-trim-stamp";
/// Cargo profile directories a trim looks into.
const PROFILES: [&str; 2] = ["debug", "release"];
/// Directory entries one measurement may visit.
const MAX_SCAN_ENTRIES: u64 = 2_000_000;

/// What one [`trim_superseded`] pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrimOutcome {
    /// The target does not exist.
    Absent,
    /// A build or another process holds the target; nothing was touched.
    Busy,
    /// No stamp yet: one was planted and nothing was removed.
    Primed,
    /// No run wrote anything since the last trim.
    Idle,
    /// Superseded artifacts were removed.
    Trimmed { units: usize, bytes: u64 },
}

/// One removable thing: the files or directory of one build unit.
struct Unit {
    /// Crate name, `-` normalised to `_`.
    name: String,
    paths: Vec<PathBuf>,
    newest: SystemTime,
}

/// Remove the artifacts of `dir`'s profiles that a newer run superseded. See
/// the module docs for the rules.
///
/// # Errors
/// An I/O error reading the target, or removing an entry.
pub fn trim_superseded(dir: &Path) -> io::Result<TrimOutcome> {
    if !dir.is_dir() {
        return Ok(TrimOutcome::Absent);
    }
    let Some(_admission) = take_admission(dir)? else {
        return Ok(TrimOutcome::Busy);
    };
    for profile in PROFILES {
        if lock_probe(&dir.join(profile).join(".cargo-lock"))?.is_none() {
            return Ok(TrimOutcome::Busy);
        }
    }
    if held_by_process(dir) {
        return Ok(TrimOutcome::Busy);
    }
    let stamp = dir.join(TRIM_STAMP);
    let Some(cutoff) = fs::metadata(&stamp).and_then(|meta| meta.modified()).ok() else {
        fs::write(&stamp, b"")?;
        return Ok(TrimOutcome::Primed);
    };
    let (mut units, mut bytes, mut active) = (0usize, 0u64, false);
    for profile in PROFILES {
        let root = dir.join(profile);
        if !written_since(&root, cutoff) {
            continue;
        }
        active = true;
        trim_profile(&root, cutoff, &mut units, &mut bytes)?;
    }
    if !active {
        return Ok(TrimOutcome::Idle);
    }
    // Rewriting moves the mtime to now: the next cutoff.
    fs::write(&stamp, b"")?;
    Ok(TrimOutcome::Trimmed { units, bytes })
}

/// True when a run created or removed anything in one of the profile's
/// artifact directories after `cutoff` (one `stat` each).
fn written_since(profile: &Path, cutoff: SystemTime) -> bool {
    ["deps", ".fingerprint", "build", "incremental"]
        .iter()
        .any(|name| {
            fs::metadata(profile.join(name))
                .and_then(|meta| meta.modified())
                .is_ok_and(|mtime| mtime > cutoff)
        })
}

fn trim_profile(
    profile: &Path,
    cutoff: SystemTime,
    units: &mut usize,
    bytes: &mut u64,
) -> io::Result<()> {
    let sweeps = [
        (deps_units(&profile.join("deps"))?, "deps"),
        (
            dir_units(&profile.join(".fingerprint"), is_metadata_hash)?,
            ".fingerprint",
        ),
        (
            dir_units(&profile.join("build"), is_metadata_hash)?,
            "build",
        ),
        (
            dir_units(&profile.join("incremental"), is_incremental_hash)?,
            "incremental",
        ),
    ];
    for (all, area) in sweeps {
        tracing::debug!(area, candidates = all.len(), "QA target trim");
        sweep(all, cutoff, units, bytes)?;
    }
    Ok(())
}

/// Remove the units of every group that a run touched, except the touched ones.
fn sweep(all: Vec<Unit>, cutoff: SystemTime, units: &mut usize, bytes: &mut u64) -> io::Result<()> {
    let mut groups: HashMap<String, Vec<Unit>> = HashMap::new();
    for unit in all {
        groups.entry(unit.name.clone()).or_default().push(unit);
    }
    for group in groups.into_values() {
        if !group.iter().any(|unit| unit.newest > cutoff) {
            continue;
        }
        for unit in group.into_iter().filter(|unit| unit.newest <= cutoff) {
            for path in &unit.paths {
                *bytes = bytes.saturating_add(size_of(path));
                remove(path)?;
            }
            *units += 1;
        }
    }
    Ok(())
}

fn size_of(path: &Path) -> u64 {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => measure(path, MAX_SCAN_ENTRIES).unwrap_or(0),
        Ok(meta) => meta.len(),
        Err(_) => 0,
    }
}

fn remove(path: &Path) -> io::Result<()> {
    let removed = match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) => Err(error),
    };
    match removed {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// The newest mtime of `path`: a file's own, or for a directory the newest of
/// itself and its direct children (a rebuild rewrites files in place without
/// touching the parent).
fn newest_mtime(path: &Path) -> Option<SystemTime> {
    let meta = fs::symlink_metadata(path).ok()?;
    let mut newest = meta.modified().ok()?;
    if meta.is_dir() {
        for child in fs::read_dir(path).ok()?.flatten() {
            if let Ok(mtime) = child.metadata().and_then(|meta| meta.modified()) {
                newest = newest.max(mtime);
            }
        }
    }
    Some(newest)
}

/// 16 lowercase hex digits: the `-C metadata` hash of a unit.
fn is_metadata_hash(hash: &str) -> bool {
    hash.len() == 16 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// An incremental-compilation directory suffix (lowercase base 36).
fn is_incremental_hash(hash: &str) -> bool {
    hash.len() >= 8
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
}

/// `<crate>-<hash>` -> (normalised crate, hash).
fn split_hash(stem: &str, valid: fn(&str) -> bool) -> Option<(String, &str)> {
    let (name, hash) = stem.rsplit_once('-')?;
    (!name.is_empty() && valid(hash)).then(|| (name.replace('-', "_"), hash))
}

/// The units of a directory whose entries are named `<crate>-<hash>`.
fn dir_units(dir: &Path, valid: fn(&str) -> bool) -> io::Result<Vec<Unit>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some((name, _)) = name.to_str().and_then(|stem| split_hash(stem, valid)) else {
            continue;
        };
        let Some(newest) = newest_mtime(&entry.path()) else {
            continue;
        };
        out.push(Unit {
            name,
            paths: vec![entry.path()],
            newest,
        });
    }
    Ok(out)
}

/// The units of `deps/`: every file of one hash is one unit (`rsid-<h>`,
/// `rsid-<h>.d`, `librsid-<h>.rlib`, `librsid-<h>.rmeta`, ...). The dep-info
/// file carries the crate name without a `lib` prefix. Build-script compile
/// units share the name `build_script_*` across unrelated crates: they are
/// never grouped, only `build/` and `.fingerprint/` say which crate they are.
fn deps_units(dir: &Path) -> io::Result<Vec<Unit>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    struct Acc {
        name: Option<String>,
        fallback: String,
        paths: Vec<PathBuf>,
        newest: SystemTime,
    }
    let mut by_hash: HashMap<String, Acc> = HashMap::new();
    for entry in entries.flatten() {
        let file = entry.file_name();
        let Some(file) = file.to_str() else { continue };
        let (stem, extension) = file.split_once('.').map_or((file, ""), |(a, b)| (a, b));
        let Some((name, hash)) = split_hash(stem, is_metadata_hash) else {
            continue;
        };
        let Some(newest) = newest_mtime(&entry.path()) else {
            continue;
        };
        let acc = by_hash.entry(hash.to_string()).or_insert_with(|| Acc {
            name: None,
            fallback: name.clone(),
            paths: Vec::new(),
            newest,
        });
        if extension == "d" {
            acc.name = Some(name);
        } else if !extension.is_empty() && acc.fallback.starts_with("lib") {
            acc.fallback = acc.fallback["lib".len()..].to_string();
        }
        acc.paths.push(entry.path());
        acc.newest = acc.newest.max(newest);
    }
    Ok(by_hash
        .into_values()
        .filter_map(|acc| {
            let name = acc.name.unwrap_or(acc.fallback);
            (!name.starts_with("build_script_")).then_some(Unit {
                name,
                paths: acc.paths,
                newest: acc.newest,
            })
        })
        .collect())
}

/// #2509: the backstop to [`trim_superseded`]. A shared QA target still over
/// `cap` bytes (`None`: no cap) is emptied of its Cargo output before the next
/// run, while its admission lock is held; a busy or process-held one is kept.
pub(crate) fn enforce_cap(target: &Path, cap: Option<u64>) {
    use crate::release_target::{CapOutcome, enforce_size_cap};
    let Some(cap) = cap else { return };
    match enforce_size_cap(target, cap) {
        Ok(CapOutcome::Reclaimed { bytes, retained }) => tracing::info!(
            target = %target.display(),
            bytes,
            cap,
            retained,
            "QA target exceeded its size cap and its Cargo output was removed"
        ),
        Ok(CapOutcome::Held { bytes }) => tracing::warn!(
            target = %target.display(),
            bytes,
            cap,
            "QA target exceeded its size cap but a process holds it; kept"
        ),
        Ok(_) => {}
        Err(error) => tracing::warn!(
            target = %target.display(),
            %error,
            "cannot enforce the QA target size cap"
        ),
    }
}

/// Set after a QA job is seen running (and at start): the next idle pass
/// trims. A `Busy` pass keeps it set so the next tick retries.
pub(crate) static TRIM_PENDING: AtomicBool = AtomicBool::new(true);

/// The jobs loop's hook: once no full-library QA job is queued or running and
/// a run may have written since the last trim, trim the shared QA target.
/// `target` resolves it lazily (production:
/// `agent_jobs::library_qa_target`), and is only called when a QA run ever
/// happened under `jobs`.
///
/// # Errors
/// A store or filesystem error.
pub(crate) async fn trim_settled_target<T>(
    store: &Arc<tokio::sync::Mutex<Store>>,
    jobs: &Path,
    pending: &AtomicBool,
    target: T,
) -> Result<Option<TrimOutcome>>
where
    T: FnOnce() -> io::Result<PathBuf> + Send + 'static,
{
    if crate::rolling_queue::satellite_qa::library_qa_active(&*store.lock().await)? {
        pending.store(true, Ordering::Relaxed);
        return Ok(None);
    }
    if !pending.load(Ordering::Relaxed) {
        return Ok(None);
    }
    if !jobs.join(crate::agent_jobs::LIBRARY_QA_RECIPE).is_dir() {
        pending.store(false, Ordering::Relaxed);
        return Ok(None);
    }
    let outcome = tokio::task::spawn_blocking(move || -> Result<TrimOutcome> {
        let target =
            target().map_err(|error| DaemonError::Process(format!("QA target: {error}")))?;
        Ok(trim_superseded(&target)?)
    })
    .await
    .map_err(|error| DaemonError::Process(format!("QA target trim: {error}")))??;
    if outcome != TrimOutcome::Busy {
        pending.store(false, Ordering::Relaxed);
    }
    Ok(Some(outcome))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn at(base: SystemTime, secs: u64) -> SystemTime {
        base + Duration::from_secs(secs)
    }

    fn write(path: &Path, bytes: usize, mtime: SystemTime) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, vec![1u8; bytes]).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
    }

    fn set_dir_mtime(path: &Path, mtime: SystemTime) {
        fs::File::open(path).unwrap().set_modified(mtime).unwrap();
    }

    /// One workspace unit (`rsid`) and one registry dependency (`serde`) in
    /// `debug`, built at `built`.
    fn unit(profile: &Path, name: &str, hash: &str, bytes: usize, built: SystemTime) {
        let lib = name.replace('-', "_");
        write(&profile.join(format!("deps/{lib}-{hash}")), bytes, built);
        write(&profile.join(format!("deps/{lib}-{hash}.d")), 1, built);
        write(
            &profile.join(format!("deps/lib{lib}-{hash}.rlib")),
            bytes,
            built,
        );
        write(
            &profile.join(format!(".fingerprint/{name}-{hash}/lib-{lib}")),
            1,
            built,
        );
        write(
            &profile.join(format!("build/{name}-{hash}/output")),
            1,
            built,
        );
        for dir in [
            profile.join(format!(".fingerprint/{name}-{hash}")),
            profile.join(format!("build/{name}-{hash}")),
        ] {
            set_dir_mtime(&dir, built);
        }
    }

    fn incremental(profile: &Path, name: &str, hash: &str, built: SystemTime) {
        write(
            &profile.join(format!("incremental/{name}-{hash}/s-1-abc/dep-graph.bin")),
            4,
            built,
        );
        set_dir_mtime(
            &profile.join(format!("incremental/{name}-{hash}/s-1-abc")),
            built,
        );
        set_dir_mtime(&profile.join(format!("incremental/{name}-{hash}")), built);
    }

    fn exists(profile: &Path, rel: &str) -> bool {
        profile.join(rel).exists()
    }

    /// A target with a stamp at `stamp`.
    fn target(stamp: SystemTime) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("rsi-build-qa");
        fs::create_dir_all(dir.join("debug")).unwrap();
        write(&dir.join(TRIM_STAMP), 0, stamp);
        (root, dir)
    }

    const OLD: &str = "aaaaaaaaaaaaaaaa";
    const NEW: &str = "bbbbbbbbbbbbbbbb";
    const SERDE: &str = "cccccccccccccccc";
    // Each unit has its own hash: the store crate's, in the same two runs.
    const OLD_STORE: &str = "dddddddddddddddd";
    const NEW_STORE: &str = "eeeeeeeeeeeeeeee";

    /// #2509: a trim keeps the newest run's artifacts and removes the older
    /// run's; a dependency no run touched stays warm.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_trim_keeps_the_newest_runs_artifacts_and_removes_the_superseded_ones() {
        let base = SystemTime::now() - Duration::from_secs(10_000);
        let (_root, dir) = target(at(base, 100));
        let debug = dir.join("debug");
        // The previous run (before the stamp) and the registry dependency.
        unit(&debug, "rsid", OLD, 10, at(base, 50));
        unit(&debug, "rsid-store", OLD_STORE, 10, at(base, 50));
        unit(&debug, "serde", SERDE, 10, at(base, 50));
        incremental(&debug, "rsid", "1abcdefgh2ijk", at(base, 50));
        // This run (after the stamp) rebuilt the workspace at a new path.
        unit(&debug, "rsid", NEW, 10, at(base, 200));
        unit(&debug, "rsid-store", NEW_STORE, 10, at(base, 200));
        incremental(&debug, "rsid", "9zyxwvuts8rqp", at(base, 200));
        for sub in ["deps", ".fingerprint", "build", "incremental"] {
            set_dir_mtime(&debug.join(sub), at(base, 200));
        }

        let outcome = trim_superseded(&dir).unwrap();
        assert!(
            matches!(outcome, TrimOutcome::Trimmed { units, bytes } if units > 0 && bytes >= 40),
            "{outcome:?}"
        );
        for kept in [
            format!("deps/rsid-{NEW}"),
            format!("deps/rsid-{NEW}.d"),
            format!("deps/librsid-{NEW}.rlib"),
            format!("deps/rsid_store-{NEW_STORE}"),
            format!(".fingerprint/rsid-{NEW}"),
            format!(".fingerprint/rsid-store-{NEW_STORE}"),
            format!("build/rsid-{NEW}"),
            "incremental/rsid-9zyxwvuts8rqp".to_string(),
            format!("deps/serde-{SERDE}"),
            format!("deps/libserde-{SERDE}.rlib"),
            format!(".fingerprint/serde-{SERDE}"),
            format!("build/serde-{SERDE}"),
        ] {
            assert!(exists(&debug, &kept), "kept {kept}");
        }
        for gone in [
            format!("deps/rsid-{OLD}"),
            format!("deps/rsid-{OLD}.d"),
            format!("deps/librsid-{OLD}.rlib"),
            format!("deps/rsid_store-{OLD_STORE}"),
            format!("deps/librsid_store-{OLD_STORE}.rlib"),
            format!(".fingerprint/rsid-{OLD}"),
            format!(".fingerprint/rsid-store-{OLD_STORE}"),
            format!("build/rsid-{OLD}"),
            "incremental/rsid-1abcdefgh2ijk".to_string(),
        ] {
            assert!(!exists(&debug, &gone), "removed {gone}");
        }
        // The next trim starts from this one: nothing new, nothing to do.
        assert_eq!(trim_superseded(&dir).unwrap(), TrimOutcome::Idle);
    }

    /// The first pass has no cutoff: it plants the stamp and removes nothing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_first_trim_only_plants_the_stamp() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("t");
        let debug = dir.join("debug");
        let base = SystemTime::now() - Duration::from_secs(1_000);
        unit(&debug, "rsid", OLD, 10, at(base, 1));
        unit(&debug, "rsid", NEW, 10, at(base, 2));
        assert_eq!(trim_superseded(&dir).unwrap(), TrimOutcome::Primed);
        assert!(dir.join(TRIM_STAMP).is_file());
        assert!(exists(&debug, &format!("deps/rsid-{OLD}")));
        assert!(exists(&debug, &format!("deps/rsid-{NEW}")));
        assert_eq!(
            trim_superseded(&root.path().join("missing")).unwrap(),
            TrimOutcome::Absent
        );
    }

    /// A run that wrote nothing (a refusal before the build) leaves everything.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_trim_after_a_run_that_built_nothing_removes_nothing() {
        let base = SystemTime::now() - Duration::from_secs(10_000);
        let (_root, dir) = target(at(base, 100));
        let debug = dir.join("debug");
        unit(&debug, "rsid", OLD, 10, at(base, 50));
        for sub in ["deps", ".fingerprint", "build"] {
            set_dir_mtime(&debug.join(sub), at(base, 50));
        }
        assert_eq!(trim_superseded(&dir).unwrap(), TrimOutcome::Idle);
        assert!(exists(&debug, &format!("deps/rsid-{OLD}")));
    }

    /// A build holding the admission lock is never raced.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_trim_never_races_a_running_job() {
        let base = SystemTime::now() - Duration::from_secs(10_000);
        let (_root, dir) = target(at(base, 100));
        let debug = dir.join("debug");
        unit(&debug, "rsid", OLD, 10, at(base, 50));
        unit(&debug, "rsid", NEW, 10, at(base, 200));
        set_dir_mtime(&debug.join("deps"), at(base, 200));
        let held = take_admission(&dir).unwrap().expect("lock is free");
        assert_eq!(trim_superseded(&dir).unwrap(), TrimOutcome::Busy);
        assert!(exists(&debug, &format!("deps/rsid-{OLD}")));
        drop(held);
        assert!(matches!(
            trim_superseded(&dir).unwrap(),
            TrimOutcome::Trimmed { .. }
        ));
        assert!(!exists(&debug, &format!("deps/rsid-{OLD}")));
    }

    /// A target above the operator cap is emptied before the run; one within
    /// it, or with no cap, is left alone.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_target_over_the_cap_is_emptied_and_one_within_it_is_kept() {
        let base = SystemTime::now() - Duration::from_secs(10_000);
        let (_root, dir) = target(at(base, 100));
        let debug = dir.join("debug");
        unit(&debug, "rsid", NEW, 2048, at(base, 200));
        enforce_cap(&dir, None);
        enforce_cap(&dir, Some(1024 * 1024));
        assert!(exists(&debug, &format!("deps/rsid-{NEW}")));
        enforce_cap(&dir, Some(1024));
        assert!(!debug.exists(), "the Cargo output is gone");
        assert!(
            dir.join(TRIM_STAMP).is_file(),
            "the stamp is not Cargo output"
        );
    }

    fn insert_qa_job(store: &Store) {
        use crate::store::agent_jobs::NewAgentJob;
        use rsi_common::agent_jobs::{JobParams, JobWake, TestJobParams};
        let id = uuid::Uuid::new_v4();
        store
            .insert_agent_job(
                &NewAgentJob {
                    id,
                    owner_session_id: uuid::Uuid::new_v4(),
                    project_id: None,
                    name: None,
                    params: JobParams::Test(TestJobParams {
                        qa_lane: None,
                        scoped_test: None,
                        recipe: Some(crate::agent_jobs::LIBRARY_QA_RECIPE.into()),
                        shard: None,
                        filterset: None,
                        package: None,
                        filters: Vec::new(),
                        exact: false,
                        lib_only: false,
                        test_target: None,
                        candidate_receipt: None,
                        timeout_minutes: Some(180),
                        cpu_quota_percent: None,
                    }),
                    cwd: "/x".into(),
                    unit_name: format!("qa-{id}"),
                    log_path: "/x/qa.log".into(),
                    status_path: "/x/qa.status".into(),
                    idempotency_key: None,
                    wake: JobWake::None,
                },
                chrono::Utc::now(),
            )
            .unwrap();
    }

    /// The loop hook never touches the target while a QA job is queued or
    /// running (they share it), and trims once none is.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn the_loop_hook_waits_for_the_active_qa_job_then_trims_once() {
        let base = SystemTime::now() - Duration::from_secs(10_000);
        let (root, dir) = target(at(base, 100));
        let jobs = root.path().join("jobs");
        fs::create_dir_all(jobs.join(crate::agent_jobs::LIBRARY_QA_RECIPE)).unwrap();
        let debug = dir.join("debug");
        unit(&debug, "rsid", OLD, 10, at(base, 50));
        unit(&debug, "rsid", NEW, 10, at(base, 200));
        set_dir_mtime(&debug.join("deps"), at(base, 200));
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        insert_qa_job(&*store.lock().await);
        let pending = AtomicBool::new(false);
        let resolve = |dir: &Path| {
            let dir = dir.to_path_buf();
            move || Ok(dir)
        };

        let outcome = trim_settled_target(&store, &jobs, &pending, resolve(&dir))
            .await
            .unwrap();
        assert_eq!(outcome, None, "a queued QA job holds the trim off");
        assert!(pending.load(Ordering::Relaxed), "and owes the next pass");
        assert!(exists(&debug, &format!("deps/rsid-{OLD}")));

        // The job settled (a fresh store has none queued or running).
        let store = Arc::new(tokio::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let outcome = trim_settled_target(&store, &jobs, &pending, resolve(&dir))
            .await
            .unwrap();
        assert!(
            matches!(outcome, Some(TrimOutcome::Trimmed { .. })),
            "{outcome:?}"
        );
        assert!(!exists(&debug, &format!("deps/rsid-{OLD}")));
        assert!(exists(&debug, &format!("deps/rsid-{NEW}")));
        assert!(!pending.load(Ordering::Relaxed));
        let again = trim_settled_target(&store, &jobs, &pending, resolve(&dir))
            .await
            .unwrap();
        assert_eq!(again, None, "nothing is owed until a QA job runs again");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn unit_names_split_on_the_hash_and_build_scripts_are_not_grouped() {
        assert_eq!(
            split_hash("rsid-store-0123456789abcdef", is_metadata_hash),
            Some(("rsid_store".to_string(), "0123456789abcdef"))
        );
        assert_eq!(split_hash("build-script-build", is_metadata_hash), None);
        assert_eq!(split_hash("rsid-0123", is_metadata_hash), None);
        let root = tempfile::tempdir().unwrap();
        let deps = root.path().join("deps");
        let now = SystemTime::now();
        write(&deps.join("build_script_build-0123456789abcdef"), 1, now);
        write(&deps.join("build_script_build-0123456789abcdef.d"), 1, now);
        write(&deps.join("README"), 1, now);
        assert!(deps_units(&deps).unwrap().is_empty());
    }
}
