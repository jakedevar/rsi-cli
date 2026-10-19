//! Hub #2954: a `build` job compiles exactly one commit.
//!
//! The daemon records the job worktree's HEAD and cleanliness when the job is
//! submitted (a sidecar next to the job log, so no schema change) and compares
//! the worktree with that record at the queued start, on every poll while the
//! unit runs (slot wait and slot grant included) and at finish. A moved HEAD,
//! or a tree that was clean and is not any more, fails the job
//! `job_source_moved` naming both SHAs: cargo reads the tree continuously, so a
//! worktree that moves mid-build yields binaries from more than one commit.
//! HEAD and cleanliness come from the bounded Git runner the sandbox custody
//! code uses (`observe_clean_head_bounded`); a worktree it cannot read (not a
//! git checkout) is not enforced, and nothing is recorded for it.

use super::{AgentJobResultV1, AgentJobV1, JobKind, JobParams, JobState};
use crate::error::{DaemonError, Result};
use rsi_common::agent_jobs::{JOB_SOURCE_MOVED, JOB_SOURCE_RECORD_FAILED, JOB_SOURCE_SHA_MISMATCH};
use std::path::{Path, PathBuf};

/// The worktree as the daemon saw it at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SourceState {
    pub head: String,
    pub dirty: bool,
}

/// Read the worktree's HEAD and cleanliness; `None` when it cannot be read.
pub(super) fn observe(cwd: &Path) -> Option<SourceState> {
    // A job directory that is not itself a worktree root (a scratch directory
    // below some enclosing repository) is not enforced: its HEAD is not the
    // source the build compiles.
    if !cwd.join(".git").exists() {
        return None;
    }
    let (clean, head) = crate::sandbox::git_worktree::observe_clean_head_bounded(cwd).ok()?;
    Some(SourceState {
        head,
        dirty: !clean,
    })
}

/// `<id>.source`, beside the job's `<id>.log`.
fn record_path(log_path: &str) -> PathBuf {
    Path::new(log_path).with_extension("source")
}

fn is_build(kind: JobKind) -> bool {
    kind == JobKind::Build
}

/// Submit-time step for a `build` job: observe the worktree and enforce the
/// optional `params.sha` pin. `None` for any other job or an unreadable tree.
///
/// # Errors
/// `job_source_sha_mismatch` when `params.sha` is not the worktree HEAD (also
/// when the HEAD cannot be read: a pin that cannot be checked is refused).
pub(super) fn at_submit(params: &JobParams, cwd: &Path) -> Result<Option<SourceState>> {
    let JobParams::Build(build) = params else {
        return Ok(None);
    };
    let observed = observe(cwd);
    let Some(pin) = build.sha.as_deref() else {
        return Ok(observed);
    };
    let Some(state) = observed else {
        return Err(DaemonError::InvalidParam(format!(
            "{JOB_SOURCE_SHA_MISMATCH}: params.sha {pin} cannot be checked because the job worktree HEAD is unreadable"
        )));
    };
    if state.head != pin {
        return Err(DaemonError::InvalidParam(format!(
            "{JOB_SOURCE_SHA_MISMATCH}: params.sha {pin} is not the job worktree HEAD {}; pass the commit the worktree is on or omit sha",
            state.head
        )));
    }
    Ok(Some(state))
}

/// Persist the submit-time state beside the job log.
///
/// A build that was observed at submit must be enforced, so a record that
/// cannot be written refuses the job (the caller runs this before the job row
/// exists or any unit launches) instead of letting it run unenforced.
///
/// # Errors
/// `job_source_record_failed` naming the path and the cause.
pub(super) fn record(log_path: &str, state: &SourceState) -> Result<()> {
    let line = format!(
        "{} {}\n",
        state.head,
        if state.dirty { "dirty" } else { "clean" }
    );
    let path = record_path(log_path);
    std::fs::write(&path, line).map_err(|error| {
        DaemonError::Process(format!(
            "{JOB_SOURCE_RECORD_FAILED}: could not write the build job source record {}: {error}; a build job is refused rather than run without its one-commit guarantee",
            path.display()
        ))
    })
}

/// Drop a record whose job never came to exist (idempotent replay, failed insert).
pub(super) fn discard(log_path: &str) {
    let _ = std::fs::remove_file(record_path(log_path));
}

fn recorded(log_path: &str) -> Option<SourceState> {
    let text = std::fs::read_to_string(record_path(log_path)).ok()?;
    let (head, dirty) = text.trim().split_once(' ')?;
    Some(SourceState {
        head: head.to_string(),
        dirty: dirty == "dirty",
    })
}

/// What changed since the record, as a sentence naming both SHAs.
fn drift(before: &SourceState, now: &SourceState, stage: &str) -> Option<String> {
    if before.head != now.head {
        return Some(format!(
            "job worktree HEAD moved: {} at submit, {} {stage}; a build job compiles exactly one commit, so the unit was not allowed to finish",
            before.head, now.head
        ));
    }
    if now.dirty && !before.dirty {
        return Some(format!(
            "job worktree became dirty: HEAD {} was clean at submit and {stage} it has uncommitted changes (HEAD {} now); a build job compiles exactly one commit",
            before.head, now.head
        ));
    }
    None
}

fn moved_result(now: &SourceState, detail: String) -> AgentJobResultV1 {
    AgentJobResultV1 {
        refusal: Some(JOB_SOURCE_MOVED.into()),
        detail: Some(detail),
        source_head: Some(now.head.clone()),
        source_dirty: Some(now.dirty),
        ..AgentJobResultV1::default()
    }
}

/// Compare the worktree with the job's submit-time record at `stage`.
///
/// `Ok(None)`: not enforced (not a build, a tree that was unreadable at submit
/// and so has no record, or a tree unreadable now).
/// `Ok(Some(state))`: unchanged. `Err(result)`: moved; the ready `failed` result.
pub(super) fn check(
    job: &AgentJobV1,
    stage: &str,
) -> std::result::Result<Option<SourceState>, AgentJobResultV1> {
    if !is_build(job.kind) {
        return Ok(None);
    }
    let Some(before) = recorded(&job.log_path) else {
        return Ok(None);
    };
    let Some(now) = observe(Path::new(&job.cwd)) else {
        return Ok(None);
    };
    match drift(&before, &now, stage) {
        Some(detail) => Err(moved_result(&now, detail)),
        None => Ok(Some(now)),
    }
}

/// Finish step: stamp `source_head`/`source_dirty` on a settling build result
/// and turn an ordinary completion (no refusal of its own) into
/// `job_source_moved` when the worktree is not the one recorded at submit. The
/// build's own exit code stays on the result.
pub(super) fn stamp_finish(job: &AgentJobV1, state: &mut JobState, result: &mut AgentJobResultV1) {
    if !is_build(job.kind) || result.source_head.is_some() {
        return;
    }
    let Some(now) = observe(Path::new(&job.cwd)) else {
        return;
    };
    // A build with no submit-time record was never enforced: the head the
    // daemon sees now is not a submit-time head, so it is not stamped.
    let Some(before) = recorded(&job.log_path) else {
        return;
    };
    let moved = Some(before)
        .filter(|_| result.refusal.is_none())
        .and_then(|before| drift(&before, &now, "when the build finished"));
    match moved {
        Some(detail) => {
            let exit_code = result.exit_code;
            let own = result.detail.take();
            *result = moved_result(&now, detail);
            result.exit_code = exit_code;
            if let Some(own) = own {
                result.detail = result.detail.take().map(|moved| format!("{moved}\n{own}"));
            }
            *state = JobState::Failed;
        }
        None => {
            result.source_head = Some(now.head);
            result.source_dirty = Some(now.dirty);
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    use super::*;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args(args)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repo with two commits; returns (dir, first, second).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    pub(in crate::agent_jobs) fn repo() -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        std::fs::write(dir.path().join("a.txt"), "1").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "target\n.rsi-build-env.*\n").unwrap();
        git(dir.path(), &["add", "a.txt", ".gitignore"]);
        git(dir.path(), &["commit", "-qm", "one"]);
        let first = git(dir.path(), &["rev-parse", "HEAD"]);
        std::fs::write(dir.path().join("a.txt"), "2").unwrap();
        git(dir.path(), &["commit", "-qam", "two"]);
        let second = git(dir.path(), &["rev-parse", "HEAD"]);
        (dir, first, second)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn observe_reports_head_and_dirty() {
        let (dir, _, second) = repo();
        let state = observe(dir.path()).unwrap();
        assert_eq!(
            state,
            SourceState {
                head: second,
                dirty: false
            }
        );
        std::fs::write(dir.path().join("a.txt"), "3").unwrap();
        assert!(observe(dir.path()).unwrap().dirty);
        assert!(observe(&dir.path().join("missing")).is_none());
        let below = dir.path().join("sub");
        std::fs::create_dir(&below).unwrap();
        assert!(
            observe(&below).is_none(),
            "only a worktree root is enforced"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn drift_names_both_shas_and_only_a_clean_to_dirty_change_counts() {
        let clean = |head: &str| SourceState {
            head: head.into(),
            dirty: false,
        };
        let dirty = |head: &str| SourceState {
            head: head.into(),
            dirty: true,
        };
        let moved = drift(&clean("aaa"), &clean("bbb"), "at slot grant").unwrap();
        assert!(moved.contains("aaa") && moved.contains("bbb"), "{moved}");
        assert!(drift(&clean("aaa"), &clean("aaa"), "x").is_none());
        assert!(drift(&clean("aaa"), &dirty("aaa"), "x").is_some());
        // Built dirty on purpose: staying dirty at the same HEAD is not a move.
        assert!(drift(&dirty("aaa"), &dirty("aaa"), "x").is_none());
        assert!(drift(&dirty("aaa"), &dirty("bbb"), "x").is_some());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn record_round_trips_beside_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("job.log").display().to_string();
        let state = SourceState {
            head: "ab".repeat(20),
            dirty: true,
        };
        record(&log, &state).unwrap();
        assert_eq!(recorded(&log), Some(state));
        assert!(recorded(&dir.path().join("none.log").display().to_string()).is_none());
    }

    /// Hub #3025: an unwritable record is a typed error naming the cause.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn record_fails_typed_when_the_sidecar_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("job.log").display().to_string();
        // A directory occupying the sidecar path makes the write fail.
        std::fs::create_dir(dir.path().join("job.source")).unwrap();
        let state = SourceState {
            head: "ab".repeat(20),
            dirty: false,
        };
        let error = record(&log, &state).unwrap_err().to_string();
        assert!(error.contains("job_source_record_failed"), "{error}");
        assert!(error.contains("job.source"), "{error}");
    }
}
