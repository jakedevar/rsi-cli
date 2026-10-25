//! #3435: a lander run that survives a daemon restart and is adopted by the
//! next daemon.
//!
//! A gate takes tens of minutes. When a lander was the daemon's attached child
//! (piped output, `PartOf=rsid.service`, killed on drop), every daemon restart
//! discarded the whole run and the next daemon gated the batch again from the
//! start, so a host that restarted its daemon more often than a gate takes
//! never published.
//!
//! A detached run keeps its state in files under `<queue>/lander-runs/<key>/`
//! instead of in the daemon's memory:
//!
//! - `stdout`, `stderr`: the lander's output, written directly by the lander;
//! - `exit`: the lander's exit status, written by the wrapper shell
//!   ([`crate::build_budget::exit_recording_command`]) when it ends;
//! - `record.json`: who started it ([`Record`]): the scope unit, the pid and
//!   the daemon `epoch` that is watching it.
//!
//! `key` names what the run gates (the lander's arguments, directory and the
//! derived filters), not when it started. After a restart, the queue
//! reconcile returns the interrupted batch's entries to `queued`; the batch
//! claimed next asks for the same `key`, finds a record written by another
//! epoch and adopts the run: it takes over the record and waits for the same
//! files. A run that finished while the daemon was down is consumed at once.
//!
//! Only one thing may be running in the queue worktree, so a fresh run first
//! stops every inherited run it does not match, and
//! [`reap_inherited_after`] stops the inherited runs nobody claimed within
//! [`INHERITED_GRACE`] (their entries were cancelled or settled elsewhere; an
//! orphan that kept going could still publish them).

use super::LanderError;
use crate::build_budget;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::Instant;

/// Directory of the detached runs under the queue workspace.
pub(super) const RUNS_SUBDIR: &str = "lander-runs";
/// An inherited run older than this is stale, not adoptable: its tip and
/// toolchain are too far behind to stand for the gate being asked for.
const MAX_ADOPT_AGE: Duration = Duration::from_secs(4 * 60 * 60);
/// How long an inherited run may wait for the restarted queue to claim it.
pub(super) const INHERITED_GRACE: Duration = Duration::from_secs(20 * 60);
/// A run directory without a record is a claim in progress; older than this
/// it is debris.
const RECORD_GRACE: Duration = Duration::from_secs(10 * 60);
const POLL: Duration = Duration::from_secs(1);
/// A liveness probe (a `systemctl` call) every this many polls.
const LIVENESS_EVERY: u32 = 5;
const SCHEMA_VERSION: u8 = 1;

/// This daemon process's identity: a record carrying another epoch was
/// started by a daemon that is gone.
pub(super) fn process_epoch() -> &'static str {
    static EPOCH: OnceLock<String> = OnceLock::new();
    EPOCH.get_or_init(|| uuid::Uuid::new_v4().to_string())
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct Record {
    schema_version: u8,
    pub key: String,
    /// The daemon process watching the run.
    pub epoch: String,
    /// The systemd scope holding the run; `None` when no scope could be made.
    pub unit: Option<String>,
    /// The wrapper shell (also its process group); 0 until spawned.
    pub pid: u32,
    pub started_unix: u64,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// One detached run's directory and record.
#[derive(Debug)]
pub(super) struct Run {
    dir: PathBuf,
    pub record: Record,
    /// Taken over from a previous daemon rather than started now.
    pub adopted: bool,
}

impl Run {
    pub fn stdout_path(&self) -> PathBuf {
        self.dir.join("stdout")
    }

    pub fn stderr_path(&self) -> PathBuf {
        self.dir.join("stderr")
    }

    pub fn exit_path(&self) -> PathBuf {
        self.dir.join("exit")
    }

    /// Persist the record (written aside and renamed: a reader never sees half).
    pub fn save(&self) -> io::Result<()> {
        write_record(&self.dir, &self.record)
    }

    fn remove(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn write_record(dir: &Path, record: &Record) -> io::Result<()> {
    let tmp = dir.join("record.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(record).map_err(io::Error::other)?)?;
    std::fs::rename(&tmp, dir.join("record.json"))
}

fn read_record(dir: &Path) -> Option<Record> {
    let record: Record = serde_json::from_slice(&std::fs::read(dir.join("record.json")).ok()?)
        .ok()
        .filter(|record: &Record| record.schema_version == SCHEMA_VERSION)?;
    // The directory is named by the key it claims, nothing else.
    (dir.file_name()?.to_str()? == record.key).then_some(record)
}

/// What a run gates: the lander's directory, arguments and derived filters. Not
/// the wall budget, the log paths or the session environment, which differ
/// between the run a dead daemon started and the one its successor asks for.
pub(super) fn run_key(
    dir: Option<&Path>,
    args: impl IntoIterator<Item = OsString>,
    derived_filters: &str,
) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut hash = Sha256::new();
    hash.update(b"rsi-lander-run-v1\0");
    if let Some(dir) = dir {
        hash.update(dir.as_os_str().as_bytes());
    }
    for arg in args {
        hash.update([0]);
        hash.update(arg.as_bytes());
    }
    hash.update(b"\0filters\0");
    hash.update(derived_filters.as_bytes());
    hex::encode(hash.finalize())
}

/// Take over the run `key` names if a previous daemon left one, else prepare a
/// fresh directory for it. Stops every other inherited run first when it
/// starts fresh. `scope` and `epoch` describe the caller.
pub(super) async fn claim(runs_dir: &Path, key: &str, epoch: &str) -> io::Result<Run> {
    std::fs::create_dir_all(runs_dir)?;
    let dir = runs_dir.join(key);
    if let Some(mut record) = read_record(&dir) {
        let fresh_enough =
            now_unix().saturating_sub(record.started_unix) <= MAX_ADOPT_AGE.as_secs();
        if record.epoch != epoch && fresh_enough {
            record.epoch = epoch.to_string();
            let run = Run {
                dir,
                record,
                adopted: true,
            };
            run.save()?;
            return Ok(run);
        }
        // Same epoch (a leftover of this process) or too old: not this run.
        stop_record(&record).await;
    }
    let _ = std::fs::remove_dir_all(&dir);
    reap_inherited(runs_dir, epoch).await;
    std::fs::create_dir_all(&dir)?;
    Ok(Run {
        dir,
        record: Record {
            schema_version: SCHEMA_VERSION,
            key: key.to_string(),
            epoch: epoch.to_string(),
            unit: None,
            pid: 0,
            started_unix: now_unix(),
        },
        adopted: false,
    })
}

/// Stop and remove every run that another epoch started. Finished ones are
/// only removed. A directory with no record yet is left to the claim that is
/// writing it unless it is old.
pub(super) async fn reap_inherited(runs_dir: &Path, epoch: &str) {
    let Ok(entries) = std::fs::read_dir(runs_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        match read_record(&dir) {
            Some(record) if record.epoch != epoch => {
                stop_record(&record).await;
                let _ = std::fs::remove_dir_all(&dir);
            }
            Some(_) => {}
            None => {
                let old = entry
                    .metadata()
                    .and_then(|meta| meta.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age >= RECORD_GRACE);
                if old {
                    let _ = std::fs::remove_dir_all(&dir);
                }
            }
        }
    }
}

/// After `grace`, stop the inherited runs the restarted queue never claimed.
pub(super) async fn reap_inherited_after(runs_dir: PathBuf, epoch: String, grace: Duration) {
    tokio::time::sleep(grace).await;
    reap_inherited(&runs_dir, &epoch).await;
}

async fn alive(record: &Record) -> bool {
    match (&record.unit, record.pid) {
        (Some(unit), _) => build_budget::scope_active(unit.clone()).await,
        (None, pid) if pid > 0 => i32::try_from(pid)
            .is_ok_and(|pid| nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()),
        (None, _) => false,
    }
}

/// Stop what a record describes (a no-op once it ended).
async fn stop_record(record: &Record) {
    if let Some(unit) = &record.unit {
        if build_budget::scope_active(unit.clone()).await {
            build_budget::stop_scope(unit.clone()).await;
        }
    } else if let Ok(pid) = i32::try_from(record.pid)
        && pid > 0
    {
        // The wrapper leads its own process group.
        let group = nix::unistd::Pid::from_raw(-pid);
        let _ = nix::sys::signal::kill(group, nix::sys::signal::Signal::SIGKILL);
    }
}

fn read_exit(run: &Run) -> Option<i32> {
    std::fs::read_to_string(run.exit_path())
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Wait for the run to end and return its output, removing its directory. Past
/// `deadline` the run is stopped and `TimedOut`. A run that vanished without an
/// exit status (its scope killed with the host) reads as killed by SIGKILL, as
/// an attached lander's death did.
pub(super) async fn wait(run: &Run, deadline: Instant) -> std::result::Result<Output, LanderError> {
    let mut polls = 0_u32;
    let status = loop {
        if let Some(code) = read_exit(run) {
            break ExitStatus::from_raw((code & 0xff) << 8);
        }
        if Instant::now() >= deadline {
            stop_record(&run.record).await;
            run.remove();
            return Err(LanderError::TimedOut);
        }
        polls += 1;
        if polls % LIVENESS_EVERY == 0 && !alive(&run.record).await {
            // It may have written its status between the two looks.
            break read_exit(run).map_or_else(
                || ExitStatus::from_raw(nix::sys::signal::Signal::SIGKILL as i32),
                |code| ExitStatus::from_raw((code & 0xff) << 8),
            );
        }
        tokio::time::sleep(POLL.min(deadline.saturating_duration_since(Instant::now()))).await;
    };
    let read = |path: PathBuf| std::fs::read(path).unwrap_or_default();
    let output = Output {
        status,
        stdout: read(run.stdout_path()),
        stderr: read(run.stderr_path()),
    };
    run.remove();
    Ok(output)
}

#[cfg(test)]
mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn the_key_names_what_is_gated_and_nothing_else() {
        let dir = Path::new("/q/worktrees/a");
        let base = run_key(Some(dir), args(&["--accepted", "aaa"]), "[]");
        assert_eq!(base, run_key(Some(dir), args(&["--accepted", "aaa"]), "[]"));
        assert_ne!(base, run_key(Some(dir), args(&["--accepted", "bbb"]), "[]"));
        assert_ne!(
            base,
            run_key(Some(dir), args(&["--accepted", "aaa"]), "[1]")
        );
        assert_ne!(
            base,
            run_key(
                Some(Path::new("/q/worktrees/b")),
                args(&["--accepted", "aaa"]),
                "[]"
            )
        );
        // Argument boundaries are part of the key.
        assert_ne!(
            run_key(None, args(&["ab", "c"]), ""),
            run_key(None, args(&["a", "bc"]), "")
        );
    }

    /// A run directory some other daemon epoch left behind.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn plant(root: &Path, key: &str, epoch: &str) -> Run {
        let dir = root.join(key);
        std::fs::create_dir_all(&dir).unwrap();
        let run = Run {
            dir,
            record: Record {
                schema_version: SCHEMA_VERSION,
                key: key.to_string(),
                epoch: epoch.to_string(),
                unit: None,
                pid: 0,
                started_unix: now_unix(),
            },
            adopted: false,
        };
        run.save().unwrap();
        run
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_record_of_another_epoch_is_adopted_and_taken_over() {
        let root = tempfile::tempdir().unwrap();
        let key = "k".repeat(64);
        let planted = plant(root.path(), &key, "old-daemon");
        std::fs::write(planted.exit_path(), "0").unwrap();
        std::fs::write(planted.stdout_path(), "published_target_id=abc\n").unwrap();

        let run = claim(root.path(), &key, "new-daemon").await.unwrap();
        assert!(run.adopted);
        assert_eq!(run.record.epoch, "new-daemon");
        assert_eq!(read_record(&run.dir).unwrap().epoch, "new-daemon");
        let output = wait(&run, Instant::now() + Duration::from_secs(5))
            .await
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"published_target_id=abc\n");
        assert!(!run.dir.exists(), "a consumed run leaves nothing behind");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_stale_record_is_not_adopted() {
        let root = tempfile::tempdir().unwrap();
        let key = "s".repeat(64);
        let mut old = plant(root.path(), &key, "old-daemon");
        old.record.started_unix = now_unix() - MAX_ADOPT_AGE.as_secs() - 60;
        old.save().unwrap();
        let fresh = claim(root.path(), &key, "new-daemon").await.unwrap();
        assert!(!fresh.adopted);
        assert_eq!(fresh.record.epoch, "new-daemon");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_fresh_claim_removes_other_inherited_runs_but_not_a_live_sibling() {
        let root = tempfile::tempdir().unwrap();
        let other = plant(root.path(), &"a".repeat(64), "old-daemon");
        let sibling = plant(root.path(), &"b".repeat(64), "new-daemon");

        let fresh = claim(root.path(), &"c".repeat(64), "new-daemon")
            .await
            .unwrap();
        assert!(!fresh.adopted);
        assert!(!other.dir.exists(), "the inherited run was reaped");
        assert!(sibling.dir.exists(), "a live run of this daemon stays");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test(start_paused = true)]
    async fn a_run_that_vanished_without_a_status_reads_as_killed() {
        let root = tempfile::tempdir().unwrap();
        let mut run = plant(root.path(), &"v".repeat(64), "daemon");
        // A pid that is not running: nothing will ever write the exit file.
        run.record.pid = u32::MAX / 2;
        run.save().unwrap();
        let output = wait(&run, Instant::now() + Duration::from_secs(30))
            .await
            .unwrap();
        assert_eq!(output.status.code(), None);
        assert_eq!(output.status.signal(), Some(9));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_overdue_run_times_out_and_is_removed() {
        let root = tempfile::tempdir().unwrap();
        let run = plant(root.path(), &"t".repeat(64), "daemon");
        let result = wait(&run, Instant::now()).await;
        assert!(matches!(result, Err(LanderError::TimedOut)));
        assert!(!run.dir.exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn the_late_reaper_stops_only_runs_of_other_epochs() {
        let root = tempfile::tempdir().unwrap();
        let old = plant(root.path(), &"o".repeat(64), "old-daemon");
        let own = plant(root.path(), &"n".repeat(64), "new-daemon");
        reap_inherited_after(
            root.path().to_path_buf(),
            "new-daemon".into(),
            Duration::ZERO,
        )
        .await;
        assert!(!old.dir.exists());
        assert!(own.dir.exists());
    }
}
