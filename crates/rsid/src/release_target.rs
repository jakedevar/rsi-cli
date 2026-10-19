//! The one persistent, daemon-owned Cargo target for manager release builds
//! (#1218).
//!
//! A manager seat's `AgentSubmitJob` `build` with `release=true` used to compile
//! every crate cold (about 4.5 minutes) because it built in the seat's own
//! `<sandbox>/target`, which is fresh for each manager. Those builds now share
//! [`release_target_dir`], `~/.rsi/staging/release-target`, so a deploy
//! recompiles only what changed. `~/.rsi/staging` is already an accepted deploy
//! source root, so `<release target>/release/<bin>` deploys as built.
//!
//! * **Who.** Only a `build` job with `release=true` owned by a manager
//!   ([`uses_release_target`]). Worker and dev builds keep their per-sandbox
//!   target (#1090); the workspace crates of a different worktree path get a
//!   different Cargo metadata hash, so they never clobber each other, and
//!   registry dependencies are shared.
//! * **Serialization.** The directory carries the same job admission lock
//!   (`.rsi-job-admission.lock`) every Linux build unit holds exclusively for
//!   its whole run, so two builds never share the target at once. Cargo's own
//!   `release/.cargo-lock` is the fallback where no admission lock exists.
//! * **For the stray-target sweep (#1864).** [`RELEASE_TARGET_RELATIVE`] is the
//!   one named root. Treat it as known and daemon-owned: never reclaim it when
//!   [`is_busy`] reports a holder, and reclaim it with [`reclaim_contents`],
//!   which empties it but keeps the lock files (a build waiting on an unlinked
//!   lock inode would otherwise race the next one).
//! * **Bounded.** [`enforce_size_cap`] runs before each release build and
//!   reclaims the target's Cargo output when it exceeds
//!   [`RELEASE_TARGET_MAX_BYTES`] (#1894). It deletes only the known Cargo
//!   layout ([`CARGO_OUTPUT`]); any other entry (a manager state file, a
//!   fixture checkout), any known entry that holds a nested Git repository, and
//!   the whole target while a process has its cwd, root, exe or an open file
//!   there, are retained.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use rsi_common::agent_jobs::JobParams;

use crate::shared_target_prune::try_lock_exclusive;

/// Where the release target lives, relative to the data directory
/// (`~/.rsi`). Inside `staging/` so a deploy accepts its binaries.
pub const RELEASE_TARGET_RELATIVE: &str = "staging/release-target";
/// Size above which the whole target is reclaimed before the next build. A
/// warm release target of this workspace is far below it.
pub const RELEASE_TARGET_MAX_BYTES: u64 = 16 * 1024 * 1024 * 1024;
/// Directory entries one size measurement may visit; a larger tree is treated
/// as over the cap.
pub const MAX_SCAN_ENTRIES: u64 = 2_000_000;
/// Build admission lock: held exclusively by a Linux build unit for its run.
pub const ADMISSION_LOCK: &str = ".rsi-job-admission.lock";
/// The shard runner's artifact lock, held shared by every job in the target.
pub const ARTIFACT_LOCK: &str = ".rsid-test-shards.lock";
/// Entries a reclaim keeps: the lock files the build units flock on.
const KEPT: [&str; 2] = [ADMISSION_LOCK, ARTIFACT_LOCK];
/// The only top-level entries a reclaim may delete: what Cargo itself writes
/// for a host build. Anything else is unknown content and is retained.
pub const CARGO_OUTPUT: [&str; 7] = [
    "release",
    "debug",
    "doc",
    "tmp",
    "package",
    ".rustc_info.json",
    "CACHEDIR.TAG",
];
/// Directory entries one nested-repository search may visit per entry.
const MAX_GIT_SCAN: u64 = 2_000_000;

/// The release target under an explicit data directory.
#[must_use]
pub fn release_target_dir_in(data_dir: &Path) -> PathBuf {
    data_dir.join(RELEASE_TARGET_RELATIVE)
}

/// The release target under `~/.rsi`.
#[must_use]
pub fn release_target_dir() -> PathBuf {
    release_target_dir_in(&rsi_common::identity::data_dir())
}

/// A manager's release build shares the persistent target; every other job
/// builds in its own.
#[must_use]
pub fn uses_release_target(params: &JobParams, owner_is_manager: bool) -> bool {
    owner_is_manager && matches!(params, JobParams::Build(build) if build.release)
}

/// The target directory one job builds in: the shared release target for a
/// manager release build, else the `scoped` per-worktree directory.
#[must_use]
pub fn select_job_target(
    params: &JobParams,
    owner_is_manager: bool,
    scoped: PathBuf,
    data_dir: &Path,
) -> PathBuf {
    if uses_release_target(params, owner_is_manager) {
        release_target_dir_in(data_dir)
    } else {
        scoped
    }
}

/// True while a build holds the target: its admission lock or Cargo's own
/// build lock cannot be taken. A target that does not exist is idle. The
/// sweep must keep a busy target.
///
/// # Errors
/// An I/O error other than a held lock.
pub fn is_busy(dir: &Path) -> io::Result<bool> {
    Ok(lock_probe(&dir.join(ADMISSION_LOCK))?.is_none()
        || lock_probe(&dir.join("release").join(".cargo-lock"))?.is_none())
}

/// Try to take `path`'s lock without creating it. `Ok(None)` means another
/// holder has it; `Ok(Some(None))` means the file is absent (nothing holds
/// it); `Ok(Some(Some(file)))` holds the lock until the file drops.
pub(crate) fn lock_probe(path: &Path) -> io::Result<Option<Option<File>>> {
    let file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Some(None)),
        Err(error) => return Err(error),
    };
    Ok(try_lock_exclusive(&file)?.then_some(Some(file)))
}

/// What [`enforce_size_cap`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapOutcome {
    /// The target does not exist yet.
    Absent,
    /// A build holds the target; nothing was measured or removed.
    Busy,
    /// Within the cap.
    WithinCap { bytes: u64 },
    /// Over the cap; the known Cargo output was removed. `retained` counts the
    /// top-level entries kept as unknown or as holding nested Git work.
    Reclaimed { bytes: u64, retained: usize },
    /// Over the cap, but a process holds the target by path; nothing removed.
    Held { bytes: u64 },
}

/// Reclaim the Cargo output of `dir` when it holds more than `cap` bytes,
/// while holding its admission lock so no build starts mid-removal. A busy or
/// process-held target is left alone and unknown or Git-bearing entries are
/// retained; the next release build retries.
///
/// # Errors
/// An I/O error measuring or removing.
pub fn enforce_size_cap(dir: &Path, cap: u64) -> io::Result<CapOutcome> {
    if !dir.is_dir() {
        return Ok(CapOutcome::Absent);
    }
    let Some(_admission) = take_admission(dir)? else {
        return Ok(CapOutcome::Busy);
    };
    if lock_probe(&dir.join("release").join(".cargo-lock"))?.is_none() {
        return Ok(CapOutcome::Busy);
    }
    let bytes = measure(dir, MAX_SCAN_ENTRIES).unwrap_or(u64::MAX);
    if bytes <= cap {
        return Ok(CapOutcome::WithinCap { bytes });
    }
    if held_by_process(dir) {
        return Ok(CapOutcome::Held { bytes });
    }
    let retained = remove_contents(dir)?;
    Ok(CapOutcome::Reclaimed { bytes, retained })
}

/// Empty the target for the stray-target sweep, refusing while a build holds
/// it. Returns false when it was busy or absent.
///
/// # Errors
/// An I/O error removing.
pub fn reclaim_contents(dir: &Path) -> io::Result<bool> {
    if !dir.is_dir() {
        return Ok(false);
    }
    let Some(_admission) = take_admission(dir)? else {
        return Ok(false);
    };
    if lock_probe(&dir.join("release").join(".cargo-lock"))?.is_none() {
        return Ok(false);
    }
    if held_by_process(dir) {
        return Ok(false);
    }
    remove_contents(dir)?;
    Ok(true)
}

/// What [`reclaim_build_output`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputReclaim {
    /// The target does not exist.
    Absent,
    /// A build or another process holds the target; nothing was removed.
    Busy,
    /// Its build output was removed. `bytes` is what the target held before
    /// (`None` when it was too large to measure).
    Reclaimed { bytes: Option<u64> },
}

/// #2176: empty a daemon-owned registered Cargo target after a run, so its
/// disk returns: every directory in it (profiles, `tmp`, tool subtargets) is
/// removed while its admission lock is held, and its regular files (the
/// allocation record, `CACHEDIR.TAG`, `.rustc_info.json`, the lock files) are
/// kept so the registered allocation stays valid for the next run. A busy or
/// process-held target is left alone.
///
/// # Errors
/// An I/O error removing.
pub fn reclaim_build_output(dir: &Path) -> io::Result<OutputReclaim> {
    if !dir.is_dir() {
        return Ok(OutputReclaim::Absent);
    }
    let Some(_admission) = take_admission(dir)? else {
        return Ok(OutputReclaim::Busy);
    };
    for profile in ["debug", "release"] {
        if lock_probe(&dir.join(profile).join(".cargo-lock"))?.is_none() {
            return Ok(OutputReclaim::Busy);
        }
    }
    if held_by_process(dir) {
        return Ok(OutputReclaim::Busy);
    }
    let bytes = measure(dir, MAX_SCAN_ENTRIES);
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(OutputReclaim::Reclaimed { bytes })
}

/// Take (creating if needed) the exclusive admission lock; `None` if held.
pub(crate) fn take_admission(dir: &Path) -> io::Result<Option<File>> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(ADMISSION_LOCK))?;
    Ok(try_lock_exclusive(&file)?.then_some(file))
}

/// Remove the known Cargo output of `dir`, keeping the lock files, unknown
/// entries and any entry that contains a nested Git repository. Returns how
/// many top-level entries were retained for those reasons.
fn remove_contents(dir: &Path) -> io::Result<usize> {
    let mut retained = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if KEPT.iter().any(|kept| name == *kept) {
            continue;
        }
        let path = entry.path();
        let kind = entry.file_type()?;
        let known = CARGO_OUTPUT.iter().any(|known| name == *known);
        if !known || (kind.is_dir() && contains_git(&path)) {
            retained += 1;
            continue;
        }
        if kind.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(retained)
}

/// True when a Git repository (a `.git` entry) sits anywhere under `dir`, or
/// the search cannot finish; symlinks are not followed.
fn contains_git(dir: &Path) -> bool {
    let mut seen = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = fs::read_dir(&next) else {
            return true;
        };
        for entry in entries {
            let Ok(entry) = entry else { return true };
            seen += 1;
            if seen > MAX_GIT_SCAN || entry.file_name() == ".git" {
                return true;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(entry.path()),
                Ok(_) => {}
                Err(_) => return true,
            }
        }
    }
    false
}

/// True when some process has its cwd, root, exe or an open descriptor under
/// `dir`, other than this daemon itself (which holds only the admission lock).
/// Linux only; elsewhere the build locks are the only proof. A process that
/// exited mid-scan or belongs to another user cannot be one of ours.
#[cfg(target_os = "linux")]
pub(crate) fn held_by_process(dir: &Path) -> bool {
    let Ok(dir) = fs::canonicalize(dir) else {
        return true;
    };
    let Ok(procs) = fs::read_dir("/proc") else {
        return true;
    };
    let under = |link: PathBuf| fs::read_link(link).is_ok_and(|target| target.starts_with(&dir));
    let own = std::process::id().to_string();
    procs.flatten().any(|proc| {
        let name = proc.file_name();
        if !name
            .to_str()
            .is_some_and(|name| name != own && name.bytes().all(|b| b.is_ascii_digit()))
        {
            return false;
        }
        let base = proc.path();
        ["cwd", "root", "exe"]
            .iter()
            .any(|name| under(base.join(name)))
            || fs::read_dir(base.join("fd"))
                .is_ok_and(|fds| fds.flatten().any(|fd| under(fd.path())))
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn held_by_process(_dir: &Path) -> bool {
    false
}

/// Total file bytes under `dir` (symlinks not followed); `None` once more than
/// `max_entries` entries were visited.
pub(crate) fn measure(dir: &Path, max_entries: u64) -> Option<u64> {
    let mut seen = 0u64;
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > max_entries {
                return None;
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(entry.path());
            } else if let Ok(meta) = entry.metadata() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::agent_jobs::{BuildCommand, BuildJobParams};

    fn build(release: bool) -> JobParams {
        JobParams::Build(BuildJobParams {
            command: BuildCommand::Build,
            package: None,
            workspace: true,
            all_targets: false,
            release,
            no_default_features: false,
            features: Vec::new(),
            shards: Vec::new(),
            cpu_quota_percent: None,
            sha: None,
        })
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn manager_release_builds_use_the_shared_target_and_everything_else_its_own() {
        let data = Path::new("/home/op/.rsi");
        let scoped = PathBuf::from("/sandbox/a/target");
        let shared = data.join("staging/release-target");
        let pick =
            |params: &JobParams, manager| select_job_target(params, manager, scoped.clone(), data);
        let (release, dev) = (build(true), build(false));
        assert_eq!(pick(&release, true), shared);
        // A worker's release build and any dev build stay in the sandbox.
        assert_eq!(pick(&release, false), scoped);
        assert_eq!(pick(&dev, true), scoped);
        assert_eq!(pick(&dev, false), scoped);
        // Only builds: a manager's landing job is never redirected.
        let landing = JobParams::Landing(rsi_common::agent_jobs::LandingJobParams {
            accepted: "a".repeat(40),
            test_filters: Vec::new(),
        });
        assert_eq!(pick(&landing, true), scoped);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_release_target_sits_under_the_deploy_staging_root() {
        let data = Path::new("/home/op/.rsi");
        assert!(release_target_dir_in(data).starts_with(data.join("staging")));
    }

    fn fill(dir: &Path, bytes: usize) {
        fs::create_dir_all(dir.join("release/deps")).unwrap();
        fs::write(dir.join("release/deps/libbig.rlib"), vec![7u8; bytes]).unwrap();
        fs::write(dir.join(ADMISSION_LOCK), b"").unwrap();
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_target_within_the_cap_is_kept_and_an_oversized_one_is_emptied_but_keeps_its_locks() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("release-target");
        assert_eq!(
            enforce_size_cap(&dir, 1024).unwrap(),
            CapOutcome::Absent,
            "a target never built is not created by the cap"
        );
        fill(&dir, 4096);
        assert_eq!(
            enforce_size_cap(&dir, 1 << 20).unwrap(),
            CapOutcome::WithinCap { bytes: 4096 }
        );
        assert!(dir.join("release/deps/libbig.rlib").exists());
        assert_eq!(
            enforce_size_cap(&dir, 1024).unwrap(),
            CapOutcome::Reclaimed {
                bytes: 4096,
                retained: 0
            }
        );
        assert!(!dir.join("release").exists());
        assert!(dir.join(ADMISSION_LOCK).exists(), "the lock file survives");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_oversized_target_keeps_unknown_contents_and_nested_git_work() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("release-target");
        fill(&dir, 4096);
        fs::write(dir.join("manager-state.json"), b"{}").unwrap();
        fs::create_dir_all(dir.join("fixture/src")).unwrap();
        fs::write(dir.join("fixture/src/lib.rs"), b"dirty").unwrap();
        // A known Cargo directory that nevertheless holds a repository.
        fs::create_dir_all(dir.join("doc/checkout/.git")).unwrap();
        fs::write(dir.join("doc/checkout/work.txt"), b"uncommitted").unwrap();
        fs::write(dir.join("CACHEDIR.TAG"), b"Signature").unwrap();
        assert_eq!(
            enforce_size_cap(&dir, 1024).unwrap(),
            CapOutcome::Reclaimed {
                bytes: 4096 + 2 + 5 + 11 + 9,
                retained: 3
            }
        );
        assert!(!dir.join("release").exists(), "known output is removed");
        assert!(!dir.join("CACHEDIR.TAG").exists());
        assert_eq!(fs::read(dir.join("manager-state.json")).unwrap(), b"{}");
        assert!(dir.join("fixture/src/lib.rs").exists());
        assert!(dir.join("doc/checkout/work.txt").exists());
        assert!(dir.join(ADMISSION_LOCK).exists());
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_process_working_inside_an_oversized_target_keeps_it_whole() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("release-target");
        fill(&dir, 4096);
        let mut consumer = std::process::Command::new("sleep")
            .arg("60")
            .current_dir(dir.join("release/deps"))
            .spawn()
            .unwrap();
        let outcome = enforce_size_cap(&dir, 1024);
        let _ = consumer.kill();
        let _ = consumer.wait();
        assert_eq!(outcome.unwrap(), CapOutcome::Held { bytes: 4096 });
        assert!(dir.join("release/deps/libbig.rlib").exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_tree_too_large_to_scan_counts_as_over_the_cap() {
        let root = tempfile::tempdir().unwrap();
        for n in 0..5 {
            fs::write(root.path().join(format!("f{n}")), b"x").unwrap();
        }
        assert_eq!(measure(root.path(), 100), Some(5));
        assert_eq!(measure(root.path(), 3), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_build_holding_the_admission_lock_blocks_the_cap_and_the_sweep() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("release-target");
        fill(&dir, 4096);
        assert!(!is_busy(&dir).unwrap());
        let holder = take_admission(&dir).unwrap().expect("lock is free");
        assert!(is_busy(&dir).unwrap());
        assert_eq!(enforce_size_cap(&dir, 1).unwrap(), CapOutcome::Busy);
        assert!(!reclaim_contents(&dir).unwrap());
        assert!(
            dir.join("release/deps/libbig.rlib").exists(),
            "a busy target is never touched"
        );
        drop(holder);
        assert!(!is_busy(&dir).unwrap());
        assert!(reclaim_contents(&dir).unwrap());
        assert!(!dir.join("release").exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn cargos_own_build_lock_also_marks_the_target_busy() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("release-target");
        fill(&dir, 16);
        let cargo_lock = dir.join("release/.cargo-lock");
        fs::write(&cargo_lock, b"").unwrap();
        let held = OpenOptions::new().write(true).open(&cargo_lock).unwrap();
        assert!(try_lock_exclusive(&held).unwrap());
        assert!(is_busy(&dir).unwrap());
        assert_eq!(enforce_size_cap(&dir, 1).unwrap(), CapOutcome::Busy);
    }

    /// #2176: a run's target returns its disk. Every build-output directory
    /// goes; the regular files a registered allocation needs stay; a build
    /// holding the target (its admission lock or Cargo's own) keeps it whole.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_reclaimed_build_output_keeps_the_allocations_files_and_waits_for_a_build() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("rsi-build-qa");
        assert_eq!(reclaim_build_output(&dir).unwrap(), OutputReclaim::Absent);
        fill(&dir, 4096);
        fs::create_dir_all(dir.join("debug/incremental/x")).unwrap();
        fs::write(dir.join("debug/incremental/x/big"), vec![1u8; 1000]).unwrap();
        fs::create_dir_all(dir.join("tmp/fixture/.git")).unwrap();
        for kept in ["CACHEDIR.TAG", ".rustc_info.json", ".rsi-scratch-record"] {
            fs::write(dir.join(kept), b"k").unwrap();
        }
        let holder = take_admission(&dir).unwrap().expect("lock is free");
        assert_eq!(reclaim_build_output(&dir).unwrap(), OutputReclaim::Busy);
        drop(holder);
        let cargo_lock = dir.join("debug/.cargo-lock");
        fs::write(&cargo_lock, b"").unwrap();
        let held = OpenOptions::new().write(true).open(&cargo_lock).unwrap();
        assert!(try_lock_exclusive(&held).unwrap());
        assert_eq!(reclaim_build_output(&dir).unwrap(), OutputReclaim::Busy);
        assert!(dir.join("release/deps/libbig.rlib").exists());
        drop(held);
        assert_eq!(
            reclaim_build_output(&dir).unwrap(),
            OutputReclaim::Reclaimed {
                bytes: Some(4096 + 1000 + 3)
            }
        );
        for gone in ["release", "debug", "tmp"] {
            assert!(!dir.join(gone).exists(), "{gone}");
        }
        for kept in [
            "CACHEDIR.TAG",
            ".rustc_info.json",
            ".rsi-scratch-record",
            ADMISSION_LOCK,
        ] {
            assert!(dir.join(kept).is_file(), "{kept}");
        }
    }
}
