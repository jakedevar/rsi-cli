//! Daemon-owned rolling merge queue runner (#1007 slice S1).
//!
//! The runner claims up to N ready sources in FIFO order (the operator's batch
//! size) that share the head's project and git repository (every worker lands
//! from its own sandbox worktree, so the repository is the git common
//! directory, not the checkout path, #1945), runs the P0 lander (`rsi-rolling-land`) once with every source as an
//! `--accepted` argument (the lander chain-merges them onto the tip, renumbers
//! provisional migrations in that order, gates the candidate once with the
//! union of the members' test filters and publishes it with one fast-forward)
//! and settles each entry exactly once after verifying its ancestry. A batch
//! that is not green (red gate) is bisected: the
//! first half of the suspect window is gated against the tip it would publish
//! onto and published when green, narrowed when red, so each failure is
//! attributed to exactly one source and the innocent sources still publish. An
//! integration conflict is attributed the same way without a gate: the lander
//! names the conflicting source, only that entry is refused
//! (`queue_batch_merge_conflict`) and the batch is re-run without it (#1119).
//! A red that names a file only one source changes (a compile or panic
//! location, or a `git diff --check` static-guard finding, #3396) is attributed
//! to that source directly, with no bisect round (#1974).
//!
//! Restart reconcile runs once at boot: a `gating` entry whose source is
//! already an ancestor of `origin/rolling` settles as published; any other is
//! returned to `queued` and re-driven. Legacy terminal rows stay immutable.
//! A live run whose publication is unknown also
//! returns to `queued`, retaining its entry id and FIFO sequence without a
//! terminal wake. Only proven ancestry counts as published; an unreadable
//! probe re-queues for a fresh gate. Re-driving cannot publish twice.
//!
//! A source that reached `origin/rolling` outside the queue (an operator merge
//! or a direct landing) is never gated: each member's ancestry is re-checked
//! right before its batch is gated, before every bisect step and after a
//! lander run that reports a source "already integrated", and a landed member
//! settles `published` with the tip that contains it (#1208). Each lander run
//! of a batch (the full gate and every bisect step) is bounded by the
//! operator's `rolling_queue_gate_timeout_mins`, the batch by a ceiling that
//! scales with its bisect depth; past a run's budget the members still suspect
//! are refused with `queue_gate_timeout` and the members a red run had left
//! out go back to the queue (#3267).

use crate::config::RuntimeConfig;
use crate::error::{DaemonError, Result};
use crate::store::Store;
use crate::store::rolling_queue::{ClaimedQueueBatch, LanePackages};
use chrono::Utc;
use rsi_common::agent_landing_candidate::RollingMergeProbe;
use rsi_common::rolling_queue::{
    LANDER_FAILURE_CLASS_REMOTE_READ, PublishedCommit, QUEUE_BATCH_ANCESTRY_UNVERIFIED,
    QUEUE_BATCH_MERGE_CONFLICT, QUEUE_BATCH_POLICY_REFUSED, QUEUE_BISECT_NO_PROGRESS,
    QUEUE_CLAIMED_FIX_STILL_RED, QUEUE_CRASH_LOOP_STOP_LIMIT, QUEUE_FILTER_MATCHES_NO_TESTS,
    QUEUE_GATE_TIMEOUT, QUEUE_MIGRATION_OUT_OF_ORDER, QUEUE_REGATE_EXHAUSTED,
    QUEUE_RELEASE_BUILD_FAILED, QUEUE_REMOTE_READ_REQUEUE_LIMIT, QUEUE_REMOTE_READ_UNAVAILABLE,
    QUEUE_SOURCE_ALREADY_INTEGRATED, QUEUE_SUSPECT_CRASH_LOOP,
    ROLLING_QUEUE_DEFAULT_GATE_TIMEOUT_MINS, ROLLING_QUEUE_MAX_BATCH_SIZE, RollingQueueEntryState,
    RollingQueueEntryV1, RollingQueueOutcome,
};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::process::Command;
use uuid::Uuid;

mod bounded_qa;
mod compile_probe;
#[cfg(test)]
pub(crate) use bounded_qa::has_explicit_filters;
pub(crate) use bounded_qa::note_explicit_filters;
mod detached_run;
mod gate_base_red;
mod known_failure_snapshot;
mod lanes;
mod provisional_probe;
pub(crate) mod satellite_qa;
mod tip_conflict;

/// Poll interval of the runner loop.
const TICK: Duration = Duration::from_secs(5);
/// The lander's refusal of an `--accepted` source `origin/rolling` already
/// contains ("accepted source <sha> is already integrated; ...").
const ALREADY_INTEGRATED_MARKER: &str = "is already integrated";
/// The out-of-band budget (#1940): the wall time the batch has left. A rolling
/// tip that moves under a gate re-runs only what the advance overlaps, as often
/// as the time already spent plus the cost of the re-run fits, instead of a
/// fixed count of re-gates.
const REGATE_BUDGET_ENV: &str = "RSI_LANDER_REGATE_BUDGET_SECS";
/// Queue runs never publish an untested tree where an advance could change a
/// result: the lander reuses the earlier gate across a moved tip only for what
/// the advance provably does not touch, and re-runs the rest (#1007, #1940).
const EXACT_GATE_ENV: &str = "RSI_LANDER_EXACT_GATE";
/// A batch member's commit is not in the queue checkout's object store (its
/// sandbox was reclaimed and the commit pruned): the member is refused so its
/// owner re-enqueues from a live checkout, and the rest of the batch goes on
/// (#1945).
pub(crate) const QUEUE_SOURCE_UNREACHABLE: &str = "queue_source_unreachable";
/// Bytes of a lander's stderr/stdout the queue outcome detail keeps (the newest).
pub const OUTCOME_TAIL_BYTES: usize = 2000;
/// Decoded stdout/stderr excerpt retained for the decisive red gate (#3936).
const GATE_FAILURE_EXCERPT_BYTES: usize = 8 * 1024;
const GATE_FAILURE_EXCERPT_KEY: &str = "gate_failure_excerpt";
const MAX_FAILING_TESTS: usize = 32;
/// Per-run lander logs (#1135): each stream keeps its newest bytes, and only the
/// newest files stay in the log directory.
const RUN_LOG_STREAM_BYTES: usize = 1 << 20;
const RUN_LOG_KEEP_FILES: usize = 40;
/// Failing tests named in an event's cause (the log has the rest).
const CAUSE_TESTS: usize = 5;

/// Files that many concurrent sources touch. Informational only: the queue
/// never waits on, claims or seals them (operator directive 2026-09-29).
const HOT_FILES: &[&str] = &[
    "crates/rsid-store/src/config.rs",
    // Before the #1021 S4 crate split (stale sources still touch it).
    "crates/rsid/src/config.rs",
    "crates/rsi-common/src/rpc.rs",
    "crates/rsi-common/src/agent_control_schema.rs",
    "crates/rsi-common/src/bin/rsi-rpc.rs",
    "crates/rsi/src/settings_registry.rs",
    "crates/rsi/src/settings_keys.rs",
];
/// Each schema version is its own file (`vNNN.rs`) here; the head is the
/// highest one, so adding a migration never edits a shared file.
const MIGRATION_DIR: &str = "crates/rsid-store/src/store/migrations";
/// The layout before the #1021 S4 crate split (the store lived in `crates/rsid`):
/// a rolling base or a stale source cut before it is still read.
const PRE_SPLIT_MIGRATION_DIR: &str = "crates/rsid/src/store/migrations";
/// A source that declares a provisional migration is renumbered by the lander
/// at landing, so its number is assigned in queue order.
const PROVISIONAL_DIR: &str = "tools/provisional-migrations/";
/// Older revisions declared the head as a constant in this file.
const LEGACY_MIGRATION_FILES: [&str; 2] = [
    "crates/rsid-store/src/store/mod.rs",
    "crates/rsid/src/store/mod.rs",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SourceFacts {
    pub migration_version: Option<u32>,
    pub hot_files: Vec<String>,
}

/// Environment an UNFENCED queue git inherits from the daemon: locale, the
/// user's identity and home, and the transport settings a fetch needs. Every
/// other name (above all `GIT_TRACE*`, which git opens for append wherever it
/// points, and the `GIT_*` knobs that reconfigure it) is dropped (#1160).
fn inherited_git_env(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    const KEEP: &[&str] = &[
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "LANGUAGE",
        "TZ",
        "TMPDIR",
        "XDG_CONFIG_HOME",
        "XDG_RUNTIME_DIR",
        "SSH_AUTH_SOCK",
        "GIT_SSH",
        "GIT_SSH_COMMAND",
        "GIT_ASKPASS",
        "SSH_ASKPASS",
        "GIT_TERMINAL_PROMPT",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "CURL_CA_BUNDLE",
    ];
    vars.into_iter()
        .filter(|(name, _)| {
            let name = name.to_string_lossy();
            KEEP.contains(&name.as_ref()) || name.starts_with("LC_")
        })
        .collect()
}

/// The git binary every probe here runs. Tests may route it through a shim
/// (#2912) that stalls or delays selected calls; every other call passes through.
#[cfg(test)]
pub(crate) static TEST_GIT_PROGRAM: std::sync::OnceLock<std::ffi::OsString> =
    std::sync::OnceLock::new();

/// #2912: route every probe git through a shim that stalls or delays a subcommand
/// in a repository that asks for it (`stall_git`, `delay_git`) and passes everything
/// else to the real git, so it is safe to install once for the whole test process.
#[cfg(test)]
pub(crate) fn install_stalling_git_shim() {
    use std::os::unix::fs::PermissionsExt;
    TEST_GIT_PROGRAM.get_or_init(|| {
        let real = std::env::var_os("PATH")
            .and_then(|path| {
                std::env::split_paths(&path)
                    .map(|dir| dir.join("git"))
                    .find(|candidate| candidate.is_file())
            })
            .expect("git on PATH");
        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("git");
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"-C\" ] && [ -f \"$2/.rsi-stall\" ] \
             && [ \"$3\" = \"$(cat \"$2/.rsi-stall\")\" ]; then\n  echo $$ > \"$2/.rsi-stall-pid\"\n  \
             exec sleep 60\nfi\n\
             if [ \"$1\" = \"-C\" ] && [ -f \"$2/.rsi-delay-$3\" ]; then\n  \
             sleep \"$(cat \"$2/.rsi-delay-$3\")\"\nfi\nexec {} \"$@\"\n",
            real.display()
        );
        std::fs::write(&shim, script).unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::mem::forget(dir);
        shim.into_os_string()
    });
}

/// Make the next `git -C <repo> <subcommand> ...` stall for a minute.
#[cfg(test)]
pub(crate) fn stall_git(repo: &Path, subcommand: &str) {
    install_stalling_git_shim();
    let _ = std::fs::remove_file(repo.join(".rsi-stall-pid"));
    std::fs::write(repo.join(".rsi-stall"), subcommand).unwrap();
}

/// Add finite subprocess latency without generating CPU load (#519).
#[cfg(test)]
pub(crate) fn delay_git(repo: &Path, subcommand: &str, delay: Duration) {
    install_stalling_git_shim();
    std::fs::write(
        repo.join(format!(".rsi-delay-{subcommand}")),
        delay.as_secs_f64().to_string(),
    )
    .unwrap();
}

/// Whether the git `stall_git` stalled was started and has since been killed.
#[cfg(test)]
pub(crate) fn stalled_git_was_killed(repo: &Path) -> bool {
    let Ok(pid) = std::fs::read_to_string(repo.join(".rsi-stall-pid")) else {
        return false;
    };
    let Ok(pid) = pid.trim().parse::<i32>() else {
        return false;
    };
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { nix::libc::kill(pid, 0) != 0 }
}

fn git_program() -> std::ffi::OsString {
    #[cfg(test)]
    if let Some(program) = TEST_GIT_PROGRAM.get() {
        return program.clone();
    }
    "git".into()
}

/// A `git` command with the daemon's environment reduced to the allowlist.
fn git_command() -> std::process::Command {
    let mut command = std::process::Command::new(git_program());
    command
        .env_clear()
        .envs(inherited_git_env(std::env::vars_os()));
    command
}

thread_local! {
    /// #2912: the wall deadline of the git reads this thread runs (see
    /// [`with_git_deadline`]), and whether one of them ran into it.
    static GIT_DEADLINE: std::cell::Cell<Option<std::time::Instant>> =
        const { std::cell::Cell::new(None) };
    static GIT_DEADLINE_HIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The most output one deadline-bound git read keeps; a larger answer is a read
/// that could not finish.
const BOUNDED_GIT_MAX_OUTPUT: u64 = 32 << 20;

/// #2912: run `read` with every [`git`] and [`git_grep_hits`] call it makes
/// bounded by the wall `deadline`: each runs in its own process group, which is
/// killed and reaped when the deadline passes, and none starts after it. The
/// bool is whether a call ran into the deadline, so the caller can tell a read
/// that was cut short (retry) from one that found nothing. Blocking; the
/// bound is per thread, so run it inside the same `spawn_blocking` closure
/// that makes the reads.
pub(crate) fn with_git_deadline<T>(
    deadline: std::time::Instant,
    read: impl FnOnce() -> T,
) -> (T, bool) {
    struct Restore(Option<std::time::Instant>, bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            GIT_DEADLINE.set(self.0);
            GIT_DEADLINE_HIT.set(self.1);
        }
    }
    let _restore = Restore(
        GIT_DEADLINE.replace(Some(deadline)),
        GIT_DEADLINE_HIT.replace(false),
    );
    let value = read();
    (value, GIT_DEADLINE_HIT.get())
}

/// Run `command` under this thread's [`with_git_deadline`] bound, if any.
/// `None` when there is no bound (the caller runs it unbounded); `Some(None)`
/// when the read did not complete in time.
fn run_under_git_deadline(
    command: std::process::Command,
) -> Option<Option<(Option<i32>, Vec<u8>)>> {
    let deadline = GIT_DEADLINE.get()?;
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let outcome = if remaining.is_zero() {
        None
    } else {
        run_bounded_optional(command, remaining, BOUNDED_GIT_MAX_OUTPUT, Stdio::null())
    };
    if outcome.is_none() && std::time::Instant::now() >= deadline {
        GIT_DEADLINE_HIT.set(true);
    }
    Some(outcome)
}

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let mut command = git_command();
    command.arg("-C").arg(repo).args(args);
    if let Some(outcome) = run_under_git_deadline(command) {
        let (code, stdout) = outcome?;
        return (code == Some(0)).then(|| String::from_utf8_lossy(&stdout).into_owned());
    }
    let output = git_command()
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// What fetching a satellite-reported source from `origin` found (#3345).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OriginFetch {
    /// The commit is in the object store and an `origin` ref contains it, or
    /// `origin` served it by id.
    Fetched,
    /// `origin` answered and does not serve the commit.
    NotOnOrigin,
    /// The fetch could not finish (no network, a timeout): proves nothing.
    Unknown,
}

/// Fetch `source` from `origin` into `repo` and say whether `origin` has it:
/// one `git fetch origin` (every branch, so the satellite's pushed branch
/// arrives), then, when no `origin` ref contains the commit, one fetch of the
/// commit by id. Bounded by `budget` as a whole (every git call is killed at the
/// deadline). Blocking; call from `spawn_blocking`.
pub(crate) fn fetch_origin_source(repo: &Path, source: &str, budget: Duration) -> OriginFetch {
    let deadline = std::time::Instant::now() + budget;
    let (outcome, timed_out) = with_git_deadline(deadline, || {
        let all_fetched = git(repo, &["fetch", "--quiet", "--no-tags", "origin"]).is_some();
        let contained = || {
            source_commit_exists(repo, source)
                && git(
                    repo,
                    &[
                        "for-each-ref",
                        "--count=1",
                        "--format=%(refname)",
                        "--contains",
                        source,
                        "refs/remotes/origin",
                    ],
                )
                .is_some_and(|refs| !refs.trim().is_empty())
        };
        if contained() {
            return OriginFetch::Fetched;
        }
        let by_id = git(repo, &["fetch", "--quiet", "--no-tags", "origin", source]).is_some();
        if by_id && source_commit_exists(repo, source) {
            OriginFetch::Fetched
        } else if all_fetched {
            OriginFetch::NotOnOrigin
        } else {
            OriginFetch::Unknown
        }
    });
    if timed_out {
        OriginFetch::Unknown
    } else {
        outcome
    }
}

/// Whether `source` names a commit in `repo`'s object store.
pub(crate) fn source_commit_exists(repo: &Path, source: &str) -> bool {
    git(repo, &["cat-file", "-e", &format!("{source}^{{commit}}")]).is_some()
}

/// The commit `HEAD` names in `repo` (an operator landing's default source).
pub(crate) fn head_commit(repo: &Path) -> Option<String> {
    let head = git(repo, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    Some(head.trim().to_owned())
}

/// The commit `branch` names in `repo`.
pub(crate) fn branch_commit(repo: &Path, branch: &str) -> Option<String> {
    let spec = format!("refs/heads/{branch}^{{commit}}");
    let tip = git(repo, &["rev-parse", "--verify", &spec])?;
    Some(tip.trim().to_owned())
}

/// Most bytes of `git diff --name-only` output the landing-candidate check
/// reads before it gives up (#2344).
const DELTA_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Most paths the landing-candidate check collects before it gives up.
const DELTA_MAX_PATHS: usize = 50_000;
/// How long the landing-candidate diff may run before it is killed.
const DELTA_DEADLINE: Duration = Duration::from_secs(20);

/// Bounds on one streamed git listing.
#[derive(Clone, Copy)]
pub(crate) struct DeltaLimits {
    pub max_bytes: usize,
    pub max_paths: usize,
    pub deadline: Duration,
}

impl DeltaLimits {
    const DEFAULT: Self = Self {
        max_bytes: DELTA_MAX_BYTES,
        max_paths: DELTA_MAX_PATHS,
        deadline: DELTA_DEADLINE,
    };
}

/// Why a bounded delta listing is not a path list.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DeltaRead {
    Paths(Vec<String>),
    /// Output past the byte or path cap, or the deadline: the delta is too
    /// large to classify, so the caller must fail closed. Says why.
    TooLarge(&'static str),
    /// git could not read both commits.
    Unreadable,
}

/// Paths that differ between two commits (`git diff --name-only from to`),
/// for the landing-candidate check (#2310). Output is streamed under a byte
/// and path cap and a subprocess deadline (#2344).
pub(crate) fn changed_paths(repo: &Path, from: &str, to: &str) -> DeltaRead {
    let mut command = git_command();
    let Ok(child) = command
        .arg("-C")
        .arg(repo)
        .args(["diff", "--name-only", "--no-renames", from, to, "--"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return DeltaRead::Unreadable;
    };
    read_delta(child, DeltaLimits::DEFAULT)
}

/// #2814: when `source` has several merge bases with rolling (it merged an
/// earlier commit of rolling that landed under another SHA), the merge-base diff
/// re-counts what rolling already holds. The candidate's real delta is the clean
/// merge result against the rolling tip: returns `(rolling ref, merge tree)`.
/// `None` for a single merge base, a conflicted merge or any git failure, so
/// the caller keeps the merge-base diff.
fn multi_base_merge_tree(repo: &Path, source: &str) -> Option<(String, String)> {
    let reference = ["origin/rolling", "rolling"]
        .into_iter()
        .find(|reference| {
            git(
                repo,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{reference}^{{commit}}"),
                ],
            )
            .is_some()
        })?;
    let bases = git(repo, &["merge-base", "--all", source, reference])?;
    if bases.split_whitespace().count() < 2 {
        return None;
    }
    let merged = git(repo, &["merge-tree", "--write-tree", reference, source])?;
    let tree = merged.split_whitespace().next()?.to_string();
    Some((reference.to_string(), tree))
}

/// Paths `source` changes relative to the rolling tip it was cut from
/// (`git diff --name-only <merge-base> source`), bounded like
/// [`changed_paths`]. No rolling ref or merge base is `Unreadable`, so a caller
/// that must know the delta fails closed.
pub(crate) fn source_changed_paths(repo: &Path, source: &str) -> DeltaRead {
    if let Some((reference, tree)) = multi_base_merge_tree(repo, source) {
        return changed_paths(repo, &reference, &tree);
    }
    let base = ["origin/rolling", "rolling"].iter().find_map(|reference| {
        git(repo, &["merge-base", source, reference]).map(|out| out.trim().to_string())
    });
    match base.filter(|base| !base.is_empty()) {
        Some(base) => changed_paths(repo, &base, source),
        None => DeltaRead::Unreadable,
    }
}

/// Collect a spawned child's stdout lines under `limits`; the child is killed
/// the moment a limit is hit.
fn read_delta(mut child: std::process::Child, limits: DeltaLimits) -> DeltaRead {
    use std::io::Read;
    use std::sync::mpsc;

    enum Done {
        Complete(Vec<u8>),
        TooLarge(&'static str),
        Failed,
    }

    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return DeltaRead::Unreadable;
    };
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 8192];
        let mut lines = 0usize;
        let done = loop {
            match stdout.read(&mut chunk) {
                Ok(0) => break Done::Complete(bytes),
                Ok(n) => {
                    lines += chunk[..n].iter().filter(|b| **b == b'\n').count();
                    if lines > limits.max_paths {
                        break Done::TooLarge("path limit");
                    }
                    if bytes.len() + n > limits.max_bytes {
                        break Done::TooLarge("output limit");
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break Done::Failed,
            }
        };
        let _ = sender.send(done);
    });
    let done = receiver.recv_timeout(limits.deadline);
    let outcome = match done {
        Ok(Done::Complete(bytes)) => match child.wait() {
            Ok(status) if status.success() => {
                let listing = String::from_utf8_lossy(&bytes);
                DeltaRead::Paths(listing.lines().map(str::to_owned).collect())
            }
            _ => DeltaRead::Unreadable,
        },
        Ok(Done::TooLarge(why)) => DeltaRead::TooLarge(why),
        Ok(Done::Failed) => DeltaRead::Unreadable,
        Err(_) => DeltaRead::TooLarge("deadline"),
    };
    // A no-op for a child already reaped; otherwise stops the runaway diff.
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    outcome
}

/// What an operator is shown before a landing, read from the local
/// `origin/rolling` without a fetch (the queue re-checks ancestry against a
/// fresh tip before it gates): commits the source has beyond it, and whether
/// the source is already an ancestor of it.
pub(crate) fn land_position(repo: &Path, source: &str) -> (Option<u32>, bool) {
    let on_rolling = git(
        repo,
        &["merge-base", "--is-ancestor", source, "origin/rolling"],
    )
    .is_some();
    let ahead = git(
        repo,
        &["rev-list", "--count", &format!("origin/rolling..{source}")],
    )
    .and_then(|count| count.trim().parse().ok());
    (ahead, on_rolling)
}

/// Whether the working tree of `repo` has uncommitted changes to tracked files.
pub(crate) fn worktree_dirty(repo: &Path) -> bool {
    git(repo, &["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|status| !status.trim().is_empty())
}

/// Whether the replacement contains the queued source's history.
pub(crate) fn source_descends_from(repo: &Path, old: &str, new: &str) -> bool {
    git(repo, &["merge-base", "--is-ancestor", old, new]).is_some()
}

/// How long one conflict probe may run before it is killed (#2706).
const MERGE_PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// The most probe output read; a larger answer is a probe that could not run.
const MERGE_PROBE_MAX_OUTPUT: u64 = 1 << 20;

/// Run `command` (stdout piped) until it exits or `timeout` passes. On timeout
/// the child's whole process group is killed and reaped and the result is
/// `None`: a bounded outcome, never a hang (#2706). At most `max_output` bytes
/// of stdout are read; a larger answer is `None` too. The child's stderr goes
/// to `stderr` (a probe discards it, a caller that wants the reason passes a
/// file).
fn run_bounded_optional(
    command: std::process::Command,
    timeout: Duration,
    max_output: u64,
    stderr: Stdio,
) -> Option<(Option<i32>, Vec<u8>)> {
    run_bounded(command, timeout, max_output, stderr).ok()
}

/// Keep infrastructure causes for callers that must refuse an incomplete proof.
fn run_bounded(
    mut command: std::process::Command,
    timeout: Duration,
    max_output: u64,
    stderr: Stdio,
) -> std::result::Result<(Option<i32>, Vec<u8>), String> {
    use std::io::Read;
    use std::os::unix::process::CommandExt;
    if timeout.is_zero() {
        return Err("timeout: probe deadline exhausted".into());
    }
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .map_err(|error| format!("could not start {program}: {error}"))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(max_output + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                break Err(format!(
                    "timeout: {program} exceeded its probe step deadline"
                ));
            }
            Err(error) => break Err(format!("could not wait for {program}: {error}")),
        }
    };
    if status.is_err() {
        // SAFETY: the child was started in its own process group.
        unsafe {
            nix::libc::kill(-(child.id() as i32), nix::libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    }
    let bytes = reader
        .join()
        .map_err(|_| "probe stdout reader panicked")?
        .map_err(|error| format!("could not read {program} stdout: {error}"))?;
    let status = status?;
    if bytes.len() as u64 > max_output {
        return Err(format!("{program} output exceeded the probe byte limit"));
    }
    Ok((status.code(), bytes))
}

/// Make `command` a probe git: the allowlisted environment, no system, global
/// or user config, no lazy fetch of a missing object from a promisor remote and
/// no optional lock, so nothing the repository configures can run (#2706,
/// #2714).
fn isolate_probe_git(command: &mut std::process::Command) {
    command
        .env_clear()
        .envs(inherited_git_env(std::env::vars_os()))
        .env("HOME", "/dev/null")
        .env("XDG_CONFIG_HOME", "/dev/null")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
}

/// One probe git read in `repo` under the probe isolation, killed when the
/// shared `deadline` passes (#2714). `Some(stdout)` only on exit status 0.
fn probe_git(repo: &Path, args: &[&str], deadline: std::time::Instant) -> Option<String> {
    let (code, stdout) = probe_git_exit(repo, args, deadline)?;
    (code == Some(0)).then(|| String::from_utf8_lossy(&stdout).into_owned())
}

/// [`probe_git`] with the exit code kept: `None` is a read that did not
/// complete (the deadline passed and the process group was killed, or it could
/// not run); `Some((code, stdout))` is one that did, whatever it answered
/// (#2912).
fn probe_git_exit(
    repo: &Path,
    args: &[&str],
    deadline: std::time::Instant,
) -> Option<(Option<i32>, Vec<u8>)> {
    let mut command = std::process::Command::new(git_program());
    isolate_probe_git(&mut command);
    command.arg("-C").arg(repo).args(args);
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    run_bounded_optional(command, remaining, MERGE_PROBE_MAX_OUTPUT, Stdio::null())
}

/// The object id `name` names in `repo`, read without opening an object: a
/// ref or a full object id (#2714). Peeling to a commit happens in the scratch
/// directory, which has no promisor remote to fetch from.
fn resolve_name(repo: &Path, name: &str, deadline: std::time::Instant) -> Option<String> {
    let id = probe_git(repo, &["rev-parse", "--verify", "--quiet", name], deadline)?;
    let id = id.trim();
    (!id.is_empty()).then(|| id.to_owned())
}

/// A scratch git directory for one merge probe (#2706): its own `HEAD`, an
/// empty `refs`, a config that names no merge driver and no promisor remote,
/// and the source repository's objects as an alternate. The probe sees none of
/// the source's repository config, `info/attributes` or hooks, writes the
/// merged tree into the scratch objects (nothing reaches the source
/// repository), and cannot run a driver or a fetch helper the source
/// repository configures.
fn isolated_probe_dir(repo: &Path, deadline: std::time::Instant) -> Option<tempfile::TempDir> {
    let objects = probe_git(
        repo,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects",
        ],
        deadline,
    )?;
    let objects = objects.trim();
    if objects.is_empty() {
        return None;
    }
    let dir = tempfile::Builder::new()
        .prefix("rsi-merge-probe-")
        .tempdir()
        .ok()?;
    let root = dir.path();
    std::fs::create_dir_all(root.join("objects/info")).ok()?;
    std::fs::create_dir_all(root.join("refs")).ok()?;
    std::fs::write(root.join("HEAD"), "ref: refs/heads/probe\n").ok()?;
    std::fs::write(
        root.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = true\n",
    )
    .ok()?;
    std::fs::write(root.join("objects/info/alternates"), format!("{objects}\n")).ok()?;
    Some(dir)
}

/// Point `command` at the scratch git directory `scratch`.
fn use_probe_dir(command: &mut std::process::Command, scratch: &Path) {
    command
        .env("GIT_DIR", scratch)
        .env("GIT_COMMON_DIR", scratch)
        .env("GIT_OBJECT_DIRECTORY", scratch.join("objects"))
        .env_remove("GIT_WORK_TREE")
        .current_dir(scratch);
}

/// The commit `id` peels to, checked in the scratch directory (#2714).
fn peel_commit(scratch: &Path, id: &str, deadline: std::time::Instant) -> Option<String> {
    let mut command = std::process::Command::new(git_program());
    isolate_probe_git(&mut command);
    use_probe_dir(&mut command, scratch);
    command.args([
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("{id}^{{commit}}"),
    ]);
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let (code, stdout) =
        run_bounded_optional(command, remaining, MERGE_PROBE_MAX_OUTPUT, Stdio::null())?;
    if code != Some(0) {
        return None;
    }
    let id = String::from_utf8_lossy(&stdout).trim().to_owned();
    (!id.is_empty()).then_some(id)
}

#[derive(Debug, PartialEq, Eq)]
enum MergeConflictProbe {
    Clean,
    Conflicts(Vec<String>),
    Unknown { reason: &'static str },
}

/// #2118: the paths `ours` and `theirs` would conflict on, from an in-memory
/// `git merge-tree --write-tree` (nothing is written to the work tree, the
/// index or the source repository's object store). Keep clean, conflicting
/// (possibly with no named paths), and unknown outcomes distinct: an unknown
/// probe proves nothing about conflict attribution and never refuses enqueue.
///
/// #2706: the probe is side-effect free and bounded. It runs in an isolated git
/// directory (no system, global or repository config, so no external merge
/// driver can run whatever `.gitattributes` names) with built-in merge
/// semantics, and is killed after [`MERGE_PROBE_TIMEOUT`].
fn merge_conflict_paths_with(
    repo: &Path,
    ours: &str,
    theirs: &str,
    launcher: &[&str],
    timeout: Duration,
) -> MergeConflictProbe {
    match merge_probe_with(repo, ours, theirs, launcher, timeout) {
        Some(RollingMergeProbe::Clean) => MergeConflictProbe::Clean,
        Some(RollingMergeProbe::Conflict(paths)) => MergeConflictProbe::Conflicts(paths),
        _ => MergeConflictProbe::Unknown {
            reason: "merge probe could not complete (missing object, unsupported git, timeout or invalid output)",
        },
    }
}

/// The underlying merge verdict (`None` when the probe could not run), also
/// used by callers that report "merges cleanly" (#348).
fn merge_probe_with(
    repo: &Path,
    ours: &str,
    theirs: &str,
    launcher: &[&str],
    timeout: Duration,
) -> Option<RollingMergeProbe> {
    // Every read, the preflight ones included, runs isolated and under one
    // deadline (#2714).
    let deadline = std::time::Instant::now() + timeout;
    let (ours, theirs) = (
        resolve_name(repo, ours, deadline)?,
        resolve_name(repo, theirs, deadline)?,
    );
    let scratch = isolated_probe_dir(repo, deadline)?;
    let (ours, theirs) = (
        peel_commit(scratch.path(), &ours, deadline)?,
        peel_commit(scratch.path(), &theirs, deadline)?,
    );
    let (program, lead) = launcher.split_first()?;
    let mut command = std::process::Command::new(program);
    command.args(lead);
    isolate_probe_git(&mut command);
    use_probe_dir(&mut command, scratch.path());
    command.args([
        "merge-tree",
        "--write-tree",
        "--name-only",
        "--no-messages",
        &ours,
        &theirs,
    ]);
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let (code, stdout) =
        run_bounded_optional(command, remaining, MERGE_PROBE_MAX_OUTPUT, Stdio::null())?;
    if code == Some(0) {
        return Some(RollingMergeProbe::Clean);
    }
    if code != Some(1) {
        return None;
    }
    let text = String::from_utf8_lossy(&stdout);
    let mut lines = text.lines();
    // Line 1 is the (partially merged) tree id. An unknown commit also exits 1
    // but prints no tree id on stdout: that is a probe that could not run.
    let tree = lines.next()?.trim();
    if tree.len() < 40 || !tree.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut paths: Vec<String> = Vec::new();
    for line in lines.map(str::trim) {
        if !line.is_empty() && !paths.iter().any(|seen| seen == line) {
            paths.push(line.to_owned());
        }
    }
    Some(RollingMergeProbe::Conflict(paths))
}

/// #348: a fresh trial merge of `source` onto the current rolling tip
/// (`origin/rolling`, else `rolling`), read-only and bounded like the enqueue
/// conflict scan. Blocking; call from `spawn_blocking`.
pub(crate) fn rolling_merge_probe(repo: &Path, source: &str) -> RollingMergeProbe {
    let deadline = std::time::Instant::now() + MERGE_PROBE_TIMEOUT;
    for reference in ["origin/rolling", "rolling"] {
        match probe_git_exit(
            repo,
            &["rev-parse", "--verify", "--quiet", reference],
            deadline,
        ) {
            Some((Some(0), _)) => {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                return merge_probe_with(repo, reference, source, &["git"], remaining)
                    .unwrap_or(RollingMergeProbe::Unknown);
            }
            Some(_) => {}
            None => return RollingMergeProbe::Unknown,
        }
    }
    RollingMergeProbe::Unknown
}

/// The most live entries one enqueue probes (#3741). A probe is a `git
/// merge-tree` of roughly 10-50 ms, so the cap is generous; the wall budget
/// ([`ENQUEUE_CONFLICT_SCAN_BUDGET`]) is the real bound. Entries are probed
/// nearest-first (descending sequence), so whatever the cap or the budget
/// skips is the farthest entries, never the ones just ahead.
pub(crate) const CONFLICT_PROBE_MAX_ENTRIES: usize = 256;

/// How many skipped sequences the incomplete warning lists by name.
const SKIPPED_SEQUENCES_LISTED: usize = 32;

/// Enqueue distinguishes a source defect from an incomplete infrastructure proof.
#[derive(Debug)]
pub(crate) enum ProvisionalSourceRefusal {
    Unprovable(String),
    Inconclusive(String),
}

/// #3397, #3545: validate declarations at enqueue, including still-free numbers.
/// Unlike the best-effort sweep, enqueue refuses an incomplete proof retryably.
pub(crate) fn provisional_source_refusal(
    repo: &Path,
    source: &str,
    deadline: std::time::Instant,
) -> Option<ProvisionalSourceRefusal> {
    for reference in ["origin/rolling", "rolling"] {
        if let Some(tip) = probe_git(
            repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{reference}^{{commit}}"),
            ],
            deadline,
        ) {
            return match provisional_probe::prove_against_tip(repo, tip.trim(), source, deadline) {
                provisional_probe::Proof::Unprovable(reason) => {
                    Some(ProvisionalSourceRefusal::Unprovable(reason))
                }
                provisional_probe::Proof::Inconclusive { cause } => {
                    Some(ProvisionalSourceRefusal::Inconclusive(cause.to_string()))
                }
                _ => None,
            };
        }
    }
    // Counting additions also needs rolling. A source without any declaration
    // remains unaffected even in an older/toy repository with no rolling ref.
    if probe_git(
        repo,
        &[
            "ls-tree",
            "-r",
            "--name-only",
            source,
            "--",
            PROVISIONAL_DIR,
        ],
        deadline,
    )
    .is_some_and(|paths| !paths.lines().any(|path| path.ends_with(".json")))
    {
        return None;
    }
    Some(ProvisionalSourceRefusal::Inconclusive(
        "could not resolve rolling ref origin/rolling or rolling (missing ref, git read failure or timeout); fetch origin/rolling and check git availability".into(),
    ))
}

/// #2907: the wall budget of one enqueue's whole conflict scan. The scan is a
/// warning, so it never holds the call longer than this; what it did not reach
/// is reported as [`QUEUE_CONFLICT_SCAN_INCOMPLETE`].
///
/// [`QUEUE_CONFLICT_SCAN_INCOMPLETE`]: rsi_common::rolling_queue::QUEUE_CONFLICT_SCAN_INCOMPLETE
pub(crate) const ENQUEUE_CONFLICT_SCAN_BUDGET: Duration = Duration::from_secs(4);

/// The scan with no practical bound, for tests that assert the warnings
/// themselves.
#[cfg(test)]
pub(crate) fn queue_conflict_warnings(
    repo: &Path,
    source: &str,
    ahead: &[(Uuid, i64, String)],
) -> Vec<rsi_common::rolling_queue::QueueConflictWarningV1> {
    queue_conflict_warnings_within(repo, source, ahead, Duration::from_secs(3600))
}

/// #2118: the typed warnings for a new `source`: a conflict with the rolling
/// tip, then with each live entry ahead of it (`ahead`: id, sequence, source
/// commit). An entry's existing conflicts with rolling are reported on that
/// entry, rather than attributed to the new source. Blocking; call from
/// `spawn_blocking`.
///
/// #2907: the whole scan is bounded by `budget`. Each probe runs under the time
/// left, no probe starts after the budget is spent, and a scan that did not
/// reach every entry ends with one [`QUEUE_CONFLICT_SCAN_INCOMPLETE`] warning
/// naming how many probes it skipped and which queue sequences they were
/// (#3741: entries are probed nearest-first), so a missing warning never reads as "no
/// conflict" when the scan was cut short.
///
/// [`QUEUE_CONFLICT_SCAN_INCOMPLETE`]: rsi_common::rolling_queue::QUEUE_CONFLICT_SCAN_INCOMPLETE
pub(crate) fn queue_conflict_warnings_within(
    repo: &Path,
    source: &str,
    ahead: &[(Uuid, i64, String)],
    budget: Duration,
) -> Vec<rsi_common::rolling_queue::QueueConflictWarningV1> {
    queue_conflict_warnings_with_probe(
        repo,
        source,
        ahead,
        budget,
        |ours, theirs, timeout| merge_conflict_paths_with(repo, ours, theirs, &["git"], timeout),
        |ours, theirs, deadline| provisional_probe::classify_conflict(repo, ours, theirs, deadline),
    )
}

/// The conflict warnings with the merge probe (`ours`, `theirs`, timeout) and
/// the renumber classifier (`ours`, `theirs`, deadline) injected. A conflict the
/// classifier proves renumberable (#3336) is reported as a `*_renumberable_*`
/// notice, never as a conflict.
fn queue_conflict_warnings_with_probe(
    repo: &Path,
    source: &str,
    ahead: &[(Uuid, i64, String)],
    budget: Duration,
    mut probe: impl FnMut(&str, &str, Duration) -> MergeConflictProbe,
    mut classify: impl FnMut(&str, &str, std::time::Instant) -> provisional_probe::Renumber,
) -> Vec<rsi_common::rolling_queue::QueueConflictWarningV1> {
    use provisional_probe::Renumber;
    use rsi_common::rolling_queue::{
        ENTRY_CONFLICTS_WITH_ROLLING, ENTRY_RENUMBERABLE_WITH_ROLLING,
        QUEUE_CONFLICT_SCAN_INCOMPLETE, QUEUE_CONFLICT_WARNING_MAX_FILES,
        QUEUE_CONFLICTS_WITH_ENTRY, QUEUE_CONFLICTS_WITH_ROLLING, QUEUE_RENUMBERABLE_WITH_ENTRY,
        QUEUE_RENUMBERABLE_WITH_ROLLING, QueueConflictWarningV1,
    };
    let capped = |mut files: Vec<String>| {
        files.truncate(QUEUE_CONFLICT_WARNING_MAX_FILES);
        files
    };
    let started = std::time::Instant::now();
    let deadline = started + budget;
    let mut probe = |ours: &str, theirs: &str| {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        probe(ours, theirs, remaining.min(MERGE_PROBE_TIMEOUT))
    };
    let spent = || std::time::Instant::now() >= deadline;
    // Merge probes and renumber proofs share the same total scan deadline.
    // An unfinished proof is unknown, never a conflict with repair advice.
    let mut classify = |ours: &str, theirs: &str, files: &[String]| {
        if files.is_empty() {
            Renumber::Conflict(None)
        } else {
            classify(ours, theirs, deadline)
        }
    };
    let incomplete = |entry_id: Option<Uuid>, entry_sequence: Option<i64>, files, detail| {
        QueueConflictWarningV1 {
            code: QUEUE_CONFLICT_SCAN_INCOMPLETE.into(),
            entry_id,
            entry_sequence,
            files: capped(files),
            detail,
        }
    };
    let reason_note = |reason: &Option<String>| {
        reason.as_ref().map_or_else(String::new, |reason| {
            format!(" (its declared provisional migration does not cover them: {reason})")
        })
    };
    let mut warnings = Vec::new();
    let mut skipped = 0_usize;
    // #3741: the queue sequences whose probes were skipped, named in the
    // incomplete warning.
    let mut skipped_sequences: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
    // #3741: nearest the new queue position first (descending sequence).
    let mut nearest_first: Vec<&(Uuid, i64, String)> = ahead.iter().collect();
    nearest_first.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    // #2912: the ref resolve runs under the scan's deadline too. A resolve that
    // does not complete proves nothing about the tip, so it counts as a skipped
    // probe rather than as "no rolling ref".
    let mut tip = None;
    for reference in ["origin/rolling", "rolling"] {
        match probe_git_exit(
            repo,
            &["rev-parse", "--verify", "--quiet", reference],
            deadline,
        ) {
            Some((Some(0), stdout)) => {
                // Pin every probe to the same tip even if the ref advances.
                tip = Some(String::from_utf8_lossy(&stdout).trim().to_owned());
                break;
            }
            Some(_) => {}
            None => {
                skipped += 1;
                break;
            }
        }
    }
    if let Some(tip) = tip.as_deref() {
        if spent() {
            skipped += 1;
        } else {
            match probe(tip, source) {
                MergeConflictProbe::Conflicts(files) => {
                    warnings.push(match classify(tip, source, &files) {
                        Renumber::Unknown => {
                            skipped += 1;
                            incomplete(None, None, files,
                                "the source's renumber probe against rolling could not decide; conflict classification is unknown".into())
                        }
                        Renumber::Renumberable(note) => QueueConflictWarningV1 {
                            code: QUEUE_RENUMBERABLE_WITH_ROLLING.into(),
                            entry_id: None,
                            entry_sequence: None,
                            files: capped(files),
                            detail: format!(
                                "not a conflict: {note}; these files clash only on the migration version with the rolling tip and the lander resolves them, so no merge is needed"
                            ),
                        },
                        Renumber::Conflict(reason) => QueueConflictWarningV1 {
                            code: QUEUE_CONFLICTS_WITH_ROLLING.into(),
                            entry_id: None,
                            entry_sequence: None,
                            files: capped(files),
                            detail: format!(
                                "the source conflicts with the rolling tip: merge origin/rolling into the worker's branch, re-verify and enqueue that commit instead{}",
                                reason_note(&reason)
                            ),
                        },
                    });
                }
                MergeConflictProbe::Clean => {}
                MergeConflictProbe::Unknown { reason } => {
                    tracing::debug!(reason, "source-vs-rolling conflict probe incomplete");
                    skipped += 1;
                }
            }
        }
    }
    // Entries can share a source commit. Probe each commit against this tip
    // once per scan, retaining all paths until attribution is complete.
    let mut entry_rolling_conflicts = HashMap::new();
    let mut entry_rolling_kinds: HashMap<&str, Renumber> = HashMap::new();
    let mut probed = 0_usize;
    for (id, sequence, commit) in nearest_first
        .iter()
        .copied()
        .take(CONFLICT_PROBE_MAX_ENTRIES)
    {
        if commit == source {
            continue;
        }
        if spent() {
            skipped += 1;
            skipped_sequences.insert(*sequence);
            continue;
        }
        probed += 1;
        let entry_probe = if let Some(tip) = tip.as_deref() {
            Some(
                entry_rolling_conflicts
                    .entry(commit.as_str())
                    .or_insert_with(|| {
                        let result = probe(tip, commit);
                        if let MergeConflictProbe::Unknown { reason } = &result {
                            tracing::debug!(reason, "entry-vs-rolling conflict probe incomplete");
                            skipped += 1;
                            skipped_sequences.insert(*sequence);
                        }
                        result
                    }),
            )
        } else {
            None
        };
        let entry_files = match entry_probe.as_deref() {
            Some(MergeConflictProbe::Conflicts(files)) => Some(files),
            _ => None,
        };
        if let (Some(files), Some(tip)) = (entry_files, tip.as_deref()) {
            let kind = entry_rolling_kinds
                .entry(commit.as_str())
                .or_insert_with(|| classify(tip, commit, files))
                .clone();
            warnings.push(match kind {
                Renumber::Unknown => {
                    skipped += 1;
                    skipped_sequences.insert(*sequence);
                    incomplete(Some(*id), Some(*sequence), files.to_vec(),
                        format!("queue entry {id} (sequence {sequence}) ahead: the renumber probe against rolling could not decide; conflict classification is unknown"))
                }
                Renumber::Renumberable(note) => QueueConflictWarningV1 {
                    code: ENTRY_RENUMBERABLE_WITH_ROLLING.into(),
                    entry_id: Some(*id),
                    entry_sequence: Some(*sequence),
                    files: capped(files.to_vec()),
                    detail: format!(
                        "not a conflict: queue entry {id} (sequence {sequence}) ahead: {note}; these files clash only on the migration version with the rolling tip and the lander resolves them"
                    ),
                },
                Renumber::Conflict(reason) => QueueConflictWarningV1 {
                    code: ENTRY_CONFLICTS_WITH_ROLLING.into(),
                    entry_id: Some(*id),
                    entry_sequence: Some(*sequence),
                    files: capped(files.to_vec()),
                    detail: format!(
                        "queue entry {id} (sequence {sequence}) ahead already conflicts with the rolling tip: have that entry's worker merge origin/rolling, re-verify and enqueue the repaired commit{}",
                        reason_note(&reason)
                    ),
                },
            });
        }
        if spent() {
            skipped += 1;
            skipped_sequences.insert(*sequence);
            continue;
        }
        let mut files = match probe(commit, source) {
            MergeConflictProbe::Conflicts(files) => files,
            MergeConflictProbe::Clean => continue,
            MergeConflictProbe::Unknown { reason } => {
                tracing::debug!(reason, "entry-vs-source conflict probe incomplete");
                skipped += 1;
                skipped_sequences.insert(*sequence);
                continue;
            }
        };
        // Without the entry's rolling baseline we cannot tell which side owns
        // these paths, even though the pairwise probe itself succeeded.
        if matches!(
            entry_probe.as_deref(),
            Some(MergeConflictProbe::Unknown { .. })
        ) {
            continue;
        }
        if !files.is_empty() {
            if let Some(entry_files) = entry_files {
                files.retain(|path| !entry_files.contains(path));
            }
            if files.is_empty() {
                continue;
            }
        }
        let kind = classify(commit, source, &files);
        warnings.push(match kind {
            Renumber::Unknown => {
                skipped += 1;
                skipped_sequences.insert(*sequence);
                incomplete(Some(*id), Some(*sequence), files,
                    format!("the source's renumber probe against queue entry {id} (sequence {sequence}) ahead could not decide; conflict classification is unknown"))
            }
            Renumber::Renumberable(note) => QueueConflictWarningV1 {
                code: QUEUE_RENUMBERABLE_WITH_ENTRY.into(),
                entry_id: Some(*id),
                entry_sequence: Some(*sequence),
                files: capped(files),
                detail: format!(
                    "not a conflict: {note}; these files clash only on the migration version with queue entry {id} (sequence {sequence}) ahead and the lander resolves them once that entry lands, so no merge or cancellation is needed"
                ),
            },
            Renumber::Conflict(reason) => QueueConflictWarningV1 {
                code: QUEUE_CONFLICTS_WITH_ENTRY.into(),
                entry_id: Some(*id),
                entry_sequence: Some(*sequence),
                files: capped(files),
                detail: format!(
                    "the source conflicts with queue entry {id} (sequence {sequence}) ahead of it: once that entry lands, have the worker merge origin/rolling and re-verify, or cancel this entry until then; if that entry is withdrawn or refused this warning no longer applies{}",
                    reason_note(&reason)
                ),
            },
        });
    }
    // Entries past the probe cap are never inspected: say so, so a missing
    // warning for them never reads as "no conflict".
    for (_, sequence, _) in nearest_first
        .iter()
        .copied()
        .skip(CONFLICT_PROBE_MAX_ENTRIES)
    {
        skipped += 1;
        skipped_sequences.insert(*sequence);
    }
    tracing::info!(
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        probed,
        skipped,
        warnings = warnings.len(),
        "enqueue phase: conflict_scan"
    );
    if skipped > 0 {
        warnings.push(QueueConflictWarningV1 {
            code: QUEUE_CONFLICT_SCAN_INCOMPLETE.into(),
            entry_id: None,
            entry_sequence: None,
            files: Vec::new(),
            detail: format!(
                "the conflict scan stopped after {} s with {skipped} probe(s) not completed{}, so the absence of a warning for them proves nothing; the scan is advisory and never holds or refuses a source, and the lander still checks the merge",
                budget.as_secs().max(1),
                skipped_sequences_note(&skipped_sequences)
            ),
        });
    }
    warnings
}

/// #3741: the " (queue sequences not fully checked: ...)" clause of the
/// incomplete warning, nearest first, or empty when no entry was skipped.
fn skipped_sequences_note(sequences: &std::collections::BTreeSet<i64>) -> String {
    if sequences.is_empty() {
        return String::new();
    }
    let listed: Vec<String> = sequences
        .iter()
        .rev()
        .take(SKIPPED_SEQUENCES_LISTED)
        .map(i64::to_string)
        .collect();
    let more = sequences.len().saturating_sub(SKIPPED_SEQUENCES_LISTED);
    let tail = if more == 0 {
        String::new()
    } else {
        format!(" and {more} more")
    };
    format!(
        " (queue sequences not fully checked: {}{tail})",
        listed.join(", ")
    )
}

/// #3036: the wall budget of one refused land's scan for conflicting entries
/// ahead. It runs once per refused land attempt (the executor records the
/// result), off any RPC path.
pub(crate) const LAND_CONFLICT_SCAN_BUDGET: Duration = Duration::from_secs(30);

/// #3036: the live entries ahead (`ahead`: id, sequence, source commit) whose
/// own change conflicts with `source`, a source the queue just refused
/// `queue_batch_merge_conflict`. An entry counts when `git merge-tree` of the
/// two sources conflicts on a path the entry itself changes relative to the
/// rolling tip (`<tip>...<entry>`): that conflict is the entry's, so a repair
/// that merges rolling before the entry publishes is refused again. A conflict
/// with rolling that every newer entry merely carries is not the entry's and
/// never holds a repair. A conflict git names no path for, or with no rolling
/// ref to compare against, counts (a wait ends when the entry settles); a probe
/// that cannot run or a spent `budget` proves nothing and adds none, so the
/// repair runs at once. Blocking; call from `spawn_blocking`.
pub(crate) fn conflicting_entries_ahead(
    repo: &Path,
    source: &str,
    ahead: &[(Uuid, i64, String)],
    budget: Duration,
) -> Vec<Uuid> {
    let deadline = std::time::Instant::now() + budget;
    let tip = ["origin/rolling", "rolling"].into_iter().find(|reference| {
        matches!(
            probe_git_exit(
                repo,
                &["rev-parse", "--verify", "--quiet", reference],
                deadline
            ),
            Some((Some(0), _))
        )
    });
    let mut blockers = Vec::new();
    for (id, _, commit) in ahead.iter().take(CONFLICT_PROBE_MAX_ENTRIES) {
        if commit == source {
            continue;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let MergeConflictProbe::Conflicts(paths) = merge_conflict_paths_with(
            repo,
            commit,
            source,
            &["git"],
            remaining.min(MERGE_PROBE_TIMEOUT),
        ) else {
            continue;
        };
        if entry_owns_conflict(repo, tip, commit, &paths, deadline) {
            blockers.push(*id);
        }
    }
    blockers
}

/// Whether entry `commit` itself changes one of the conflict `paths` relative
/// to `tip`; an unnamed conflict or an unknown tip counts as the entry's.
fn entry_owns_conflict(
    repo: &Path,
    tip: Option<&str>,
    commit: &str,
    paths: &[String],
    deadline: std::time::Instant,
) -> bool {
    let Some(tip) = tip else {
        return true;
    };
    if paths.is_empty() {
        return true;
    }
    let range = format!("{tip}...{commit}");
    let Some((Some(0), stdout)) = probe_git_exit(
        repo,
        &["diff", "--name-only", "--no-renames", &range],
        deadline,
    ) else {
        return false;
    };
    let text = String::from_utf8_lossy(&stdout);
    text.lines()
        .map(str::trim)
        .any(|changed| paths.iter().any(|path| path == changed))
}

/// Exit status of `git grep` run in `repo`: `Some(true)` on a match,
/// `Some(false)` on none, `None` when git itself failed.
fn git_grep_hits(repo: &Path, args: &[&str]) -> Option<(bool, String)> {
    let mut command = git_command();
    command.arg("-C").arg(repo).arg("grep").args(args);
    let (code, stdout) = match run_under_git_deadline(command) {
        Some(outcome) => outcome?,
        None => {
            let output = git_command()
                .arg("-C")
                .arg(repo)
                .arg("grep")
                .args(args)
                .stdin(Stdio::null())
                .output()
                .ok()?;
            (output.status.code(), output.stdout)
        }
    };
    match code {
        Some(0) => Some((true, String::from_utf8_lossy(&stdout).into_owned())),
        Some(1) => Some((false, String::new())),
        _ => None,
    }
}

/// First test filter in `filters` that provably selects no test at `commit`
/// (#1129), or `None`. A cheap static necessary condition, never a refusal on
/// doubt: every `::`-separated segment of the filter's test-name pattern must
/// occur in the source of the package (for `rsid=shard:S:test(N)`, of the
/// files carrying shard S's gate). Forms the check cannot reason about
/// (`bin:`/`test:` targets, filtersets other than `test(...)`) are accepted.
#[must_use]
pub fn filter_selecting_no_tests(repo: &Path, commit: &str, filters: &[String]) -> Option<String> {
    filters
        .iter()
        .find(|filter| filter_provably_empty(repo, commit, filter))
        .cloned()
}

/// What to add to a refusal that a test filter selects no test (#1937). A
/// plain `PACKAGE=NAME` filters the package's library tests only, so the name
/// of an integration test target (`tests/NAME.rs`) or a binary (`src/bin/NAME.rs`,
/// `src/main.rs`) selects nothing; those targets are `PACKAGE=test:NAME` and
/// `PACKAGE=bin:NAME`. Names the form a refused filter should have when
/// `commit` has such a target. Starts with a space, so it appends to a message.
#[must_use]
pub fn no_test_filter_hint(repo: &Path, commit: &str, filters: &[String]) -> String {
    let exists = |path: &str| git(repo, &["cat-file", "-e", &format!("{commit}:{path}")]).is_some();
    let mut specific = Vec::new();
    for filter in filters {
        let Some((package, name)) = filter.split_once('=') else {
            continue;
        };
        if name.is_empty() || name.contains([':', '(', '/']) {
            continue;
        }
        let dir = format!("crates/{package}");
        if exists(&format!("{dir}/tests/{name}.rs"))
            || exists(&format!("{dir}/tests/{name}/main.rs"))
        {
            specific.push(format!(
                "`{filter}` is an integration test target: use `{package}=test:{name}`"
            ));
        }
        if exists(&format!("{dir}/src/bin/{name}.rs"))
            || exists(&format!("{dir}/src/bin/{name}/main.rs"))
            || (name == package && exists(&format!("{dir}/src/main.rs")))
        {
            specific.push(format!(
                "`{filter}` is a binary: use `{package}=bin:{name}`"
            ));
        }
    }
    let mut hint = String::from(
        " (a plain `PACKAGE=NAME` filters library tests; an integration test target is \
         `PACKAGE=test:NAME` and a binary is `PACKAGE=bin:NAME`",
    );
    for form in specific {
        hint.push_str("; ");
        hint.push_str(&form);
    }
    hint.push(')');
    hint
}

fn filter_provably_empty(repo: &Path, commit: &str, filter: &str) -> bool {
    let Some((package, value)) = filter.split_once('=') else {
        return false;
    };
    let (shard, pattern) = match value.strip_prefix("shard:") {
        Some(rest) => match rest.split_once(':') {
            Some((shard, filterset)) => match filterset
                .strip_prefix("test(")
                .and_then(|inner| inner.strip_suffix(')'))
            {
                Some(name) => (Some(shard), name),
                None => return false,
            },
            // A whole shard always has tests.
            None => return false,
        },
        None => (None, value),
    };
    if pattern.is_empty()
        || pattern.starts_with("bin:")
        || pattern.starts_with("test:")
        || !pattern
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_:.-".contains(&byte))
    {
        return false;
    }
    let dir = format!("crates/{package}");
    // A tree without the package (a fixture, a foreign repo) cannot be reasoned about.
    if git(repo, &["ls-tree", "--name-only", commit, &dir]).is_none_or(|out| out.trim().is_empty())
    {
        return false;
    }
    let mut scope: Vec<String> = vec![dir.clone()];
    let mut package_dirs: Vec<String> = vec![dir];
    // #1244: the lander runs a focused `rsid`/`rsid-store` filter (shard form
    // or plain name) on one lib build of both packages, so the name may live
    // in either package and in any shard; only an unknown shard still refuses.
    let lib_family = rsid_lib_family_dirs(repo, commit, package);
    if let Some(shard) = shard {
        // A shard spans every package that declares its feature (rsid and
        // rsid-store since the #1021 S4 crate split), so the gate lookup covers
        // all of them at this commit; with no manifest declaring it (a fixture,
        // an older layout) it stays in the named package.
        let declaring = shard_declaring_package_dirs(repo, commit, shard);
        let search_dirs = if declaring.is_empty() {
            scope.clone()
        } else {
            declaring
        };
        let gate = format!("test-shard-{shard}\"");
        let mut grep_args = vec!["-l", "-F", "-e", &gate, commit, "--"];
        grep_args.extend(search_dirs.iter().map(String::as_str));
        let Some((hit, listing)) = git_grep_hits(repo, &grep_args) else {
            return false;
        };
        if !hit {
            return true;
        }
        scope = listing
            .lines()
            .filter_map(|line| line.split_once(':').map(|(_, path)| path.to_string()))
            .collect();
        package_dirs = search_dirs;
    }
    if !lib_family.is_empty() {
        scope.clone_from(&lib_family);
        package_dirs = lib_family;
    }
    pattern
        .split("::")
        .filter(|segment| !segment.is_empty())
        .any(|segment| segment_provably_absent(repo, commit, segment, &scope, &package_dirs))
}

/// Whether the test-name `segment` provably names nothing at `commit` (#1169).
/// A nextest test path is `<module path>::<fn>`: a segment is present when it
/// occurs in the text of a gated file in `scope`, or names a module the way
/// the file tree does (a path component or stem of a `scope` file:
/// `thread_stacks.rs` carries `thread_stacks::tests::...` without ever
/// spelling the name), or is declared `mod <segment>` anywhere in
/// `package_dirs` (a `#[path]`/inline module). Git failing is not a miss.
fn segment_provably_absent(
    repo: &Path,
    commit: &str,
    segment: &str,
    scope: &[String],
    package_dirs: &[String],
) -> bool {
    let mut args = vec!["-q", "-F", "-e", segment, commit, "--"];
    args.extend(scope.iter().map(String::as_str));
    if !matches!(git_grep_hits(repo, &args), Some((false, _))) {
        return false;
    }
    let names_a_module = |path: &str| {
        path.split('/')
            .any(|component| component.strip_suffix(".rs").unwrap_or(component) == segment)
    };
    if scope.iter().any(|path| names_a_module(path)) {
        return false;
    }
    let declaration = format!(r"\bmod[[:space:]]+{segment}\b");
    let mut args = vec!["-q", "-E", "-e", declaration.as_str(), commit, "--"];
    args.extend(package_dirs.iter().map(String::as_str));
    matches!(git_grep_hits(repo, &args), Some((false, _)))
}

/// `crates/rsid` and `crates/rsid-store` as present at `commit` when
/// `package` is one of them (empty otherwise): the packages whose library
/// tests one focused lander run selects from (#1244).
fn rsid_lib_family_dirs(repo: &Path, commit: &str, package: &str) -> Vec<String> {
    if package != "rsid" && package != "rsid-store" {
        return Vec::new();
    }
    ["crates/rsid", "crates/rsid-store"]
        .into_iter()
        .filter(|dir| {
            git(repo, &["ls-tree", "--name-only", commit, dir])
                .is_some_and(|out| !out.trim().is_empty())
        })
        .map(str::to_owned)
        .collect()
}

/// `crates/<package>` for every workspace package whose manifest at `commit`
/// declares the `test-shard-<shard>` feature (empty when none does).
fn shard_declaring_package_dirs(repo: &Path, commit: &str, shard: &str) -> Vec<String> {
    let declaration = format!("test-shard-{shard} = ");
    let Some((true, listing)) = git_grep_hits(
        repo,
        &[
            "-l",
            "-F",
            "-e",
            &declaration,
            commit,
            "--",
            ":(glob)crates/*/Cargo.toml",
        ],
    ) else {
        return Vec::new();
    };
    let mut dirs: Vec<String> = listing
        .lines()
        .filter_map(|line| {
            let (_, path) = line.split_once(':')?;
            Some(path.strip_suffix("/Cargo.toml")?.to_string())
        })
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

/// The schema head at `revision`: the highest `migrations/vNNN.rs`, else the
/// legacy constant in `store/mod.rs`.
fn schema_version_at(repo: &Path, revision: &str) -> Option<u32> {
    // The head comes from the directory the revision's store build consumes:
    // the rsid-store one when populated, else the pre-split one (#1021 S4). A
    // unit in the other directory never runs, so it must not count.
    let head_in = |directory: &str| {
        git(
            repo,
            &["ls-tree", "--name-only", revision, &format!("{directory}/")],
        )
        .and_then(|listing| {
            listing
                .lines()
                .filter_map(|path| {
                    let name = path.rsplit('/').next()?;
                    name.strip_prefix('v')?.strip_suffix(".rs")?.parse().ok()
                })
                .max()
        })
    };
    let from_files = head_in(MIGRATION_DIR).or_else(|| head_in(PRE_SPLIT_MIGRATION_DIR));
    from_files.or_else(|| {
        let text = LEGACY_MIGRATION_FILES
            .iter()
            .find_map(|file| git(repo, &["show", &format!("{revision}:{file}")]))?;
        let marker = "pub const LATEST_SCHEMA_VERSION: i32 = ";
        text.split(marker)
            .nth(1)?
            .split(';')
            .next()?
            .trim()
            .parse()
            .ok()
    })
}

/// Migration version and hot files the source adds relative to rolling.
/// Best effort: any git failure leaves the facts empty.
pub(crate) fn derive_source_facts(repo: &Path, source: &str) -> SourceFacts {
    let base = ["origin/rolling", "rolling"].iter().find_map(|reference| {
        git(repo, &["merge-base", source, reference]).map(|out| out.trim().to_string())
    });
    let Some(base) = base.filter(|base| !base.is_empty()) else {
        return SourceFacts::default();
    };
    let version_at = |revision: &str| schema_version_at(repo, revision);
    let migration_version = match (version_at(source), version_at(&base)) {
        (Some(source_version), Some(base_version)) if source_version > base_version => {
            Some(source_version)
        }
        _ => None,
    };
    let changed = git(
        repo,
        &["diff", "--name-only", "--no-renames", &base, source],
    )
    .unwrap_or_default();
    let hot_files = changed
        .lines()
        .filter(|path| HOT_FILES.contains(path))
        .map(str::to_string)
        .collect();
    SourceFacts {
        migration_version,
        hot_files,
    }
}

/// Wall-clock cap on one filter derivation (`check-touched-shards --filters-only`).
const DERIVE_FILTERS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// #3372: the most changed files a filter derivation is attempted for. The
/// checker traces each changed source through the call graph, so a wider diff
/// (a mechanical rename across hundreds of files) cannot finish inside
/// [`DERIVE_FILTERS_TIMEOUT`]; it is recorded with `lander_selects` at once
/// instead of burning the whole budget first.
const DERIVE_FILTERS_MAX_CHANGED_FILES: usize = 300;

/// The budget one derivation runs under: wall clock and changed-file count.
#[derive(Debug, Clone, Copy)]
struct DeriveBudget {
    timeout: std::time::Duration,
    max_changed_files: usize,
}

const DEFAULT_DERIVE_BUDGET: DeriveBudget = DeriveBudget {
    timeout: DERIVE_FILTERS_TIMEOUT,
    max_changed_files: DERIVE_FILTERS_MAX_CHANGED_FILES,
};

/// Per-repository budget overrides, so a test shrinks the budget of its own
/// fixture repository without touching any other test's derivation.
#[cfg(test)]
static DERIVE_BUDGET_OVERRIDES: std::sync::LazyLock<
    std::sync::Mutex<HashMap<PathBuf, DeriveBudget>>,
> = std::sync::LazyLock::new(Default::default);

fn derive_budget(repo: &Path) -> DeriveBudget {
    #[cfg(test)]
    if let Some(budget) = DERIVE_BUDGET_OVERRIDES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(repo)
    {
        return *budget;
    }
    let _ = repo;
    DEFAULT_DERIVE_BUDGET
}

/// Run derivations of `repo` under a budget of `timeout` and `max_changed_files`.
#[cfg(test)]
pub(crate) fn set_derive_budget_for_test(
    repo: &Path,
    timeout: std::time::Duration,
    max_changed_files: usize,
) {
    DERIVE_BUDGET_OVERRIDES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            repo.to_path_buf(),
            DeriveBudget {
                timeout,
                max_changed_files,
            },
        );
}

/// Why a filter derivation produced no filters (#3372).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DerivationFailure {
    /// The derivation exceeded its budget (it timed out, or the diff is wider
    /// than [`DERIVE_FILTERS_MAX_CHANGED_FILES`]). Not a fault of the source:
    /// the entry is kept and the lander selects its tests
    /// (`QueueFiltersState::LanderSelects`).
    OverBudget(String),
    /// The checker did not start or printed no usable receipt.
    Failed(String),
}

/// #1807/#1781/#1786: derive the landing filters of `source` the way a hand
/// landing does, so no agent copies, prunes or splits them. Runs the repo's
/// `scripts/check-touched-shards --filters-only --json` on `source` against the
/// rolling tip, then normalizes the suggested filters (dedupe focused
/// rsid/rsid-store filters across shards, split filters over the byte limit;
/// the count is never capped).
///
/// Any failure (no script, no rolling ref, a timeout, unparsable output)
/// yields no filters: the unfiltered gate is the always-correct fallback.
pub(crate) async fn derive_test_filters(repo: &Path, source: &str) -> Vec<String> {
    try_derive_test_filters(repo, source)
        .await
        .unwrap_or_default()
}

/// [`derive_test_filters`] that tells a derivation that FAILED (the checker did
/// not start or printed no usable receipt) or went OVER BUDGET (#3372: timed
/// out, or the diff is too wide to trace in time) from one with nothing to
/// derive (no checker script, no rolling ref: `Ok` of no filters, the always
/// correct unfiltered gate). #2993: an entry recorded before its derivation
/// finished is refused on a failure, never gated unfiltered by surprise; one
/// over budget is kept and the lander selects its tests.
pub(crate) async fn try_derive_test_filters(
    repo: &Path,
    source: &str,
) -> std::result::Result<Vec<String>, DerivationFailure> {
    let script = repo.join("scripts").join("check-touched-shards");
    if !script.is_file() {
        tracing::warn!(repo = %repo.display(), "derive_filters: no scripts/check-touched-shards");
        return Ok(Vec::new());
    }
    let repo = repo.to_path_buf();
    let Some(base) = ["origin/rolling", "rolling"].into_iter().find(|reference| {
        git(
            &repo,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{reference}^{{commit}}"),
            ],
        )
        .is_some()
    }) else {
        return Ok(Vec::new());
    };
    let budget = derive_budget(&repo);
    // #3372: a diff too wide to trace is not run at all; the count is one cheap
    // git read, so the entry is kept at once instead of after the whole timeout.
    let counted = {
        let (probe_repo, probe_base, probe_source) =
            (repo.clone(), base.to_owned(), source.to_owned());
        tokio::task::spawn_blocking(move || {
            changed_file_count(&probe_repo, &probe_base, &probe_source)
        })
        .await
        .unwrap_or_default()
    };
    if let Some(count) = counted.filter(|count| *count > budget.max_changed_files) {
        tracing::warn!(count, "derive_filters: diff too wide to derive");
        return Err(DerivationFailure::OverBudget(format!(
            "the source changes {count} files, over the {} the filter derivation traces in time",
            budget.max_changed_files
        )));
    }
    let mut command =
        tokio::process::Command::new(if cfg!(windows) { "python" } else { "python3" });
    // #3391: the checker is a script from the repository under test.
    crate::lander_env_policy::apply_tokio(&mut command);
    command
        .arg(&script)
        .args(["--filters-only", "--json", "--repo"])
        .arg(&repo)
        .args(["--base", base, "--head", source])
        .current_dir(&repo)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(budget.timeout, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            tracing::warn!(%error, "derive_filters: check-touched-shards did not start");
            return Err(DerivationFailure::Failed(format!(
                "check-touched-shards did not start: {error}"
            )));
        }
        Err(_) => {
            tracing::warn!("derive_filters: check-touched-shards timed out");
            return Err(DerivationFailure::OverBudget(format!(
                "check-touched-shards timed out after {} s",
                budget.timeout.as_secs()
            )));
        }
    };
    // A nonzero exit still prints a receipt (a failed audit); its filters are
    // still the diff's filters.
    parse_receipt_filters(&String::from_utf8_lossy(&output.stdout)).ok_or_else(|| {
        DerivationFailure::Failed(format!(
            "check-touched-shards exited {} without a usable suggested_filters receipt",
            output
                .status
                .code()
                .map_or_else(|| "by signal".to_owned(), |code| code.to_string())
        ))
    })
}

/// How many files `source` changes against `base` (#3372), or `None` when git
/// cannot say (then the checker runs under its wall-clock budget as before).
fn changed_file_count(repo: &Path, base: &str, source: &str) -> Option<usize> {
    let range = format!("{base}...{source}");
    git(repo, &["diff", "--name-only", &range]).map(|names| names.lines().count())
}

/// #2907: how long `AgentEnqueueLandingSource` waits inline for the filter
/// derivation before it records the entry with its filters pending (#2993).
pub(crate) const ENQUEUE_DERIVE_WAIT: Duration = Duration::from_secs(4);

/// #2912: the wall bound of `AgentEnqueueLandingSource` as a whole. The filter
/// derivation wait ([`ENQUEUE_DERIVE_WAIT`]) and the source probe share it, so
/// the probe gets whatever the derivation left; every git read of the call
/// (tip, source, conflict scan) is killed at its phase's deadline, and a probe
/// the deadline cut short is refused `queue_probe_timeout` (retryable).
pub(crate) const ENQUEUE_WALL_BOUND: Duration = Duration::from_secs(5);

/// How long a finished derivation stays reusable for the same repository,
/// source and rolling tip.
const DERIVATION_REUSE: Duration = Duration::from_secs(600);

type DerivationKey = (PathBuf, String, String);

/// What a derivation ended with: its filters, or why it failed (#2993).
type DerivationResult = std::result::Result<Vec<String>, DerivationFailure>;

struct Derivation {
    tx: tokio::sync::watch::Sender<Option<DerivationResult>>,
    finished: Option<std::time::Instant>,
}

/// In-flight and recently finished derivations (#2907). A derivation outlives
/// the call that started it: a call that stops waiting leaves it running, and
/// the retry joins it (or reads its result) instead of starting again.
static DERIVATIONS: std::sync::LazyLock<std::sync::Mutex<HashMap<DerivationKey, Derivation>>> =
    std::sync::LazyLock::new(Default::default);

fn derivations() -> std::sync::MutexGuard<'static, HashMap<DerivationKey, Derivation>> {
    DERIVATIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The commit `origin/rolling` (else `rolling`) names; empty when neither does.
/// `None` when a read did not complete by `deadline` (#2912): the tip is then
/// unknown, which is not the same as having none.
fn base_tip(repo: &Path, deadline: std::time::Instant) -> Option<String> {
    for reference in ["origin/rolling", "rolling"] {
        let spec = format!("{reference}^{{commit}}");
        let (code, stdout) =
            probe_git_exit(repo, &["rev-parse", "--verify", "--quiet", &spec], deadline)?;
        if code == Some(0) {
            return Some(String::from_utf8_lossy(&stdout).trim().to_owned());
        }
    }
    Some(String::new())
}

/// How a [`derive_test_filters_shared`] wait ended (#2993). Only the first two
/// are answers; the other two keep the caller from reading a missing answer as
/// "no filters".
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SharedDerivation {
    /// The derivation finished: its filters (none when there was nothing to
    /// derive, the always correct unfiltered gate).
    Ready(Vec<String>),
    /// The derivation ran and failed; the reason. Not reused: a retry runs it
    /// again.
    Failed(String),
    /// #3372: the derivation exceeded its budget (timed out, or the diff is too
    /// wide to trace in time); the reason. Not a fault of the source: the
    /// caller keeps the entry and the lander selects its tests. Not reused.
    OverBudget(String),
    /// Still running at the caller's limit. It keeps running in the
    /// background and joins the next caller.
    Running,
    /// The rolling-tip read behind the derivation key did not complete by the
    /// caller's deadline (#2912, a slow or stalled git): the tip, so the
    /// derivation, is unknown. No derivation was started.
    TipUnreadable,
}

/// [`try_derive_test_filters`] run at most once per (repository, source, rolling
/// tip) and shared by every caller that asks while it runs or shortly after
/// (#2907). `wait` bounds how long this caller waits: `None` waits for the
/// answer, `Some(limit)` returns [`SharedDerivation::Running`] when the
/// derivation is still running at `limit` (it keeps running in the background
/// and the next call joins it). A derivation that yielded no filters or failed
/// is not reused, so a retry runs it again.
pub(crate) async fn derive_test_filters_shared(
    repo: &Path,
    source: &str,
    wait: Option<Duration>,
) -> SharedDerivation {
    // #2912: the wait is one deadline over the whole call: the tip read runs
    // under it and the answer wait gets what is left. A tip read that does not
    // complete is its own outcome (#2993), not a derivation that is running.
    let deadline = wait.map(|limit| std::time::Instant::now() + limit);
    let tip = {
        let repo = repo.to_path_buf();
        let read_by = deadline.unwrap_or_else(|| std::time::Instant::now() + MERGE_PROBE_TIMEOUT);
        tokio::task::spawn_blocking(move || base_tip(&repo, read_by))
            .await
            .unwrap_or_default()
    };
    let Some(tip) = tip else {
        return SharedDerivation::TipUnreadable;
    };
    let key: DerivationKey = (repo.to_path_buf(), source.to_owned(), tip);
    let mut rx = {
        let mut map = derivations();
        map.retain(|_, derivation| {
            derivation
                .finished
                .is_none_or(|at| at.elapsed() < DERIVATION_REUSE)
        });
        if let Some(derivation) = map.get(&key) {
            derivation.tx.subscribe()
        } else {
            let (tx, rx) = tokio::sync::watch::channel(None);
            map.insert(
                key.clone(),
                Derivation {
                    tx: tx.clone(),
                    finished: None,
                },
            );
            let (repo, source) = (key.0.clone(), key.1.clone());
            let task_key = key.clone();
            tokio::spawn(async move {
                let started = std::time::Instant::now();
                let derived = try_derive_test_filters(&repo, &source).await;
                tracing::info!(
                    elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    filters = derived.as_ref().map_or(0, Vec::len),
                    failed = derived.is_err(),
                    "enqueue phase: derive_filters"
                );
                {
                    let mut map = derivations();
                    if derived.as_ref().is_ok_and(|filters| !filters.is_empty()) {
                        if let Some(derivation) = map.get_mut(&task_key) {
                            derivation.finished = Some(std::time::Instant::now());
                        }
                    } else {
                        map.remove(&task_key);
                    }
                }
                tx.send_replace(Some(derived));
            });
            rx
        }
    };
    let answer = async {
        match rx.wait_for(Option::is_some).await {
            Ok(done) => match done.clone() {
                Some(Ok(filters)) => SharedDerivation::Ready(filters),
                Some(Err(DerivationFailure::Failed(reason))) => SharedDerivation::Failed(reason),
                Some(Err(DerivationFailure::OverBudget(reason))) => {
                    SharedDerivation::OverBudget(reason)
                }
                None => SharedDerivation::Running,
            },
            Err(_) => {
                SharedDerivation::Failed("the derivation task ended without an answer".into())
            }
        }
    };
    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline.into(), answer)
            .await
            .unwrap_or(SharedDerivation::Running),
        None => answer.await,
    }
}

/// Entries whose filter derivation a task in this process is finishing (#2993).
static FINISHING_DERIVATIONS: std::sync::LazyLock<std::sync::Mutex<HashSet<Uuid>>> =
    std::sync::LazyLock::new(Default::default);

/// Finish the filters of `entry`, recorded with its derivation pending (#2993),
/// in the background: wait for the shared derivation of `commit` in `repo`, then
/// store its filters so the lander can admit the entry, or settle it `refused`
/// with its typed reason (`queue_filter_derivation_failed`,
/// `queue_tip_read_timeout`, `filter_matches_no_tests: <filter>`) and wake its
/// enqueuer once. At most one task per entry; a call for an entry a task already
/// finishes does nothing, so the enqueue verb and the loop's sweep never race.
pub(crate) fn finish_filter_derivation_in_background(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    entry: Uuid,
    repo: PathBuf,
    commit: String,
) {
    {
        let mut finishing = FINISHING_DERIVATIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !finishing.insert(entry) {
            return;
        }
    }
    let store = Arc::clone(store);
    tokio::spawn(async move {
        if let Err(error) = finish_filter_derivation(&store, entry, &repo, &commit).await {
            tracing::warn!(%entry, %error, "rolling queue filter derivation could not be recorded; the next tick retries");
        }
        FINISHING_DERIVATIONS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&entry);
    });
}

async fn finish_filter_derivation(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    entry: Uuid,
    repo: &Path,
    commit: &str,
) -> Result<()> {
    let refusal = match derive_test_filters_shared(repo, commit, None).await {
        SharedDerivation::Ready(filters) => {
            // The inline path refuses a filter that provably selects no test;
            // an entry recorded earlier gets the same verdict as its outcome.
            let (probe_repo, probe_commit, probe_filters) =
                (repo.to_path_buf(), commit.to_owned(), filters.clone());
            let empty = tokio::task::spawn_blocking(move || {
                with_git_deadline(std::time::Instant::now() + MERGE_PROBE_TIMEOUT, || {
                    filter_selecting_no_tests(&probe_repo, &probe_commit, &probe_filters)
                })
            })
            .await
            .map_err(|error| DaemonError::Process(format!("filter probe join: {error}")))?;
            match empty {
                (Some(filter), _) => Some((
                    rsi_common::rolling_queue::QUEUE_FILTER_MATCHES_NO_TESTS,
                    format!("a derived filter selects no test: {filter}"),
                )),
                (None, _) => {
                    with_store(store, move |s| {
                        s.finish_rolling_queue_filter_derivation(entry, &filters)
                    })
                    .await?;
                    None
                }
            }
        }
        SharedDerivation::Failed(reason) => Some((
            rsi_common::rolling_queue::QUEUE_FILTER_DERIVATION_FAILED,
            reason,
        )),
        SharedDerivation::OverBudget(reason) => {
            // #3372: not a refusal. Keep the entry with no derived filters; the
            // lander's bounded closure selection and the automatic full-library
            // QA cover it.
            tracing::warn!(%entry, %reason, "rolling queue filter derivation over budget; the lander selects the tests");
            with_store(store, move |s| {
                s.mark_rolling_queue_filters_lander_selects(entry, true)
            })
            .await?;
            None
        }
        SharedDerivation::TipUnreadable | SharedDerivation::Running => Some((
            rsi_common::rolling_queue::QUEUE_TIP_READ_TIMEOUT,
            "the rolling-tip read behind the filter derivation did not finish".to_owned(),
        )),
    };
    if let Some((code, detail)) = refusal {
        with_store(store, move |s| {
            s.refuse_rolling_queue_filter_derivation(entry, code, &detail, Utc::now())
        })
        .await?;
    }
    Ok(())
}

/// Re-attach a derivation to every entry recorded with its filters pending
/// (#2993): after a daemon restart, or a task that did not record its result.
/// Called on every queue tick; an entry a task already finishes is skipped.
pub(crate) async fn resume_pending_filter_derivations(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
) {
    match with_store(store, |s| s.rolling_queue_entries_deriving_filters()).await {
        Ok(pending) => {
            for (entry, repo, commit) in pending {
                finish_filter_derivation_in_background(store, entry, PathBuf::from(repo), commit);
            }
        }
        Err(error) => tracing::warn!(%error, "rolling queue pending-derivation sweep deferred"),
    }
}

/// The normalized `suggested_filters` of a `check-touched-shards --json` receipt.
/// Any malformed part (unparsable receipt, a non-string entry, a filter the
/// queue would refuse) yields no filters: never a partial set, which would
/// gate less than the checker selected.
#[cfg(test)]
fn parse_suggested_filters(stdout: &str) -> Vec<String> {
    parse_receipt_filters(stdout).unwrap_or_default()
}

/// [`parse_suggested_filters`] telling a malformed receipt (`None`) from one
/// that suggests no filters (`Some` of none).
fn parse_receipt_filters(stdout: &str) -> Option<Vec<String>> {
    let receipt = serde_json::from_str::<serde_json::Value>(stdout).ok()?;
    let value = receipt.get("suggested_filters")?;
    let filters = serde_json::from_value::<Vec<String>>(value.clone()).ok()?;
    let filters = rsi_common::rolling_queue::normalize_test_filters(filters);
    filters
        .iter()
        .all(|filter| rsi_common::rolling_queue::valid_test_filter(filter))
        .then_some(filters)
}

/// After a fetch, the published tip when `source` is an ancestor of
/// `origin/rolling`. `None` when it is not (or git cannot say).
pub(crate) fn landed_tip(repo: &Path, source: &str) -> Option<String> {
    landed_tips(repo, &[source.to_string()]).pop().flatten()
}

/// #2732: whether git positively says `commit` is NOT an ancestor of a freshly
/// fetched `origin/rolling` (the commit is supplied work that has not landed).
/// `false` when it is an ancestor, or when the fetch or git cannot say, so an
/// outage never reads as unlanded work: a failed fetch leaves a cached
/// `origin/rolling` that may predate the commit's landing, so it is never read.
pub(crate) fn commit_off_rolling(repo: &Path, commit: &str) -> bool {
    if fetch_rolling(repo).is_err() {
        return false;
    }
    let Some(tip) = read_origin_tip(repo) else {
        return false;
    };
    git_command()
        .arg("-C")
        .arg(repo)
        .args(["merge-base", "--is-ancestor", commit, &tip])
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|output| output.status.code() == Some(1))
}

/// After ONE fetch, the published tip for each source that is an ancestor of
/// `origin/rolling`, in input order (`None`: not landed, or git cannot say).
pub(crate) fn landed_tips(repo: &Path, sources: &[String]) -> Vec<Option<String>> {
    let Some(tip) = fetch_rolling(repo).ok().and_then(|_| read_origin_tip(repo)) else {
        return vec![None; sources.len()];
    };
    sources
        .iter()
        .map(|source| {
            git(repo, &["merge-base", "--is-ancestor", source, &tip]).map(|_| tip.clone())
        })
        .collect()
}

/// The committer identity only the queue's lander writes (`rsi-rolling-land`:
/// its integration merges, its renumbered provisional-migration commits and its
/// pin commits). The manager `Integrate` executor commits as
/// `integration@rsi.local`, so this separates the two writers of the same
/// `rsi integration: merge <source> into rolling` subject (#4040).
const LANDER_COMMITTER_EMAIL: &str = "rsi-rolling-land@rsi.invalid";

/// #4009/#4040: the commit the queue's lander itself published for `source` on
/// `origin/rolling`. It is the commit committed by the lander's identity that has
/// `source` as a parent, whatever its subject: the integration merge, the
/// renumbered provisional-migration commit (parents: target and source) and the
/// pin commit on a fast-forwarded source all qualify, while a hand push or a
/// manager `Integrate` with the same subject does not. `None` when no lander
/// commit names the source (it landed another way, or by fast-forward, which
/// writes no commit) or git cannot say.
pub(crate) fn lander_publication_commit(repo: &Path, source: &str) -> Option<String> {
    let full = git(
        repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{source}^{{commit}}"),
        ],
    )?
    .trim()
    .to_string();
    let log = git(
        repo,
        &[
            "log",
            "--format=%H%x09%ce%x09%P",
            "--ancestry-path",
            "--reverse",
            &format!("{full}..origin/rolling"),
        ],
    )?;
    log.lines().find_map(|line| {
        let mut fields = line.split('\t');
        let sha = fields.next()?;
        let email = fields.next()?;
        let parents = fields.next()?;
        (email == LANDER_COMMITTER_EMAIL && parents.split_whitespace().any(|p| p == full))
            .then(|| sha.to_string())
    })
}

/// #4040: the gate receipt of an entry the queue's lander published before the
/// settle was lost is not on the entry; say so, and name a daemon restart only
/// when the entry's own events show one (`requeued_after_restart`).
fn missing_receipt_clause(restarts: u32) -> String {
    if restarts > 0 {
        format!(
            "its gate receipt (gate_timing, fixes_tests verdict) is not on this entry: the \
             daemon restarted while it was gating (requeued_after_restart x{restarts})"
        )
    } else {
        "its gate receipt (gate_timing, fixes_tests verdict) is not on this entry".to_string()
    }
}

/// The outcome text and landed commit for a member found already on
/// `origin/rolling` (#4009, #4040). Attribution rests on evidence the queue
/// writes, never on a commit subject: the lander's committer identity naming
/// the source, else the entry's own `requeued_after_restart` events (the queue
/// had started gating it before the daemon restarted, so a fast-forward, which
/// writes no commit, is the queue's too). Only an entry with neither is called
/// an outside landing. Publication decisions never read this.
fn already_landed_outcome(
    source: &str,
    tip: String,
    lander_commit: Option<String>,
    restarts: u32,
) -> (String, String) {
    match lander_commit {
        Some(commit) => {
            let clause = missing_receipt_clause(restarts);
            (
                commit.clone(),
                format!(
                    "{source} was published to origin/rolling by the queue's lander as \
                     {commit}; {clause}"
                ),
            )
        }
        None if restarts > 0 => (
            tip,
            format!(
                "{source} was on origin/rolling when this entry was re-driven after a daemon \
                 restart (requeued_after_restart x{restarts}); the queue had started gating \
                 it and no other writer is recorded, so the queue's earlier lander run most \
                 likely published it (a fast-forward writes no commit); its gate receipt \
                 (gate_timing, fixes_tests verdict) is not on this entry"
            ),
        ),
        None => (
            tip,
            format!(
                "{source} was already on origin/rolling (landed outside the queue); \
                 settled without a gate"
            ),
        ),
    }
}

/// The migration number the lander actually gave `source`, read from the
/// published history: the provisional commit the lander writes has the queue's
/// source as one parent, and the migration file it adds relative to its other
/// parent carries the final number. `None` when no such commit exists (a plain
/// merge or fast-forward keeps the source's own number) or git cannot say.
pub(crate) fn published_migration_number(repo: &Path, source: &str) -> Option<u32> {
    let history = git(
        repo,
        &[
            "log",
            "--format=%H %P",
            "--ancestry-path",
            &format!("{source}..origin/rolling"),
        ],
    )?;
    let (commit, other_parent) = history.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let commit = fields.next()?;
        let parents: Vec<&str> = fields.collect();
        (parents.len() == 2 && parents.contains(&source)).then(|| {
            let other = parents.iter().find(|parent| **parent != source)?;
            Some((commit.to_string(), (*other).to_string()))
        })?
    })?;
    let added = git(
        repo,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "--diff-filter=A",
            &other_parent,
            &commit,
            "--",
            &format!("{MIGRATION_DIR}/"),
            &format!("{PRE_SPLIT_MIGRATION_DIR}/"),
        ],
    )?;
    added
        .lines()
        .filter_map(|path| {
            let name = path.rsplit('/').next()?;
            name.strip_prefix('v')?
                .strip_suffix(".rs")?
                .parse::<u32>()
                .ok()
        })
        .max()
}

/// Whether `source` adds a provisional-migration declaration relative to
/// rolling: the lander renumbers such a source mechanically at landing.
pub(crate) fn source_declares_provisional(repo: &Path, source: &str) -> bool {
    let base = ["origin/rolling", "rolling"].iter().find_map(|reference| {
        git(repo, &["merge-base", source, reference]).map(|out| out.trim().to_string())
    });
    let Some(base) = base.filter(|base| !base.is_empty()) else {
        return false;
    };
    git(
        repo,
        &["diff", "--name-only", "--no-renames", &base, source],
    )
    .unwrap_or_default()
    .lines()
    .any(|path| path.starts_with(PROVISIONAL_DIR) && path.ends_with(".json"))
}

/// Count newly added provisional declaration files in one source. Multiple
/// declarations are refused at enqueue because the lander's proof currently
/// models one provisional migration per accepted source.
pub(crate) fn source_provisional_declaration_count(repo: &Path, source: &str) -> Option<usize> {
    let base = ["origin/rolling", "rolling"].iter().find_map(|reference| {
        git(repo, &["merge-base", source, reference]).map(|out| out.trim().to_string())
    });
    let base = base.filter(|base| !base.is_empty())?;
    let changed = git(
        repo,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "--diff-filter=A",
            &base,
            source,
        ],
    )?;
    Some(
        changed
            .lines()
            .filter(|path| path.starts_with(PROVISIONAL_DIR) && path.ends_with(".json"))
            .count(),
    )
}

/// One batch member's migration facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MigrationMember {
    pub id: Uuid,
    /// The schema head the source carries (`None`: no migration).
    pub derived: Option<u32>,
    /// Whether the source declares a provisional migration (renumberable).
    pub declared: bool,
}

/// Migration numbers the queue assigns a batch, in queue order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MigrationPlan {
    pub assigned: Vec<(Uuid, u32)>,
    /// Members whose fixed number is already taken by a predecessor.
    pub out_of_order: Vec<Uuid>,
}

/// Assign migration numbers tip + 1, + 2, ... in queue order. A declared
/// source is renumbered to the next free number; an undeclared source keeps
/// its fixed number, which must not be one a predecessor already took (the
/// lander's tip + 1 rule then checks every prefix).
pub(crate) fn plan_migration_order(tip_version: u32, members: &[MigrationMember]) -> MigrationPlan {
    let mut plan = MigrationPlan::default();
    let mut next = tip_version + 1;
    for member in members {
        let Some(derived) = member.derived else {
            continue;
        };
        if member.declared {
            plan.assigned.push((member.id, next));
            next += 1;
        } else if derived < next {
            plan.out_of_order.push(member.id);
        } else {
            plan.assigned.push((member.id, derived));
            next = derived + 1;
        }
    }
    plan
}

/// How to launch the lander.
#[derive(Debug, Clone)]
pub struct LanderLauncher {
    binary: PathBuf,
    // Production launchers refresh the shared diagnostic snapshot before a
    // gate. Tests opt in with a private path instead of touching operator data.
    known_failure_snapshot: Option<PathBuf>,
    /// The daemon-owned queue directory (`~/.rsi/queue`): the persistent cargo
    /// target and the detached landing worktrees live under it (#1108). `None`
    /// runs the lander in the entry's own checkout with the daemon's env.
    workspace: Option<PathBuf>,
    /// How the host's Landlock ABI is read before the queue worktree is reset;
    /// replaced only by tests.
    fence_abi: write_fence::AbiProbe,
    /// Wall-time budget of ONE lander run (#1208, #3267): every run of a batch
    /// (the full gate and each bisect step) gets this much, under a batch
    /// ceiling that scales with the bisect depth ([`GateBudget`]). The runner
    /// loop refreshes it from the operator setting
    /// `rolling_queue_gate_timeout_mins` before each batch.
    gate_timeout: Duration,
    /// #2119: run each lander in a scope of `rsi-build.slice`, so the gate's
    /// cargo builds and tests share the hub build budget. Off for tests.
    build_slice: bool,
    /// #3394: compile-probe wide queued entries against the tip and the entries
    /// ahead of them before a batch claims them. Needs `workspace`. Off for tests.
    compile_probe: bool,
    /// #3435: keep each lander run in files under the queue workspace instead
    /// of in the daemon's memory, so it survives a daemon restart and the next
    /// daemon adopts it (`detached_run`). Needs a workspace.
    detached_runs: bool,
    /// The identity this daemon process claims runs under; `None` is the
    /// process's own. Only tests set another, to play the next daemon.
    run_epoch: Option<String>,
    /// Test-local additions to the launcher's inherited environment, without
    /// changing the process environment shared by concurrent tests.
    #[cfg(test)]
    inherited_env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
}

impl LanderLauncher {
    #[must_use]
    pub fn new(binary: PathBuf) -> Self {
        Self {
            binary,
            known_failure_snapshot: (!cfg!(test)).then(known_failure_snapshot::default_path),
            workspace: None,
            fence_abi: WriteFence::kernel_abi,
            gate_timeout: Duration::from_secs(
                u64::from(ROLLING_QUEUE_DEFAULT_GATE_TIMEOUT_MINS) * 60,
            ),
            build_slice: false,
            compile_probe: false,
            detached_runs: false,
            run_epoch: None,
            #[cfg(test)]
            inherited_env: Vec::new(),
        }
    }

    /// Hold a wide queued entry until `cargo check --workspace --all-targets`
    /// of the rolling tip, the entries ahead of it and the entry has a verdict,
    /// and refuse one that does not compile (#3394). It builds in the queue
    /// workspace, so it does nothing for a launcher that has none.
    #[must_use]
    pub fn with_compile_probe(mut self) -> Self {
        self.compile_probe = true;
        self
    }

    /// The directory the compile probe keeps its clone and cargo target in.
    fn compile_probe_root(&self) -> Option<PathBuf> {
        self.workspace
            .as_ref()
            .filter(|_| self.compile_probe)
            .map(|root| root.join("compile-probe"))
    }

    /// Run each lander in a transient scope of `rsi-build.slice` (#2119) when
    /// the slice can be configured; otherwise it runs as the daemon's child.
    #[must_use]
    pub fn with_build_slice(mut self) -> Self {
        self.build_slice = true;
        self.detached_runs = true;
        self
    }

    /// Keep each lander run adoptable across a daemon restart (#3435).
    #[cfg(all(test, target_os = "linux"))]
    fn with_detached_runs(mut self) -> Self {
        self.detached_runs = true;
        self
    }

    /// Where detached runs are kept, when they are on and the launcher owns a
    /// workspace.
    fn detached_runs_dir(&self) -> Option<PathBuf> {
        self.detached_runs
            .then(|| self.workspace.as_ref())
            .flatten()
            .map(|root| root.join(detached_run::RUNS_SUBDIR))
    }

    fn run_epoch(&self) -> &str {
        match self.run_epoch.as_deref() {
            Some(epoch) => epoch,
            None => detached_run::process_epoch(),
        }
    }

    /// Bound each lander run of a batch to `timeout` of wall time (#1208).
    #[must_use]
    pub fn with_gate_timeout(mut self, timeout: Duration) -> Self {
        self.gate_timeout = timeout;
        self
    }

    /// The wall-time budget one lander run is given.
    #[must_use]
    pub fn gate_timeout(&self) -> Duration {
        self.gate_timeout
    }

    /// Run landings in daemon-owned worktrees and a dedicated cargo target
    /// under `root`, never in the entry's checkout (#1108).
    #[must_use]
    pub fn with_workspace(mut self, root: PathBuf) -> Self {
        self.workspace = Some(root);
        self
    }

    /// The persistent cargo target the lander is given, when workspace-owned.
    #[must_use]
    pub fn target_dir(&self) -> Option<PathBuf> {
        self.workspace.as_ref().map(|root| root.join("target"))
    }

    /// Where each lander run's stdout and stderr are kept, when workspace-owned.
    #[must_use]
    pub fn log_dir(&self) -> Option<PathBuf> {
        self.workspace.as_ref().map(|root| root.join("logs"))
    }

    /// The lander executable this launcher runs.
    #[must_use]
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    /// `RSI_ROLLING_LAND_BIN`, the daemon's sibling binary, the shared debug
    /// build, then `PATH`; always with the daemon-owned queue workspace.
    #[must_use]
    pub fn discover() -> Self {
        Self::discover_binary().with_workspace(rsi_common::identity::data_dir().join("queue"))
    }

    fn discover_binary() -> Self {
        if let Some(path) = std::env::var_os("RSI_ROLLING_LAND_BIN") {
            return Self::new(PathBuf::from(path));
        }
        let sibling = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("rsi-rolling-land")))
            .filter(|path| path.is_file());
        if let Some(path) = sibling {
            return Self::new(path);
        }
        let shared = std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".cargo/shared-target/debug/rsi-rolling-land"))
            .filter(|path| path.is_file());
        Self::new(shared.unwrap_or_else(|| PathBuf::from("rsi-rolling-land")))
    }
}

/// FNV-1a: a stable, dependency-free key for a repository's git directory.
fn stable_key(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn git_ok(repo: &Path, args: &[&str]) -> std::result::Result<String, String> {
    let output = git_command()
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// The queue's one fetch of the source repository's `origin/rolling`.
///
/// The source (an operator checkout or a session sandbox) is only read by the
/// queue, but a fetch necessarily writes its object store and its
/// `refs/remotes/origin/rolling`. It is NOT run under the write fence (#1170):
/// the transport children (`ssh`, credential helpers, `git-remote-https`) write
/// their own state (`known_hosts`, control sockets, credential caches) and
/// inherit a fence, and a ref update needs the `packed-refs` lock in the common
/// directory root, which a directory grant cannot give without also granting
/// `config` and `hooks`. A read-only copy (fetching into a private directory
/// with the source's objects as an alternate) would leave the fetched commits
/// out of the shared store the lander and the queue worktree read, so it is not
/// done either. What is done instead is to remove every way the SOURCE's own
/// configuration can make this fetch run a program or write elsewhere:
/// hooks (`reference-transaction`, `pre-auto-gc`), `fsmonitor`, automatic
/// `gc`/maintenance (which can repack the operator's store in the background)
/// and the commit-graph, submodule recursion, tag following, `FETCH_HEAD` (no
/// reader needs it: the tip is read from `origin/rolling`) and the `ext::`
/// transport are all off on the command line, which wins over every config
/// file. The daemon's environment is already reduced to the allowlist. The
/// source's transport settings (`core.sshCommand`, `core.gitProxy`,
/// `remote.origin.uploadpack`, credential helpers, `url.*.insteadOf`) are left
/// alone on purpose: they carry the operator's authentication (see
/// `write_fence.rs` for the full trade-off).
const FETCH_ROLLING: &[&str] = &[
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "gc.auto=0",
    "-c",
    "maintenance.auto=false",
    "-c",
    "fetch.writeCommitGraph=false",
    "-c",
    "fetch.recurseSubmodules=false",
    "-c",
    "protocol.ext.allow=never",
    "fetch",
    "--quiet",
    "--no-write-fetch-head",
    "--no-recurse-submodules",
    "--no-auto-maintenance",
    "--no-auto-gc",
    "--no-tags",
    "origin",
    "rolling",
];

fn fetch_rolling(repo: &Path) -> std::result::Result<String, String> {
    git_ok(repo, FETCH_ROLLING)
}

/// The daemon-owned landing checkout for the repository `source` belongs to: a
/// queue-owned private clone, detached at the fetched `origin/rolling`, created
/// once and refreshed on every call so builds stay warm (#1170).
///
/// The clone is a repository of its own under the queue workspace. It borrows
/// the source's object store through a read-only alternate (`objects/info/alternates`)
/// so any accepted source commit is reachable, and fetches `origin/rolling` itself
/// from the source's `origin` URL. Nothing about it is registered in the source:
/// no `worktrees/<name>` entry, no ref, no object, no config and no file of
/// `source` is ever written, and the queue's fetch runs against the clone, not
/// the source. `source` is only read (`rev-parse`, `remote get-url`,
/// `config --get-all`). Callers serialize use (the queue runs one batch at a
/// time).
#[cfg(test)]
pub(crate) fn ensure_queue_worktree(
    root: &Path,
    source: &Path,
) -> std::result::Result<PathBuf, String> {
    ensure_queue_worktree_with(root, source, WriteFence::kernel_abi)
}

/// [`ensure_queue_worktree`] with the Landlock ABI probe injected (tests).
fn ensure_queue_worktree_with(
    root: &Path,
    source: &Path,
    fence_abi: write_fence::AbiProbe,
) -> std::result::Result<PathBuf, String> {
    let common = git_ok(
        source,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    // Fail closed before any git command runs in (or deletes) a queue path:
    // the workspace, its worktrees directory and the worktree are real,
    // owned directories, never symlinks or aliases (#1113).
    prepare_queue_workspace(root)?;
    // The fence guards every reset; a host that cannot enforce it is refused
    // before any clone is created or touched (#1160).
    write_fence::require_abi(fence_abi())?;
    let path = root.join("worktrees").join(stable_key(&common));
    let mut pinned = if path.symlink_metadata().is_ok() {
        match pin_queue_worktree(root, &path) {
            Ok(pinned) => pinned,
            // An empty directory (a crashed creation) or a slot an earlier
            // daemon registered as a linked worktree of the source is retired;
            // anything else is refused with nothing touched.
            Err(refusal) => {
                retire_unusable_slot(root, &path, &common).map_err(|_| refusal)?;
                create_queue_clone(root, &path)?
            }
        }
    } else {
        create_queue_clone(root, &path)?
    };
    pinned.fence_abi = fence_abi;
    // The clone's config and alternate are rewritten from the source on every
    // call (the origin URL can change; a replaced file heals).
    sync_queue_clone(&pinned, source, &common)?;
    let rev = queue_rolling_rev(&pinned, source)?;
    // Populate exactly like every later reset, through the pinned, fenced runner
    // (#1160).
    reset_pinned_worktree(&pinned, &rev)?;
    Ok(path)
}

/// First line of a queue clone's `config`: what a validated slot must carry.
const QUEUE_CLONE_MARKER: &str = "# rsi-queue-clone v1\n";

/// Transport settings of the source that a fetch needs and the clone cannot
/// derive: they carry the operator's authentication and network path. Only the
/// repository-local values are copied (the clone reads the global config like
/// the source did). Each is `(section, subsection, name)`.
const COPIED_TRANSPORT_KEYS: &[(&str, Option<&str>, &str)] = &[
    ("core", None, "sshCommand"),
    ("core", None, "gitProxy"),
    ("credential", None, "helper"),
    ("http", None, "proxy"),
    ("http", None, "sslCAInfo"),
    ("remote", Some("origin"), "uploadpack"),
    ("remote", Some("origin"), "proxy"),
];

/// `value` as a git config string, or `None` when it cannot be written on one
/// line.
fn config_quote(value: &str) -> Option<String> {
    match value.contains(['\n', '\r', '\0']) {
        true => None,
        false => Some(format!(
            "\"{}\"",
            value.replace('\\', "\\\\").replace('"', "\\\"")
        )),
    }
}

/// A remote URL as the clone must fetch it: a relative local path is made
/// absolute against the source, since the clone lives elsewhere.
fn absolute_remote_url(source: &Path, url: &str) -> String {
    let local = !url.contains("://") && !url.contains(':') && Path::new(url).is_relative();
    match local {
        true if source.join(url).exists() => source.join(url).display().to_string(),
        _ => url.to_string(),
    }
}

/// The text of a queue clone's `config`, derived from reads of `source` only.
fn queue_clone_config(source: &Path) -> String {
    let mut config = String::from(QUEUE_CLONE_MARKER);
    config.push_str(
        "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\
         \tlogAllRefUpdates = false\n\thooksPath = /dev/null\n\
         [gc]\n\tauto = 0\n[maintenance]\n\tauto = false\n",
    );
    let url = git_ok(source, &["remote", "get-url", "origin"])
        .ok()
        .map(|url| absolute_remote_url(source, &url))
        .and_then(|url| config_quote(&url));
    if let Some(url) = url {
        config.push_str(&format!(
            "[remote \"origin\"]\n\turl = {url}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n"
        ));
        let push = git_ok(source, &["remote", "get-url", "--push", "origin"])
            .ok()
            .map(|push| absolute_remote_url(source, &push))
            .and_then(|push| config_quote(&push));
        if let Some(push) = push {
            config.push_str(&format!("[remote \"origin\"]\n\tpushurl = {push}\n"));
        }
    }
    for (section, subsection, name) in COPIED_TRANSPORT_KEYS {
        let key = match subsection {
            Some(subsection) => format!("{section}.{subsection}.{name}"),
            None => format!("{section}.{name}"),
        };
        let Ok(values) = git_ok(source, &["config", "--local", "--get-all", &key]) else {
            continue;
        };
        let header = match subsection {
            Some(subsection) => format!("[{section} \"{subsection}\"]"),
            None => format!("[{section}]"),
        };
        for value in values.lines().filter_map(config_quote) {
            config.push_str(&format!("{header}\n\t{name} = {value}\n"));
        }
    }
    config
}

/// Write the clone's `config` and its read-only alternate (the source's object
/// store), both by exclusive create plus rename relative to the pinned git
/// directory, so a file a racing process put there is replaced, never written
/// through.
fn sync_queue_clone(
    pinned: &PinnedWorktree,
    source: &Path,
    common: &str,
) -> std::result::Result<(), String> {
    if common.contains('\n') {
        return Err("the source's object store path has a newline".to_string());
    }
    pinned.ensure_admin_unchanged()?;
    let objects = private_git_dir::open_dir_at(&pinned.admin_dir, "objects")?;
    let info = private_git_dir::open_dir_at(&objects, "info")?;
    private_git_dir::replace_file_at(
        &info,
        "alternates",
        format!("{common}/objects\n").as_bytes(),
        0o644,
    )?;
    private_git_dir::replace_file_at(
        &pinned.admin_dir,
        "config",
        queue_clone_config(source).as_bytes(),
        0o644,
    )?;
    pinned.ensure_admin_unchanged()
}

/// Run git with the pinned clone as its current directory (the handle is
/// resolved between `fork` and `exec`, so the path is never looked up by name).
/// Unfenced, like every read of the queue's own clone.
fn git_in_clone(pinned: &PinnedWorktree, args: &[&str]) -> std::result::Result<String, String> {
    pinned.ensure_admin_unchanged()?;
    let output = git_command()
        .current_dir(pinned.handle_path())
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("cannot run git: {error}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    Err(format!(
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// The commit the queue clone is reset to: the freshly fetched `origin/rolling`
/// of the CLONE, or, offline or with no remote, the source's `HEAD` (the lander
/// reports the real error). The fetch writes only the clone (#1170); the source's
/// own `origin/rolling` is no longer touched by the queue.
fn queue_rolling_rev(
    pinned: &PinnedWorktree,
    source: &Path,
) -> std::result::Result<String, String> {
    if git_in_clone(pinned, FETCH_ROLLING).is_ok()
        && let Ok(rev) = git_in_clone(
            pinned,
            &["rev-parse", "--verify", "origin/rolling^{commit}"],
        )
    {
        return Ok(rev);
    }
    git_ok(source, &["rev-parse", "--verify", "HEAD^{commit}"])
}

/// Create the queue clone at `path`: a repository skeleton (git directory with
/// `HEAD`, `config`, `objects`, `refs`; no `hooks`, no `info`, no templates)
/// made relative to the vetted `worktrees` handle with exclusive creates and no
/// symlink follow. No git process runs and nothing outside `path` is written.
/// Returned pinned; [`sync_queue_clone`] gives it its real config and alternate.
fn create_queue_clone(root: &Path, path: &Path) -> std::result::Result<PinnedWorktree, String> {
    let (parent, name) = vetted_slot_parent(root, path)?;
    private_git_dir::make_dir_at(&parent, &name)?;
    let built = (|| {
        let slot = private_git_dir::open_dir_at(&parent, &name)?;
        private_git_dir::make_dir_at(&slot, ".git")?;
        let admin = private_git_dir::open_dir_at(&slot, ".git")?;
        private_git_dir::create_file_at(
            &admin,
            "HEAD",
            b"ref: refs/heads/rsi-queue-unborn\n",
            0o644,
        )?;
        private_git_dir::create_file_at(&admin, "config", QUEUE_CLONE_MARKER.as_bytes(), 0o644)?;
        for dir in ["objects", "refs"] {
            private_git_dir::make_dir_at(&admin, dir)?;
        }
        let objects = private_git_dir::open_dir_at(&admin, "objects")?;
        for dir in ["info", "pack"] {
            private_git_dir::make_dir_at(&objects, dir)?;
        }
        let refs = private_git_dir::open_dir_at(&admin, "refs")?;
        for dir in ["heads", "tags"] {
            private_git_dir::make_dir_at(&refs, dir)?;
        }
        pin_queue_worktree(root, path)
    })();
    if built.is_err() {
        // The directory was created just now by this call and is ours alone.
        let _ = private_git_dir::remove_tree_at(&parent, &name);
    }
    built
}

/// The vetted `worktrees` directory handle and the slot's name in it. The
/// handle is opened without following a symlink and must resolve to the
/// queue's own `worktrees` directory, owned by this user.
fn vetted_slot_parent(
    root: &Path,
    path: &Path,
) -> std::result::Result<(std::fs::File, String), String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    let refuse = |why: &str| Err(format!("cannot use queue slot {}: {why}", path.display()));
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return refuse("no file name");
    };
    let parent_path = root.join("worktrees");
    if path.parent() != Some(parent_path.as_path()) {
        return refuse("not directly inside the worktrees directory");
    }
    let parent = open_queue_dir(&parent_path)?;
    let parent_meta = parent
        .metadata()
        .map_err(|error| format!("cannot stat {}: {error}", parent_path.display()))?;
    // SAFETY: `geteuid` has no preconditions and does not mutate state.
    if parent_meta.uid() != unsafe { nix::libc::geteuid() } {
        return refuse("the worktrees directory is not owned by the daemon user");
    }
    let real_parent = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()))
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", parent_path.display()))?;
    let real_root = root
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", root.display()))?;
    if real_parent != real_root.join("worktrees") {
        return refuse("the worktrees directory is not the queue's");
    }
    Ok((parent, name.to_string()))
}

/// Retire a slot that cannot be used as a queue clone: an empty directory, or
/// the linked worktree an earlier daemon registered in the source repository.
/// Removing the latter touches only the slot (handle-relative, no symlink
/// follow); its registration in the source is left for git's own
/// `worktree prune`, because the queue no longer writes the source at all.
fn retire_unusable_slot(root: &Path, path: &Path, common: &str) -> std::result::Result<(), String> {
    if legacy_registered_worktree(root, path, common) {
        let (parent, name) = vetted_slot_parent(root, path)?;
        return private_git_dir::remove_tree_at(&parent, &name);
    }
    remove_empty_queue_dir(root, path)
}

/// `path` is a worktree an earlier queue registered in the source repository: a
/// real owned directory in the workspace whose `.git` FILE points at a
/// `<common>/worktrees/<name>` registration that points back at `path`.
fn legacy_registered_worktree(root: &Path, path: &Path, common: &str) -> bool {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    let Ok(dir) = open_queue_dir(path) else {
        return false;
    };
    // SAFETY: `geteuid` has no preconditions and does not mutate state.
    let uid = unsafe { nix::libc::geteuid() };
    if dir.metadata().map(|meta| meta.uid()).ok() != Some(uid) {
        return false;
    }
    let (Ok(real), Ok(real_root)) = (
        PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd())).canonicalize(),
        root.canonicalize(),
    ) else {
        return false;
    };
    if !real.starts_with(real_root.join("worktrees")) {
        return false;
    }
    let Ok(Some(pointer)) = private_git_dir::read_file_at(&dir, ".git") else {
        return false;
    };
    let pointer = String::from_utf8_lossy(&pointer);
    let Some(admin) = pointer.trim().strip_prefix("gitdir:").map(str::trim) else {
        return false;
    };
    let Ok(admin_dir) = open_queue_dir(Path::new(admin)) else {
        return false;
    };
    let admin_handle = PathBuf::from(format!("/proc/self/fd/{}", admin_dir.as_raw_fd()));
    let (Ok(admin_real), Ok(common)) = (
        admin_handle.canonicalize(),
        Path::new(common).canonicalize(),
    ) else {
        return false;
    };
    if admin_dir.metadata().map(|meta| meta.uid()).ok() != Some(uid)
        || admin_real.parent() != Some(common.join("worktrees").as_path())
    {
        return false;
    }
    let back = std::fs::read_to_string(admin_handle.join("gitdir")).unwrap_or_default();
    Path::new(back.trim()).canonicalize().ok().as_deref() == Some(real.join(".git").as_path())
}

/// Retire an empty queue slot without trusting any path: open the vetted
/// `worktrees` directory without following symlinks, check the slot through
/// that handle (`fstatat`, no-follow) and remove it with `unlinkat` relative to
/// the same handle. `AT_REMOVEDIR` never removes a symlink or a non-empty
/// directory, so a swap of the slot or of `worktrees` after the check cannot
/// redirect the removal (#1141).
fn remove_empty_queue_dir(root: &Path, path: &Path) -> std::result::Result<(), String> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    let refuse = |why: &str| {
        Err(format!(
            "cannot retire queue slot {}: {why}",
            path.display()
        ))
    };
    let (parent, name) = match vetted_slot_parent(root, path) {
        Ok(vetted) => vetted,
        Err(why) => return refuse(&why),
    };
    // SAFETY: `geteuid` has no preconditions and does not mutate state.
    let uid = unsafe { nix::libc::geteuid() };
    let name = CString::new(name).map_err(|_| "slot name has a NUL".to_string())?;
    // SAFETY: `stat` is plain old data, `name` is NUL-terminated and the parent
    // handle stays open for both calls.
    let removed = unsafe {
        let mut stat: nix::libc::stat = std::mem::zeroed();
        if nix::libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            &mut stat,
            nix::libc::AT_SYMLINK_NOFOLLOW,
        ) != 0
        {
            return refuse("the slot is unreadable");
        }
        if stat.st_mode & nix::libc::S_IFMT != nix::libc::S_IFDIR || stat.st_uid != uid {
            return refuse("the slot is not an owned directory");
        }
        nix::libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), nix::libc::AT_REMOVEDIR)
    };
    if removed != 0 {
        return refuse(&std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

/// `path` is a real directory (no-follow metadata) owned by this process.
fn vet_queue_dir(path: &Path) -> std::result::Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let meta = path
        .symlink_metadata()
        .map_err(|error| format!("queue path {} unreadable: {error}", path.display()))?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(format!(
            "queue path {} is a symlink or not a directory; refusing to use it",
            path.display()
        ));
    }
    // SAFETY: `geteuid` has no preconditions and does not mutate state.
    let uid = unsafe { nix::libc::geteuid() };
    if meta.uid() != uid {
        return Err(format!(
            "queue path {} is not owned by the daemon user; refusing to use it",
            path.display()
        ));
    }
    Ok(())
}

/// Create (when absent) and vet the queue workspace: the root, its `worktrees`
/// directory and its `target` directory. Parents of the root belong to the
/// daemon's data directory and are not inspected.
pub(crate) fn prepare_queue_workspace(root: &Path) -> std::result::Result<(), String> {
    if root.symlink_metadata().is_err() {
        std::fs::create_dir_all(root)
            .map_err(|error| format!("cannot create the queue workspace: {error}"))?;
    }
    vet_queue_dir(root)?;
    for name in ["worktrees", "target"] {
        let child = root.join(name);
        if child.symlink_metadata().is_err() {
            std::fs::create_dir(&child)
                .map_err(|error| format!("cannot create the queue workspace: {error}"))?;
        }
        vet_queue_dir(&child)?;
    }
    Ok(())
}

mod private_git_dir;
mod write_fence;
use private_git_dir::PrivateGitDir;
use write_fence::WriteFence;

/// A queue clone validated once and then held by open directory handles.
/// Destructive git commands run from the handles, never from mutable paths, so a
/// slot or a git directory swapped for a symlink after validation cannot
/// redirect them (#1126, #1141).
pub(crate) struct PinnedWorktree {
    dir: std::fs::File,
    /// The clone's own git directory (`<slot>/.git`) opened without following a
    /// symlink. Git is never pointed at it: each reset runs in a fresh private
    /// directory beneath it, and only `index` and `HEAD` are published back by
    /// exclusive create plus rename relative to this handle.
    admin_dir: std::fs::File,
    /// Where the git directory was validated; the path is only used to detect a
    /// swap, never to run git.
    admin: PathBuf,
    admin_id: (u64, u64),
    /// The clone's object store (`<slot>/.git/objects`), read by the private git
    /// directory's git through `GIT_OBJECT_DIRECTORY`. Its alternate names the
    /// source's store, which is only ever read.
    objects: PathBuf,
    /// How the kernel's Landlock ABI is read; replaced only by tests.
    fence_abi: write_fence::AbiProbe,
    /// The environment the fenced git is filtered from instead of the daemon's
    /// own (`None` in production); tests use it to prove an inherited
    /// `GIT_TRACE` is never honored.
    inherited_env: Option<Vec<(std::ffi::OsString, std::ffi::OsString)>>,
}

impl PinnedWorktree {
    /// `/proc/self/fd/N` names the pinned inode, not whatever `path` now is.
    fn handle_path(&self) -> PathBuf {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!("/proc/self/fd/{}", self.dir.as_raw_fd()))
    }

    fn admin_handle_path(&self) -> PathBuf {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!("/proc/self/fd/{}", self.admin_dir.as_raw_fd()))
    }

    /// The git directory path still names the pinned inode: same device and
    /// inode, not a symlink, not moved. Fail closed otherwise.
    fn ensure_admin_unchanged(&self) -> std::result::Result<(), String> {
        use std::os::unix::fs::MetadataExt;
        let swapped = |why: &str| {
            Err(format!(
                "queue clone git directory {} changed under the queue ({why}); refusing to run git",
                self.admin.display()
            ))
        };
        let Ok(meta) = self.admin.symlink_metadata() else {
            return swapped("missing");
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return swapped("symlink or not a directory");
        }
        if (meta.dev(), meta.ino()) != self.admin_id {
            return swapped("identity changed");
        }
        match self.admin_handle_path().canonicalize() {
            Ok(real) if real == self.admin => Ok(()),
            _ => swapped("moved"),
        }
    }
}

/// Open `path` as a directory without following a final symlink.
fn open_queue_dir(path: &Path) -> std::result::Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open(path)
        .map_err(|error| {
            format!(
                "queue path {} is a symlink or not a directory ({error}); refusing to use it",
                path.display()
            )
        })
}

/// `path` is a queue clone this queue created: a real owned directory inside
/// the workspace holding its own `.git` DIRECTORY (opened without following a
/// symlink, owned by this user, carrying the queue's marker config) at its real
/// location. Every check runs against no-follow directory handles, which are
/// returned so the caller keeps using exactly the inodes that were validated.
fn pin_queue_worktree(root: &Path, path: &Path) -> std::result::Result<PinnedWorktree, String> {
    use std::os::unix::fs::MetadataExt;
    let dir = open_queue_dir(path)?;
    let meta = dir
        .metadata()
        .map_err(|error| format!("queue path {} unreadable: {error}", path.display()))?;
    // SAFETY: `geteuid` has no preconditions and does not mutate state.
    let uid = unsafe { nix::libc::geteuid() };
    if meta.uid() != uid {
        return Err(format!(
            "queue path {} is not owned by the daemon user; refusing to use it",
            path.display()
        ));
    }
    let handle = {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()))
    };
    let refuse = |why: &str| {
        Err(format!(
            "queue path {} is not a queue clone ({why}); refusing to reset it",
            path.display()
        ))
    };
    // The handle resolves to the pinned inode's real location.
    let real = handle
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", path.display()))?;
    let real_root = root
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", root.display()))?;
    if !real.starts_with(real_root.join("worktrees")) {
        return refuse("outside the queue workspace");
    }
    // The git directory is pinned itself: every check below reads through the
    // handle, so a swap of the `.git` path after this point cannot change what
    // is validated or what a reset later publishes into.
    let Ok(admin_dir) = private_git_dir::open_dir_at(&dir, ".git") else {
        return refuse("no .git directory, or it is a symlink");
    };
    let admin_meta = admin_dir.metadata().map_err(|error| {
        format!(
            "cannot stat the git directory of {}: {error}",
            path.display()
        )
    })?;
    if admin_meta.uid() != uid {
        return refuse("the git directory is not owned by the daemon user");
    }
    let marked = matches!(
        private_git_dir::read_file_at(&admin_dir, "config"),
        Ok(Some(config)) if config.starts_with(QUEUE_CLONE_MARKER.as_bytes())
    );
    if !marked {
        return refuse("the git directory does not carry the queue's marker");
    }
    let admin_real = {
        use std::os::fd::AsRawFd;
        PathBuf::from(format!("/proc/self/fd/{}", admin_dir.as_raw_fd())).canonicalize()
    };
    let Ok(admin_real) = admin_real else {
        return refuse("the git directory is missing");
    };
    if admin_real != real.join(".git") {
        return refuse("the git directory is not the slot's own");
    }
    let pinned = PinnedWorktree {
        dir,
        admin_dir,
        objects: admin_real.join("objects"),
        admin: admin_real,
        admin_id: (admin_meta.dev(), admin_meta.ino()),
        fence_abi: WriteFence::kernel_abi,
        inherited_env: None,
    };
    pinned.ensure_admin_unchanged()?;
    Ok(pinned)
}

/// Run git inside the pinned worktree against the private git directory: the
/// child's current directory comes from the inherited worktree handle and
/// `GIT_DIR` names the inherited private-directory handle, so neither the
/// worktree path nor the clone's git directory path is resolved again. The child gets no
/// operator config, hooks or filters (the private directory has its own config
/// and no hooks; global and system config are off) and runs under a kernel
/// write fence that permits writes only beneath those two handles (#1160). The
/// git directory identity is also checked before and after, failing closed on a
/// swap. `env` is extra environment for the child (tests park git between its
/// work-tree normalization and its `chdir`).
fn git_private(
    pinned: &PinnedWorktree,
    private: &PrivateGitDir,
    args: &[&str],
    env: &[(String, String)],
) -> std::result::Result<String, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    pinned.ensure_admin_unchanged()?;
    let fence = WriteFence::new(&[&pinned.dir, &private.dir], pinned.fence_abi)?;
    let private_fd = private.dir.as_raw_fd();
    let mut command = std::process::Command::new("git");
    // A clean environment: only what git needs to start, the inherited names
    // the test seam names (production inherits none), and what is set below.
    command.env_clear();
    let raw = pinned
        .inherited_env
        .clone()
        .unwrap_or_else(|| std::env::vars_os().collect());
    command.envs(inherited_git_env(raw).into_iter().filter(|(name, _)| {
        let name = name.to_string_lossy();
        name == "PATH" || name == "LANG" || name == "TZ" || name.starts_with("LC_")
    }));
    // The settings that stop an external program or an append-in-place from
    // being configured are on the command line, which wins over the private
    // directory's own config file.
    command
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.attributesFile=/dev/null",
            "-c",
            "core.logAllRefUpdates=false",
            "-c",
            "core.sharedRepository=false",
        ])
        .args(args)
        .current_dir(pinned.handle_path());
    command
        .env("HOME", "/dev/null")
        .env("GIT_DIR", private.handle_path())
        // Naming the common directory stops git from honoring a `commondir`
        // file planted in the private directory, which would redirect config,
        // refs and `info/` to wherever it points; replace refs are ignored for
        // the same reason (#1170).
        .env("GIT_COMMON_DIR", private.handle_path())
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_WORK_TREE", ".")
        .env("GIT_OBJECT_DIRECTORY", &pinned.objects)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .envs(
            env.iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        )
        .stdin(Stdio::null());
    // SAFETY: the closure runs between fork and exec and only calls `fcntl`,
    // which is async-signal-safe. It lets the child (and git's own children)
    // inherit the private-directory handle that `GIT_DIR` names; the parent's
    // copy stays close-on-exec so no unrelated process inherits it.
    unsafe {
        command.pre_exec(move || {
            let flags = nix::libc::fcntl(private_fd, nix::libc::F_GETFD);
            if flags < 0
                || nix::libc::fcntl(
                    private_fd,
                    nix::libc::F_SETFD,
                    flags & !nix::libc::FD_CLOEXEC,
                ) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    fence.confine_on_exec(&mut command);
    let output = command.output().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => format!("cannot run git: {error}"),
        // `pre_exec` (no-new-privs, `landlock_restrict_self`) or exec failed:
        // git did not run, and the fence is what could not be applied.
        _ => format!(
            "{}: cannot start git under the write fence: {error}",
            write_fence::UNSUPPORTED_CODE
        ),
    })?;
    pinned.ensure_admin_unchanged()?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }
    Err(format!(
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

/// Reset the pinned queue worktree to `rev` and drop untracked files.
///
/// git never touches the clone's own git directory: it works in a fresh private
/// git directory seeded with a copy of the clone's index, and the resulting
/// `HEAD` and index are published back by exclusive create plus rename relative
/// to the pinned git directory handle. So nothing there, nor any file a link
/// there aliases, is ever written in place.
fn reset_pinned_worktree(pinned: &PinnedWorktree, rev: &str) -> std::result::Result<(), String> {
    reset_pinned_worktree_with_env(pinned, rev, &|_| Vec::new(), &|_, _| {})
}

/// [`reset_pinned_worktree`] with test seams: extra child environment per git
/// subcommand, and a callback run just before each subcommand with the private
/// directory's path (tests plant files in it, as a racing same-uid process
/// would).
fn reset_pinned_worktree_with_env(
    pinned: &PinnedWorktree,
    rev: &str,
    env_for: &dyn Fn(&str) -> Vec<(String, String)>,
    before_git: &dyn Fn(&str, &Path),
) -> std::result::Result<(), String> {
    pinned.ensure_admin_unchanged()?;
    private_git_dir::retire_stale_scratch(&pinned.admin_dir);
    let private = PrivateGitDir::create(&pinned.admin_dir)?;
    let outcome = reset_in_private_dir(pinned, &private, rev, env_for, before_git);
    let removed = private.remove(&pinned.admin_dir);
    outcome?;
    removed
}

fn reset_in_private_dir(
    pinned: &PinnedWorktree,
    private: &PrivateGitDir,
    rev: &str,
    env_for: &dyn Fn(&str) -> Vec<(String, String)>,
    before_git: &dyn Fn(&str, &Path),
) -> std::result::Result<(), String> {
    let run = |args: &[&str]| {
        before_git(args[0], &private.handle_path());
        git_private(pinned, private, args, &env_for(args[0]))
    };
    let commit = run(&["rev-parse", "--verify", &format!("{rev}^{{commit}}")])?;
    // `read-tree` updates the index and the worktree and touches no ref, so no
    // reflog is ever opened (`reset --hard` would append to `logs/HEAD`).
    run(&["read-tree", "--reset", "-u", &commit])?;
    run(&["clean", "--quiet", "-fd"])?;
    pinned.ensure_admin_unchanged()?;
    private_git_dir::replace_file_at(&pinned.admin_dir, "index", &private.index()?, 0o644)?;
    private_git_dir::replace_file_at(
        &pinned.admin_dir,
        "HEAD",
        format!("{commit}\n").as_bytes(),
        0o644,
    )
}

/// The repository checkout a claimed batch runs in: the daemon-owned worktree
/// when the launcher owns a workspace, else the entry's own checkout.
async fn queue_repo(
    launcher: &LanderLauncher,
    repo_path: &str,
) -> std::result::Result<PathBuf, String> {
    let Some(root) = launcher.workspace.clone() else {
        return Ok(PathBuf::from(repo_path));
    };
    let source = PathBuf::from(repo_path);
    let fence_abi = launcher.fence_abi;
    tokio::task::spawn_blocking(move || {
        prepare_queue_workspace(&root)?;
        ensure_queue_worktree_with(&root, &source, fence_abi)
    })
    .await
    .map_err(|error| format!("queue workspace task: {error}"))?
}

/// The repository a checkout belongs to: its git common directory, which every
/// worktree of one repository shares (and so shares one object database). `None`
/// when git cannot say (the checkout is gone or is not a repository).
fn repository_identity(checkout: &Path) -> Option<String> {
    let common = git_ok(
        checkout,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()?;
    let common = PathBuf::from(common);
    Some(
        std::fs::canonicalize(&common)
            .unwrap_or(common)
            .display()
            .to_string(),
    )
}

/// The key queued entries batch under. A checkout git cannot resolve keeps its
/// own path, so it only ever batches with entries of that exact path (what the
/// queue did before #1945) and never with a resolved repository.
fn repository_key(checkout: &str) -> String {
    match repository_identity(Path::new(checkout)) {
        Some(common) => format!("git:{common}"),
        None => format!("path:{checkout}"),
    }
}

/// Claim the next batch: the FIFO head plus up to `size - 1` later entries of
/// its project and repository. Every Issue worker lands from its own sandbox
/// worktree, so entries batch by git common directory, not by checkout path
/// (#1945). Git runs here, outside the store's write transaction; the claim only
/// looks the answers up, and an entry enqueued after the probe is not in the
/// map, so it keeps its own path and waits for a later claim.
#[cfg(test)]
async fn claim_batch(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    size: usize,
) -> Result<Option<ClaimedQueueBatch>> {
    claim_batch_held(store, size, None).await
}

/// [`claim_batch`] that does not claim the queued entry whose source is `held`
/// (#3394), nor anything behind it.
async fn claim_batch_held(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    size: usize,
    held: Option<String>,
) -> Result<Option<ClaimedQueueBatch>> {
    let is_held = move |source: &str| held.as_deref() == Some(source);
    let checkouts = match size {
        0 | 1 => Vec::new(),
        _ => with_store(store, |s| s.queued_rolling_queue_repo_paths()).await?,
    };
    if checkouts.len() < 2 {
        // One checkout (or a batch of one): the path is the repository.
        return with_store(store, move |s| {
            s.claim_rolling_queue_batch_lanes_held(
                Utc::now(),
                size,
                &|path| path.to_string(),
                &|_| None,
                &is_held,
            )
        })
        .await;
    }
    let keys: HashMap<String, String> = tokio::task::spawn_blocking(move || {
        checkouts
            .into_iter()
            .map(|checkout| {
                let key = repository_key(&checkout);
                (checkout, key)
            })
            .collect()
    })
    .await
    .map_err(|error| DaemonError::Process(format!("queue repository probe: {error}")))?;
    with_store(store, move |s| {
        s.claim_rolling_queue_batch_lanes_held(
            Utc::now(),
            size,
            &|checkout| {
                keys.get(checkout)
                    .cloned()
                    .unwrap_or_else(|| format!("path:{checkout}"))
            },
            &|_| None,
            &is_held,
        )
    })
    .await
}

/// Queued entries whose package sets one lane claim resolves (#1224). An entry
/// further back is read as whole-workspace, which keeps it, and everything
/// behind it, in order and bounds the git work of an attempt.
const LANE_PROBE_WINDOW: usize = 32;

/// [`repository_key`], remembered per checkout: a lane claim asks on every tick
/// while a batch gates, and a checkout's repository does not change.
fn repository_key_cached(checkout: &str) -> String {
    static KEYS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, String>>> =
        std::sync::OnceLock::new();
    let keys = KEYS.get_or_init(Default::default);
    if let Some(known) = keys
        .lock()
        .ok()
        .and_then(|keys| keys.get(checkout).cloned())
    {
        return known;
    }
    let key = repository_key(checkout);
    if let Ok(mut keys) = keys.lock() {
        if keys.len() >= 1024 {
            keys.clear();
        }
        keys.insert(checkout.to_string(), key.clone());
    }
    key
}

/// [`claim_batch`] with lanes (#1224). With nothing gating this is the plain
/// claim. While a batch gates, the queued sources and the gating ones are
/// resolved to package sets (git, outside the store's write transaction) and the
/// store claims a second batch only for a source disjoint from every gating
/// batch: an rsi-only entry no longer waits behind an rsid shard batch.
#[cfg(test)]
async fn claim_batch_in_lanes(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    size: usize,
) -> Result<Option<ClaimedQueueBatch>> {
    claim_batch_in_lanes_held(store, size, None).await
}

/// [`claim_batch_in_lanes`] that does not claim the queued entry whose source is
/// `held`, nor anything behind it (#3394).
async fn claim_batch_in_lanes_held(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    size: usize,
    held: Option<String>,
) -> Result<Option<ClaimedQueueBatch>> {
    let (gating, queued) = with_store(store, |s| {
        s.rolling_queue_lane_candidates(LANE_PROBE_WINDOW)
    })
    .await?;
    if gating.is_empty() || queued.is_empty() {
        return claim_batch_held(store, size, held).await;
    }
    let is_held = move |source: &str| held.as_deref() == Some(source);
    let (keys, packages): (HashMap<String, String>, HashMap<String, LanePackages>) =
        tokio::task::spawn_blocking(move || {
            let mut keys = HashMap::new();
            let mut packages = HashMap::new();
            for (source, checkout) in gating.iter().chain(queued.iter()) {
                keys.entry(checkout.clone())
                    .or_insert_with(|| repository_key_cached(checkout));
                packages
                    .entry(source.clone())
                    .or_insert_with(|| lanes::source_lane_packages(Path::new(checkout), source));
            }
            (keys, packages)
        })
        .await
        .map_err(|error| DaemonError::Process(format!("queue lane probe: {error}")))?;
    with_store(store, move |s| {
        s.claim_rolling_queue_batch_lanes_held(
            Utc::now(),
            size,
            &|checkout| {
                keys.get(checkout)
                    .cloned()
                    .unwrap_or_else(|| format!("path:{checkout}"))
            },
            &|source| packages.get(source).cloned().flatten(),
            &is_held,
        )
    })
    .await
}

/// The queue checkout a claimed batch runs in: made from the first member
/// checkout that still resolves. Members of one repository share its object
/// store and remotes, so any of them gives the same worktree; a head whose
/// sandbox was reclaimed no longer fails the whole batch. With one checkout the
/// refusal is that checkout's, as before.
async fn queue_repo_for_batch(
    launcher: &LanderLauncher,
    checkouts: &[String],
) -> std::result::Result<PathBuf, String> {
    let mut tried: Vec<&String> = Vec::new();
    let mut first_error = None;
    for checkout in checkouts {
        if tried.contains(&checkout) {
            continue;
        }
        tried.push(checkout);
        match queue_repo(launcher, checkout).await {
            Ok(repo) => return Ok(repo),
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    Err(first_error.unwrap_or_else(|| "the batch has no member checkout".to_string()))
}

/// The refusal code a failed queue workspace settles with: a host that cannot
/// enforce the write fence gets its own typed code (#1160), anything else is
/// the generic `lander_unavailable`.
fn workspace_refusal_code(message: &str) -> &'static str {
    match message.starts_with(write_fence::UNSUPPORTED_CODE) {
        true => write_fence::UNSUPPORTED_CODE,
        false => "lander_unavailable",
    }
}

/// What a lander run resolved to, before settlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RunResult {
    pub state: RollingQueueEntryState,
    pub outcome: RollingQueueOutcome,
}

/// The queue outcome detail for `text`: its newest [`OUTCOME_TAIL_BYTES`].
pub fn tail(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= OUTCOME_TAIL_BYTES {
        return trimmed.to_string();
    }
    let mut start = trimmed.len() - OUTCOME_TAIL_BYTES;
    while !trimmed.is_char_boundary(start) {
        start += 1;
    }
    trimmed[start..].to_string()
}

/// One JSON-escaped line for the lander log and the queue outcome. Reserve
/// equal room for both streams so verbose stdout cannot crowd out a panic on
/// stderr. Escaping keeps test output from becoming lander receipt fields.
pub fn gate_failure_excerpt_line(test: &str, stdout: &str, stderr: &str) -> String {
    let header = format!("failing test: {}\nstdout tail:\n", clip_line(test));
    const STDERR_HEADER: &str = "\nstderr tail:\n";
    let stream_bytes = (GATE_FAILURE_EXCERPT_BYTES - header.len() - STDERR_HEADER.len()) / 2;
    let excerpt = format!(
        "{header}{}{STDERR_HEADER}{}",
        newest_bytes(stdout, stream_bytes),
        newest_bytes(stderr, stream_bytes),
    );
    format!("{GATE_FAILURE_EXCERPT_KEY}={}", serde_json::json!(excerpt))
}

fn gate_failure_excerpt(stdout: &str) -> Option<String> {
    stdout.lines().rev().find_map(|line| {
        let encoded = line
            .strip_prefix(GATE_FAILURE_EXCERPT_KEY)?
            .strip_prefix('=')?;
        let excerpt: String = serde_json::from_str(encoded).ok()?;
        Some(newest_bytes(&excerpt, GATE_FAILURE_EXCERPT_BYTES).to_string())
    })
}

/// The last `key=value` line's value: a gate re-run after a stale advance
/// prints its timing again, and the final line is the one that decided the run.
pub(crate) fn last_line_value<'a>(stdout: &'a str, key: &str) -> Option<&'a str> {
    stdout
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
}

pub(crate) fn line_value<'a>(stdout: &'a str, key: &str) -> Option<&'a str> {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
}

/// The marker the lander appends to an integration conflict message: the one
/// accepted source whose merge conflicted with the target plus the sources
/// merged before it. A batch refuses only that member (#1119).
pub const CONFLICT_SOURCE_KEY: &str = "conflict_source";

/// Append the conflicting source to a lander `error` that is an integration
/// conflict; any other error is returned unchanged.
#[must_use]
pub fn mark_conflict_source(error: String, source: &str) -> String {
    if !error.contains("Conflict {") {
        return error;
    }
    format!("{error}; {CONFLICT_SOURCE_KEY}={source}")
}

/// The source a lander conflict message names (`None`: an older lander, or
/// not a conflict).
#[must_use]
pub fn conflict_source(stderr: &str) -> Option<String> {
    let marker = format!("{CONFLICT_SOURCE_KEY}=");
    let (_, rest) = stderr.rsplit_once(&marker)?;
    let source: String = rest
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect();
    Some(source).filter(|source| !source.is_empty())
}

/// The receipt key for one failing test: the lander prints one
/// `failing_test=<name>` stdout line per new failure it refuses on.
pub const FAILING_TEST_KEY: &str = "failing_test";

/// One machine-readable receipt line for a failing test.
#[must_use]
pub fn failing_test_line(name: &str) -> String {
    format!("{FAILING_TEST_KEY}={name}")
}

/// The failing tests a run reported: the lander's `failing_test=` receipt
/// lines first, else the libtest output: `test ... FAILED` lines, the
/// `---- name stdout ----` headers and the closing `failures:` name list. The
/// last two matter because a test that prints while it runs (a child process,
/// a fixture) splits its `test name ... ok|FAILED` line, so the verdict never
/// shares a line with the name (#1831).
#[must_use]
pub fn failing_tests(text: &str) -> Vec<String> {
    let mut names = all_failing_tests(text);
    names.truncate(MAX_FAILING_TESTS);
    names
}

/// [`failing_tests`] before the cap: every name, sorted and deduplicated, so a
/// caller can count what its own bound drops (#185).
pub(crate) fn all_failing_tests(text: &str) -> Vec<String> {
    let marked: Vec<String> = text
        .lines()
        .filter_map(|line| line_value(line.trim(), FAILING_TEST_KEY))
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();
    let mut names: Vec<String> = if marked.is_empty() {
        libtest_failure_names(text)
    } else {
        marked
    };
    names.sort();
    names.dedup();
    names
}

/// The libtest failure header: `---- name stdout ----`.
fn libtest_header_name(line: &str) -> Option<&str> {
    line.trim()
        .strip_prefix("---- ")?
        .strip_suffix(" stdout ----")
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

/// Every failing test name in libtest output, unsorted and possibly repeated.
fn libtest_failure_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_block = false;
    let mut block_has_names = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("test ") {
            if let Some(name) = rest.strip_suffix(" ... FAILED") {
                names.push(name.to_string());
            }
        }
        if let Some(name) = libtest_header_name(line) {
            names.push(name.to_string());
        }
        if line == "failures:" {
            in_block = true;
            block_has_names = false;
        } else if in_block {
            if line.starts_with("    ") && !trimmed.is_empty() {
                block_has_names = true;
                names.push(trimmed.to_string());
            } else if !(trimmed.is_empty() && !block_has_names) {
                in_block = false;
            }
        }
    }
    names
}

const SUMMARY_LINE_BYTES: usize = 300;
const SUMMARY_BYTES: usize = 2048;

fn clip_line(line: &str) -> String {
    let line = line.trim();
    if line.len() <= SUMMARY_LINE_BYTES {
        return line.to_string();
    }
    let mut end = SUMMARY_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &line[..end])
}

/// A short libtest failures summary: the failing names, the first panic or
/// `Error:` line of each failing test and the `test result:` line. `None`
/// when the output holds no libtest failure.
fn libtest_failure_summary(text: &str) -> Option<String> {
    let names = failing_tests(text);
    if names.is_empty() {
        return None;
    }
    let mut summary = format!("failing tests: {}", names.join(", "));
    let mut current: Option<&str> = None;
    let mut found = false;
    let mut result_line = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(name) = libtest_header_name(line) {
            current = Some(name);
            found = false;
            continue;
        }
        if trimmed.starts_with("test result: FAILED") {
            result_line = Some(clip_line(trimmed));
        }
        let Some(name) = current else { continue };
        if found {
            continue;
        }
        if trimmed.contains("panicked at") || trimmed.starts_with("Error: ") {
            found = true;
            let evidence = format!("\n[{name}] {}", clip_line(trimmed));
            if summary.len() + evidence.len() <= SUMMARY_BYTES {
                summary.push_str(&evidence);
            }
        }
    }
    if let Some(result) = result_line {
        summary.push('\n');
        summary.push_str(&result);
    }
    Some(summary)
}

/// `text` without cargo/rustc `warning` blocks (a `warning:` line through the
/// next blank line), so a refusal's detail is not filled with compiler noise.
fn without_cargo_warnings(text: &str) -> String {
    let mut kept = Vec::new();
    let mut skipping = false;
    for line in text.lines() {
        if line.starts_with("warning:") || line.starts_with("warning[") {
            skipping = true;
            continue;
        }
        if skipping {
            // A block ends at a blank line or at the first line that is not a
            // continuation (indented, a source gutter, `...`, `note:`/`help:`):
            // the one-line `warning: `x` generated N warnings` summary has no
            // blank line after it.
            let continues = line.starts_with(char::is_whitespace)
                || line.starts_with('.')
                || line.starts_with(|ch: char| ch.is_ascii_digit())
                || line.starts_with("note:")
                || line.starts_with("help:");
            if line.trim().is_empty() {
                skipping = false;
                continue;
            }
            if continues {
                continue;
            }
            skipping = false;
        }
        kept.push(line);
    }
    kept.join("\n")
}

/// Whether `line` is a cargo progress line (`Blocking waiting for file lock`,
/// `Compiling`, `Checking`, ...): status chatter that says nothing about why a
/// build failed.
fn is_cargo_progress_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    [
        "Blocking waiting for file lock",
        "Compiling ",
        "Checking ",
        "Finished ",
        "Fresh ",
        "Downloaded ",
        "Downloading ",
        "Updating ",
        "Locking ",
        "Adding ",
        "Building [",
    ]
    .iter()
    .any(|prefix| trimmed.starts_with(prefix))
}

/// `text` without cargo progress lines.
fn without_cargo_progress(text: &str) -> String {
    text.lines()
        .filter(|line| !is_cargo_progress_line(line))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A compiler error line: `error[E0425]: ...` or `error: ...`, except cargo's
/// own closing lines, which name no cause.
fn is_compiler_error_line(line: &str) -> bool {
    let is_error = line.starts_with("error[") || line.starts_with("error:");
    is_error
        && !line.starts_with("error: could not compile")
        && !line.starts_with("error: aborting due to")
        && !line.starts_with("error: test failed")
        && !line.starts_with("error: build failed")
}

/// The lead of a failed build's detail: the first compiler error with its
/// `--> file:line` location and the error count, e.g.
/// `compile error: error[E0425]: cannot find function `names_in` at
/// crates/rsid/src/x.rs:639 (1 error)`. `None` when the output holds no
/// compiler error. Progress lines and `note:` sections never lead, because
/// only an `error` line is considered.
fn compile_error_summary(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().map(str::trim_start).collect();
    let mut count = 0usize;
    let mut first: Option<(usize, &str)> = None;
    for (index, line) in lines.iter().enumerate() {
        if is_compiler_error_line(line) {
            count += 1;
            first.get_or_insert((index, line));
        }
    }
    let (index, line) = first?;
    let location = lines[index + 1..]
        .iter()
        .take_while(|next| !is_compiler_error_line(next) && !next.is_empty())
        .find_map(|next| next.strip_prefix("--> "))
        .map(|location| format!(" at {}", location.trim()))
        .unwrap_or_default();
    let plural = if count == 1 { "error" } else { "errors" };
    Some(format!(
        "compile error: {}{location} ({count} {plural})",
        clip_line(line)
    ))
}

/// Map one lander exit (`exit`, stdout evidence lines, stderr message) to a
/// typed result. `landed` is the ancestry probe used when publication is
/// uncertain, so a push that succeeded before the lander could report it is
/// still recorded as published.
pub(crate) fn classify_run(
    exit: Option<i32>,
    stdout: &str,
    stderr: &str,
    landed: impl FnOnce() -> Option<String>,
) -> RunResult {
    // The lander's gate timing (#1132): shards, base cache hits and runs,
    // prebuild and wall seconds. Kept in the outcome detail so the manager can
    // read the queue ETA from the entry row.
    let timing = last_line_value(stdout, "gate_timing").map(|value| format!("gate_timing={value}"));
    let published = |sha: String| RunResult {
        state: RollingQueueEntryState::Published,
        outcome: RollingQueueOutcome {
            landed_sha: Some(sha),
            detail: if line_value(stdout, "landing_outcome") == Some("already_on_rolling") {
                Some(match &timing {
                    Some(timing) => format!("already_on_rolling; {timing}"),
                    None => "already_on_rolling".into(),
                })
            } else {
                timing.clone()
            },
            ..RollingQueueOutcome::default()
        },
    };
    // The libtest failures summary leads the detail, then the stderr tail with
    // cargo warnings dropped (#1831).
    let summary = libtest_failure_summary(&format!("{stdout}\n{stderr}"));
    // #3393: a failed build leads with its first compiler error, so a head cut
    // or a long progress preamble never hides the cause.
    let compile_error = compile_error_summary(&format!("{stdout}\n{stderr}"));
    let stderr_text = Some(tail(&without_cargo_progress(&without_cargo_warnings(
        stderr,
    ))))
    .filter(|text| !text.is_empty());
    let detail = {
        let parts: Vec<String> = [
            compile_error,
            summary,
            stderr_text,
            timing.clone(),
            gate_failure_excerpt(stdout),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!parts.is_empty()).then(|| parts.join("\n"))
    };
    let refuse = |code: &str, state: RollingQueueEntryState| RunResult {
        state,
        outcome: RollingQueueOutcome {
            landed_sha: None,
            refusal: Some(code.to_string()),
            failing_tests: failing_tests(&format!("{stdout}\n{stderr}")),
            detail: detail.clone(),
            superseded_by: None,
            published_commits: Vec::new(),
        },
    };
    if exit == Some(0) {
        return match line_value(stdout, "published_target_id") {
            Some(tip) => published(tip.to_string()),
            None => refuse("lander_report_missing_tip", RollingQueueEntryState::Failed),
        };
    }
    let status = line_value(stdout, "publication_status");
    if matches!(status, Some("published" | "unknown")) {
        // The push may have landed before the report failed: trust ancestry.
        return match landed() {
            Some(tip) => published(tip),
            // Infrastructure loss says nothing about the source. Re-admit
            // this entry in place; a fresh gate/ancestry check decides it.
            None => refuse("publication_unknown", RollingQueueEntryState::Queued),
        };
    }
    // Hub #3873: the lander could not read the remote `rolling` ref after its
    // own retries. Infrastructure says nothing about the sources: the batch goes
    // back to the queue (never bisected, never refused) and the cause is named.
    if line_value(stdout, "failure_class") == Some(LANDER_FAILURE_CLASS_REMOTE_READ) {
        let mut result = refuse(
            QUEUE_REMOTE_READ_UNAVAILABLE,
            RollingQueueEntryState::Queued,
        );
        result.outcome.failing_tests = Vec::new();
        result.outcome.detail = Some(format!(
            "cause=remote_read_unavailable (git ls-remote/fetch of rolling failed after the lander's retries; \
             infrastructure, not a test failure)\n{}",
            detail.as_deref().unwrap_or_default()
        ));
        return result;
    }
    if stderr.contains("stale retries exhausted") {
        return refuse(QUEUE_REGATE_EXHAUSTED, RollingQueueEntryState::Refused);
    }
    if stderr.contains(ALREADY_INTEGRATED_MARKER) {
        // Already on rolling: the landing happened, just not through this run
        // (#1208). Never a red gate.
        return match landed() {
            Some(tip) => RunResult {
                state: RollingQueueEntryState::Published,
                outcome: RollingQueueOutcome {
                    landed_sha: Some(tip),
                    detail: Some("already on origin/rolling: landed outside the queue".into()),
                    ..RollingQueueOutcome::default()
                },
            },
            None => refuse(
                QUEUE_SOURCE_ALREADY_INTEGRATED,
                RollingQueueEntryState::Refused,
            ),
        };
    }
    if stderr.contains("Conflict {") {
        return refuse(QUEUE_BATCH_MERGE_CONFLICT, RollingQueueEntryState::Refused);
    }
    if stderr.contains("error: no tests to run") {
        return refuse(
            QUEUE_FILTER_MATCHES_NO_TESTS,
            RollingQueueEntryState::Refused,
        );
    }
    // #2167: a claimed fix whose tests are still red (or never ran).
    if stderr.contains(&format!("{QUEUE_CLAIMED_FIX_STILL_RED}: ")) {
        return refuse(QUEUE_CLAIMED_FIX_STILL_RED, RollingQueueEntryState::Refused);
    }
    if let Some(package) = release_build_failed_crate(stderr) {
        return refuse(
            &format!("{QUEUE_RELEASE_BUILD_FAILED}:{package}"),
            RollingQueueEntryState::Refused,
        );
    }
    if exit == Some(8) {
        let fence = line_value(stdout, "policy_fence").unwrap_or("unknown");
        return refuse(
            &format!("policy_refused:{fence}"),
            RollingQueueEntryState::Refused,
        );
    }
    refuse("gate_failed", RollingQueueEntryState::Refused)
}

/// The crate named by the lander's `release_build_failed:<crate>` refusal
/// (#1970), if the gate output carries one.
pub(crate) fn release_build_failed_crate(stderr: &str) -> Option<&str> {
    let marker = format!("{QUEUE_RELEASE_BUILD_FAILED}:");
    let rest = &stderr[stderr.find(&marker)? + marker.len()..];
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

/// Whether a changed path can alter what the Rust gate builds or tests: any
/// Rust source (migrations and `build.rs` included), a Cargo manifest or lock,
/// the toolchain pin, or cargo configuration.
fn path_affects_rust_gate(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.ends_with(".rs")
        || name == "Cargo.toml"
        || name == "Cargo.lock"
        || name == "rust-toolchain.toml"
        || name == "rust-toolchain"
        || path.starts_with(".cargo/")
}

/// Whether `source` is provably free of Rust, Cargo and migration changes
/// relative to rolling. Any git failure answers `false`: the member is then
/// treated as code, so an unknown diff never narrows the gate.
pub(crate) fn source_is_rust_free(repo: &Path, source: &str) -> bool {
    if let Some((reference, tree)) = multi_base_merge_tree(repo, source) {
        return git(
            repo,
            &["diff", "--name-only", "--no-renames", &reference, &tree],
        )
        .is_some_and(|changed| !changed.lines().any(path_affects_rust_gate));
    }
    let base = ["origin/rolling", "rolling"].iter().find_map(|reference| {
        git(repo, &["merge-base", source, reference]).map(|out| out.trim().to_string())
    });
    let Some(base) = base.filter(|base| !base.is_empty()) else {
        return false;
    };
    git(
        repo,
        &["diff", "--name-only", "--no-renames", &base, source],
    )
    .is_some_and(|changed| !changed.lines().any(path_affects_rust_gate))
}

/// Union of the members' filters. A member with Rust changes and no filters
/// makes the batch run the lander's full affected gate; a member whose diff
/// touches no Rust, Cargo or migration file (`rust_free`) cannot change what
/// the gate tests, so it contributes only the filters it carries and never
/// widens the batch (#1527).
pub(crate) fn union_filters(
    entries: &[RollingQueueEntryV1],
    rust_free: impl Fn(&RollingQueueEntryV1) -> bool,
) -> Vec<String> {
    if entries
        .iter()
        .any(|entry| entry.test_filters.is_empty() && !rust_free(entry))
    {
        return Vec::new();
    }
    let mut union: Vec<String> = Vec::new();
    for filter in entries.iter().flat_map(|entry| &entry.test_filters) {
        if !union.contains(filter) {
            union.push(filter.clone());
        }
    }
    union
}

/// A filter stays strict if any member requested it by hand. Missing legacy
/// provenance is strict too; it can never grant the derived-empty fallback.
fn union_derived_filters(entries: &[RollingQueueEntryV1]) -> Vec<String> {
    let explicit: HashSet<&String> = entries
        .iter()
        .flat_map(|entry| {
            entry
                .test_filters
                .iter()
                .filter(|filter| !entry.derived_test_filters.contains(filter))
        })
        .collect();
    entries
        .iter()
        .flat_map(|entry| &entry.derived_test_filters)
        .filter(|filter| !explicit.contains(filter))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// #2167: every member's claimed fixes, first claim first. A batch's gate
/// must run each green; a bisect step claims only its own members' fixes.
pub(crate) fn union_fix_claims(entries: &[RollingQueueEntryV1]) -> Vec<String> {
    entries.iter().fold(Vec::new(), |claims, entry| {
        rsi_common::rolling_queue::union_fix_claims(&claims, &entry.fixes_tests)
    })
}

/// Keep the newest `max` bytes of `text`.
fn newest_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Keep the newest `keep` runs: each `.log` with its `.timings.tsv` sidecar,
/// deleted together. Retention of logs is unchanged by the sidecar (#2085).
fn prune_logs(dir: &Path, keep: usize) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = read
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            (meta.is_file() && entry.path().extension().is_some_and(|ext| ext == "log")).then(
                || {
                    (
                        meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                        entry.path(),
                    )
                },
            )
        })
        .collect();
    if files.len() <= keep {
        return;
    }
    files.sort();
    let excess = files.len() - keep;
    for (_, path) in files.into_iter().take(excess) {
        let _ = std::fs::remove_file(path.with_extension("timings.tsv"));
        let _ = std::fs::remove_file(path);
    }
}

static RUN_LOG_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Reserve `<dir>/<batch>-<run>.log` and its `.timings.tsv` sidecar before the
/// lander starts (the directory rotated), so the lander can append per-test
/// durations while it runs. Best effort: `None` on an I/O error.
fn prepare_run_log(dir: &Path, batch_id: Uuid) -> Option<PathBuf> {
    std::fs::create_dir_all(dir).ok()?;
    let seq = RUN_LOG_SEQ.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!(
        "{batch_id}-{}-{seq}.log",
        Utc::now().timestamp_millis()
    ));
    std::fs::write(&path, "lander running\n").ok()?;
    // The sidecar is evidence only: a failure to create it never costs the log.
    let _ = std::fs::write(
        path.with_extension("timings.tsv"),
        crate::integration::TEST_TIMINGS_HEADER,
    );
    prune_logs(dir, RUN_LOG_KEEP_FILES);
    Some(path)
}

/// Reserve and fill one run's log in a single step (tests only).
#[cfg(test)]
fn write_run_log(
    dir: &Path,
    batch_id: Uuid,
    header: &str,
    stdout: &str,
    stderr: &str,
) -> Option<PathBuf> {
    let path = prepare_run_log(dir, batch_id)?;
    finish_run_log(&path, header, stdout, stderr)
}

/// Write one lander run's bounded output to its reserved log and return the
/// path. Best effort: `None` on an I/O error.
fn finish_run_log(path: &Path, header: &str, stdout: &str, stderr: &str) -> Option<PathBuf> {
    let body = format!(
        "{header}\n--- stdout ---\n{}\n--- stderr ---\n{}\n",
        newest_bytes(stdout, RUN_LOG_STREAM_BYTES),
        newest_bytes(stderr, RUN_LOG_STREAM_BYTES),
    );
    std::fs::write(path, body).ok()?;
    if let Some(dir) = path.parent() {
        prune_logs(dir, RUN_LOG_KEEP_FILES);
    }
    Some(path.to_path_buf())
}

/// The conflicting paths a lander conflict message names
/// (`Conflict { paths: ["a", "b"] }`).
fn conflict_paths(stderr: &str) -> Option<String> {
    let (_, rest) = stderr.split_once("Conflict { paths: [")?;
    let (list, _) = rest.split_once(']')?;
    let paths: Vec<&str> = list
        .split(',')
        .map(|path| path.trim().trim_matches('"'))
        .filter(|path| !path.is_empty())
        .collect();
    (!paths.is_empty()).then(|| paths.join(","))
}

/// A short, human-readable cause for a non-green run: the refusal code, the
/// conflicting paths or the first failing tests (#1135).
fn run_cause(result: &RunResult, stderr: &str) -> Option<String> {
    let refusal = result.outcome.refusal.as_deref()?;
    let mut cause = refusal.to_string();
    if let Some(paths) = conflict_paths(stderr) {
        cause.push_str(&format!("; conflict_paths={paths}"));
    }
    let tests = &result.outcome.failing_tests;
    if !tests.is_empty() {
        let shown: Vec<&str> = tests.iter().take(CAUSE_TESTS).map(String::as_str).collect();
        cause.push_str(&format!("; failing_tests={}", shown.join(",")));
        if tests.len() > CAUSE_TESTS {
            cause.push_str(&format!(" (+{} more)", tests.len() - CAUSE_TESTS));
        }
    }
    Some(cause)
}

/// Why a lander run produced no report.
#[derive(Debug)]
enum LanderError {
    /// The lander could not be started or waited for.
    Unavailable(String),
    /// The batch's gate wall-time budget ran out (#1208); the run (if any) was
    /// stopped.
    TimedOut,
}

/// Write `derived` aside and rename it into `path`, so a reader never sees half.
fn write_derived_filters_file(path: &Path, derived: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, derived)?;
    std::fs::rename(&tmp, path)
}

/// The derived filters in a private temporary file (an undetached run has no
/// run directory); removed when the returned path drops.
fn write_temp_derived_filters(derived: &str) -> std::io::Result<tempfile::TempPath> {
    use std::io::Write as _;
    let mut file = tempfile::Builder::new()
        .prefix("rsi-derived-filters-")
        .suffix(".json")
        .tempfile()?;
    file.write_all(derived.as_bytes())?;
    file.flush()?;
    Ok(file.into_temp_path())
}

async fn run_lander(
    launcher: &LanderLauncher,
    repo: &Path,
    entries: &[RollingQueueEntryV1],
    expected_tip: Option<&str>,
    deadline: tokio::time::Instant,
    timings_path: Option<&Path>,
) -> std::result::Result<std::process::Output, LanderError> {
    let budget = deadline.saturating_duration_since(tokio::time::Instant::now());
    if budget.is_zero() {
        return Err(LanderError::TimedOut);
    }
    let mut inner = Command::new(&launcher.binary);
    // #3391: the lander runs agent-written build scripts, test modules and
    // hooks; it gets the allowlisted environment, never the daemon's whole one.
    // First, because `env_clear` drops anything set before it.
    crate::lander_env_policy::apply_tokio(&mut inner);
    #[cfg(test)]
    inner.envs(
        launcher
            .inherited_env
            .iter()
            .map(|(key, value)| (key, value)),
    );
    inner
        .current_dir(repo)
        .arg("--repo")
        .arg(repo)
        .args(["--remote", "origin"]);
    for entry in entries {
        inner.args(["--accepted", &entry.source_commit]);
    }
    for filter in union_filters(entries, |entry| {
        source_is_rust_free(repo, &entry.source_commit)
    }) {
        inner.args(["--test-filter", &filter]);
    }
    // #3615: the derived set travels in a file. One environment string over
    // 128 KiB (MAX_ARG_STRLEN) makes `execve` fail with E2BIG before any gate
    // runs, and the set grows with the diff. Even small sets use only the file.
    let derived = serde_json::to_string(&union_derived_filters(entries)).unwrap();
    for name in union_fix_claims(entries) {
        inner.args(["--fixes-test", &name]);
    }
    if let Some(tip) = expected_tip {
        // The tip the queue probed just before this run: a different initial
        // fetch is an out-of-band advance the lander charges as a re-gate.
        inner.args(["--expected-tip", tip]);
    }
    if let Some(path) = &launcher.known_failure_snapshot {
        inner.env("RSI_KNOWN_FAILURES_SNAPSHOT", path);
    }
    if let Some(path) = timings_path {
        inner.env(crate::integration::TEST_TIMINGS_ENV, path);
    }
    inner
        .env(REGATE_BUDGET_ENV, budget.as_secs().to_string())
        .env(EXACT_GATE_ENV, "1")
        .env_remove("RSI_SESSION_TOKEN");
    if let Some(target) = launcher.target_dir() {
        // The queue owns its build directory: no per-session environment.
        std::fs::create_dir_all(&target).map_err(|error| {
            LanderError::Unavailable(format!("cannot create the queue target: {error}"))
        })?;
        inner.env("CARGO_TARGET_DIR", target);
    }
    // #3435: the run lives in files, not in this daemon, so the next daemon
    // can adopt it.
    if let Some(runs_dir) = launcher.detached_runs_dir() {
        return run_detached_lander(launcher, inner, &runs_dir, deadline, &derived).await;
    }
    // Held until the run ends: the file is removed when this drops.
    let derived_file = write_temp_derived_filters(&derived).map_err(|error| {
        LanderError::Unavailable(format!("cannot write the derived filters: {error}"))
    })?;
    inner.env(
        rsi_common::rolling_queue::DERIVED_FILTERS_FILE_ENV,
        derived_file.as_os_str(),
    );
    // #2119: the gate's cargo builds and tests run in the hub build slice, so
    // they share its CPU budget with the job units. The scope is only used when
    // the slice could be configured; otherwise the lander runs as before.
    let mut scope_unit = None;
    let mut command =
        if launcher.build_slice && !crate::build_budget::apply("lander").await.is_unavailable() {
            let (command, unit) = crate::build_budget::lander_scope_command(&inner);
            scope_unit = Some(unit);
            command
        } else {
            inner
        };
    command
        // Remove this after wrapping: the scope wrapper reapplies the parent
        // environment, which can carry a stale inline set from an outer gate.
        .env_remove(rsi_common::rolling_queue::DERIVED_FILTERS_INLINE_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Its own process group, so a timed-out run is stopped with everything it
    // started that stayed in the group. The group is signalled only while the
    // lander is unreaped, so a saved id can never hit a reused group (#1251).
    command.process_group(0);
    let child = command
        .spawn()
        .map_err(|error| LanderError::Unavailable(format!("cannot start the lander: {error}")))?;
    let result = crate::process_control::capture_owned_group(
        child,
        deadline,
        LANDER_SETTLE_LIMITS,
        "rolling queue gate wall-time budget ran out; stopping the lander group",
    )
    .await
    .map_err(|error| match error {
        crate::process_control::OwnedGroupError::TimedOut => LanderError::TimedOut,
        crate::process_control::OwnedGroupError::Io(error) => {
            LanderError::Unavailable(format!("lander wait failed: {error}"))
        }
    });
    if let (Err(_), Some(unit)) = (&result, scope_unit) {
        // A descendant that left the lander's group is still in its scope.
        crate::build_budget::kill_scope(unit).await;
    }
    result
}

/// One lander run kept in files under `runs_dir` (#3435): a run a previous
/// daemon left for the same gate is adopted and waited for, otherwise `inner`
/// is started in its own scope (when the build slice is on) with its output and
/// exit status in files. The run is not tied to this daemon: dropping this
/// future or stopping the daemon leaves it running for the next one, and only
/// the deadline, a vanished scope or a different gate ends it.
async fn run_detached_lander(
    launcher: &LanderLauncher,
    mut inner: Command,
    runs_dir: &Path,
    deadline: tokio::time::Instant,
    derived: &str,
) -> std::result::Result<std::process::Output, LanderError> {
    let key = detached_run::run_key(
        inner.as_std().get_current_dir(),
        inner.as_std().get_args().map(std::ffi::OsString::from),
        derived,
    );
    let mut run = detached_run::claim(runs_dir, &key, launcher.run_epoch())
        .await
        .map_err(|error| {
            LanderError::Unavailable(format!("cannot prepare the lander run: {error}"))
        })?;
    if run.adopted {
        tracing::info!(
            key = %run.record.key,
            unit = ?run.record.unit,
            "rolling queue adopted the lander run a previous daemon left running"
        );
        return detached_run::wait(&run, deadline).await;
    }
    let unavailable = |run: &detached_run::Run, what: &str, error: &dyn std::fmt::Display| {
        let _ = std::fs::remove_dir_all(runs_dir.join(&run.record.key));
        LanderError::Unavailable(format!("{what}: {error}"))
    };
    // The run directory is private to this run and removed with it (#3615).
    let derived_path = runs_dir.join(&run.record.key).join("derived-filters.json");
    write_derived_filters_file(&derived_path, derived)
        .map_err(|error| unavailable(&run, "cannot write the derived filters", &error))?;
    inner.env(
        rsi_common::rolling_queue::DERIVED_FILTERS_FILE_ENV,
        &derived_path,
    );
    let wrapped = crate::build_budget::exit_recording_command(&inner, &run.exit_path());
    let mut command =
        if launcher.build_slice && !crate::build_budget::apply("lander").await.is_unavailable() {
            let (command, unit) = crate::build_budget::detached_lander_scope_command(&wrapped);
            run.record.unit = Some(unit);
            command
        } else {
            wrapped
        };
    let files = std::fs::File::create(run.stdout_path())
        .and_then(|stdout| Ok((stdout, std::fs::File::create(run.stderr_path())?)));
    let (stdout, stderr) =
        files.map_err(|error| unavailable(&run, "cannot open the lander output", &error))?;
    command
        // The exit-recording and scope wrappers reapply the parent environment.
        // Only this run's file may supply its derived filters (#3615).
        .env_remove(rsi_common::rolling_queue::DERIVED_FILTERS_INLINE_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        // The run outlives this daemon: nothing here kills it on drop.
        .kill_on_drop(false);
    // Its own process group, so a stopped run takes down what it started.
    command.process_group(0);
    // The record names the scope before the process exists, so a daemon that
    // dies right after the spawn still leaves a run its successor can stop.
    run.save()
        .map_err(|error| unavailable(&run, "cannot record the lander run", &error))?;
    let child = command
        .spawn()
        .map_err(|error| unavailable(&run, "cannot start the lander", &error))?;
    run.record.pid = child.id().unwrap_or(0);
    // Reaped in the background when it ends; this daemon never owns its life.
    drop(child);
    run.save()
        .map_err(|error| unavailable(&run, "cannot record the lander run", &error))?;
    detached_run::wait(&run, deadline).await
}

/// How long a finished lander's output may stay open (a descendant that left
/// its group still holding the pipes), and how long a stopped lander gets to
/// exit, before the queue moves on (#1251).
const LANDER_SETTLE_LIMITS: crate::process_control::OwnedGroupLimits =
    crate::process_control::OwnedGroupLimits {
        post_exit_drain: Duration::from_secs(10),
        terminate_grace: Duration::from_secs(2),
        cleanup: Duration::from_secs(5),
    };

async fn with_store<T: Send + 'static>(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    op: impl FnOnce(&Store) -> Result<T> + Send + 'static,
) -> Result<T> {
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || op(&store.blocking_lock()))
        .await
        .map_err(|error| DaemonError::Process(format!("rolling queue store join: {error}")))?
}

/// Extra lander runs a batch may spend beyond its full gate and the
/// `ceil(log2(n))` halving steps of a bisect: the tail gate after an isolated
/// member and the complement gate that confirms or rejects a direct
/// attribution (#3267).
const BATCH_EXTRA_RUNS: u32 = 4;

/// The wall-time a batch's gating may spend (#1208, #3267). One lander run is
/// bounded by `per_run` (the operator's `rolling_queue_gate_timeout_mins`), not
/// by what the batch's earlier runs left: a 4-member batch with one
/// non-attributable red needs the full run plus its bisect steps, each about
/// as long as a first gate, so a single shared 60 minutes could not name the
/// culprit. The batch as a whole is bounded by a `ceiling` that scales with
/// the bisect depth (`ceil(log2(n)) + BATCH_EXTRA_RUNS` runs), so a stuck
/// batch still cannot hold the queue without limit.
#[derive(Clone, Copy, Debug)]
struct GateBudget {
    per_run: Duration,
    ceiling: tokio::time::Instant,
}

impl GateBudget {
    fn new(per_run: Duration, members: usize) -> Self {
        let depth = members.max(1).next_power_of_two().trailing_zeros();
        Self {
            per_run,
            ceiling: tokio::time::Instant::now() + per_run * (depth + BATCH_EXTRA_RUNS),
        }
    }

    /// The wall deadline of a lander run that starts now.
    fn run_deadline(&self) -> tokio::time::Instant {
        (tokio::time::Instant::now() + self.per_run).min(self.ceiling)
    }
}

/// One lander run over `entries`, classified. `stdout` is kept for the
/// evidence lines (`fetched_target_id`, `migration_N_*`).
struct GroupRun {
    result: RunResult,
    stdout: String,
    /// The exact commit this run pushed (the lander's `published_target_id`,
    /// or its forward revert): the only tip the queue may call its own. The
    /// observed remote head in `result.outcome.landed_sha` can be a later
    /// external descendant. `None` when the run reported no published commit.
    own_tip: Option<String>,
    /// The member an integration conflict is attributable to, when the lander
    /// named it.
    conflict_source: Option<String>,
    /// The per-run log of the lander's stdout and stderr (#1135).
    log_path: Option<PathBuf>,
    /// Short cause of a non-green run: refusal code, conflict paths or the
    /// first failing tests.
    cause: Option<String>,
    /// The lander refused a source as already integrated into the tip (#1208).
    already_integrated: bool,
}

impl GroupRun {
    /// The event detail fields every gate event carries: the cause and the log.
    fn evidence(&self) -> serde_json::Value {
        serde_json::json!({
            "cause": self.cause,
            "log": self.log_path.as_ref().map(|path| path.display().to_string()),
        })
    }
}

/// Tag the lander's `gate_timing=` line in an outcome detail with the number of
/// sources the run gated together, so the manager reads the batch size beside
/// the gate's timing (#1945). A detail without that line is returned unchanged.
fn note_batch_size(detail: Option<String>, size: usize) -> Option<String> {
    detail.map(|text| {
        if !text.lines().any(|line| line.starts_with("gate_timing=")) {
            return text;
        }
        text.lines()
            .map(|line| match line.starts_with("gate_timing=") {
                true => format!("{line} batch_size={size}"),
                false => line.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn own_published_tip(stdout: &str) -> Option<String> {
    let reverted = line_value(stdout, "forward_revert_status") == Some("published");
    let revert = line_value(stdout, "forward_revert_id").filter(|_| reverted);
    revert
        .or_else(|| line_value(stdout, "published_target_id"))
        .map(|tip| tip.trim().to_string())
        .filter(|tip| !tip.is_empty())
}

async fn run_group(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    launcher: &LanderLauncher,
    batch_id: Uuid,
    repo: &Path,
    entries: &[RollingQueueEntryV1],
    expected_tip: Option<&str>,
    deadline: tokio::time::Instant,
) -> GroupRun {
    let snapshot_diagnostic = if let Some(path) = launcher.known_failure_snapshot.clone() {
        with_store(store, move |store| {
            Ok(known_failure_snapshot::refresh_if_needed(
                &path,
                store,
                Utc::now(),
            ))
        })
        .await
        .unwrap_or_else(|error| format!("known_failures=refresh_failed:{error}\n"))
    } else {
        String::new()
    };
    let log_path = launcher
        .log_dir()
        .and_then(|dir| prepare_run_log(&dir, batch_id));
    let timings_path = log_path
        .as_ref()
        .map(|path| path.with_extension("timings.tsv"))
        .filter(|path| path.is_file());
    match run_lander(
        launcher,
        repo,
        entries,
        expected_tip,
        deadline,
        timings_path.as_deref(),
    )
    .await
    {
        Ok(output) => {
            let stdout = format!(
                "{snapshot_diagnostic}{}",
                String::from_utf8_lossy(&output.stdout)
            );
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let already_integrated =
                output.status.code() != Some(0) && stderr.contains(ALREADY_INTEGRATED_MARKER);
            // Only an uncertain publication, or a lone source the lander calls
            // already integrated, needs the ancestry probe.
            let uncertain = output.status.code() != Some(0)
                && (matches!(
                    line_value(&stdout, "publication_status"),
                    Some("published" | "unknown")
                ) || (already_integrated && entries.len() == 1));
            let landed = if uncertain {
                let source = entries[0].source_commit.clone();
                let probe_repo = repo.to_path_buf();
                tokio::task::spawn_blocking(move || landed_tip(&probe_repo, &source))
                    .await
                    .ok()
                    .flatten()
            } else {
                None
            };
            let mut result = classify_run(output.status.code(), &stdout, &stderr, || landed);
            result.outcome.detail = note_batch_size(result.outcome.detail.take(), entries.len());
            let log_path = log_path.and_then(|path| {
                let header = format!(
                    "lander exit={:?} sources={}\n{snapshot_diagnostic}",
                    output.status.code(),
                    entries
                        .iter()
                        .map(|entry| entry.source_commit.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                );
                finish_run_log(&path, &header, &stdout, &stderr)
            });
            // #2705: a gate that failed reports the base reds it observed too.
            // (A published run files them in `publish_group_with`, once its
            // sources are verified.)
            if result.state != RollingQueueEntryState::Published {
                let lines = gate_base_red::file_base_reds(
                    store,
                    batch_id,
                    entries,
                    &stdout,
                    log_path.as_deref(),
                )
                .await;
                result.outcome.detail =
                    gate_base_red::with_issue_lines(result.outcome.detail.take(), &lines);
            }
            let cause = run_cause(&result, &stderr);
            if let Some(path) = &log_path {
                let line = format!("lander_log={}", path.display());
                result.outcome.detail = Some(match result.outcome.detail.take() {
                    Some(detail) => format!("{detail}\n{line}"),
                    None => line,
                });
            }
            let own_tip = own_published_tip(&stdout);
            let conflict_source = conflict_source(&stderr);
            GroupRun {
                result,
                stdout,
                own_tip,
                conflict_source,
                log_path,
                cause,
                already_integrated,
            }
        }
        Err(LanderError::TimedOut) => GroupRun {
            result: gate_timeout_result(launcher),
            stdout: String::new(),
            own_tip: None,
            conflict_source: None,
            log_path: log_path.and_then(|path| {
                finish_run_log(
                    &path,
                    QUEUE_GATE_TIMEOUT,
                    "",
                    "lander stopped at gate wall-time budget",
                )
            }),
            cause: Some(QUEUE_GATE_TIMEOUT.into()),
            already_integrated: false,
        },
        Err(LanderError::Unavailable(message)) => GroupRun {
            result: RunResult {
                state: RollingQueueEntryState::Failed,
                outcome: RollingQueueOutcome {
                    refusal: Some("lander_unavailable".into()),
                    detail: Some(message),
                    ..RollingQueueOutcome::default()
                },
            },
            stdout: String::new(),
            own_tip: None,
            conflict_source: None,
            log_path: log_path.and_then(|path| {
                finish_run_log(
                    &path,
                    "lander_unavailable",
                    "",
                    "lander could not start or be supervised",
                )
            }),
            cause: Some("lander_unavailable".into()),
            already_integrated: false,
        },
    }
}

/// The typed refusal of a batch that ran past its gate wall-time budget.
fn gate_timeout_result(launcher: &LanderLauncher) -> RunResult {
    RunResult {
        state: RollingQueueEntryState::Refused,
        outcome: RollingQueueOutcome {
            refusal: Some(QUEUE_GATE_TIMEOUT.to_string()),
            detail: Some(format!(
                "a lander run of the batch gate ran past its {}-minute wall-time budget \
                 (operator setting rolling_queue_gate_timeout_mins, or the batch's \
                 bisect ceiling); the lander was stopped and nothing was published \
                 for this source: re-enqueue it",
                launcher.gate_timeout.as_secs() / 60
            )),
            ..RollingQueueOutcome::default()
        },
    }
}

fn is_gate_timeout(result: &RunResult) -> bool {
    result.outcome.refusal.as_deref() == Some(QUEUE_GATE_TIMEOUT)
}

/// Hub #3873: a run that stopped only because the remote `rolling` ref could
/// not be read. It is never bisectable and never a refusal.
fn is_remote_read_unavailable(result: &RunResult) -> bool {
    result.state == RollingQueueEntryState::Queued
        && result.outcome.refusal.as_deref() == Some(QUEUE_REMOTE_READ_UNAVAILABLE)
}

/// The pause before the queue claims again after the `requeues`th remote-read
/// requeue: 30 s doubling to a 5 minute cap, so an outage is not hammered.
fn remote_read_backoff(requeues: u32) -> Duration {
    if cfg!(test) {
        return Duration::from_millis(5);
    }
    let seconds = 30u64.saturating_mul(1 << requeues.saturating_sub(1).min(4));
    Duration::from_secs(seconds.min(300))
}

/// Hub #3873: settle `members` after a run whose only failure was an unreadable
/// remote `rolling` ref. Every member returns to `queued` at its own position
/// (no bisect, no refusal) after a bounded backoff; one that already spent
/// [`QUEUE_REMOTE_READ_REQUEUE_LIMIT`] requeues fails typed
/// `remote_read_unavailable`, so a permanent outage surfaces instead of looping.
async fn settle_remote_read_unavailable(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    batch_id: Uuid,
    members: &[RollingQueueEntryV1],
    result: &RunResult,
    settled: &mut Vec<(Uuid, RunResult)>,
) -> Result<()> {
    let mut requeue = Vec::new();
    let mut exhausted = Vec::new();
    let mut spent = 0u32;
    for entry in members {
        let id = entry.id;
        let prior = with_store(store, move |s| s.rolling_queue_remote_read_requeues(id)).await?;
        if prior >= QUEUE_REMOTE_READ_REQUEUE_LIMIT {
            exhausted.push((id, prior));
            continue;
        }
        spent = spent.max(prior + 1);
        requeue.push(id);
    }
    bisect_event(
        store,
        batch_id,
        "remote_read_unavailable",
        None,
        serde_json::json!({
            "requeued": requeue.len(),
            "failed": exhausted.len(),
            "detail": result.outcome.detail,
        }),
    )
    .await?;
    if !requeue.is_empty() {
        let detail = serde_json::json!({
            "reason": QUEUE_REMOTE_READ_UNAVAILABLE,
            "limit": QUEUE_REMOTE_READ_REQUEUE_LIMIT,
            "detail": result.outcome.detail,
        });
        let returned = with_store(store, move |s| {
            s.requeue_remote_read_rolling_queue_entries(batch_id, &requeue, &detail, Utc::now())
        })
        .await?;
        settled.extend(returned.into_iter().map(|id| (id, result.clone())));
    }
    if !exhausted.is_empty() {
        let group: Vec<(Uuid, RunResult)> = exhausted
            .into_iter()
            .map(|(id, prior)| {
                let mut failed = result.clone();
                failed.state = RollingQueueEntryState::Failed;
                failed.outcome.detail = Some(format!(
                    "cause=remote_read_unavailable: the remote rolling ref was unreadable on {} \
                     consecutive gates (limit {QUEUE_REMOTE_READ_REQUEUE_LIMIT}); the host cannot \
                     reach the remote. Re-enqueue once it can.\n{}",
                    prior + 1,
                    result.outcome.detail.as_deref().unwrap_or_default()
                ));
                (id, failed)
            })
            .collect();
        settle_group(store, &group).await?;
        settled.extend(group);
    }
    if spent > 0 {
        tokio::time::sleep(remote_read_backoff(spent)).await;
    }
    Ok(())
}

/// Settle every member that already reached `origin/rolling` outside the
/// queue as `published` with the tip that contains it, so a landed commit is
/// never gated, re-gated or refused (#1208). Returns the members still to
/// gate, in queue order, and the tip they were checked against when any member
/// had landed.
async fn settle_already_landed(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    repo: &Path,
    batch_id: Uuid,
    members: Vec<RollingQueueEntryV1>,
    settled: &mut Vec<(Uuid, RunResult)>,
) -> Result<(Vec<RollingQueueEntryV1>, Option<String>)> {
    if members.is_empty() {
        return Ok((members, None));
    }
    let probe_repo = repo.to_path_buf();
    let sources: Vec<String> = members.iter().map(|m| m.source_commit.clone()).collect();
    let tips = tokio::task::spawn_blocking(move || landed_tips(&probe_repo, &sources))
        .await
        .map_err(|error| DaemonError::Process(format!("landed probe: {error}")))?;
    let mut kept = Vec::new();
    let mut landed = Vec::new();
    for (entry, tip) in members.into_iter().zip(tips) {
        match tip {
            Some(tip) => landed.push((entry, tip)),
            None => kept.push(entry),
        }
    }
    let observed = landed.first().map(|(_, tip)| tip.clone());
    let mut group = Vec::with_capacity(landed.len());
    for (entry, tip) in landed {
        let id = entry.id;
        bisect_event(
            store,
            batch_id,
            "already_landed",
            Some(id),
            serde_json::json!({ "source": entry.source_commit, "tip": tip }),
        )
        .await?;
        if entry.migration_version.is_some() {
            // The number it actually landed with, when a provisional commit
            // renumbered it; otherwise it kept its own.
            let (probe_repo, source) = (repo.to_path_buf(), entry.source_commit.clone());
            let published = tokio::task::spawn_blocking(move || {
                published_migration_number(&probe_repo, &source)
            })
            .await
            .map_err(|error| DaemonError::Process(format!("landed migration probe: {error}")))?;
            if let Some(version) = published {
                with_store(store, move |s| {
                    s.record_rolling_queue_assigned_migration(id, version)
                })
                .await?;
            }
        }
        // #4009/#4040: attribute the landing to the queue from evidence only it
        // writes (the lander's committer identity, the entry's restart events),
        // and call it an outside landing only when there is none.
        let (probe_repo, source) = (repo.to_path_buf(), entry.source_commit.clone());
        let lander_commit =
            tokio::task::spawn_blocking(move || lander_publication_commit(&probe_repo, &source))
                .await
                .map_err(|error| DaemonError::Process(format!("lander commit probe: {error}")))?;
        let restarts = with_store(store, move |s| s.rolling_queue_restart_requeues(id)).await?;
        let (landed_sha, detail) =
            already_landed_outcome(&entry.source_commit, tip, lander_commit, restarts);
        group.push((
            id,
            RunResult {
                state: RollingQueueEntryState::Published,
                outcome: RollingQueueOutcome {
                    landed_sha: Some(landed_sha),
                    detail: Some(detail),
                    ..RollingQueueOutcome::default()
                },
            },
        ));
    }
    settle_group(store, &group).await?;
    settled.extend(group);
    Ok((kept, observed))
}

/// Refuse the members of a batch that came from several checkouts whose commit
/// the queue checkout cannot read, returning the readable rest in queue order.
/// Members of one repository share its object store, so a commit missing here is
/// missing everywhere (its sandbox was reclaimed and the commit pruned): there
/// is nothing to fetch, and the member is settled `queue_source_unreachable`
/// with the checkout it was queued from instead of failing the whole batch's
/// lander run (#1945). A batch from one checkout is left to the lander.
async fn settle_unreachable_sources(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    repo: &Path,
    batch_id: Uuid,
    members: Vec<RollingQueueEntryV1>,
    checkouts: &HashMap<Uuid, String>,
    settled: &mut Vec<(Uuid, RunResult)>,
) -> Result<Vec<RollingQueueEntryV1>> {
    let mut distinct: Vec<&String> = checkouts.values().collect();
    distinct.sort();
    distinct.dedup();
    if distinct.len() < 2 {
        return Ok(members);
    }
    let probe_repo = repo.to_path_buf();
    let sources: Vec<String> = members.iter().map(|m| m.source_commit.clone()).collect();
    let present = tokio::task::spawn_blocking(move || {
        sources
            .iter()
            .map(|source| source_commit_exists(&probe_repo, source))
            .collect::<Vec<bool>>()
    })
    .await
    .map_err(|error| DaemonError::Process(format!("source reachability probe: {error}")))?;
    let mut kept = Vec::new();
    let mut refused = Vec::new();
    for (entry, present) in members.into_iter().zip(present) {
        if present {
            kept.push(entry);
            continue;
        }
        let checkout = checkouts.get(&entry.id).cloned().unwrap_or_default();
        bisect_event(
            store,
            batch_id,
            "source_unreachable",
            Some(entry.id),
            serde_json::json!({ "source": entry.source_commit, "checkout": checkout }),
        )
        .await?;
        refused.push((
            entry.id,
            RunResult {
                state: RollingQueueEntryState::Refused,
                outcome: RollingQueueOutcome {
                    refusal: Some(QUEUE_SOURCE_UNREACHABLE.to_string()),
                    detail: Some(format!(
                        "{} (queued from {checkout}) is not in the repository's object \
                         store; its sandbox was probably reclaimed: re-enqueue from a \
                         live checkout",
                        entry.source_commit
                    )),
                    ..RollingQueueOutcome::default()
                },
            },
        ));
    }
    settle_group(store, &refused).await?;
    settled.extend(refused);
    Ok(kept)
}

/// What a batch's earlier runs proved when a later run times out (#3267).
#[derive(Default)]
struct TimeoutEvidence {
    /// Members a red run left out while the red persisted: not the cause of
    /// it, so a timeout returns them to the queue instead of refusing them.
    cleared: Vec<Uuid>,
    /// Logs of every lander run of the batch so far, oldest first.
    logs: Vec<String>,
    /// Bisect rounds run before the timeout.
    rounds: usize,
}

/// The most run logs an outcome detail names; the batch events keep them all.
const OUTCOME_LOG_LIMIT: usize = 8;

/// Add a `bisect_logs=` line naming the batch's lander run logs to the result,
/// so a member settled from a bisect points at the evidence (#3267).
fn with_bisect_logs(mut result: RunResult, logs: &[String]) -> RunResult {
    if logs.is_empty() {
        return result;
    }
    let from = logs.len().saturating_sub(OUTCOME_LOG_LIMIT);
    let line = format!("bisect_logs={}", logs[from..].join(","));
    result.outcome.detail = Some(match result.outcome.detail.take() {
        Some(detail) => format!("{detail}\n{line}"),
        None => line,
    });
    result
}

/// Settle `members` after a lander run ran out of its gate wall-time budget: a
/// member that landed meanwhile is published; a member an earlier red run left
/// out (`evidence.cleared`) goes back to `queued` at its original position
/// (#3267); every other one, still a suspect, is refused with
/// `queue_gate_timeout` (#1208).
async fn settle_timed_out(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    repo: &Path,
    batch_id: Uuid,
    members: Vec<RollingQueueEntryV1>,
    evidence: &TimeoutEvidence,
    result: &RunResult,
    settled: &mut Vec<(Uuid, RunResult)>,
) -> Result<()> {
    let (rest, _) = settle_already_landed(store, repo, batch_id, members, settled).await?;
    let (spared, suspects): (Vec<_>, Vec<_>) = rest
        .into_iter()
        .partition(|entry| evidence.cleared.contains(&entry.id));
    let mut requeued = Vec::new();
    if !spared.is_empty() {
        let ids: Vec<Uuid> = spared.iter().map(|entry| entry.id).collect();
        let detail = serde_json::json!({
            "reason": QUEUE_GATE_TIMEOUT,
            "bisect_rounds": evidence.rounds,
            "logs": evidence.logs,
        });
        requeued = with_store(store, move |s| {
            s.requeue_bisected_rolling_queue_entries(batch_id, &ids, &detail, Utc::now())
        })
        .await?;
    }
    bisect_event(
        store,
        batch_id,
        "gate_timeout",
        None,
        serde_json::json!({
            "refused": suspects.len(),
            "requeued": requeued.len(),
            "detail": result.outcome.detail,
            "logs": evidence.logs,
        }),
    )
    .await?;
    let mut refusal = result.clone();
    if !requeued.is_empty() {
        let line = format!(
            "bisect_requeued={} (left out of a red run, returned to the queue)",
            requeued.len()
        );
        refusal.outcome.detail = Some(match refusal.outcome.detail.take() {
            Some(detail) => format!("{detail}\n{line}"),
            None => line,
        });
    }
    let refusal = with_bisect_logs(refusal, &evidence.logs);
    let group: Vec<(Uuid, RunResult)> = suspects
        .iter()
        .map(|entry| (entry.id, refusal.clone()))
        .collect();
    settle_group(store, &group).await?;
    settled.extend(group);
    Ok(())
}

/// Remove and return the member a conflicting multi-member run names, so only
/// that entry is refused and the rest go on. `None` when the run is not a
/// conflict, the lander named no member of `members`, or the group is a single
/// entry (then the ordinary settlement refuses it).
fn take_conflict_culprit(
    members: &mut Vec<RollingQueueEntryV1>,
    run: &GroupRun,
) -> Option<RollingQueueEntryV1> {
    if members.len() < 2
        || run.result.outcome.refusal.as_deref() != Some(QUEUE_BATCH_MERGE_CONFLICT)
    {
        return None;
    }
    let source = run.conflict_source.as_deref()?;
    let index = members
        .iter()
        .position(|entry| entry.source_commit == source)?;
    Some(members.remove(index))
}

/// Batch members share one candidate, so a lander policy refusal is reported
/// as the batch code with the fence kept in the detail.
fn as_batch_result(mut result: RunResult) -> RunResult {
    if let Some(fence) = result
        .outcome
        .refusal
        .as_deref()
        .and_then(|code| code.strip_prefix("policy_refused:"))
    {
        let detail = result.outcome.detail.take().unwrap_or_default();
        result.outcome.detail = Some(tail(&format!("policy_fence={fence}\n{detail}")));
        result.outcome.refusal = Some(QUEUE_BATCH_POLICY_REFUSED.to_string());
    }
    result
}

/// Whether a non-green gate is a red gate: only that is attributable to one
/// member by bisecting. A policy refusal is a property of the batch as a whole
/// and settles every member with its typed refusal; a merge conflict is
/// attributed by the lander (`take_conflict_culprit`), not by bisecting.
fn is_bisectable(result: &RunResult) -> bool {
    let release_prefix = format!("{QUEUE_RELEASE_BUILD_FAILED}:");
    result.state == RollingQueueEntryState::Refused
        && result.outcome.refusal.as_deref().is_some_and(|code| {
            matches!(
                code,
                "gate_failed" | QUEUE_FILTER_MATCHES_NO_TESTS | QUEUE_CLAIMED_FIX_STILL_RED
            ) || code.starts_with(&release_prefix)
        })
}

/// The messages `git diff --check` prints after `<path>:<line>: ` (the lander's
/// static guard, #3396).
const DIFF_CHECK_MESSAGES: [&str; 6] = [
    "trailing whitespace",
    "new blank line at EOF",
    "space before tab in indent",
    "indent with spaces",
    "tab in indent",
    "leftover conflict marker",
];

/// The file a `git diff --check` finding line names (`path:line: trailing
/// whitespace.`), also when the lander's own prefix precedes the first finding
/// (`... running git [...]: path:line: trailing whitespace.`).
fn diff_check_path(line: &str) -> Option<&str> {
    let line = line.trim();
    let located = DIFF_CHECK_MESSAGES.iter().find_map(|message| {
        let marker = format!(": {message}");
        let at = line.rfind(&marker)?;
        let rest = &line[at + marker.len()..];
        matches!(rest, "" | ".").then_some(&line[..at])
    })?;
    let (path, number) = located.rsplit_once(':')?;
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let path = path.rsplit_once("]: ").map_or(path, |(_, path)| path);
    let path = path.strip_prefix("./").unwrap_or(path).trim();
    (!path.is_empty()).then_some(path)
}

/// Source files a red gate's output points at: rustc `--> path:line:col`
/// locations, `panicked at path:line:col` sites and `git diff --check`
/// findings (a static guard that names the offending file, #3396).
fn failure_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in text.lines() {
        if let Some(path) = diff_check_path(line) {
            paths.push(path.to_string());
            continue;
        }
        let line = line.trim();
        let located = line
            .strip_prefix("--> ")
            .or_else(|| line.split_once("panicked at ").map(|(_, rest)| rest));
        let Some(rest) = located else { continue };
        let path = rest.split(':').next().unwrap_or("").trim();
        let path = path.strip_prefix("./").unwrap_or(path);
        if !path.is_empty() {
            paths.push(path.to_string());
        }
    }
    paths
}

/// Files `source` changes relative to the tip it would land on.
fn member_changed_files(repo: &Path, base: &str, source: &str) -> HashSet<String> {
    git(
        repo,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            &format!("{base}...{source}"),
        ],
    )
    .map(|out| out.lines().map(str::to_string).collect())
    .unwrap_or_default()
}

/// The one member a red gate is attributable to, by index into `members`
/// (#1974): the failure names a file (a compile error or panic location) or a
/// test module (a failing test's path component matches a changed `.rs` file's
/// stem) that exactly one member changes. `None` when the output names nothing a
/// member changes, or names files more than one member changes: only bisecting
/// can tell those apart.
fn attributable_member(
    files: &[HashSet<String>],
    text: &str,
    failing_tests: &[String],
) -> Option<usize> {
    let named = failure_paths(text);
    let modules: HashSet<&str> = failing_tests
        .iter()
        .flat_map(|name| name.split("::"))
        .filter(|part| !matches!(*part, "tests" | "test" | "mod" | "lib" | "main"))
        .collect();
    let hits = |changed: &HashSet<String>| {
        changed.iter().any(|path| {
            named.contains(path)
                || (path.ends_with(".rs")
                    && Path::new(path)
                        .file_stem()
                        .and_then(|stem| stem.to_str())
                        .is_some_and(|stem| {
                            !matches!(stem, "tests" | "test" | "mod" | "lib" | "main")
                                && modules.contains(stem)
                        }))
        })
    };
    let mut culprits = files
        .iter()
        .enumerate()
        .filter(|(_, changed)| hits(changed))
        .map(|(index, _)| index);
    let first = culprits.next()?;
    culprits.next().is_none().then_some(first)
}

/// Tag a result's `gate_timing=` line with the bisect rounds run so far and why
/// this member ended as it did (`outcome_reason`), adding the line when the
/// lander printed none (#1974).
fn note_bisect_outcome(result: &RunResult, rounds: usize, reason: &str) -> RunResult {
    let mut tagged = result.clone();
    let fields = format!("bisect_rounds={rounds} outcome_reason={reason}");
    let detail = tagged.outcome.detail.take().unwrap_or_default();
    let mut found = false;
    let mut lines: Vec<String> = detail
        .lines()
        .map(|line| {
            if line.starts_with("gate_timing=") && !found {
                found = true;
                format!("{line} {fields}")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !found {
        lines.insert(0, format!("gate_timing={fields}"));
    }
    tagged.outcome.detail = Some(lines.join("\n"));
    tagged
}

async fn settle(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    id: Uuid,
    result: &RunResult,
) -> Result<()> {
    if result.state == RollingQueueEntryState::Queued {
        with_store(store, move |s| {
            s.requeue_gating_rolling_queue_entry(id, Utc::now())
        })
        .await?;
        return Ok(());
    }
    let (state, mut outcome) = (result.state, result.outcome.clone());
    attach_published_commits(store, id, state, &mut outcome).await;
    with_store(store, move |s| {
        s.settle_rolling_queue_entry(id, state, &outcome, Utc::now())
    })
    .await?;
    Ok(())
}

/// Settle `members` with their results in one transaction: each entry keeps
/// its own outcome row, and each resolved owner gets ONE wake naming every
/// entry settled here (#1146). Separate calls still wake separately.
async fn settle_group(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    members: &[(Uuid, RunResult)],
) -> Result<()> {
    // An interrupted lander is re-driven under the same entry id and FIFO
    // sequence, without a terminal wake or a topology repair attempt.
    for (id, result) in members {
        if result.state == RollingQueueEntryState::Queued {
            settle(store, *id, result).await?;
        }
    }
    let mut items: Vec<(Uuid, RollingQueueEntryState, RollingQueueOutcome)> = members
        .iter()
        .filter(|(_, result)| result.state != RollingQueueEntryState::Queued)
        .map(|(id, result)| (*id, result.state, result.outcome.clone()))
        .collect();
    for (id, state, outcome) in &mut items {
        attach_published_commits(store, *id, *state, outcome).await;
    }
    with_store(store, move |s| {
        s.settle_rolling_queue_entries(&items, Utc::now())
    })
    .await?;
    Ok(())
}

/// Most commits read from one publication (#3369).
const PUBLISHED_COMMITS_MAX: usize = 128;
/// Longest subject kept per published commit, in bytes.
const PUBLISHED_SUBJECT_MAX: usize = 240;
/// First-parent merges walked from the landed tip to find the one that took a
/// source (a batch lands its members as a short chain of merges).
const PUBLISHED_MERGE_WALK: usize = 64;

/// #3369: before a `published` entry settles, read the subjects of the commits
/// it newly put on rolling so the store can tell the manager about an Open
/// Issue a subject names. Best effort and bounded: any git or store failure
/// leaves the outcome without commits and never blocks the settlement.
async fn attach_published_commits(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    id: Uuid,
    state: RollingQueueEntryState,
    outcome: &mut RollingQueueOutcome,
) {
    if state != RollingQueueEntryState::Published {
        return;
    }
    let Some(landed) = outcome.landed_sha.clone() else {
        return;
    };
    let Ok(Some((repo, source))) =
        with_store(store, move |s| s.rolling_queue_entry_repo_and_source(id)).await
    else {
        return;
    };
    let repo = PathBuf::from(repo);
    outcome.published_commits =
        tokio::task::spawn_blocking(move || published_commits(&repo, &source, &landed))
            .await
            .unwrap_or_default();
}

/// The commits `source` newly put on rolling in the publication that left it at
/// `landed`, newest first (at most [`PUBLISHED_COMMITS_MAX`]): the commits the
/// merge of `source` brought in over that merge's first parent. When no such
/// merge is found (a fast-forward, or a source that landed outside the queue)
/// only `source` itself is read. Empty when git cannot say.
fn published_commits(repo: &Path, source: &str, landed: &str) -> Vec<PublishedCommit> {
    const FORMAT: &str = "--format=%H%x1f%s";
    if git(repo, &["cat-file", "-e", &format!("{landed}^{{commit}}")]).is_none() {
        // The lander pushed from elsewhere: one fetch brings the tip in.
        let _ = fetch_tip(repo);
    }
    let base = if landed == source {
        None
    } else {
        git(
            repo,
            &[
                "log",
                "--first-parent",
                "-n",
                &PUBLISHED_MERGE_WALK.to_string(),
                "--format=%H %P",
                landed,
            ],
        )
        .and_then(|walk| {
            walk.lines().find_map(|line| {
                let mut shas = line.split_whitespace().skip(1);
                let first = shas.next()?;
                shas.any(|parent| parent == source)
                    .then(|| first.to_owned())
            })
        })
    };
    let max = PUBLISHED_COMMITS_MAX.to_string();
    let listing = match &base {
        Some(base) => git(
            repo,
            &["log", "-n", &max, FORMAT, source, "--not", base, "--"],
        ),
        None => git(repo, &["log", "-n", "1", FORMAT, source, "--"]),
    };
    listing
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            let (sha, subject) = line.split_once('\u{1f}')?;
            Some(PublishedCommit {
                sha: sha.to_owned(),
                subject: subject
                    .char_indices()
                    .take_while(|(at, c)| at + c.len_utf8() <= PUBLISHED_SUBJECT_MAX)
                    .map(|(_, c)| c)
                    .collect(),
            })
        })
        .collect()
}

/// What one `git merge-base --is-ancestor` probe established.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Ancestry {
    Ancestor,
    /// git ran to completion and answered "no" (exit 1): a definite negative.
    NotAncestor,
    /// git was killed (daemon shutdown SIGKILL, #2595), could not start or
    /// failed with an error: no verdict at all, never a negative one.
    Unknown,
}

/// Classify the exit of an `--is-ancestor` probe: only exit 0 (yes) and exit 1
/// (no) are verdicts; a signal death or any other status says nothing.
fn classify_ancestry(status: std::io::Result<std::process::ExitStatus>) -> Ancestry {
    match status.map(|status| status.code()) {
        Ok(Some(0)) => Ancestry::Ancestor,
        Ok(Some(1)) => Ancestry::NotAncestor,
        _ => Ancestry::Unknown,
    }
}

fn probe_ancestry(repo: &Path, source: &str) -> Ancestry {
    classify_ancestry(
        git_command()
            .arg("-C")
            .arg(repo)
            .args(["merge-base", "--is-ancestor", source, "origin/rolling"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
}

/// Attempts at an ancestry verdict, each after a fresh fetch: a source that is
/// not (yet) seen as an ancestor is re-checked against the re-fetched remote
/// tip before the queue fails it (#2595).
const ANCESTRY_ATTEMPTS: usize = 2;

/// Ancestry of each source in the published tip: after one fetch, a source is
/// verified only when it is an ancestor of `origin/rolling`. Migration
/// evidence in the lander's report never substitutes for the probe: the
/// provisional commit the lander writes keeps the original source as a parent.
/// A source that is not an ancestor is re-checked after a re-fetch; a probe
/// that never produced a verdict stays [`Ancestry::Unknown`].
fn verify_ancestry(repo: &Path, sources: &[String]) -> Vec<Ancestry> {
    verify_ancestry_with(
        sources,
        ANCESTRY_ATTEMPTS,
        |source| probe_ancestry(repo, source),
        || fetch_rolling(repo).is_ok(),
    )
}

/// `refetch` reports whether the remote ref was refreshed. When it was not,
/// `origin/rolling` may be stale: a positive answer still holds (history only
/// grows), but a "no" proves nothing about the real tip and is recorded as
/// [`Ancestry::Unknown`], never a verified negative (#2599). A definite
/// negative an earlier, successfully refreshed attempt produced is kept.
fn verify_ancestry_with(
    sources: &[String],
    attempts: usize,
    probe: impl Fn(&str) -> Ancestry,
    refetch: impl Fn() -> bool,
) -> Vec<Ancestry> {
    let mut verdicts = vec![Ancestry::Unknown; sources.len()];
    for _ in 0..attempts.max(1) {
        let fresh = refetch();
        for (verdict, source) in verdicts.iter_mut().zip(sources) {
            if *verdict == Ancestry::Ancestor {
                continue;
            }
            let probed = probe(source);
            if fresh || probed == Ancestry::Ancestor {
                *verdict = probed;
            }
        }
        if verdicts.iter().all(|v| *v == Ancestry::Ancestor) {
            break;
        }
    }
    verdicts
}

/// `(source, final_version)` per provisional migration the lander renumbered.
fn migration_evidence(stdout: &str) -> Vec<(String, u32)> {
    let mut found = Vec::new();
    for index in 0.. {
        let (Some(source), Some(version)) = (
            line_value(stdout, &format!("migration_{index}_source_id")),
            line_value(stdout, &format!("migration_{index}_final_version")),
        ) else {
            break;
        };
        let Ok(version) = version.trim().parse() else {
            break;
        };
        found.push((source.to_string(), version));
    }
    found
}

/// Refuse the members whose migration number is out of order, returning the
/// rest (still `gating`) in queue order. Best effort: without a readable tip
/// the lander's own tip + 1 rule decides.
async fn order_migrations(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    repo: &Path,
    members: Vec<RollingQueueEntryV1>,
    settled: &mut Vec<(Uuid, RunResult)>,
) -> Result<Vec<RollingQueueEntryV1>> {
    if members
        .iter()
        .all(|entry| entry.migration_version.is_none())
    {
        return Ok(members);
    }
    let probe_repo = repo.to_path_buf();
    let probe_members = members.clone();
    let plan = tokio::task::spawn_blocking(move || {
        let _ = fetch_rolling(&probe_repo);
        let tip = schema_version_at(&probe_repo, "origin/rolling")?;
        let facts: Vec<MigrationMember> = probe_members
            .iter()
            .map(|entry| MigrationMember {
                id: entry.id,
                derived: entry.migration_version,
                declared: entry.migration_version.is_some()
                    && source_declares_provisional(&probe_repo, &entry.source_commit),
            })
            .collect();
        Some(plan_migration_order(tip, &facts))
    })
    .await
    .map_err(|error| DaemonError::Process(format!("migration order probe: {error}")))?;
    let Some(plan) = plan else {
        return Ok(members);
    };
    for (id, version) in plan.assigned {
        with_store(store, move |s| {
            s.record_rolling_queue_assigned_migration(id, version)
        })
        .await?;
    }
    let mut kept = Vec::new();
    let mut refused = Vec::new();
    for entry in members {
        if plan.out_of_order.contains(&entry.id) {
            let result = RunResult {
                state: RollingQueueEntryState::Refused,
                outcome: RollingQueueOutcome {
                    refusal: Some(QUEUE_MIGRATION_OUT_OF_ORDER.to_string()),
                    detail: Some(format!(
                        "migration {} is already taken by an earlier queued source; \
                         re-enqueue after renumbering (declare a provisional migration)",
                        entry.migration_version.unwrap_or_default()
                    )),
                    ..RollingQueueOutcome::default()
                },
            };
            refused.push((entry.id, result));
        } else {
            kept.push(entry);
        }
    }
    settle_group(store, &refused).await?;
    settled.extend(refused);
    Ok(kept)
}

/// Claim the FIFO head, run it and settle it. `Ok(None)` when nothing was
/// runnable (empty queue or another entry is gating).
#[cfg(test)]
pub(crate) async fn run_next(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    launcher: &LanderLauncher,
) -> Result<Option<(Uuid, RunResult)>> {
    Ok(run_next_batch(store, launcher, 1).await?.into_iter().next())
}

/// Claim up to `batch_size` ready sources in FIFO order, gate them once as one
/// candidate, publish with one fast-forward and settle every member exactly
/// once. Empty when nothing was runnable.
#[cfg(test)]
pub(crate) async fn run_next_batch(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    launcher: &LanderLauncher,
    batch_size: usize,
) -> Result<Vec<(Uuid, RunResult)>> {
    let size = batch_size.clamp(1, ROLLING_QUEUE_MAX_BATCH_SIZE as usize);
    let Some(claimed) = claim_batch(store, size).await? else {
        return Ok(Vec::new());
    };
    run_claimed_batch(store, launcher, claimed).await
}

/// Gate, publish and settle a batch the store already claimed.
pub(crate) async fn run_claimed_batch(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    launcher: &LanderLauncher,
    claimed: ClaimedQueueBatch,
) -> Result<Vec<(Uuid, RunResult)>> {
    let batch_id = claimed.batch_id;
    let budget = GateBudget::new(launcher.gate_timeout, claimed.entries.len());
    let mut settled = Vec::new();
    let mut members = claimed.entries;
    let checkouts: HashMap<Uuid, String> = members
        .iter()
        .map(|entry| entry.id)
        .zip(claimed.repo_paths.iter().cloned())
        .collect();
    let repo = match queue_repo_for_batch(launcher, &claimed.repo_paths).await {
        Ok(repo) => repo,
        Err(message) => {
            let result = RunResult {
                state: RollingQueueEntryState::Failed,
                outcome: RollingQueueOutcome {
                    refusal: Some(workspace_refusal_code(&message).into()),
                    detail: Some(format!("queue workspace unavailable: {message}")),
                    ..RollingQueueOutcome::default()
                },
            };
            let group: Vec<(Uuid, RunResult)> = members
                .iter()
                .map(|entry| (entry.id, result.clone()))
                .collect();
            settle_group(store, &group).await?;
            settled.extend(group);
            return Ok(settled);
        }
    };
    if members.len() > 1 {
        let mut distinct: Vec<&String> = checkouts.values().collect();
        distinct.sort();
        distinct.dedup();
        bisect_event(
            store,
            batch_id,
            "batch_formed",
            None,
            serde_json::json!({
                "members": members.len(),
                "checkouts": distinct.len(),
                "queue_checkout": repo.display().to_string(),
            }),
        )
        .await?;
    }
    members = settle_unreachable_sources(store, &repo, batch_id, members, &checkouts, &mut settled)
        .await?;
    // A member already on rolling (landed outside the queue) is published
    // as is, never gated (#1208).
    (members, _) = settle_already_landed(store, &repo, batch_id, members, &mut settled).await?;
    if members.len() > 1 {
        members = order_migrations(store, &repo, members, &mut settled).await?;
    }
    if members.is_empty() {
        return Ok(settled);
    }
    let mut run = run_group(
        store,
        launcher,
        batch_id,
        &repo,
        &members,
        None,
        budget.run_deadline(),
    )
    .await;
    loop {
        if let Some(culprit) = take_conflict_culprit(&mut members, &run) {
            // An integration conflict belongs to one member: refuse that entry
            // with its conflict path and gate the rest as if it had never been
            // in the batch.
            bisect_event(
                store,
                batch_id,
                "conflict_isolated",
                Some(culprit.id),
                run.evidence(),
            )
            .await?;
            settle(store, culprit.id, &run.result).await?;
            settled.push((culprit.id, run.result.clone()));
        } else if run.already_integrated {
            // The lander found a member already on rolling: publish every
            // landed member and gate only the rest.
            let before = members.len();
            (members, _) =
                settle_already_landed(store, &repo, batch_id, members, &mut settled).await?;
            if members.is_empty() {
                return Ok(settled);
            }
            if members.len() == before {
                break;
            }
        } else {
            break;
        }
        run = run_group(
            store,
            launcher,
            batch_id,
            &repo,
            &members,
            None,
            budget.run_deadline(),
        )
        .await;
    }
    if is_gate_timeout(&run.result) {
        settle_timed_out(
            store,
            &repo,
            batch_id,
            members,
            &TimeoutEvidence::default(),
            &run.result,
            &mut settled,
        )
        .await?;
        return Ok(settled);
    }
    if is_remote_read_unavailable(&run.result) {
        // Hub #3873: an unreadable remote says nothing about the sources.
        settle_remote_read_unavailable(store, batch_id, &members, &run.result, &mut settled)
            .await?;
        return Ok(settled);
    }
    let in_batch = members.len() > 1;
    let result = if in_batch {
        as_batch_result(run.result.clone())
    } else {
        run.result.clone()
    };
    if result.state == RollingQueueEntryState::Published {
        publish_group(
            store,
            &repo,
            batch_id,
            &members,
            &run,
            &result,
            &mut settled,
        )
        .await?;
        return Ok(settled);
    }
    if in_batch && is_bisectable(&result) {
        let reason = result.outcome.refusal.clone().unwrap_or_default();
        let mut detail = run.evidence();
        detail["reason"] = serde_json::json!(reason);
        with_store(store, move |s| {
            s.begin_rolling_queue_bisect_with(batch_id, detail, Utc::now())
        })
        .await?;
        let first_base = match line_value(&run.stdout, "fetched_target_id") {
            Some(tip) => Some(tip.to_string()),
            None => probe_tip(&repo).await,
        };
        // A failure that names files only one member changes is that member's
        // alone: refuse it and gate the rest as one batch, no bisecting (#1974).
        let evidence_text = format!(
            "{}\n{}",
            result.outcome.detail.as_deref().unwrap_or_default(),
            run.stdout
        );
        let base_ref = first_base
            .clone()
            .unwrap_or_else(|| "origin/rolling".into());
        let (probe_repo, sources) = (
            repo.clone(),
            members
                .iter()
                .map(|m| m.source_commit.clone())
                .collect::<Vec<_>>(),
        );
        let files = tokio::task::spawn_blocking(move || {
            sources
                .iter()
                .map(|source| member_changed_files(&probe_repo, &base_ref, source))
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|error| DaemonError::Process(format!("member diff probe: {error}")))?;
        let mut known_red = true;
        let mut attributed = None;
        if let Some(index) =
            attributable_member(&files, &evidence_text, &result.outcome.failing_tests)
        {
            known_red = false;
            let culprit = members.remove(index);
            bisect_event(
                store,
                batch_id,
                "bisect_attributed",
                Some(culprit.id),
                run.evidence(),
            )
            .await?;
            if members.is_empty() {
                // A batch of one: its red is the member's own.
                let refused = note_bisect_outcome(&result, 0, "attributed_directly");
                settle(store, culprit.id, &refused).await?;
                settled.push((culprit.id, refused));
                return Ok(settled);
            }
            // Held back, not refused: the complement's gate is the evidence
            // that confirms or rejects the attribution.
            attributed = Some(culprit);
        }
        bisect_batch(
            store,
            launcher,
            &repo,
            batch_id,
            members,
            result,
            first_base,
            known_red,
            attributed,
            budget,
            run.log_path.clone(),
            &mut settled,
        )
        .await?;
        return Ok(settled);
    }
    let group: Vec<(Uuid, RunResult)> = members
        .iter()
        .map(|entry| (entry.id, result.clone()))
        .collect();
    settle_group(store, &group).await?;
    settled.extend(group);
    Ok(settled)
}

/// Settle a group whose lander run published: verify every source against the
/// published tip (a lander report is never trusted alone), record the final
/// migration numbers the lander assigned, and settle every member once with its
/// landed SHA or a typed `queue_batch_ancestry_unverified` failure.
async fn publish_group(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    repo: &Path,
    batch_id: Uuid,
    group: &[RollingQueueEntryV1],
    run: &GroupRun,
    result: &RunResult,
    settled: &mut Vec<(Uuid, RunResult)>,
) -> Result<()> {
    publish_group_with(
        store,
        repo,
        batch_id,
        group,
        run,
        result,
        settled,
        verify_ancestry,
    )
    .await
}

/// [`publish_group`] with the ancestry probe injected (tests).
///
/// A member whose probe produced no verdict ([`Ancestry::Unknown`]: git killed
/// by a daemon shutdown, #2595) is left `gating` and not settled: the publish
/// already happened, so restart reconcile re-checks it against the remote tip
/// and settles it `published`. Only a definite "not an ancestor" fails it.
#[allow(clippy::too_many_arguments)]
async fn publish_group_with(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    repo: &Path,
    batch_id: Uuid,
    group: &[RollingQueueEntryV1],
    run: &GroupRun,
    result: &RunResult,
    settled: &mut Vec<(Uuid, RunResult)>,
    verify: fn(&Path, &[String]) -> Vec<Ancestry>,
) -> Result<()> {
    let tip = result.outcome.landed_sha.clone();
    let sources: Vec<String> = group.iter().map(|m| m.source_commit.clone()).collect();
    let probe_repo = repo.to_path_buf();
    // For a content-equivalent no-op the source need not be an ancestor: the
    // lander checked its merged tree against this containing tip. Verify that
    // tip is on fresh rolling rather than rejecting the equivalent content.
    let already_on_rolling =
        line_value(&run.stdout, "landing_outcome") == Some("already_on_rolling");
    let probe_sources = if already_on_rolling {
        tip.iter().cloned().collect()
    } else {
        sources.clone()
    };
    let mut verified = tokio::task::spawn_blocking(move || verify(&probe_repo, &probe_sources))
        .await
        .map_err(|error| DaemonError::Process(format!("ancestry probe: {error}")))?;
    if already_on_rolling {
        verified = vec![verified.first().copied().unwrap_or(Ancestry::Unknown); group.len()];
    }
    let base = line_value(&run.stdout, "fetched_target_id").map(str::to_string);
    let candidate = line_value(&run.stdout, "candidate_id").map(str::to_string);
    let summary = serde_json::json!({
        "sources": sources,
        "published": tip,
        "verified": verified,
        "already_on_rolling": already_on_rolling,
    });
    with_store(store, move |s| {
        s.record_rolling_queue_batch_gate(batch_id, base.as_deref(), candidate.as_deref(), &summary)
    })
    .await?;
    if let Some(published) = tip.clone() {
        // The queue's own push: the next advance beyond it is external.
        bisect_event(
            store,
            batch_id,
            "expected_tip",
            None,
            serde_json::json!({
                "tip": published,
                "candidate": line_value(&run.stdout, "candidate_id"),
                "sources": group.iter().map(|m| m.source_commit.clone()).collect::<Vec<_>>(),
                "migrations": migration_evidence(&run.stdout)
                    .iter()
                    .map(|(source, version)| serde_json::json!({"source": source, "version": version}))
                    .collect::<Vec<_>>(),
            }),
        )
        .await?;
    }
    let versions = migration_evidence(&run.stdout);
    // #2698: a base red nothing owns reaches the manager as one qa Issue, and
    // the outcome (so the owner's wake) names it.
    let base_red_lines =
        gate_base_red::file_base_reds(store, batch_id, group, &run.stdout, run.log_path.as_deref())
            .await;
    let mut member_results = Vec::with_capacity(group.len());
    for (entry, verdict) in group.iter().zip(verified) {
        if verdict == Ancestry::Unknown {
            tracing::warn!(
                source = %entry.source_commit,
                entry = %entry.id,
                "ancestry probe produced no verdict; leaving the entry gating for restart reconcile"
            );
            continue;
        }
        let member_result = if verdict == Ancestry::Ancestor {
            if let Some((_, version)) = versions
                .iter()
                .find(|(source, _)| *source == entry.source_commit)
            {
                let (id, version) = (entry.id, *version);
                // The actual number is durable before it is applied, so a
                // restart never replaces it with the pre-run plan.
                bisect_event(
                    store,
                    batch_id,
                    "migration_receipt",
                    Some(id),
                    serde_json::json!({ "version": version, "source": entry.source_commit }),
                )
                .await?;
                with_store(store, move |s| {
                    s.record_rolling_queue_assigned_migration(id, version)
                })
                .await?;
            }
            let mut named = result.clone();
            named.outcome.detail =
                gate_base_red::with_issue_lines(named.outcome.detail.take(), &base_red_lines);
            named
        } else {
            RunResult {
                state: RollingQueueEntryState::Failed,
                outcome: RollingQueueOutcome {
                    refusal: Some(QUEUE_BATCH_ANCESTRY_UNVERIFIED.to_string()),
                    detail: Some(format!(
                        "{} is not contained in the published tip {}",
                        entry.source_commit,
                        tip.clone().unwrap_or_default()
                    )),
                    ..RollingQueueOutcome::default()
                },
            }
        };
        member_results.push((entry.id, member_result));
    }
    // #2112: land on compile plus the touched tests, run the full suite
    // asynchronously. Recorded before settlement so a crash keeps it.
    let derived = bounded_qa::derived_filter_members(group);
    let obliged: Vec<Uuid> = member_results
        .iter()
        .filter(|(id, r)| r.state == RollingQueueEntryState::Published && derived.contains(id))
        .map(|(id, _)| *id)
        .collect();
    if !obliged.is_empty() {
        with_store(store, move |s| {
            s.oblige_bounded_qa(batch_id, &obliged, "derived_filters", Utc::now())
        })
        .await?;
    }
    settle_group(store, &member_results).await?;
    settled.extend(member_results);
    Ok(())
}

/// `origin/rolling` after one fetch (`None` when git cannot say).
fn fetch_tip(repo: &Path) -> Option<String> {
    let _ = fetch_rolling(repo);
    read_origin_tip(repo)
}

/// The locally cached `origin/rolling` commit (no fetch).
fn read_origin_tip(repo: &Path) -> Option<String> {
    git(repo, &["rev-parse", "--verify", "origin/rolling^{commit}"])
        .map(|tip| tip.trim().to_string())
        .filter(|tip| !tip.is_empty())
}

async fn probe_tip(repo: &Path) -> Option<String> {
    let repo = repo.to_path_buf();
    tokio::task::spawn_blocking(move || fetch_tip(&repo))
        .await
        .ok()
        .flatten()
}

/// Durably record, before `group` is run, the migration numbers the lander
/// will give its members (tip + 1, ... in queue order). A crash after the
/// lander pushed but before the report was recorded is then repaired by the
/// restart reconcile from this evidence.
async fn record_migration_plan(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    repo: &Path,
    batch_id: Uuid,
    group: &[RollingQueueEntryV1],
) -> Result<()> {
    if group.iter().all(|entry| entry.migration_version.is_none()) {
        return Ok(());
    }
    let probe_repo = repo.to_path_buf();
    let members = group.to_vec();
    let planned = tokio::task::spawn_blocking(move || {
        let _ = fetch_rolling(&probe_repo);
        let tip = schema_version_at(&probe_repo, "origin/rolling")?;
        let facts: Vec<MigrationMember> = members
            .iter()
            .map(|entry| MigrationMember {
                id: entry.id,
                derived: entry.migration_version,
                declared: entry.migration_version.is_some()
                    && source_declares_provisional(&probe_repo, &entry.source_commit),
            })
            .collect();
        let plan = plan_migration_order(tip, &facts);
        Some(
            plan.assigned
                .iter()
                .filter_map(|(id, version)| {
                    let member = members.iter().find(|entry| entry.id == *id)?;
                    Some(serde_json::json!({
                        "id": id.to_string(),
                        "source": member.source_commit,
                        "version": version,
                    }))
                })
                .collect::<Vec<_>>(),
        )
    })
    .await
    .map_err(|error| DaemonError::Process(format!("migration plan probe: {error}")))?;
    let Some(assigned) = planned.filter(|list| !list.is_empty()) else {
        return Ok(());
    };
    bisect_event(
        store,
        batch_id,
        "migration_plan",
        None,
        serde_json::json!({ "assigned": assigned }),
    )
    .await
}

async fn bisect_event(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    batch_id: Uuid,
    kind: &'static str,
    entry: Option<Uuid>,
    detail: serde_json::Value,
) -> Result<()> {
    with_store(store, move |s| {
        s.record_rolling_queue_batch_event(batch_id, kind, entry, Some(detail), Utc::now())
    })
    .await
}

/// The tip a lander run gated against (its `fetched_target_id`), else the tip
/// probed just before it ran.
fn run_base(stdout: &str, probed: Option<&String>) -> Option<String> {
    line_value(stdout, "fetched_target_id")
        .map(str::to_string)
        .or_else(|| probed.cloned())
}

/// A red batch: attribute the failure to exactly one source per red, and
/// publish every innocent one.
///
/// `pending[..window]` is a prefix known to be red as a candidate on
/// `red_base` (`None`: unknown). Each step gates the first half of the
/// window against the tip it would publish onto: green publishes it (one
/// fast-forward) and the rest of the window stays red; red narrows the window
/// to that half. A window of one is the isolated bad source and settles with
/// the failing tests of the gate that condemned it. The tail after an
/// isolated source is gated whole against the new tip.
///
/// Red evidence is only as fresh as the base it was observed on: when the tip
/// the remaining window would publish onto is not that base (an external push
/// landed, or a prefix was gated on a different tip), the old red is stale and
/// the window is gated again instead of refused. `expected_tip` follows the
/// queue's own pushes; any other advance is external and is re-gated within the
/// batch's wall-time budget (#1940: the lander re-runs only what the advance
/// overlaps, and the `deadline` settles whatever the budget cannot finish with
/// `queue_gate_timeout`), however many external advances there are.
/// Every step shrinks the window or the pending list;
/// `queue_bisect_no_progress` settles the rest if that ever fails to hold.
async fn bisect_batch(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    launcher: &LanderLauncher,
    repo: &Path,
    batch_id: Uuid,
    members: Vec<RollingQueueEntryV1>,
    first_red: RunResult,
    first_base: Option<String>,
    known_red: bool,
    mut attributed: Option<RollingQueueEntryV1>,
    gate_budget: GateBudget,
    first_log: Option<PathBuf>,
    settled: &mut Vec<(Uuid, RunResult)>,
) -> Result<()> {
    let mut pending = members;
    // What the bisect has proved, kept for a timeout (#3267): members a red
    // run left out while the red persisted are not the cause of it and go
    // back to the queue instead of being refused; `logs` are the runs behind
    // that.
    let mut proved = TimeoutEvidence {
        logs: first_log
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        ..TimeoutEvidence::default()
    };
    // `known_red`: `pending` as a whole is the red candidate the first red
    // observed. With an `attributed` member held back it is not, and the
    // complement is gated whole first: green confirms the attribution (the
    // member is necessary for the failure), red rejects it and the halving
    // bisect runs over the complement with the member still in play (#1974).
    let mut complement_green = false;
    let mut attribution_rejected = false;
    let published_reason = |rejected: bool| {
        if rejected {
            "attribution_rejected"
        } else {
            "published_after_bisect"
        }
    };
    // The base the attributing red was observed on: the complement confirms
    // the attribution only when it gated on this very base (#2023).
    let attribution_base = first_base.clone();
    let mut window: Option<usize> = known_red.then_some(pending.len());
    let mut rounds = 0usize;
    let mut evidence = first_red;
    let mut red_base = first_base.clone();
    let mut expected_tip = first_base;
    let mut budget = (pending.len() + usize::from(attributed.is_some())) * 4 + 4;
    loop {
        if pending.is_empty() {
            match attributed.take() {
                Some(entry) if complement_green => {
                    // The member may have reached rolling while the complement
                    // gated: it is published as is, never refused (#2024).
                    let (rest, _) =
                        settle_already_landed(store, repo, batch_id, vec![entry], settled).await?;
                    let Some(entry) = rest.into_iter().next() else {
                        break;
                    };
                    bisect_event(
                        store,
                        batch_id,
                        "bisect_attribution_confirmed",
                        Some(entry.id),
                        serde_json::json!({ "bisect_rounds": rounds }),
                    )
                    .await?;
                    let refused = with_bisect_logs(
                        note_bisect_outcome(&evidence, rounds, "attributed_confirmed"),
                        &proved.logs,
                    );
                    settle(store, entry.id, &refused).await?;
                    settled.push((entry.id, refused));
                }
                Some(entry) => {
                    // The complement never produced a verdict (everything in
                    // it reached rolling elsewhere): gate the member itself.
                    pending.push(entry);
                    window = None;
                    red_base = None;
                    proved.cleared.clear();
                }
                None => {}
            }
            if pending.is_empty() {
                break;
            }
        }
        let no_progress = budget == 0 || window.is_some_and(|w| w == 0 || w > pending.len());
        if no_progress {
            pending.extend(attributed.take());
            let result = RunResult {
                state: RollingQueueEntryState::Refused,
                outcome: RollingQueueOutcome {
                    refusal: Some(QUEUE_BISECT_NO_PROGRESS.to_string()),
                    detail: Some(format!(
                        "bisect made no progress with {} source(s) pending",
                        pending.len()
                    )),
                    ..RollingQueueOutcome::default()
                },
            };
            bisect_event(
                store,
                batch_id,
                "bisect_no_progress",
                None,
                serde_json::json!({ "pending": pending.len() }),
            )
            .await?;
            let rest: Vec<(Uuid, RunResult)> = pending
                .drain(..)
                .map(|entry| (entry.id, result.clone()))
                .collect();
            settle_group(store, &rest).await?;
            settled.extend(rest);
            break;
        }
        budget -= 1;
        // A member that reached rolling outside the queue since the last step
        // is published as is (#1208). The advance that landed it is accounted
        // for, not charged as an out-of-band re-gate, and any red evidence that
        // was observed with it in the window is dropped.
        let before = pending.len();
        let observed;
        (pending, observed) =
            settle_already_landed(store, repo, batch_id, pending, settled).await?;
        if pending.len() != before {
            window = None;
            red_base = None;
            proved.cleared.clear();
            if observed.is_some() {
                expected_tip = observed;
            }
            if pending.is_empty() {
                continue;
            }
        }
        let current = probe_tip(repo).await;
        if let Some(current) = current.as_ref() {
            if expected_tip.as_ref().is_some_and(|tip| tip != current) {
                // Not the queue's own push: an external advance. The lander
                // re-runs what it overlaps, within the batch's remaining time.
                bisect_event(
                    store,
                    batch_id,
                    "regate_spent",
                    None,
                    serde_json::json!({ "expected": expected_tip, "observed": current }),
                )
                .await?;
            }
            expected_tip = Some(current.clone());
            if window.is_some() && red_base.as_ref() != Some(current) {
                bisect_event(
                    store,
                    batch_id,
                    "bisect_red_stale",
                    None,
                    serde_json::json!({ "red_base": red_base, "tip": current }),
                )
                .await?;
                window = None;
                red_base = None;
                proved.cleared.clear();
            }
        }
        if window == Some(1) {
            let entry = pending.remove(0);
            bisect_event(
                store,
                batch_id,
                "bisect_isolated",
                Some(entry.id),
                serde_json::json!({
                    "refusal": evidence.outcome.refusal,
                    "failing_tests": evidence.outcome.failing_tests,
                }),
            )
            .await?;
            let isolated = with_bisect_logs(
                note_bisect_outcome(
                    &evidence,
                    rounds,
                    if attribution_rejected {
                        "attribution_rejected"
                    } else {
                        "bisect_isolated"
                    },
                ),
                &proved.logs,
            );
            settle(store, entry.id, &isolated).await?;
            settled.push((entry.id, isolated));
            window = None;
            red_base = None;
            continue;
        }
        let take = window.map_or(pending.len(), |w| w.div_ceil(2));
        let group: Vec<RollingQueueEntryV1> = pending[..take].to_vec();
        record_migration_plan(store, repo, batch_id, &group).await?;
        rounds += 1;
        let run = run_group(
            store,
            launcher,
            batch_id,
            repo,
            &group,
            expected_tip.as_deref(),
            gate_budget.run_deadline(),
        )
        .await;
        proved
            .logs
            .extend(run.log_path.iter().map(|path| path.display().to_string()));
        proved.rounds = rounds;
        if is_gate_timeout(&run.result) {
            pending.extend(attributed.take());
            settle_timed_out(
                store,
                repo,
                batch_id,
                pending,
                &proved,
                &run.result,
                settled,
            )
            .await?;
            break;
        }
        if is_remote_read_unavailable(&run.result) {
            // Hub #3873: the whole remaining batch returns to the queue; the
            // unreadable remote proves nothing about any member.
            pending.extend(attributed.take());
            settle_remote_read_unavailable(store, batch_id, &pending, &run.result, settled).await?;
            break;
        }
        if run.already_integrated {
            // Publish the landed member(s) and gate the rest again: this run's
            // outcome says nothing about them.
            let before = pending.len();
            let observed;
            (pending, observed) =
                settle_already_landed(store, repo, batch_id, pending, settled).await?;
            if pending.len() != before {
                window = None;
                red_base = None;
                proved.cleared.clear();
                expected_tip = observed.or(expected_tip);
                continue;
            }
        }
        let mut result = as_batch_result(run.result.clone());
        let base = run_base(&run.stdout, expected_tip.as_ref());
        if result.state == RollingQueueEntryState::Published {
            result = note_bisect_outcome(&result, rounds, published_reason(attribution_rejected));
        }
        let outcome = if result.state == RollingQueueEntryState::Published {
            "green"
        } else if is_bisectable(&result) {
            "red"
        } else {
            "unattributable"
        };
        bisect_event(
            store,
            batch_id,
            "bisect_gate",
            None,
            serde_json::json!({
                "sources": group.iter().map(|e| e.source_commit.clone()).collect::<Vec<_>>(),
                "window": window,
                "bisect_rounds": rounds,
                "outcome": outcome,
                "base": base,
                "cause": run.cause,
                "log": run.log_path.as_ref().map(|path| path.display().to_string()),
            }),
        )
        .await?;
        match outcome {
            "green" => {
                publish_group(store, repo, batch_id, &group, &run, &result, settled).await?;
                pending.drain(..take);
                // The red evidence carries over only when the prefix was gated
                // on the very base the red was observed on.
                // The queue's own push is the exact candidate the lander
                // reported; the observed remote head can be a later external
                // descendant that may have fixed (or changed) the red.
                let landed = result.outcome.landed_sha.clone();
                let exact = match (run.own_tip.as_ref(), landed.as_ref()) {
                    (Some(own), Some(landed)) => own == landed,
                    _ => false,
                };
                let carried = exact && window.is_some() && red_base.is_some() && red_base == base;
                if !exact {
                    proved.cleared.clear();
                    bisect_event(
                        store,
                        batch_id,
                        "bisect_external_descendant",
                        None,
                        serde_json::json!({ "own": run.own_tip, "observed": landed }),
                    )
                    .await?;
                }
                if attributed.is_some() {
                    if exact && attribution_base.is_some() && attribution_base == base {
                        complement_green = true;
                    } else {
                        // The complement gated on another base (an external
                        // advance) or published an external descendant: the
                        // attributing red is stale evidence. The member goes
                        // back to pending and is gated on the current tip.
                        bisect_event(
                            store,
                            batch_id,
                            "bisect_attribution_stale",
                            attributed.as_ref().map(|entry| entry.id),
                            serde_json::json!({
                                "red_base": attribution_base,
                                "complement_base": base,
                            }),
                        )
                        .await?;
                        pending.extend(attributed.take());
                        complement_green = false;
                    }
                }
                window = if carried {
                    window.map(|w| w - take)
                } else {
                    None
                };
                red_base = if carried {
                    result.outcome.landed_sha.clone()
                } else {
                    None
                };
                // Anything past the exact candidate is external: expecting the
                // candidate makes the next probe charge the re-gate. Without a
                // reported candidate the head is adopted (nothing to charge).
                expected_tip = run.own_tip.clone().or(landed).or(expected_tip);
            }
            "red" => {
                if let Some(entry) = attributed.take() {
                    // The complement is red without the attributed member:
                    // the attribution was wrong. The member stays in play,
                    // after the complement the red was observed on.
                    bisect_event(
                        store,
                        batch_id,
                        "bisect_attribution_rejected",
                        Some(entry.id),
                        serde_json::json!({ "bisect_rounds": rounds }),
                    )
                    .await?;
                    pending.push(entry);
                    attribution_rejected = true;
                }
                // The red persisted without everything past this group, and
                // this group is red again: whoever was left out is not the
                // cause, and whoever is in it is a suspect again (#3267).
                proved
                    .cleared
                    .retain(|id| !group.iter().any(|member| member.id == *id));
                proved
                    .cleared
                    .extend(pending[take..].iter().map(|entry| entry.id));
                evidence = result;
                window = Some(take);
                red_base = base.clone();
                expected_tip = base.or(expected_tip);
            }
            _ => {
                pending.extend(attributed.take());
                let mut rest = group.clone();
                if let Some(culprit) = take_conflict_culprit(&mut rest, &run) {
                    // Only the conflicting member is refused; the group is
                    // gated again without it.
                    settle(store, culprit.id, &result).await?;
                    settled.push((culprit.id, result.clone()));
                    pending.retain(|entry| entry.id != culprit.id);
                    window = None;
                    red_base = None;
                    expected_tip = base.or(expected_tip);
                    continue;
                }
                let refused: Vec<(Uuid, RunResult)> = group
                    .iter()
                    .map(|entry| (entry.id, result.clone()))
                    .collect();
                settle_group(store, &refused).await?;
                settled.extend(refused);
                pending.drain(..take);
                if window.is_some() {
                    // The remaining window's red was observed with this group
                    // in it: do not trust it once the group is gone.
                    window = None;
                    red_base = None;
                }
                expected_tip = base.or(expected_tip);
            }
        }
    }
    Ok(())
}

/// The operator-visible detail of a [`QUEUE_SUSPECT_CRASH_LOOP`] refusal.
fn crash_loop_detail(stops: u32) -> String {
    format!(
        "the daemon stopped {stops} times while this entry was gating (restart reconcile); \
         it is held as suspect and not gated again. Likely a test or step in its gate \
         signals or stops the daemon. Re-enqueue it alone once that is understood \
         (bisect it in a lane that cannot signal the hub, #3478)"
    )
}

/// One-shot restart reconcile: settle or re-queue every `gating` entry.
/// Returns `(settled, requeued)`.
pub(crate) async fn reconcile_gating(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
) -> Result<(usize, usize)> {
    let (mut settled, mut requeued) = (0, 0);
    loop {
        let gating = with_store(store, |s| {
            s.list_rolling_queue_entries(Some(RollingQueueEntryState::Gating), 64)
        })
        .await?;
        if gating.is_empty() {
            break;
        }
        let before = (settled, requeued);
        for (entry, repo_path) in gating {
            let source = entry.source_commit.clone();
            let repo = PathBuf::from(repo_path);
            let probe_repo = repo.clone();
            let tip = tokio::task::spawn_blocking(move || landed_tip(&repo, &source))
                .await
                .map_err(|error| DaemonError::Process(format!("reconcile probe: {error}")))?;
            let id = entry.id;
            match tip {
                Some(tip) => {
                    // The run published before its report was settled. The actual
                    // number wins, in order: the lander receipt the queue saved,
                    // the number in the published provisional commit, and only
                    // then the number the queue planned before the run.
                    let receipt =
                        with_store(store, move |s| s.rolling_queue_migration_receipt(id)).await?;
                    let recovered = match receipt {
                        Some(version) => Some(version),
                        None => {
                            let (repo, source) = (probe_repo.clone(), entry.source_commit.clone());
                            tokio::task::spawn_blocking(move || {
                                published_migration_number(&repo, &source)
                            })
                            .await
                            .map_err(|error| {
                                DaemonError::Process(format!("reconcile migration probe: {error}"))
                            })?
                        }
                    };
                    let planned = match recovered {
                        Some(version) => Some(version),
                        None => {
                            with_store(store, move |s| s.planned_rolling_queue_migration(id))
                                .await?
                        }
                    };
                    if let Some(version) = planned {
                        with_store(store, move |s| {
                            s.record_rolling_queue_assigned_migration(id, version)
                        })
                        .await?;
                    }
                    let mut outcome = RollingQueueOutcome {
                    landed_sha: Some(tip),
                    detail: Some("settled by restart reconcile after ancestry check; selection=unknown reason=publication_recovered_without_gate_receipt".into()),
                    ..RollingQueueOutcome::default()
                };
                    attach_published_commits(
                        store,
                        id,
                        RollingQueueEntryState::Published,
                        &mut outcome,
                    )
                    .await;
                    let woke = with_store(store, move |s| {
                        s.settle_rolling_queue_entry(
                            id,
                            RollingQueueEntryState::Published,
                            &outcome,
                            Utc::now(),
                        )
                    })
                    .await?;
                    settled += usize::from(woke.is_some());
                }
                None => {
                    // #3480: this restart is the entry's (prior + 1)th stop while
                    // it gated. At the limit it is held as suspect (an operator
                    // visible refusal) instead of gated again, so one batch that
                    // kills the daemon cannot keep doing it on every start.
                    let prior =
                        with_store(store, move |s| s.rolling_queue_restart_requeues(id)).await?;
                    if prior + 1 >= QUEUE_CRASH_LOOP_STOP_LIMIT {
                        let outcome = RollingQueueOutcome {
                            refusal: Some(QUEUE_SUSPECT_CRASH_LOOP.to_owned()),
                            detail: Some(crash_loop_detail(prior + 1)),
                            ..RollingQueueOutcome::default()
                        };
                        let woke = with_store(store, move |s| {
                            s.settle_rolling_queue_entry(
                                id,
                                RollingQueueEntryState::Refused,
                                &outcome,
                                Utc::now(),
                            )
                        })
                        .await?;
                        if woke.is_some() {
                            tracing::warn!(
                                %id,
                                stops = prior + 1,
                                "rolling queue entry held as suspect: the daemon stopped while it gated, repeatedly"
                            );
                        }
                        settled += usize::from(woke.is_some());
                        continue;
                    }
                    let done = with_store(store, move |s| {
                        s.requeue_gating_rolling_queue_entry(id, Utc::now())
                    })
                    .await?;
                    requeued += usize::from(done);
                }
            }
        }
        // Drain all interrupted entries in bounded pages. If a malformed entry
        // cannot move, leave it pending rather than looping or claiming success.
        if (settled, requeued) == before {
            break;
        }
    }
    Ok((settled, requeued))
}

/// One queue tick that respects the deploy drain (#1128): while a deploy is
/// holding queue admission no new batch is admitted (queued entries wait;
/// the caller finished any running batch, as the loop is sequential), so the deploy's
/// `landing_in_progress` quiet-point blocker can clear. The held queue is
/// reported in `deploy_drain.held`.
#[cfg(test)]
pub(crate) async fn run_drain_aware_batch(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    launcher: &LanderLauncher,
    size: usize,
    drain: &crate::deploy_drain::DeployDrain,
) -> Result<Vec<(Uuid, RunResult)>> {
    if admission_held(store, drain).await? {
        return Ok(Vec::new());
    }
    run_next_batch(store, launcher, size).await
}

/// Whether a deploy holds queue admission (see [`run_drain_aware_batch`]).
async fn admission_held(
    store: &Arc<rsid_core::store_lock::Mutex<Store>>,
    drain: &crate::deploy_drain::DeployDrain,
) -> Result<bool> {
    let queued = with_store(store, |s| {
        s.list_rolling_queue_entries(Some(RollingQueueEntryState::Queued), 1)
    })
    .await?;
    // #1806: a running batch can outlast the worker-start hold. #2190: the
    // queue yields at every admission opportunity while a deploy is staged (to
    // its deadline), so landings cannot starve the drain of its quiet point.
    Ok(drain.hold_queue_admission(!queued.is_empty()))
}

/// The daemon task: reconcile once, then run the FIFO head whenever the
/// operator has the queue enabled, and a second batch beside it when their
/// packages are disjoint (#1224).
pub async fn run_rolling_queue_loop(
    store: Arc<rsid_core::store_lock::Mutex<Store>>,
    config: Arc<RuntimeConfig>,
    launcher: LanderLauncher,
    drain: Arc<crate::deploy_drain::DeployDrain>,
    host_load: Arc<crate::host_load::HostLoadAdmission>,
) {
    // Independent of the publication loop: a long gate or a paused queue must
    // not postpone full coverage of earlier bounded landings (#2092).
    tokio::spawn(bounded_qa::run_loop(
        Arc::clone(&store),
        Arc::clone(&config),
        launcher.clone(),
        Arc::clone(&drain),
        host_load,
    ));
    // #3435: a lander run a previous daemon left running is adopted by the
    // batch that asks for the same gate. One nobody claims in time belongs to
    // entries that were cancelled or settled elsewhere and must not publish.
    if let Some(runs_dir) = launcher.detached_runs_dir() {
        tokio::spawn(detached_run::reap_inherited_after(
            runs_dir,
            launcher.run_epoch().to_string(),
            detached_run::INHERITED_GRACE,
        ));
    }
    resume_pending_filter_derivations(&store).await;
    match reconcile_gating(&store).await {
        Ok((settled, requeued)) if settled + requeued > 0 => {
            tracing::info!(settled, requeued, "rolling queue restart reconcile");
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "rolling queue restart reconcile deferred"),
    }
    let mut launcher = launcher;
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // One task per lane (#1224): a batch whose packages are disjoint from the
    // gating one's gates beside it instead of waiting behind it.
    let mut lanes: tokio::task::JoinSet<Result<Vec<(Uuid, RunResult)>>> =
        tokio::task::JoinSet::new();
    loop {
        interval.tick().await;
        resume_pending_filter_derivations(&store).await;
        // #3293: a queued source that already conflicts with the rolling tip
        // is refused now, not when the queue drains to it.
        tip_conflict::spawn_sweep(&store);
        // #3394: a wide queued entry is compile-probed against the tip and the
        // entries ahead of it; the claim below waits for its first verdict.
        let probe_root = launcher.compile_probe_root();
        if let Some(root) = &probe_root {
            compile_probe::spawn_sweep(&store, root.clone());
        }
        while let Some(finished) = lanes.try_join_next() {
            match finished {
                Ok(Ok(results)) => {
                    for (id, result) in results {
                        tracing::info!(%id, state = result.state.as_str(), "rolling queue entry settled");
                    }
                }
                Ok(Err(error)) => tracing::warn!(%error, "rolling queue run deferred"),
                Err(error) => tracing::error!(%error, "rolling queue lane task failed"),
            }
        }
        if !config.rolling_queue_enabled.load(Ordering::Relaxed)
            || lanes.len() >= crate::store::rolling_queue::ROLLING_QUEUE_MAX_LANES
        {
            continue;
        }
        let size = (config.rolling_queue_batch_size.load(Ordering::Relaxed) as usize)
            .clamp(1, ROLLING_QUEUE_MAX_BATCH_SIZE as usize);
        let minutes = config
            .rolling_queue_gate_timeout_mins
            .load(Ordering::Relaxed);
        launcher.gate_timeout = Duration::from_secs(u64::from(minutes.max(1)) * 60);
        match admission_held(&store, &drain).await {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(%error, "rolling queue run deferred");
                continue;
            }
        }
        let held = match probe_root {
            Some(_) => {
                compile_probe::held_head_for_claim(&store, LANE_PROBE_WINDOW.max(size)).await
            }
            None => None,
        };
        match claim_batch_in_lanes_held(&store, size, held).await {
            Ok(Some(claimed)) => {
                if claimed.parallel_lane {
                    tracing::info!(batch = %claimed.batch_id, "rolling queue second lane claimed");
                }
                let (store, launcher) = (Arc::clone(&store), launcher.clone());
                lanes.spawn(async move { run_claimed_batch(&store, &launcher, claimed).await });
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "rolling queue run deferred"),
        }
    }
}

#[cfg(test)]
mod tests {
    // Tests marked `#[cfg(target_os = "linux")]` exercise the queue clone and its
    // destructive git, which are Linux-only by design (#353): the clone is
    // pinned through `/proc/self/fd/N` handles and the reset runs under a
    // Landlock write fence. Elsewhere the queue refuses with
    // `queue_write_fence_unsupported` before touching anything (pinned by the
    // portable tests below), so there is nothing for these to prove.
    use super::*;
    use rsi_common::rolling_queue::RollingQueueBinding;
    use std::os::unix::fs::PermissionsExt;

    fn delta_cmd(script: &str) -> std::process::Child {
        std::process::Command::new("sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn limits(max_bytes: usize, max_paths: usize, deadline: Duration) -> DeltaLimits {
        DeltaLimits {
            max_bytes,
            max_paths,
            deadline,
        }
    }

    const DIFF_CHECK_STDERR: &str = "rsi-rolling-land: static guard failed before any test ran (Failed { code: Some(2) }) running git [\"diff\", \"--check\", \"815af61\", \"6691884\"]: thoughts/notes/scripts/fleet.py:8: trailing whitespace.\n+    prev={}; \nthoughts/notes/scripts/perissue.py:17: trailing whitespace.\n+        nums=set(t.get('nums',[])); \ndocs/a b.md:3: new blank line at EOF.\n";

    fn changed(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|path| path.to_string()).collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_diff_check_finding_names_its_file_including_the_lander_prefix() {
        assert_eq!(
            failure_paths(DIFF_CHECK_STDERR),
            vec![
                "thoughts/notes/scripts/fleet.py",
                "thoughts/notes/scripts/perissue.py",
                "docs/a b.md",
            ]
        );
        // Added source text that merely looks like a finding is not one.
        assert!(failure_paths("+    x = 'a.py:3: trailing whitespace later'").is_empty());
        assert!(failure_paths("note: foo:bar: trailing whitespace.").is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_static_guard_whitespace_finding_is_attributed_to_the_one_member_that_changes_the_file() {
        let files = vec![
            changed(&["crates/a/src/lib.rs"]),
            changed(&["crates/b/src/lib.rs"]),
            changed(&["thoughts/notes/scripts/fleet.py", "thoughts/notes/x.md"]),
            changed(&["docs/other.md"]),
        ];
        assert_eq!(attributable_member(&files, DIFF_CHECK_STDERR, &[]), Some(2));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_whitespace_finding_in_a_file_two_members_change_is_left_to_the_bisect() {
        let files = vec![
            changed(&["thoughts/notes/scripts/fleet.py"]),
            changed(&["thoughts/notes/scripts/fleet.py"]),
        ];
        assert_eq!(attributable_member(&files, DIFF_CHECK_STDERR, &[]), None);
        let unrelated = vec![changed(&["a.rs"]), changed(&["b.rs"])];
        assert_eq!(
            attributable_member(&unrelated, DIFF_CHECK_STDERR, &[]),
            None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_delta_within_its_limits_is_listed() {
        let read = read_delta(
            delta_cmd("printf 'a.md\nb/c.rs\n'"),
            limits(1024, 10, Duration::from_secs(30)),
        );
        assert_eq!(
            read,
            DeltaRead::Paths(vec!["a.md".to_owned(), "b/c.rs".to_owned()])
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_failing_diff_is_unreadable_not_empty() {
        let read = read_delta(
            delta_cmd("printf 'a.md\n'; exit 3"),
            limits(1024, 10, Duration::from_secs(30)),
        );
        assert_eq!(read, DeltaRead::Unreadable);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_delta_past_the_path_cap_fails_closed_and_stops_the_child() {
        // An endless listing would block forever if the child were not killed.
        let read = read_delta(
            delta_cmd("yes a.md"),
            limits(usize::MAX, 100, Duration::from_secs(30)),
        );
        assert_eq!(read, DeltaRead::TooLarge("path limit"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_delta_past_the_byte_cap_fails_closed() {
        let read = read_delta(
            delta_cmd("head -c 100000 /dev/zero | tr '\\0' x"),
            limits(4096, usize::MAX, Duration::from_secs(30)),
        );
        assert_eq!(read, DeltaRead::TooLarge("output limit"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_diff_that_outlives_its_deadline_is_killed_and_fails_closed() {
        let started = std::time::Instant::now();
        let read = read_delta(
            delta_cmd("sleep 30"),
            limits(1024, 10, Duration::from_millis(200)),
        );
        assert_eq!(read, DeltaRead::TooLarge("deadline"));
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    fn ok_run(stdout: &str) -> RunResult {
        classify_run(Some(0), stdout, "", || None)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_green_lander_run_records_the_published_tip() {
        let result = ok_run("publication_status=published\npublished_target_id=abc123\n");
        assert_eq!(result.state, RollingQueueEntryState::Published);
        assert_eq!(result.outcome.landed_sha.as_deref(), Some("abc123"));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_lander_gate_timing_is_kept_in_the_outcome_detail() {
        let timing = "gate_timing=shards=2 base_hits=1 base_runs=1 prebuild_secs=9 wall_secs=30";
        let published = ok_run(&format!(
            "gate_timing=shards=9 base_hits=0 base_runs=9 prebuild_secs=1 wall_secs=1\n{timing}\npublished_target_id=abc123\n"
        ));
        assert_eq!(published.outcome.detail.as_deref(), Some(timing));
        // Even a verbose timing record must not hide the decisive reason
        // when an operator surface clips the detail to 256 characters.
        let verbose_timing = format!("{timing} shards={}", "x".repeat(300));
        let refused = classify_run(Some(1), &format!("{verbose_timing}\n"), "gate red", || None);
        let detail = refused.outcome.detail.as_deref().unwrap();
        assert_eq!(detail, format!("gate red\n{verbose_timing}"));
        assert!(
            detail
                .chars()
                .take(256)
                .collect::<String>()
                .starts_with("gate red\n")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn policy_refusal_and_gate_failure_are_typed_with_failing_tests() {
        let policy = classify_run(
            Some(8),
            "policy_fence=migration_number\n",
            "rsi-rolling-land: migration must be tip + 1",
            || None,
        );
        assert_eq!(policy.state, RollingQueueEntryState::Refused);
        assert_eq!(
            policy.outcome.refusal.as_deref(),
            Some("policy_refused:migration_number")
        );

        let gate = classify_run(
            Some(1),
            "publication_status=not_published\n",
            "test rolling_queue::red ... FAILED\ntest rolling_queue::green ... ok\n",
            || None,
        );
        assert_eq!(gate.outcome.refusal.as_deref(), Some("gate_failed"));
        assert_eq!(gate.outcome.failing_tests, vec!["rolling_queue::red"]);
    }

    /// #3936: the same bounded assertion evidence survives in the saved log
    /// and outcome, even with verbose UTF-8 output on both captured streams.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_red_gate_keeps_a_bounded_output_excerpt_in_its_log_and_outcome() {
        let stdout_tail = format!("{}\nfixture stdout tail", "界".repeat(8192));
        let stderr_tail = format!(
            "{}\nthread 'a::red' panicked at src/a.rs:9:5:\nassertion `left == right` failed\n  left: 0\n right: 1",
            "é".repeat(8192)
        );
        let line = gate_failure_excerpt_line("a::red", &stdout_tail, &stderr_tail);
        assert_eq!(line.lines().count(), 1, "test output is JSON escaped");
        let stdout = format!(
            "gate_failure_excerpt=\"earlier failure\"\n{line}\nfailing_test=a::red\npublication_status=not_published\n"
        );
        let stderr = "rsi-rolling-land: new test failures relative to rolling base: a::red";
        let gate = classify_run(Some(1), &stdout, stderr, || None);
        assert_eq!(gate.outcome.refusal.as_deref(), Some("gate_failed"));
        assert_eq!(gate.outcome.failing_tests, ["a::red"]);
        let excerpt = gate_failure_excerpt(&stdout).expect("captured excerpt");
        assert!(excerpt.len() <= GATE_FAILURE_EXCERPT_BYTES);
        for text in [
            "failing test: a::red",
            "fixture stdout tail",
            "panicked at src/a.rs:9:5:",
            "assertion `left == right` failed\n  left: 0\n right: 1",
        ] {
            assert!(excerpt.contains(text), "{text}: {excerpt}");
        }
        assert!(gate.outcome.detail.unwrap().ends_with(&excerpt));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gate.log");
        finish_run_log(&path, "lander exit=1", &stdout, stderr).unwrap();
        let log = std::fs::read_to_string(path).unwrap();
        assert_eq!(gate_failure_excerpt(&log), Some(excerpt));

        // Bound decoded evidence independently of the producer's cap, and
        // ignore malformed records rather than losing the refusal.
        let oversized = format!(
            "gate_failure_excerpt={}\ngate_failure_excerpt=invalid\n",
            serde_json::json!("界".repeat(8192) + "assertion failed")
        );
        let kept = gate_failure_excerpt(&oversized).unwrap();
        assert!(kept.len() <= GATE_FAILURE_EXCERPT_BYTES);
        assert!(kept.ends_with("assertion failed"));
    }

    const CARGO_WARNINGS: &str = "warning: unused variable: `x`\n --> src/lib.rs:1:1\n  |\n1 | let x = 1;\n  |     ^ help: prefix it\n\nwarning: `demo` (lib) generated 1 warning\n";

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_lib_target_libtest_failure_fills_failing_tests_and_leads_the_detail() {
        let stderr = format!(
            "{CARGO_WARNINGS}    Finished `test` profile\n     Running unittests src/lib.rs (target/debug/deps/demo-1)\n\
             test a::green ... ok\ntest a::red ... FAILED\n\nfailures:\n\n---- a::red stdout ----\n\
             thread 'a::red' panicked at src/a.rs:9:5:\nassertion failed: left == right\n\n\
             failures:\n    a::red\n\ntest result: FAILED. 1 passed; 1 failed; 0 ignored\n\
             error: test failed, to rerun pass `-p demo --lib`\n"
        );
        let gate = classify_run(
            Some(1),
            "publication_status=not_published\n",
            &stderr,
            || None,
        );
        assert_eq!(gate.outcome.refusal.as_deref(), Some("gate_failed"));
        assert_eq!(gate.outcome.failing_tests, vec!["a::red"]);
        let detail = gate.outcome.detail.expect("detail");
        assert!(detail.starts_with("failing tests: a::red\n"), "{detail}");
        assert!(
            detail.contains("[a::red] thread 'a::red' panicked at src/a.rs:9:5:"),
            "{detail}"
        );
        assert!(
            detail.contains("test result: FAILED. 1 passed; 1 failed"),
            "{detail}"
        );
        assert!(!detail.contains("warning:"), "{detail}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_integration_target_failure_with_a_split_test_line_still_names_the_test() {
        // seq 373: the fixture's own output landed between the name and the
        // verdict, so no `test X ... FAILED` line exists.
        let stdout = "     Running tests/e2e_tui.rs (target/debug/deps/e2e_tui-1)\n\
             test test_e2e_ok ... ok\n\
             test test_e2e_sandbox_storage_settings ... [master (root-commit) e0796a4] fixture\n\
             1 file changed, 1 insertion(+)\nFAILED\n\nfailures:\n\n\
             ---- test_e2e_sandbox_storage_settings stdout ----\nfinal screen\n\
             Error: Custom { kind: Other, error: \"target unreachable\" }\n\n\
             failures:\n    test_e2e_sandbox_storage_settings\n\n\
             test result: FAILED. 25 passed; 1 failed; 1 ignored\n";
        let stderr =
            format!("{CARGO_WARNINGS}error: test failed, to rerun pass `-p rsi --test e2e_tui`\n");
        let gate = classify_run(Some(1), stdout, &stderr, || None);
        assert_eq!(gate.outcome.refusal.as_deref(), Some("gate_failed"));
        assert_eq!(
            gate.outcome.failing_tests,
            vec!["test_e2e_sandbox_storage_settings"]
        );
        let detail = gate.outcome.detail.expect("detail");
        assert!(
            detail.starts_with("failing tests: test_e2e_sandbox_storage_settings\n"),
            "{detail}"
        );
        assert!(detail.contains("Error: Custom"), "{detail}");
        assert!(detail.contains("error: test failed"), "{detail}");
        assert!(!detail.contains("warning:"), "{detail}");
    }

    /// #1970: the lander's release-build refusal keeps the crate in a typed,
    /// bisectable code.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_release_build_failure_is_its_own_refusal_naming_the_crate() {
        let result = classify_run(
            Some(1),
            "publication_status=not_published\ngate_timing=shards=1 release_check_secs=7 wall_secs=9\n",
            "release_build_failed:rsid-store: the non-test build of crate rsid-store failed",
            || None,
        );
        assert_eq!(result.state, RollingQueueEntryState::Refused);
        assert_eq!(
            result.outcome.refusal.as_deref(),
            Some("release_build_failed:rsid-store")
        );
        assert!(is_bisectable(&result));
        assert!(
            result
                .outcome
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("release_check_secs=7"))
        );
        assert_eq!(release_build_failed_crate("nothing here"), None);
    }

    /// #3393: a cargo log with progress lines, a warning and rustc notes leads
    /// its detail with the first error, its location and the error count.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_failed_build_detail_leads_with_the_first_compiler_error() {
        let stderr = "    Blocking waiting for file lock on package cache\n\
             \x20  Compiling rsid v0.1.0 (/work/crates/rsid)\n\
             warning: unused import: `Foo`\n  --> crates/rsid/src/a.rs:1:5\n\n\
             error[E0425]: cannot find function `names_in` in this scope\n\
             \x20   --> crates/rsid/src/paste_retention.rs:639:13\n\
             \x20    |\n639 |     names_in(x)\n\
             \x20    = note: /registry/src/index.crates.io-1/tokio-1.49.0/src/sync/mutex.rs:133:1\n\n\
             error[E0308]: mismatched types\n\
             \x20   --> crates/rsid/src/b.rs:7:9\n\n\
             error: could not compile `rsid` (lib) due to 2 previous errors\n";
        let result = classify_run(
            Some(1),
            "publication_status=not_published\n",
            stderr,
            || None,
        );
        assert_eq!(result.outcome.refusal.as_deref(), Some("gate_failed"));
        let detail = result.outcome.detail.expect("detail");
        assert!(
            detail.starts_with(
                "compile error: error[E0425]: cannot find function `names_in` in this scope \
                 at crates/rsid/src/paste_retention.rs:639:13 (2 errors)"
            ),
            "{detail}"
        );
        assert!(!detail.contains("Blocking waiting"), "{detail}");
        assert!(!detail.contains("Compiling"), "{detail}");
        assert_eq!(compile_error_summary("Compiling x\nFinished dev"), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_gate_that_ran_no_tests_is_its_own_refusal_and_stays_bisectable() {
        let result = classify_run(
            Some(1),
            "publication_status=not_published\n",
            "affected-crate guard failed (Failed { code: Some(4) }) running scripts/run-rsid-test-shards.sh [\"shard\", \"session-05\", \"--filterset\", \"test(rolling_land)\"]: error: no tests to run",
            || None,
        );
        assert_eq!(result.state, RollingQueueEntryState::Refused);
        assert_eq!(
            result.outcome.refusal.as_deref(),
            Some(QUEUE_FILTER_MATCHES_NO_TESTS)
        );
        assert!(
            result
                .outcome
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("test(rolling_land)"))
        );
        assert!(is_bisectable(&result));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_refused_plain_filter_names_the_target_form_it_needs() {
        let (_dir, _origin, work) = repos();
        sh(
            &work,
            "mkdir -p crates/rsi-common/tests crates/rsi-common/src/bin crates/rsi-remote/src \
             && printf '%s\\n' '#[test]' 'fn t() {}' > crates/rsi-common/tests/portability.rs \
             && printf '%s\\n' 'fn main() {}' > crates/rsi-common/src/bin/validate.rs \
             && printf '%s\\n' 'fn main() {}' > crates/rsi-remote/src/main.rs \
             && git add -A && git commit -qm targets",
        );
        let commit = head(&work);
        let hint = |filter: &str| no_test_filter_hint(&work, &commit, &[filter.to_owned()]);
        let integration = hint("rsi-common=portability");
        assert!(
            integration.contains("use `rsi-common=test:portability`"),
            "{integration}"
        );
        let binary = hint("rsi-common=validate");
        assert!(binary.contains("use `rsi-common=bin:validate`"), "{binary}");
        let main = hint("rsi-remote=rsi-remote");
        assert!(main.contains("use `rsi-remote=bin:rsi-remote`"), "{main}");
        // Every hint states both forms, even for a name that is no target.
        let none = hint("rsi-common=nothing_here");
        assert!(
            none.contains("`PACKAGE=test:NAME`") && none.contains("`PACKAGE=bin:NAME`"),
            "{none}"
        );
        assert!(!none.contains("use `"), "{none}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_filter_that_selects_no_test_is_found_statically_and_real_filters_pass() {
        let (_dir, _origin, work) = repos();
        sh(
            &work,
            "mkdir -p crates/rsid/src && printf '%s\\n' \
               '#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-other-01\"))]' \
               '#[test]' 'fn queue_lands_things() {}' > crates/rsid/src/q.rs \
             && printf '%s\\n' \
               '#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-session-05\"))]' \
               '#[test]' 'fn other_tests() {}' > crates/rsid/src/s.rs \
             && git add -A && git commit -qm tests",
        );
        let commit = head(&work);
        let find = |filters: &[&str]| {
            let filters: Vec<String> = filters.iter().map(|f| (*f).to_string()).collect();
            filter_selecting_no_tests(&work, &commit, &filters)
        };
        // The #1129 shape (the name exists, but in another shard) is
        // accepted since #1244: the lander's focused run selects from every
        // rsid library test, not from the named shard.
        assert_eq!(find(&["rsid=shard:session-05:test(queue_lands)"]), None);
        assert_eq!(find(&["rsid=shard:other-01:test(queue_lands)"]), None);
        assert_eq!(find(&["rsid=shard:other-01"]), None);
        assert_eq!(
            find(&["rsid=shard:other-99:test(queue_lands)"]).is_some(),
            true
        );
        assert_eq!(find(&["rsid=queue_lands"]), None);
        assert_eq!(
            find(&["rsid=queue_lands", "rsid=nowhere_to_be_found"]).as_deref(),
            Some("rsid=nowhere_to_be_found")
        );
        // Forms the check cannot reason about are accepted.
        assert_eq!(
            find(&["rsid=bin:rsi-rolling-land", "rsid=test:anything"]),
            None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_filter_naming_a_module_the_source_adds_is_accepted_by_file_or_mod_declaration() {
        // #1169: `thread_stacks.rs` holds the tests of `thread_stacks::tests`
        // but never spells the module name; `lib.rs` declares it.
        let (_dir, _origin, work) = repos();
        sh(
            &work,
            "mkdir -p crates/rsid/src \
             && printf '%s\\n' 'pub mod thread_stacks;' 'pub mod renamed;' > crates/rsid/src/lib.rs \
             && printf '%s\\n' \
               '#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-other-05\"))]' \
               'mod tests {' '#[test]' 'fn a_collector_never_returns() {}' '}' > crates/rsid/src/thread_stacks.rs \
             && printf '%s\\n' \
               '#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-other-05\"))]' \
               '#[test]' 'fn kept() {}' > crates/rsid/src/other_name.rs \
             && git add -A && git commit -qm adds-module",
        );
        let commit = head(&work);
        let find = |filters: &[&str]| {
            let filters: Vec<String> = filters.iter().map(|f| (*f).to_string()).collect();
            filter_selecting_no_tests(&work, &commit, &filters)
        };
        // The module name is only the file's name.
        assert_eq!(find(&["rsid=shard:other-05:test(thread_stacks)"]), None);
        assert_eq!(
            find(&["rsid=shard:other-05:test(thread_stacks::tests::a_collector_never_returns)"]),
            None
        );
        // A truly absent module or function is still refused.
        assert_eq!(
            find(&["rsid=shard:other-05:test(no_such_module)"]).as_deref(),
            Some("rsid=shard:other-05:test(no_such_module)")
        );
        assert_eq!(
            find(&["rsid=shard:other-05:test(thread_stacks::tests::no_such_fn)"]).as_deref(),
            Some("rsid=shard:other-05:test(thread_stacks::tests::no_such_fn)")
        );
        // A module declared in a lib.rs that carries no gate is accepted even
        // when no gated file is named after it (a `#[path]` module).
        assert_eq!(find(&["rsid=shard:other-05:test(renamed)"]), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_shard_filter_searches_every_package_that_declares_the_shard() {
        // #1021 S4: the store tests live in crates/rsid-store while the
        // filter keeps its `rsid=shard:...` spelling.
        let (_dir, _origin, work) = repos();
        sh(
            &work,
            "mkdir -p crates/rsid/src crates/rsid-store/src/store \
             && printf '%s\\n' '[features]' 'test-shard-mode = []' \
                'test-shard-store-04 = [\"test-shard-mode\"]' \
                'test-shard-session-01 = [\"test-shard-mode\"]' > crates/rsid/Cargo.toml \
             && printf '%s\\n' '[features]' 'test-shard-mode = []' \
                'test-shard-store-01 = [\"test-shard-mode\"]' \
                'test-shard-store-04 = [\"test-shard-mode\"]' > crates/rsid-store/Cargo.toml \
             && printf '%s\\n' '// module store::tests' \
               '#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-store-01\"))]' \
               '#[test]' 'fn h1_store_only_keysets() {}' > crates/rsid-store/src/store/tests.rs \
             && printf '%s\\n' \
               '#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-store-04\"))]' \
               '#[test]' 'fn remote_read_page() {}' > crates/rsid/src/remote.rs \
             && printf '%s\\n' \
               '#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-store-04\"))]' \
               '#[test]' 'fn store_page_cursor() {}' > crates/rsid-store/src/store/cursor.rs \
             && printf '%s\\n' \
               '#[cfg(any(not(feature = \"test-shard-mode\"), feature = \"test-shard-session-01\"))]' \
               '#[test]' 'fn session_only() {}' > crates/rsid/src/session.rs \
             && git add -A && git commit -qm split-tests",
        );
        let commit = head(&work);
        let find = |filters: &[&str]| {
            let filters: Vec<String> = filters.iter().map(|f| (*f).to_string()).collect();
            filter_selecting_no_tests(&work, &commit, &filters)
        };
        // A store-only test, named through the unchanged `rsid=` spelling.
        assert_eq!(
            find(&["rsid=shard:store-01:test(store::tests::h1_store_only_keysets)"]),
            None
        );
        // A shard shared by both packages finds a test in either of them.
        assert_eq!(find(&["rsid=shard:store-04:test(remote_read_page)"]), None);
        assert_eq!(find(&["rsid=shard:store-04:test(store_page_cursor)"]), None);
        // #1244: a name in another shard or the sibling package is accepted,
        // the plain spelling too; the lander's run selects from both packages.
        assert_eq!(
            find(&["rsid=shard:session-01:test(h1_store_only_keysets)"]),
            None
        );
        assert_eq!(find(&["rsid=h1_store_only_keysets"]), None);
        assert_eq!(find(&["rsid-store=session_only"]), None);
        assert_eq!(
            find(&["rsid=shard:store-01:test(no_such_store_test)"]).as_deref(),
            Some("rsid=shard:store-01:test(no_such_store_test)")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_regate_that_cannot_finish_within_the_budget_is_refused_not_looped() {
        let result = classify_run(
            Some(1),
            "publication_status=not_published\nobserved_target_id=def\n",
            "rsi-rolling-land: stale retries exhausted: rolling moved to def (overlap:rsid:*) with 3000s of the 3600s re-gate wall-time budget spent and about 1400s needed to run the gate again",
            || None,
        );
        assert_eq!(result.state, RollingQueueEntryState::Refused);
        assert_eq!(
            result.outcome.refusal.as_deref(),
            Some(QUEUE_REGATE_EXHAUSTED)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_unknown_publication_is_settled_from_ancestry() {
        let landed = classify_run(Some(3), "publication_status=unknown\n", "", || {
            Some("f00d".into())
        });
        assert_eq!(landed.state, RollingQueueEntryState::Published);
        assert_eq!(landed.outcome.landed_sha.as_deref(), Some("f00d"));
        let unknown = classify_run(Some(3), "publication_status=unknown\n", "", || None);
        assert_eq!(unknown.state, RollingQueueEntryState::Queued);
        assert_eq!(
            unknown.outcome.refusal.as_deref(),
            Some("publication_unknown")
        );
    }

    fn sh(dir: &Path, script: &str) {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            // A fixed identity so a clone that never set `user.name` still
            // commits on a host with no global gitconfig (#4012).
            .env("GIT_AUTHOR_NAME", "Queue Fixture")
            .env("GIT_AUTHOR_EMAIL", "queue-fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Queue Fixture")
            .env("GIT_COMMITTER_EMAIL", "queue-fixture@example.invalid")
            .status()
            .unwrap();
        assert!(status.success(), "{script}");
    }

    /// origin (bare) + a work clone with one commit on `rolling`.
    fn repos() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let origin = dir.path().join("origin.git");
        let work = dir.path().join("work");
        sh(dir.path(), "git init -q --bare -b rolling origin.git");
        sh(dir.path(), "git clone -q origin.git work 2>/dev/null");
        sh(
            &work,
            "git config user.email t@t && git config user.name t && git checkout -q -b rolling \
             && mkdir -p crates/rsid-store/src/store \
             && printf 'pub const LATEST_SCHEMA_VERSION: i32 = 900;\\n' > crates/rsid-store/src/store/mod.rs \
             && git add -A && git commit -qm base && git push -q origin rolling",
        );
        (dir, origin, work)
    }

    fn head(repo: &Path) -> String {
        git(repo, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn source_facts_report_the_migration_and_hot_files_a_source_adds() {
        let (_dir, _origin, work) = repos();
        sh(
            &work,
            "git checkout -q -b feature \
             && mkdir -p crates/rsid-store/src/store/migrations crates/rsid/src \
             && printf 'impl Store {}\\n' > crates/rsid-store/src/store/migrations/v901.rs \
             && printf '// hot\\n' > crates/rsid-store/src/config.rs \
             && printf '// router\\n' > crates/rsid/src/rpc.rs \
             && git add -A && git commit -qm feature",
        );
        let source = head(&work);
        assert!(source_commit_exists(&work, &source));
        assert!(!source_commit_exists(&work, &"1".repeat(40)));
        let facts = derive_source_facts(&work, &source);
        assert_eq!(facts.migration_version, Some(901));
        assert_eq!(
            facts.hot_files,
            vec!["crates/rsid-store/src/config.rs".to_string()]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn landed_tip_reports_ancestry_of_origin_rolling() {
        let (_dir, _origin, work) = repos();
        let base = head(&work);
        assert_eq!(landed_tip(&work, &base), Some(base.clone()));
        sh(
            &work,
            "git checkout -q -b feature && echo x > x && git add x && git commit -qm x",
        );
        assert_eq!(landed_tip(&work, &head(&work)), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn commit_off_rolling_is_true_only_for_unlanded_work_after_a_successful_fetch() {
        let (_dir, _origin, work) = repos();
        let base = head(&work);
        // Landed commit: an ancestor of the fetched tip is not off rolling.
        assert!(!commit_off_rolling(&work, &base));
        // Unlanded commit: positively not an ancestor of the fetched tip.
        sh(
            &work,
            "git checkout -q -b feature && echo x > x && git add x && git commit -qm x",
        );
        assert!(commit_off_rolling(&work, &head(&work)));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn commit_off_rolling_is_false_when_the_fetch_fails_on_a_stale_cached_tip() {
        let (_dir, _origin, work) = repos();
        // C landed on the remote, but the cached origin/rolling predates it.
        sh(
            &work,
            "echo c > c && git add c && git commit -qm c && git push -q origin rolling \
             && git update-ref refs/remotes/origin/rolling HEAD~1",
        );
        let landed = head(&work);
        // Sanity: with a working fetch, C is on rolling.
        assert!(!commit_off_rolling(&work, &landed));
        sh(
            &work,
            "git update-ref refs/remotes/origin/rolling HEAD~1 \
             && git remote set-url origin /nonexistent/rsi-unreachable-origin.git",
        );
        assert!(
            git(
                &work,
                &["merge-base", "--is-ancestor", &landed, "origin/rolling"]
            )
            .is_none(),
            "the cached tip must predate the commit",
        );
        assert!(!commit_off_rolling(&work, &landed));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn published_commits_reads_the_subjects_a_merge_brought_onto_rolling_3369() {
        let (_dir, _origin, work) = repos();
        let old_tip = head(&work);
        sh(
            &work,
            "git checkout -q -b feature \
             && echo 1 > one && git add one && git commit -qm 'feat(hub #3011): first slice' \
             && echo 2 > two && git add two && git commit -qm 'feat(hub #3011): second slice'",
        );
        let source = head(&work);
        sh(
            &work,
            "git checkout -q rolling \
             && echo r > r && git add r && git commit -qm 'unrelated rolling commit' \
             && git merge -q --no-ff feature -m 'rsi integration: merge into rolling'",
        );
        let landed = head(&work);

        let commits = published_commits(&work, &source, &landed);
        let subjects: Vec<&str> = commits.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(
            subjects,
            vec![
                "feat(hub #3011): second slice",
                "feat(hub #3011): first slice"
            ]
        );
        assert_eq!(commits[0].sha, source);

        // A fast-forward or an unlocated source reads only the source commit.
        assert_eq!(published_commits(&work, &landed, &landed).len(), 1);
        assert_eq!(published_commits(&work, &old_tip, &landed).len(), 1);
        // An unknown landed commit with no reachable remote reads nothing new
        // for an unknown source.
        assert!(published_commits(&work, &"0".repeat(40), &landed).is_empty());
    }

    fn fake_lander(dir: &Path, body: &str) -> LanderLauncher {
        let path = dir.join("fake-lander.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        LanderLauncher::new(path)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn snapshot_refresh_precedes_gate_and_failure_reason_reaches_gate_log() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let path = dir.path().join("snapshot.json");
        let stale = rsi_common::failure_signature::Snapshot {
            schema_version: 1,
            exported_at: (Utc::now() - chrono::Duration::hours(25)).to_rfc3339(),
            source_rolling: None,
            records: Vec::new(),
        };
        std::fs::write(&path, serde_json::to_vec(&stale).unwrap()).unwrap();
        let mut launcher = fake_lander(
            dir.path(),
            "test -s \"$RSI_KNOWN_FAILURES_SNAPSHOT\" || exit 9\necho snapshot_seen=1",
        );
        launcher.workspace = Some(dir.path().join("queue"));
        launcher.known_failure_snapshot = Some(path.clone());
        let run = run_group(
            &store,
            &launcher,
            Uuid::new_v4(),
            dir.path(),
            &[],
            None,
            tokio::time::Instant::now() + Duration::from_secs(10),
        )
        .await;
        let snapshot: rsi_common::failure_signature::Snapshot =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(snapshot.stale_warning(Utc::now()), None);
        assert!(run.stdout.contains("snapshot_seen=1"));
        let log = std::fs::read_to_string(run.log_path.unwrap()).unwrap();
        assert!(log.contains("known_failures=refreshed:"));
        // An export failure is advisory and is visible in the gate's log.
        launcher.known_failure_snapshot = Some(path.join("blocked.json"));
        std::fs::write(&launcher.binary, "#!/bin/sh\necho gate_ran=1\n").unwrap();
        let run = run_group(
            &store,
            &launcher,
            Uuid::new_v4(),
            dir.path(),
            &[],
            None,
            tokio::time::Instant::now() + Duration::from_secs(10),
        )
        .await;
        assert!(run.stdout.contains("gate_ran=1"));
        let log = std::fs::read_to_string(run.log_path.unwrap()).unwrap();
        assert!(log.contains("known_failures=refresh_failed:"));
        assert!(log.contains("reason=cannot create"));
    }

    async fn queued(
        store: &Arc<rsid_core::store_lock::Mutex<Store>>,
        repo: &Path,
        commit: &str,
        key: &str,
    ) -> Uuid {
        queued_by(store, repo, commit, key, Uuid::new_v4()).await
    }

    async fn queued_by(
        store: &Arc<rsid_core::store_lock::Mutex<Store>>,
        repo: &Path,
        commit: &str,
        key: &str,
        session: Uuid,
    ) -> Uuid {
        queued_in_project(store, repo, commit, key, session, None).await
    }

    async fn queued_in_project(
        store: &Arc<rsid_core::store_lock::Mutex<Store>>,
        repo: &Path,
        commit: &str,
        key: &str,
        session: Uuid,
        project_id: Option<Uuid>,
    ) -> Uuid {
        let new = crate::store::rolling_queue::NewQueueEntry {
            project_id,
            repo_path: repo.display().to_string(),
            source_commit: commit.into(),
            source_session_id: session,
            owner_epic_id: None,
            binding: RollingQueueBinding::Unbound,
            work_key: None,
            migration_version: None,
            hot_files: vec![],
            test_filters: vec!["rsid=rolling_queue".into()],
            fixes_tests: Vec::new(),
            idempotency_key: key.into(),
        };
        store
            .lock()
            .await
            .enqueue_rolling_queue_source(&new, Utc::now())
            .unwrap()
            .0
            .id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn run_next_settles_a_green_run_and_wakes_the_owner_once() {
        let (dir, _origin, work) = repos();
        let sha = commit_file(&work, "a", "a", "a");
        let log = dir.path().join("lander.log");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let id = queued(&store, &work, &sha, "k").await;
        let (settled, result) = run_next(&store, &launcher).await.unwrap().unwrap();
        assert_eq!(settled, id);
        assert_eq!(result.state, RollingQueueEntryState::Published);
        assert!(run_next(&store, &launcher).await.unwrap().is_none());
        let landed = entry_of(&store, id).await.outcome.unwrap().landed_sha;
        assert_eq!(landed.as_deref(), Some(head(&work).as_str()));
        assert!(is_ancestor_of_origin(&work, &sha));
        assert_eq!(wake_count(&store).await, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_staged_deploy_outlasting_the_worker_hold_admits_no_new_queue_batch() {
        let (dir, _origin, work) = repos();
        let first = commit_file(&work, "a", "a", "a");
        let second = commit_file(&work, "b", "b", "b");
        let log = dir.path().join("lander.log");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let a = queued(&store, &work, &first, "a").await;
        let b = queued(&store, &work, &second, "b").await;
        let drain = crate::deploy_drain::DeployDrain::new();

        // No deploy: the head batch (size 1) runs to settlement.
        let ran = run_drain_aware_batch(&store, &launcher, 1, &drain)
            .await
            .unwrap();
        assert_eq!(ran.len(), 1);
        assert_eq!(ran[0].0, a);
        assert_eq!(
            entry_of(&store, a).await.state,
            RollingQueueEntryState::Published
        );

        // Simulate the first batch outlasting the worker-start hold (#1806).
        // The next admission must still yield, for as long as the deploy is
        // staged (#2190).
        let now = Utc::now();
        drain.set_hold_cap_secs(60);
        drain.sync_until(
            Some(&crate::store::agent_deploys::DeployRow {
                id: Uuid::new_v4(),
                owner_session_id: Some(Uuid::new_v4()),
                sha: "0".repeat(40),
                manifest: Vec::new(),
                state: rsi_common::agent_deploy::DeployState::Staged,
                reason: None,
                deadline_at: now + chrono::Duration::minutes(5),
                operator: false,
                forced: false,
            }),
            Some(now - chrono::Duration::seconds(1)),
            true,
            now,
        );
        assert!(!drain.is_draining(), "the worker-start hold has elapsed");
        let held = run_drain_aware_batch(&store, &launcher, 1, &drain)
            .await
            .unwrap();
        assert!(held.is_empty(), "no batch is admitted while draining");
        assert_eq!(
            entry_of(&store, b).await.state,
            RollingQueueEntryState::Queued
        );
        assert_eq!(drain.status().held.len(), 1);
        assert_eq!(drain.status().held[0].kind, "queue_batch");
        assert!(drain.status().draining, "the queue pause is visible");
        assert!(drain.status().release_by.is_some());
        // The queued-but-unadmitted entry is not a quiet-point blocker.
        let blockers = store
            .lock()
            .await
            .deploy_quiet_blockers(Uuid::new_v4())
            .unwrap();
        assert!(!blockers.contains(&"landing_in_progress"), "{blockers:?}");

        // The deploy settles; the queue resumes and lands `b`.
        drain.sync(None, true, Utc::now());
        let ran = run_drain_aware_batch(&store, &launcher, 1, &drain)
            .await
            .unwrap();
        assert_eq!(ran.len(), 1);
        assert_eq!(ran[0].0, b);
        assert_eq!(
            entry_of(&store, b).await.state,
            RollingQueueEntryState::Published
        );
        assert!(drain.status().held.is_empty());
    }

    /// Stage a deploy (a new owner session and a live row) requested at `at`.
    fn stage_deploy(store: &Store, at: chrono::DateTime<Utc>) -> Uuid {
        let owner = Uuid::new_v4();
        store
            .conn
            .execute(
                "INSERT INTO sessions (id, provider, query, working_dir, status, created_at, \
                 updated_at, session_kind) VALUES (?1,'Claude','q','/tmp','Running',?2,?2,'Task')",
                rusqlite::params![
                    owner.to_string(),
                    at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
        let id = Uuid::new_v4();
        let sha = "0123456789abcdef0123456789abcdef01234567";
        store
            .insert_agent_deploy(
                &crate::store::agent_deploys::NewDeploy {
                    id,
                    owner_session_id: owner,
                    idempotency_key: "k",
                    sha,
                    fingerprint: crate::store::agent_deploys::deploy_fingerprint_with(
                        sha, "x", 3600, false,
                    ),
                    manifest: &[],
                    max_wait_secs: 3600,
                    interrupt_workers: false,
                },
                at,
            )
            .unwrap();
        id
    }

    /// #2202: a daemon that restarts while a deploy is staged restores the hold
    /// from the durable row before the queue loop starts, so the first admission
    /// opportunity yields instead of publishing a batch past the drain.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_restart_restores_the_staged_deploy_hold_before_the_queue_admits_a_batch() {
        let (dir, _origin, work) = repos();
        let sha = commit_file(&work, "a", "a", "a");
        let log = dir.path().join("lander.log");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let entry = queued(&store, &work, &sha, "k").await;
        // The deploy was staged long enough ago that the worker-start hold
        // (600 s) is over: only the whole-deploy queue hold applies (#2190).
        let staged = Utc::now() - chrono::Duration::minutes(20);
        let deploy = stage_deploy(&*store.lock().await, staged);

        // The restarted daemon: durable state survives, the in-memory drain does not.
        let drain = crate::deploy_drain::DeployDrain::new();
        assert!(
            !drain.hold_queue_admission(true),
            "a fresh drain admits a batch: the window the startup order must close"
        );
        let config = crate::config::RuntimeConfig::from_config(&crate::config::Config::default());
        let admitted =
            crate::deploy::start_admission_after_restore(&store, &drain, &config, || {
                run_drain_aware_batch(&store, &launcher, 1, &drain)
            })
            .await
            .unwrap()
            .unwrap();
        assert!(
            admitted.is_empty(),
            "the first admission after a restart yields to the staged deploy"
        );
        assert_eq!(
            entry_of(&store, entry).await.state,
            RollingQueueEntryState::Queued
        );
        assert_eq!(drain.status().held[0].kind, "queue_batch");
        assert!(!log.exists(), "the lander never ran");

        // The deploy settles; the restored hold is derived from the row, so the
        // queue resumes and lands the entry.
        store
            .lock()
            .await
            .settle_agent_deploy(
                deploy,
                rsi_common::agent_deploy::DeployState::Succeeded,
                None,
                Utc::now(),
            )
            .unwrap();
        crate::deploy::restore_drain_at_startup(&store, &drain, &config, Utc::now())
            .await
            .unwrap();
        let ran = run_drain_aware_batch(&store, &launcher, 1, &drain)
            .await
            .unwrap();
        assert_eq!(ran.len(), 1);
        assert_eq!(ran[0].0, entry);
    }

    /// A workspace lander that records where and with what build directory it
    /// ran, then reports an unpublished refusal (the settlement is not under
    /// test).
    fn recording_lander(dir: &Path, log: &Path, source: &str, workspace: &Path) -> LanderLauncher {
        let body = r#"
echo "cwd $(pwd -P)" >> @LOG@
echo "target ${CARGO_TARGET_DIR:-unset}" >> @LOG@
echo "args $*" >> @LOG@
echo "head $(git rev-parse HEAD)" >> @LOG@
git cat-file -e @SRC@^{commit} && echo "reachable yes" >> @LOG@
echo publication_status=not_published
exit 1
"#
        .replace("@LOG@", &log.display().to_string())
        .replace("@SRC@", source);
        fake_lander(dir, &body).with_workspace(workspace.to_path_buf())
    }

    fn logged(log: &Path, key: &str) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap()
            .lines()
            .filter_map(|line| line.strip_prefix(&format!("{key} ")).map(str::to_string))
            .collect()
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn the_queue_lands_in_its_own_worktree_with_its_own_cargo_target() {
        let (dir, _origin, work) = repos();
        let sha = commit_file(&work, "a", "a", "a");
        // The operator's checkout is on `a` with a dirty file; it must not move.
        sh(&work, "echo scratch > operator-note.txt");
        let operator_head = head(&work);
        let log = dir.path().join("lander.log");
        let workspace = dir.path().join("queue");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let launcher = recording_lander(dir.path(), &log, &sha, &workspace);
        queued(&store, &work, &sha, "k").await;
        run_next(&store, &launcher).await.unwrap().unwrap();

        let target = workspace.join("target");
        assert!(target.is_dir(), "the queue creates its persistent target");
        assert_eq!(
            logged(&log, "target"),
            vec![target.display().to_string()],
            "the lander gets the queue's own CARGO_TARGET_DIR"
        );
        let cwd = PathBuf::from(&logged(&log, "cwd")[0]);
        assert!(cwd.starts_with(workspace.join("worktrees").canonicalize().unwrap()));
        assert_ne!(cwd, work.canonicalize().unwrap());
        assert_eq!(
            logged(&log, "args")[0]
                .split_whitespace()
                .nth(1)
                .map(PathBuf::from),
            Some(cwd.clone()),
            "--repo is the queue worktree"
        );
        assert_eq!(logged(&log, "reachable"), vec!["yes".to_string()]);
        // Detached at the fetched rolling tip, not at the operator's HEAD.
        let rolling = git(&work, &["rev-parse", "origin/rolling"]).unwrap();
        assert_eq!(logged(&log, "head"), vec![rolling.trim().to_string()]);
        assert_ne!(rolling.trim(), operator_head);
        // The operator checkout is untouched.
        assert_eq!(head(&work), operator_head);
        assert!(work.join("operator-note.txt").is_file());
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn the_queue_worktree_is_reused_and_refreshed_between_landings() {
        let (dir, _origin, work) = repos();
        let first = commit_file(&work, "a", "a", "a");
        let log = dir.path().join("lander.log");
        let workspace = dir.path().join("queue");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let launcher = recording_lander(dir.path(), &log, &first, &workspace);
        queued(&store, &work, &first, "k1").await;
        run_next(&store, &launcher).await.unwrap().unwrap();
        // rolling moves; the next landing sees the new tip in the same place.
        sh(
            &work,
            "git checkout -q rolling && echo more > more.txt && git add more.txt \
             && git commit -qm more && git push -q origin rolling",
        );
        let advanced = head(&work);
        let second = commit_file(&work, "b", "b", "b");
        queued(&store, &work, &second, "k2").await;
        run_next(&store, &launcher).await.unwrap().unwrap();

        let cwds = logged(&log, "cwd");
        assert_eq!(cwds.len(), 2);
        assert_eq!(cwds[0], cwds[1], "one reusable worktree");
        assert_eq!(logged(&log, "head")[1], advanced);
    }

    /// The operator checkout with a dirty tracked file and an untracked file:
    /// a refused queue must leave both exactly as they are.
    fn dirty_operator(work: &Path) -> (String, PathBuf) {
        sh(work, "echo scratch > operator-note.txt");
        sh(work, "echo changed >> crates/rsid-store/src/store/mod.rs");
        (head(work), work.join("operator-note.txt"))
    }

    fn assert_operator_untouched(work: &Path, head_before: &str, note: &Path) {
        assert_eq!(head(work), head_before);
        assert!(note.is_file(), "untracked operator file survives `clean`");
        let dirty =
            std::fs::read_to_string(work.join("crates/rsid-store/src/store/mod.rs")).unwrap();
        assert!(
            dirty.ends_with("changed\n"),
            "tracked edit survives `checkout --force`"
        );
    }

    #[cfg(target_os = "linux")]
    fn worktree_dir(workspace: &Path, work: &Path) -> PathBuf {
        let common = git(
            work,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .unwrap()
        .trim()
        .to_string();
        workspace.join("worktrees").join(stable_key(&common))
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_symlinked_worktree_dir_is_refused_and_the_operator_checkout_is_untouched() {
        let (dir, _origin, work) = repos();
        let (before, note) = dirty_operator(&work);
        let workspace = dir.path().join("queue");
        prepare_queue_workspace(&workspace).unwrap();
        // An attacker (or a bad config) aims the worktree slot at the operator
        // checkout, whose common dir matches: the old health check accepted it.
        let slot = worktree_dir(&workspace, &work);
        std::os::unix::fs::symlink(&work, &slot).unwrap();
        let error = ensure_queue_worktree(&workspace, &work).unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert!(slot.symlink_metadata().unwrap().file_type().is_symlink());
        assert_operator_untouched(&work, &before, &note);
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_slot_swapped_for_a_symlink_after_validation_cannot_redirect_the_reset() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        std::fs::write(slot.join("queue-scratch.txt"), "junk").unwrap();
        let (before, note) = dirty_operator(&work);
        // Validation passes against the real queue clone...
        let pinned = pin_queue_worktree(&workspace, &slot).unwrap();
        // ...then the slot is swapped for a symlink to the operator checkout.
        let moved = dir.path().join("moved-queue-worktree");
        std::fs::rename(&slot, &moved).unwrap();
        std::os::unix::fs::symlink(&work, &slot).unwrap();
        // The clone's git directory lives in the slot, so a moved slot fails
        // closed before any git runs.
        let error = reset_pinned_worktree(&pinned, &head(&work)).unwrap_err();
        assert!(error.contains("git directory"), "{error}");
        // The symlink target keeps its tracked and untracked edits.
        assert_operator_untouched(&work, &before, &note);
        assert!(moved.join("queue-scratch.txt").exists());
        assert!(moved.join(".git").is_dir());
        // A slot that is already a symlink is refused at validation.
        assert!(pin_queue_worktree(&workspace, &slot).is_err());
    }

    /// #1141 (#1126 F9): the clone's git directory is swapped for a
    /// symlink to the operator checkout's git dir between validation and the
    /// destructive `checkout --force` / `clean`. Git must not follow it: the
    /// command is refused and the operator checkout (HEAD, index, tracked and
    /// untracked edits) is exactly as it was.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_git_directory_swapped_for_a_symlink_cannot_redirect_the_reset() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        std::fs::write(slot.join("queue-scratch.txt"), "junk").unwrap();
        // The operator checkout moves ahead of the queue clone's commit.
        sh(
            &work,
            "echo ahead > ahead.txt && git add ahead.txt && git commit -qm ahead",
        );
        let (before, note) = dirty_operator(&work);
        let operator_index = std::fs::read(work.join(".git/index")).unwrap();
        let operator_head = std::fs::read(work.join(".git/HEAD")).unwrap();
        let pinned = pin_queue_worktree(&workspace, &slot).unwrap();
        // Between validation and the destructive command the git directory is
        // replaced by a symlink to the operator's git dir.
        let registration = pinned.admin.clone();
        let moved = dir.path().join("moved-git-directory");
        std::fs::rename(&registration, &moved).unwrap();
        std::os::unix::fs::symlink(work.join(".git"), &registration).unwrap();
        // The queue worktree is not at the tip the operator checkout is at, so a
        // redirected `checkout --force` would rewrite the operator's HEAD/index.
        let rev = git(&work, &["rev-parse", "HEAD~1"])
            .unwrap()
            .trim()
            .to_string();
        let error = reset_pinned_worktree(&pinned, &rev).unwrap_err();
        assert!(error.contains("git directory"), "{error}");
        assert_operator_untouched(&work, &before, &note);
        assert_eq!(
            std::fs::read(work.join(".git/index")).unwrap(),
            operator_index
        );
        assert_eq!(
            std::fs::read(work.join(".git/HEAD")).unwrap(),
            operator_head
        );
        // The queue worktree was not reset either: the refusal came first.
        assert!(slot.join("queue-scratch.txt").exists());
        // A fresh validation refuses the swapped git directory too.
        assert!(pin_queue_worktree(&workspace, &slot).is_err());
    }

    /// #1141: a git directory that is moved (not replaced) is also refused, and
    /// git is never run against the moved copy.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_moved_git_directory_is_refused_before_git_runs() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        std::fs::write(slot.join("queue-scratch.txt"), "junk").unwrap();
        let pinned = pin_queue_worktree(&workspace, &slot).unwrap();
        std::fs::rename(&pinned.admin, dir.path().join("moved-git-directory")).unwrap();
        let error = reset_pinned_worktree(&pinned, &head(&work)).unwrap_err();
        assert!(error.contains("git directory"), "{error}");
        assert!(slot.join("queue-scratch.txt").exists());
    }

    #[cfg(target_os = "linux")]
    fn common_dir(work: &Path) -> String {
        git(
            work,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .unwrap()
        .trim()
        .to_string()
    }

    /// `(asleep, voluntary context switches)` of the process whose environment
    /// carries `marker`.
    #[cfg(target_os = "linux")]
    fn probe_marked_process(marker: &str) -> Option<(bool, u64)> {
        let entries = std::fs::read_dir("/proc").ok()?;
        entries.flatten().find_map(|entry| {
            let dir = entry.path();
            let environ = std::fs::read(dir.join("environ")).ok()?;
            if !String::from_utf8_lossy(&environ).contains(marker) {
                return None;
            }
            let stat = std::fs::read_to_string(dir.join("stat")).ok()?;
            let asleep = stat
                .rsplit(')')
                .next()
                .and_then(|rest| rest.split_whitespace().next())
                == Some("S");
            let status = std::fs::read_to_string(dir.join("status")).ok()?;
            let switches = status
                .lines()
                .find_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))?
                .trim()
                .parse()
                .ok()?;
            Some((asleep, switches))
        })
    }

    /// Reset `pinned` to `rev` and park the `target` git subcommand on a FIFO
    /// that is its global config (git reads it before it looks at its cwd, again
    /// after it has normalized the work tree to an absolute path and before it
    /// enters that path, and again after). At the second read the queue slot is
    /// renamed away and replaced by a symlink to `decoy`, so git's `chdir` by
    /// pathname lands in `decoy` (#1160).
    #[cfg(target_os = "linux")]
    fn reset_with_slot_swapped_in_flight(
        pinned: &PinnedWorktree,
        rev: &str,
        target: &str,
        slot: &Path,
        moved: &Path,
        decoy: &Path,
    ) -> std::result::Result<(), String> {
        use std::os::unix::fs::OpenOptionsExt;
        let fifo = moved.with_file_name(format!("config-{}.fifo", Uuid::new_v4()));
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        let marker = fifo.display().to_string();
        // A write open succeeds only while git has the FIFO open (or is blocked
        // opening it) for reading; closing it again hands git an empty config.
        let release = || {
            let opened = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(nix::libc::O_NONBLOCK)
                .open(&fifo);
            opened.is_ok()
        };
        // Asleep in a blocking read-open for several polls, past `after`.
        let park = |after: u64| -> u64 {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            let mut stable = 0;
            loop {
                assert!(
                    std::time::Instant::now() < deadline,
                    "git never parked on the config fifo"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
                match probe_marked_process(&marker) {
                    Some((true, switches)) if switches > after => stable += 1,
                    _ => stable = 0,
                }
                if stable >= 5 {
                    return probe_marked_process(&marker).map_or(after, |probed| probed.1);
                }
            }
        };
        std::thread::scope(|scope| {
            let runner = scope.spawn(|| {
                reset_pinned_worktree_with_env(
                    pinned,
                    rev,
                    &|command| match command == target {
                        true => vec![("GIT_CONFIG_GLOBAL".to_string(), marker.clone())],
                        false => Vec::new(),
                    },
                    &|_, _| {},
                )
            });
            let first = park(0);
            while !release() {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            park(first);
            std::fs::rename(slot, moved).unwrap();
            std::os::unix::fs::symlink(decoy, slot).unwrap();
            while !runner.is_finished() {
                release();
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            runner.join().unwrap()
        })
    }

    /// Mode and modification time: what a hook, a filter or a redirected git
    /// could change without touching a file's bytes.
    #[cfg(target_os = "linux")]
    fn mode_and_mtime(path: &Path) -> (u32, std::time::SystemTime) {
        use std::os::unix::fs::MetadataExt;
        let meta =
            std::fs::metadata(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        (meta.mode(), meta.modified().unwrap())
    }

    /// #1160 major 1: git normalizes its work tree to an absolute path and later
    /// enters it by that pathname, so a slot swapped for the operator checkout
    /// inside git's own process redirects the destructive step. Whichever step
    /// is redirected (the worktree rewrite or `clean`), the operator checkout's
    /// tracked edits, untracked files, index, HEAD, modes and timestamps must be
    /// exactly as they were, and the reset must fail closed.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_slot_swapped_inside_gits_own_process_cannot_redirect_the_reset() {
        for target in ["read-tree", "clean"] {
            let (dir, _origin, work) = repos();
            let workspace = dir.path().join("queue");
            let slot = ensure_queue_worktree(&workspace, &work).unwrap();
            let (before, note) = dirty_operator(&work);
            let tracked = work.join("crates/rsid-store/src/store/mod.rs");
            let operator_index = std::fs::read(work.join(".git/index")).unwrap();
            let operator_head = std::fs::read(work.join(".git/HEAD")).unwrap();
            let metadata = (mode_and_mtime(&tracked), mode_and_mtime(&note));
            let pinned = pin_queue_worktree(&workspace, &slot).unwrap();
            let moved = dir.path().join("moved-queue-worktree");
            let outcome =
                reset_with_slot_swapped_in_flight(&pinned, &before, target, &slot, &moved, &work);
            assert_operator_untouched(&work, &before, &note);
            assert!(
                std::fs::read(work.join(".git/index")).unwrap() == operator_index,
                "the operator index was rewritten"
            );
            assert!(
                std::fs::read(work.join(".git/HEAD")).unwrap() == operator_head,
                "the operator HEAD was rewritten"
            );
            assert!(
                (mode_and_mtime(&tracked), mode_and_mtime(&note)) == metadata,
                "an operator file's mode or timestamp changed"
            );
            assert!(outcome.is_err(), "{target} must fail closed: {outcome:?}");
        }
    }

    /// #1160 major 2: the registration directory keeps its identity when one of
    /// its entries is replaced by a symlink. The reset never follows it: it
    /// fails closed and the operator's index stays byte-identical.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_registration_entry_swapped_for_a_symlink_cannot_rewrite_the_operator_index() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        std::fs::write(slot.join("queue-scratch.txt"), "junk").unwrap();
        sh(
            &work,
            "echo ahead > ahead.txt && git add ahead.txt && git commit -qm ahead",
        );
        let (before, note) = dirty_operator(&work);
        let operator_index = std::fs::read(work.join(".git/index")).unwrap();
        let operator_head = std::fs::read(work.join(".git/HEAD")).unwrap();
        let pinned = pin_queue_worktree(&workspace, &slot).unwrap();
        // The directory (device, inode, location) is unchanged; only its index
        // entry now names the operator's index.
        let index = pinned.admin.join("index");
        std::fs::remove_file(&index).unwrap();
        std::os::unix::fs::symlink(work.join(".git/index"), &index).unwrap();
        let rev = git(&work, &["rev-parse", "HEAD~1"])
            .unwrap()
            .trim()
            .to_string();
        let outcome = reset_pinned_worktree(&pinned, &rev);
        assert!(
            std::fs::read(work.join(".git/index")).unwrap() == operator_index,
            "the operator index was rewritten"
        );
        assert!(
            std::fs::read(work.join(".git/HEAD")).unwrap() == operator_head,
            "the operator HEAD was rewritten"
        );
        assert_operator_untouched(&work, &before, &note);
        assert!(outcome.is_err(), "must fail closed: {outcome:?}");
    }

    /// #1160 blocker: a directory grant covers a hard link planted under it, so
    /// the reset must never write in place to a pre-existing file there. The
    /// registration's `logs/HEAD` (which git appends to), `index` and `ORIG_HEAD`
    /// are hard links to operator files, and so are a tracked and an untracked
    /// file in the queue worktree. The reset succeeds and every operator file
    /// keeps its bytes, mode and modification time.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn hard_links_to_operator_files_are_never_written_through() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        sh(
            &work,
            "echo ahead > ahead.txt && git add ahead.txt && git commit -qm ahead \
             && echo secret-a > operator-a.txt && echo secret-b > operator-b.txt",
        );
        let ahead = head(&work);
        let operator_files = [
            work.join(".git/index"),
            work.join(".git/config"),
            work.join(".git/description"),
            work.join("operator-a.txt"),
            work.join("operator-b.txt"),
        ];
        let before: Vec<_> = operator_files
            .iter()
            .map(|file| (std::fs::read(file).unwrap(), mode_and_mtime(file)))
            .collect();
        let pinned = pin_queue_worktree(&workspace, &slot).unwrap();
        let alias = |operator: &Path, name: &Path| {
            let _ = std::fs::remove_file(name);
            std::fs::create_dir_all(name.parent().unwrap()).unwrap();
            std::fs::hard_link(operator, name).unwrap();
        };
        alias(&operator_files[0], &pinned.admin.join("logs/HEAD"));
        alias(&operator_files[1], &pinned.admin.join("index"));
        alias(&operator_files[2], &pinned.admin.join("ORIG_HEAD"));
        // A tracked file the reset rewrites, and an untracked file `clean` removes.
        alias(
            &operator_files[3],
            &slot.join("crates/rsid-store/src/store/mod.rs"),
        );
        alias(&operator_files[4], &slot.join("untracked-link.txt"));
        reset_pinned_worktree(&pinned, &ahead).unwrap();
        for (file, (bytes, metadata)) in operator_files.iter().zip(&before) {
            assert!(
                &std::fs::read(file).unwrap() == bytes,
                "{} was written through a hard link",
                file.display()
            );
            assert!(
                &mode_and_mtime(file) == metadata,
                "{} changed mode or timestamp",
                file.display()
            );
        }
        // The reset itself worked: the slot is at the requested commit.
        assert_eq!(
            std::fs::read_to_string(pinned.admin.join("HEAD"))
                .unwrap()
                .trim(),
            ahead
        );
        assert_eq!(
            std::fs::read_to_string(slot.join("crates/rsid-store/src/store/mod.rs")).unwrap(),
            "pub const LATEST_SCHEMA_VERSION: i32 = 900;\n"
        );
        assert!(slot.join("ahead.txt").is_file());
        assert!(slot.join("untracked-link.txt").symlink_metadata().is_err());
    }

    /// #1160 major 3: Landlock does not stop chmod or utimes, so no program the
    /// operator's repository configures may run during the reset. A smudge
    /// filter and a `post-index-change` hook in the operator repo each chmod an
    /// operator file; neither may run, and the checked-out bytes are unfiltered.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_operators_hooks_and_filters_do_not_run_during_the_reset() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        let victim = work.join("operator-victim.txt");
        std::fs::write(&victim, "victim\n").unwrap();
        let metadata = mode_and_mtime(&victim);
        let script = |name: &str, body: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let chmod = format!("chmod 000 {}", victim.display());
        let smudge = script("smudge.sh", &format!("{chmod}\nsed 's/^/FILTERED:/'"));
        let hook = script("hook.sh", &chmod);
        sh(
            &work,
            "printf '*.evil filter=evil\\n' > .gitattributes && echo payload > a.evil \
             && git add .gitattributes a.evil && git commit -qm filtered",
        );
        // Armed after the operator's own commit, so only the queue triggers them.
        std::fs::copy(&hook, work.join(".git/hooks/post-index-change")).unwrap();
        sh(
            &work,
            &format!("git config filter.evil.smudge {}", smudge.display()),
        );
        let pinned = pin_queue_worktree(&workspace, &slot).unwrap();
        reset_pinned_worktree(&pinned, &head(&work)).unwrap();
        assert_eq!(
            std::fs::read_to_string(slot.join("a.evil")).unwrap(),
            "payload\n"
        );
        assert_eq!(mode_and_mtime(&victim), metadata);
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "victim\n");
    }

    /// #1160: ABI 1 and 2 cannot deny truncation, so the version policy refuses
    /// them (and a missing Landlock) with the typed unsupported-fence code.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_fence_version_policy_requires_truncation_control() {
        for abi in [0, 1, 2] {
            let error = write_fence::require_abi(Ok(abi)).unwrap_err();
            assert!(error.starts_with(write_fence::UNSUPPORTED_CODE), "{error}");
        }
        let missing = std::io::Error::from_raw_os_error(nix::libc::ENOSYS);
        let error = write_fence::require_abi(Err(missing)).unwrap_err();
        assert!(error.starts_with(write_fence::UNSUPPORTED_CODE), "{error}");
        assert_eq!(write_fence::require_abi(Ok(3)).unwrap(), 3);
        assert_eq!(write_fence::require_abi(Ok(7)).unwrap(), 7);
    }

    /// #1160: a host whose Landlock is too old settles the batch with the typed
    /// refusal before any worktree is created or reset; the operator checkout is
    /// untouched.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_host_without_the_write_fence_settles_with_the_typed_refusal() {
        let (dir, _origin, work) = repos();
        let sha = commit_file(&work, "a", "a", "a");
        let (before, note) = dirty_operator(&work);
        let log = dir.path().join("lander.log");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let mut launcher = recording_lander(dir.path(), &log, &sha, &dir.path().join("queue"));
        launcher.fence_abi = || Ok(2);
        queued(&store, &work, &sha, "k").await;
        let (_, result) = run_next(&store, &launcher).await.unwrap().unwrap();
        assert_eq!(result.state, RollingQueueEntryState::Failed);
        assert_eq!(
            result.outcome.refusal.as_deref(),
            Some(write_fence::UNSUPPORTED_CODE)
        );
        let detail = result.outcome.detail.unwrap_or_default();
        assert!(detail.contains("ABI 2"), "{detail}");
        assert!(
            !dir.path()
                .join("queue/worktrees")
                .read_dir()
                .unwrap()
                .any(|_| true)
        );
        assert_operator_untouched(&work, &before, &note);
    }

    /// A hard link to `operator` named `link` (an alias a granted directory
    /// would let a write pass through).
    #[cfg(target_os = "linux")]
    fn alias_file(operator: &Path, link: &Path) {
        let _ = std::fs::remove_file(link);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::fs::hard_link(operator, link).unwrap();
    }

    /// Operator state whose bytes, mode and timestamps a test pins.
    #[cfg(target_os = "linux")]
    fn pin_files(files: &[PathBuf]) -> Vec<(Vec<u8>, (u32, std::time::SystemTime))> {
        files
            .iter()
            .map(|file| (std::fs::read(file).unwrap(), mode_and_mtime(file)))
            .collect()
    }

    #[cfg(target_os = "linux")]
    fn assert_files_unchanged(
        files: &[PathBuf],
        pinned: &[(Vec<u8>, (u32, std::time::SystemTime))],
    ) {
        for (file, (bytes, metadata)) in files.iter().zip(pinned) {
            assert!(
                &std::fs::read(file).unwrap() == bytes,
                "{} was written",
                file.display()
            );
            assert!(
                &mode_and_mtime(file) == metadata,
                "{} changed mode or timestamp",
                file.display()
            );
        }
    }

    /// A `#!/bin/sh` script under `dir` that chmods `victim` to 000.
    #[cfg(target_os = "linux")]
    fn chmod_script(dir: &Path, name: &str, victim: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!("#!/bin/sh\nchmod 000 {}\n", victim.display()),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// #1160 (first use): the queue worktree is first populated through the
    /// same pinned, fenced runner as every later reset. A slot swapped for the
    /// operator checkout while the FIRST population's git is in flight is
    /// refused and leaves the operator checkout untouched.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_slot_swapped_during_the_first_population_cannot_redirect_it() {
        for target in ["read-tree", "clean"] {
            let (dir, _origin, work) = repos();
            let (before, note) = dirty_operator(&work);
            let tracked = work.join("crates/rsid-store/src/store/mod.rs");
            let operator_index = std::fs::read(work.join(".git/index")).unwrap();
            let metadata = (mode_and_mtime(&tracked), mode_and_mtime(&note));
            let workspace = dir.path().join("queue");
            prepare_queue_workspace(&workspace).unwrap();
            let slot = worktree_dir(&workspace, &work);
            // The clone exists, nothing is checked out yet.
            let created = create_queue_clone(&workspace, &slot).unwrap();
            sync_queue_clone(&created, &work, &common_dir(&work)).unwrap();
            drop(created);
            assert!(
                slot.join("crates").symlink_metadata().is_err(),
                "the creation step must not check anything out"
            );
            let pinned = pin_queue_worktree(&workspace, &slot).unwrap();
            let moved = dir.path().join("moved-queue-worktree");
            let outcome =
                reset_with_slot_swapped_in_flight(&pinned, &before, target, &slot, &moved, &work);
            assert_operator_untouched(&work, &before, &note);
            assert!(
                std::fs::read(work.join(".git/index")).unwrap() == operator_index,
                "the operator index was rewritten"
            );
            assert!(
                (mode_and_mtime(&tracked), mode_and_mtime(&note)) == metadata,
                "an operator file's mode or timestamp changed"
            );
            assert!(outcome.is_err(), "{target} must fail closed: {outcome:?}");
        }
    }

    /// #1160 (first use): creating the worktree must not run the operator's
    /// hooks or filters either (`git worktree add` would run a checkout, its
    /// filters and `post-checkout`).
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_first_creation_runs_no_operator_hook_or_filter() {
        let (dir, _origin, work) = repos();
        let victim = work.join("operator-victim.txt");
        std::fs::write(&victim, "victim\n").unwrap();
        let metadata = mode_and_mtime(&victim);
        let smudge = dir.path().join("smudge.sh");
        std::fs::write(
            &smudge,
            format!(
                "#!/bin/sh\nchmod 000 {}\nsed 's/^/FILTERED:/'\n",
                victim.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&smudge, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        sh(
            &work,
            "printf '*.evil filter=evil\\n' > .gitattributes && echo payload > a.evil \
             && git add .gitattributes a.evil && git commit -qm filtered && git push -q origin rolling",
        );
        let hook = chmod_script(dir.path(), "hook.sh", &victim);
        for name in ["post-checkout", "post-index-change"] {
            std::fs::copy(&hook, work.join(".git/hooks").join(name)).unwrap();
        }
        sh(
            &work,
            &format!("git config filter.evil.smudge {}", smudge.display()),
        );
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        assert_eq!(
            std::fs::read_to_string(slot.join("a.evil")).unwrap(),
            "payload\n"
        );
        assert_eq!(mode_and_mtime(&victim), metadata);
    }

    /// #1170 (R2): the source fetch runs no hook of the source repository, no
    /// automatic gc or maintenance and writes no FETCH_HEAD, while still
    /// advancing `origin/rolling`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_source_fetch_runs_no_hook_or_maintenance_and_writes_no_fetch_head() {
        let checkout = Path::new(rsi_common::manifest_dir!())
            .join("../..")
            .canonicalize()
            .unwrap();
        let policy = rsi_repo_guards::scratch_git::Policy::from_process(&checkout).unwrap();
        let dir = policy.tempdir("rsi-source-fetch-").unwrap();
        let empty_config = dir.path().join("empty.gitconfig");
        std::fs::write(&empty_config, "").unwrap();
        let fixture_git = |repo: &Path, args: &[&str]| {
            let mut command = policy.command(repo, args).unwrap();
            // Neither clone may depend on the runner's identity or Git config.
            command
                .env("GIT_CONFIG_GLOBAL", &empty_config)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_COUNT", "0")
                .env_remove("GIT_CONFIG_PARAMETERS")
                .env_remove("GIT_CONFIG")
                .env("GIT_AUTHOR_NAME", "Source Fetch Fixture")
                .env("GIT_AUTHOR_EMAIL", "source-fetch@example.invalid")
                .env("GIT_COMMITTER_NAME", "Source Fetch Fixture")
                .env("GIT_COMMITTER_EMAIL", "source-fetch@example.invalid");
            command
        };
        let run = |command: &mut std::process::Command| {
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "fixture Git failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        };
        let origin = dir.path().join("origin.git");
        let work = dir.path().join("work");
        run(fixture_git(dir.path(), &["init", "-q", "--bare", "-b", "rolling"]).arg(&origin));
        run(fixture_git(dir.path(), &["clone", "-q"])
            .arg(&origin)
            .arg(&work));
        std::fs::write(work.join("base.txt"), "base\n").unwrap();
        run(&mut fixture_git(&work, &["add", "base.txt"]));
        run(&mut fixture_git(&work, &["commit", "-qm", "base"]));
        run(&mut fixture_git(
            &work,
            &["push", "-q", "origin", "HEAD:rolling"],
        ));
        let marker = dir.path().join("hook-ran");
        let hook = work.join(".git/hooks/reference-transaction");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\necho ran >> {}\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        // Automatic gc would run (and pack the loose objects) after any fetch.
        run(&mut fixture_git(
            &work,
            &["config", "--local", "gc.auto", "1"],
        ));
        run(&mut fixture_git(
            &work,
            &["config", "--local", "gc.autoDetach", "false"],
        ));
        run(&mut fixture_git(
            &work,
            &["config", "--local", "maintenance.autoDetach", "false"],
        ));
        // Origin moves ahead through a second clone.
        let other = dir.path().join("other");
        run(fixture_git(dir.path(), &["clone", "-q"])
            .arg(&origin)
            .arg(&other));
        std::fs::write(other.join("n.txt"), "n\n").unwrap();
        run(&mut fixture_git(&other, &["add", "n.txt"]));
        run(&mut fixture_git(&other, &["commit", "-qm", "n"]));
        run(&mut fixture_git(
            &other,
            &["push", "-q", "origin", "HEAD:rolling"],
        ));
        let expected_head = run(&mut fixture_git(&other, &["rev-parse", "HEAD"]));
        let packs = || {
            let mut names: Vec<_> = std::fs::read_dir(work.join(".git/objects/pack"))
                .unwrap()
                .flatten()
                .map(|entry| entry.file_name())
                .collect();
            names.sort();
            names
        };
        let fetch_head = || std::fs::read(work.join(".git/FETCH_HEAD")).ok();
        let (packs_before, fetch_head_before) = (packs(), fetch_head());
        fetch_rolling(&work).unwrap();
        assert_eq!(
            run(&mut fixture_git(&work, &["rev-parse", "origin/rolling"])),
            expected_head,
            "the fetch itself still happens"
        );
        assert_eq!(std::fs::read_to_string(&marker).ok(), None, "no hook ran");
        assert_eq!(packs(), packs_before, "no automatic gc repacked the store");
        assert_eq!(fetch_head(), fetch_head_before, "FETCH_HEAD untouched");
    }

    /// The first population, with `plant` run just before git subcommand
    /// `target` with the private git directory's path (as a racing same-uid
    /// process would). Returns the slot.
    #[cfg(target_os = "linux")]
    fn populate_with_plant(
        dir: &Path,
        work: &Path,
        target: &str,
        plant: &dyn Fn(&Path),
    ) -> PathBuf {
        let workspace = dir.join("queue");
        let slot = ensure_queue_worktree(&workspace, work).unwrap();
        let pinned = pin_queue_worktree(&workspace, &slot).unwrap();
        let rev = git(work, &["rev-parse", "origin/rolling"]).unwrap();
        reset_pinned_worktree_with_env(
            &pinned,
            rev.trim(),
            &|_| Vec::new(),
            &|command, private| {
                if command == target {
                    plant(private);
                }
            },
        )
        .unwrap();
        slot
    }

    /// #1170 (R3): a `commondir` file planted in the private git directory does
    /// not redirect git's common directory (and with it `info/exclude`) to a
    /// directory of the planter's choosing: `clean` still removes the file a
    /// planted exclude would have protected.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_commondir_planted_in_the_private_git_dir_is_ignored() {
        let (dir, _origin, work) = repos();
        let decoy = dir.path().join("decoy-common");
        sh(
            dir.path(),
            &format!("git init -q --bare {}", decoy.display()),
        );
        std::fs::create_dir_all(decoy.join("info")).unwrap();
        std::fs::write(decoy.join("info/exclude"), "junk.txt\n").unwrap();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        std::fs::write(slot.join("junk.txt"), "junk").unwrap();
        let pinned = pin_queue_worktree(&workspace, &slot).unwrap();
        let rev = git(&work, &["rev-parse", "origin/rolling"]).unwrap();
        reset_pinned_worktree_with_env(
            &pinned,
            rev.trim(),
            &|_| Vec::new(),
            &|command, private| {
                if command == "clean" {
                    std::fs::write(private.join("commondir"), format!("{}\n", decoy.display()))
                        .unwrap();
                }
            },
        )
        .unwrap();
        assert!(
            slot.join("junk.txt").symlink_metadata().is_err(),
            "clean honored an exclude from the planted common directory"
        );
    }

    /// #1170 (R3): a replace ref planted in the private git directory does not
    /// substitute the tree the reset checks out.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_replace_ref_planted_in_the_private_git_dir_is_ignored() {
        let (dir, _origin, work) = repos();
        // A commit with an empty tree: what a planted replace ref would
        // substitute for the real tip.
        let empty = git(&work, &["hash-object", "-t", "tree", "-w", "--stdin"]).unwrap();
        let alt = git(&work, &["commit-tree", empty.trim(), "-m", "alt"]).unwrap();
        let rev = git(&work, &["rev-parse", "origin/rolling"]).unwrap();
        let slot = populate_with_plant(dir.path(), &work, "read-tree", &|private| {
            std::fs::create_dir_all(private.join("refs/replace")).unwrap();
            std::fs::write(
                private.join("refs/replace").join(rev.trim()),
                format!("{}\n", alt.trim()),
            )
            .unwrap();
        });
        assert!(
            slot.join("crates/rsid-store/src/store/mod.rs").is_file(),
            "the checked-out tree was replaced by a planted replace ref"
        );
    }

    /// #1170 (R3): `info` is a regular file, so a directory of that name (where
    /// `attributes` would name a filter driver) cannot be added beside the files
    /// the daemon creates exclusively.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_info_directory_cannot_be_planted_in_the_private_git_dir() {
        let (dir, _origin, work) = repos();
        let refused = std::cell::Cell::new(false);
        populate_with_plant(dir.path(), &work, "rev-parse", &|private| {
            refused.set(std::fs::create_dir(private.join("info")).is_err());
        });
        assert!(refused.get(), "an info directory was planted");
    }

    /// #1160: the child gets a clean environment. An inherited `GIT_TRACE`,
    /// `GIT_TRACE2_EVENT` or `GIT_TRACE_SETUP` naming a hard link to an operator
    /// file is never opened for append, and the unfenced runners drop them too.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_inherited_git_trace_cannot_write_through_a_hard_link() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        let operator = work.join("operator-trace-target.txt");
        std::fs::write(&operator, "operator\n").unwrap();
        let files = [operator.clone()];
        let pinned_state = pin_files(&files);
        let trace = slot.join("trace-alias.log");
        alias_file(&operator, &trace);
        let mut pinned = pin_queue_worktree(&workspace, &slot).unwrap();
        let var = |name: &str, value: &Path| {
            (
                std::ffi::OsString::from(name),
                std::ffi::OsString::from(value),
            )
        };
        pinned.inherited_env = Some(vec![
            var("PATH", Path::new(&std::env::var_os("PATH").unwrap())),
            var("GIT_TRACE", &trace),
            var("GIT_TRACE_SETUP", &trace),
            var("GIT_TRACE_PERFORMANCE", &trace),
            var("GIT_TRACE2", &trace),
            var("GIT_TRACE2_EVENT", &trace),
        ]);
        reset_pinned_worktree(&pinned, &head(&work)).unwrap();
        assert_files_unchanged(&files, &pinned_state);
        // The unfenced runners keep transport settings and drop the rest.
        let kept = inherited_git_env([
            (
                std::ffi::OsString::from("SSH_AUTH_SOCK"),
                std::ffi::OsString::from("/sock"),
            ),
            (
                std::ffi::OsString::from("LC_ALL"),
                std::ffi::OsString::from("C"),
            ),
            (
                std::ffi::OsString::from("GIT_TRACE"),
                std::ffi::OsString::from("/x"),
            ),
            (
                std::ffi::OsString::from("GIT_TEST_SPLIT_INDEX"),
                std::ffi::OsString::from("1"),
            ),
            (
                std::ffi::OsString::from("GIT_INDEX_FILE"),
                std::ffi::OsString::from("/x"),
            ),
        ]);
        let names: Vec<String> = kept
            .iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["SSH_AUTH_SOCK", "LC_ALL"]);
    }

    /// #1160: a split registration index (its shared half beside it) is not
    /// copied as a broken half; the reset publishes a full index, and a second
    /// and third reset (each refreshing it) keep working.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_split_registration_index_is_normalized_and_survives_repeated_resets() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        sh(&slot, "git update-index --split-index");
        let pinned = pin_queue_worktree(&workspace, &slot).unwrap();
        let shared = std::fs::read_dir(&pinned.admin)
            .unwrap()
            .flatten()
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("sharedindex.")
            });
        assert!(shared, "the fixture must hold a split index");
        for _ in 0..3 {
            std::fs::write(slot.join("queue-scratch.txt"), "junk").unwrap();
            reset_pinned_worktree(&pinned, &head(&work)).unwrap();
            assert!(slot.join("queue-scratch.txt").symlink_metadata().is_err());
            // The lander's own git, run in the slot by path, reads the index.
            sh(&slot, "git status --porcelain > /dev/null");
        }
    }

    /// #1160: a registration index replaced by a FIFO cannot block the reset.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_registration_index_fifo_is_refused_without_blocking() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        let pinned = pin_queue_worktree(&workspace, &slot).unwrap();
        let index = pinned.admin.join("index");
        std::fs::remove_file(&index).unwrap();
        nix::unistd::mkfifo(&index, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let rev = head(&work);
        let pinned = std::sync::Arc::new(pinned);
        // Unscoped, so a regression that blocks leaves this thread behind and
        // the timeout below fails the test instead of hanging it.
        std::thread::spawn(move || {
            let _ = sender.send(reset_pinned_worktree(&pinned, &rev));
        });
        let outcome = receiver
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the reset blocked on the FIFO");
        assert!(outcome.is_err(), "{outcome:?}");
    }

    /// Documented behaviour change (#1160): the queue's `clean -fd` no longer
    /// reads the operator's `.git/info/exclude`, so a file ignored only there
    /// is removed from the queue worktree like any other untracked file.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn clean_ignores_the_operators_private_excludes() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        std::fs::write(
            work.join(".git/info/exclude"),
            "excluded-only-by-operator.tmp\n",
        )
        .unwrap();
        std::fs::write(slot.join("excluded-only-by-operator.tmp"), "x").unwrap();
        let pinned = pin_queue_worktree(&workspace, &slot).unwrap();
        reset_pinned_worktree(&pinned, &head(&work)).unwrap();
        assert!(
            slot.join("excluded-only-by-operator.tmp")
                .symlink_metadata()
                .is_err()
        );
    }

    /// #1160: a same-uid writer that plants files in the fresh private git dir
    /// after it exists and before git runs cannot enable a hook, replace the
    /// config to turn one on, or redirect an append: the settings that matter
    /// are on git's command line, `logs` is a file, and no ref is updated.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn files_planted_in_the_private_git_dir_in_flight_are_not_honored() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        sh(
            &work,
            "echo ahead > ahead.txt && git add ahead.txt && git commit -qm ahead",
        );
        let victim = work.join("operator-victim.txt");
        std::fs::write(&victim, "victim\n").unwrap();
        let files = [
            victim.clone(),
            work.join(".git/index"),
            work.join(".git/HEAD"),
        ];
        let pinned_state = pin_files(&files);
        let hook = chmod_script(dir.path(), "plant.sh", &victim);
        let pinned = pin_queue_worktree(&workspace, &slot).unwrap();
        let planted = std::sync::atomic::AtomicBool::new(false);
        let plant = |_command: &str, scratch: &Path| {
            if planted.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let hooks = scratch.join("hooks");
            std::fs::create_dir(&hooks).unwrap();
            for name in [
                "post-index-change",
                "post-checkout",
                "reference-transaction",
            ] {
                std::fs::copy(&hook, hooks.join(name)).unwrap();
            }
            // A replacement config that turns hooks, reflogs and a filter on.
            std::fs::remove_file(scratch.join("config")).unwrap();
            std::fs::write(
                scratch.join("config"),
                format!(
                    "[core]\n\trepositoryformatversion = 0\n\thooksPath = {}\n\tlogAllRefUpdates = true\n\
                     \tfsmonitor = {}\n[filter \"evil\"]\n\tsmudge = {}\n",
                    hooks.display(),
                    hook.display(),
                    hook.display()
                ),
            )
            .unwrap();
            // `logs` replaced by a directory whose HEAD aliases the operator index.
            let _ = std::fs::remove_file(scratch.join("logs"));
            alias_file(&work.join(".git/index"), &scratch.join("logs/HEAD"));
        };
        reset_pinned_worktree_with_env(&pinned, &head(&work), &|_| Vec::new(), &plant).unwrap();
        assert!(planted.load(std::sync::atomic::Ordering::SeqCst));
        assert_files_unchanged(&files, &pinned_state);
        assert!(slot.join("ahead.txt").is_file());
    }

    /// #1160: the fence permits writes beneath the handles it was built from
    /// and nothing else, including through a symlink aimed outside them.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_write_fence_allows_its_handles_and_refuses_every_other_write() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("allowed");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&allowed).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("keep"), "keep").unwrap();
        std::os::unix::fs::symlink(outside.join("keep"), allowed.join("link")).unwrap();
        let handle = std::fs::File::open(&allowed).unwrap();
        let fence = WriteFence::new(&[&handle], WriteFence::kernel_abi).unwrap();
        let run = |script: &str| {
            let mut command = std::process::Command::new("sh");
            command
                .arg("-c")
                .arg(script)
                .current_dir(dir.path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            fence.confine_on_exec(&mut command);
            command.status().unwrap().success()
        };
        assert!(run(
            "echo ok > allowed/new && mv allowed/new allowed/moved && rm allowed/moved"
        ));
        assert!(!run("echo no > outside/new"));
        assert!(!run("echo no > allowed/link"));
        assert!(!run("rm outside/keep"));
        assert!(!run("ln -s allowed outside/link"));
        assert_eq!(
            std::fs::read_to_string(outside.join("keep")).unwrap(),
            "keep"
        );
        assert!(!outside.join("new").exists() && !outside.join("link").exists());
    }

    /// #1141: the empty-slot fallback deletes relative to the pinned
    /// `worktrees` handle, so neither a symlinked slot nor a `worktrees`
    /// directory swapped for a symlink to a same-named empty directory
    /// elsewhere can redirect the removal.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_empty_slot_fallback_deletes_fd_relative_and_never_follows_a_swap() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("queue");
        prepare_queue_workspace(&workspace).unwrap();
        let slot = workspace.join("worktrees").join("slot");
        // A real empty slot is retired.
        std::fs::create_dir(&slot).unwrap();
        remove_empty_queue_dir(&workspace, &slot).unwrap();
        assert!(slot.symlink_metadata().is_err());
        // A non-empty slot is refused and kept.
        std::fs::create_dir(&slot).unwrap();
        std::fs::write(slot.join("keep"), "x").unwrap();
        assert!(remove_empty_queue_dir(&workspace, &slot).is_err());
        assert!(slot.join("keep").exists());
        std::fs::remove_file(slot.join("keep")).unwrap();
        std::fs::remove_dir(&slot).unwrap();
        // A slot swapped for a symlink to an empty directory is refused.
        let decoy = dir.path().join("decoy");
        std::fs::create_dir(&decoy).unwrap();
        std::os::unix::fs::symlink(&decoy, &slot).unwrap();
        assert!(remove_empty_queue_dir(&workspace, &slot).is_err());
        assert!(decoy.is_dir());
        std::fs::remove_file(&slot).unwrap();
        // `worktrees` swapped for a symlink to a directory holding an empty
        // same-named slot: the decoy slot survives.
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(elsewhere.join("slot")).unwrap();
        std::fs::remove_dir(workspace.join("worktrees")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, workspace.join("worktrees")).unwrap();
        assert!(remove_empty_queue_dir(&workspace, &slot).is_err());
        assert!(elsewhere.join("slot").is_dir());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_symlinked_workspace_root_is_refused_and_nothing_is_created_through_it() {
        let (dir, _origin, work) = repos();
        let (before, note) = dirty_operator(&work);
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let workspace = dir.path().join("queue");
        std::os::unix::fs::symlink(&elsewhere, &workspace).unwrap();
        let error = ensure_queue_worktree(&workspace, &work).unwrap_err();
        assert!(error.contains("symlink"), "{error}");
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        // A symlinked `worktrees` directory is refused the same way.
        std::fs::remove_file(&workspace).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        std::os::unix::fs::symlink(&elsewhere, workspace.join("worktrees")).unwrap();
        assert!(ensure_queue_worktree(&workspace, &work).is_err());
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        assert_operator_untouched(&work, &before, &note);
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_plain_non_worktree_directory_is_refused_and_kept() {
        let (dir, _origin, work) = repos();
        let (before, note) = dirty_operator(&work);
        let workspace = dir.path().join("queue");
        prepare_queue_workspace(&workspace).unwrap();
        let slot = worktree_dir(&workspace, &work);
        std::fs::create_dir(&slot).unwrap();
        std::fs::write(slot.join("precious.txt"), "keep").unwrap();
        let error = ensure_queue_worktree(&workspace, &work).unwrap_err();
        assert!(error.contains("not a queue clone"), "{error}");
        assert_eq!(
            std::fs::read_to_string(slot.join("precious.txt")).unwrap(),
            "keep"
        );
        // A directory whose `.git` is a copy of the operator's pointer, or a
        // worktree registered elsewhere, is refused too.
        std::fs::write(
            slot.join(".git"),
            format!("gitdir: {}/.git\n", work.display()),
        )
        .unwrap();
        assert!(ensure_queue_worktree(&workspace, &work).is_err());
        assert!(slot.join("precious.txt").is_file());
        assert_operator_untouched(&work, &before, &note);
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_empty_leftover_directory_is_recovered_into_a_real_worktree() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        prepare_queue_workspace(&workspace).unwrap();
        let slot = worktree_dir(&workspace, &work);
        std::fs::create_dir(&slot).unwrap();
        let path = ensure_queue_worktree(&workspace, &work).unwrap();
        assert_eq!(path, slot);
        assert!(slot.join(".git").is_dir());
        // And the healthy worktree is reused on the next call.
        assert_eq!(ensure_queue_worktree(&workspace, &work).unwrap(), slot);
    }

    /// Every file under `dir` with its length and modification time: what any
    /// write by the queue, however small, would change.
    #[cfg(target_os = "linux")]
    fn tree_state(dir: &Path) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
        fn walk(dir: &Path, out: &mut Vec<(PathBuf, u64, std::time::SystemTime)>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                let meta = std::fs::symlink_metadata(&path).unwrap();
                match meta.is_dir() {
                    true => walk(&path, out),
                    false => out.push((path, meta.len(), meta.modified().unwrap())),
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, &mut out);
        out.sort();
        out
    }

    /// #1170: nothing about the queue is written into the source repository: no
    /// worktree registration, no fetched ref or object, no `FETCH_HEAD`, no
    /// hook (a `reference-transaction` hook and automatic gc are armed) and no
    /// config. The queue's fetch lands in its own clone, which reaches the new
    /// tip while the source's `origin/rolling` stays where it was.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_queue_writes_nothing_into_the_source_repository() {
        let (dir, origin, work) = repos();
        let marker = dir.path().join("hook-ran");
        let hook = work.join(".git/hooks/reference-transaction");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\necho ran >> {}\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        sh(
            &work,
            "git config gc.auto 1 && git config gc.autoDetach false",
        );
        let other = dir.path().join("other");
        sh(
            dir.path(),
            &format!(
                "git clone -q {} {} && cd {} && echo n > n.txt && git add n.txt \
                 && git commit -qm n && git push -q origin HEAD:rolling",
                origin.display(),
                other.display(),
                other.display()
            ),
        );
        let stale_tip = git(&work, &["rev-parse", "origin/rolling"]).unwrap();
        let before = tree_state(&work.join(".git"));
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        // A second call (the reuse path) writes nothing there either.
        assert_eq!(ensure_queue_worktree(&workspace, &work).unwrap(), slot);
        assert_eq!(tree_state(&work.join(".git")), before, "the source changed");
        assert!(
            !work.join(".git/worktrees").exists(),
            "a registration was written"
        );
        assert_eq!(std::fs::read_to_string(&marker).ok(), None, "a hook ran");
        assert_eq!(
            git(&work, &["rev-parse", "origin/rolling"]).unwrap(),
            stale_tip
        );
        // The queue's own clone fetched the new tip and holds it.
        assert_eq!(
            git(&slot, &["rev-parse", "HEAD"]).unwrap().trim(),
            head(&other)
        );
        assert!(slot.join("n.txt").is_file());
        let own = |sha: &str| {
            slot.join(".git/objects")
                .join(&sha[..2])
                .join(&sha[2..])
                .is_file()
        };
        assert!(own(&head(&other)), "the fetched commit lives in the clone");
        // The source is not a linked worktree owner of the slot.
        let listed = git(&work, &["worktree", "list", "--porcelain"]).unwrap();
        assert!(!listed.contains(&slot.display().to_string()), "{listed}");
    }

    /// #1170: an unpublished source commit (an accepted session commit) is
    /// reachable in the queue clone through the read-only alternate, so the
    /// lander's `git clone --shared --no-checkout` of the clone reaches it, and
    /// the clone carries the source's origin so the lander can publish.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_unpublished_source_commit_is_reachable_through_the_clone_by_the_lander() {
        let (dir, origin, work) = repos();
        let unpublished = commit_file(&work, "a", "a", "a");
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        assert!(source_commit_exists(&slot, &unpublished));
        let scratch = dir.path().join("lander-clone");
        sh(
            dir.path(),
            &format!(
                "git clone -q --shared --no-checkout {} {}",
                slot.display(),
                scratch.display()
            ),
        );
        assert!(source_commit_exists(&scratch, &unpublished));
        assert_eq!(
            git(&slot, &["remote", "get-url", "--push", "origin"])
                .unwrap()
                .trim(),
            origin.display().to_string()
        );
        // Detached at the fetched rolling tip, not at the source's HEAD.
        assert_eq!(
            head(&slot),
            git(&work, &["rev-parse", "origin/rolling"]).unwrap().trim()
        );
        assert_ne!(head(&slot), unpublished);
    }

    /// #1170: a worktree an earlier daemon registered in the source repository
    /// is retired into a queue clone. The source's registration is left exactly
    /// as it is (git's own `worktree prune` owns it), and nothing else of the
    /// source changes.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_worktree_registered_by_an_earlier_queue_is_replaced_by_a_clone() {
        let (dir, _origin, work) = repos();
        let workspace = dir.path().join("queue");
        prepare_queue_workspace(&workspace).unwrap();
        let slot = worktree_dir(&workspace, &work);
        sh(
            &work,
            &format!(
                "git worktree add -q --no-checkout --detach --force {} HEAD",
                slot.display()
            ),
        );
        assert!(slot.join(".git").is_file());
        let registrations = tree_state(&work.join(".git/worktrees"));
        assert!(!registrations.is_empty());
        let (before, note) = dirty_operator(&work);
        assert_eq!(ensure_queue_worktree(&workspace, &work).unwrap(), slot);
        assert!(slot.join(".git").is_dir());
        assert!(slot.join("crates/rsid-store/src/store/mod.rs").is_file());
        assert_eq!(tree_state(&work.join(".git/worktrees")), registrations);
        assert_operator_untouched(&work, &before, &note);
    }

    /// #1170: with no reachable remote the clone is reset to the source's
    /// `HEAD` (the lander reports the real error), as the queue always did; the
    /// source's transport settings that the fetch needs are copied into the
    /// clone's config, quoted, and a value that cannot be written on one line is
    /// dropped.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_offline_source_resets_to_its_head_and_transport_settings_are_copied() {
        let (dir, _origin, work) = repos();
        let local = commit_file(&work, "a", "a", "a");
        sh(
            &work,
            "git remote set-url origin /nonexistent/origin.git \
             && git config core.sshCommand 'ssh -i \"/keys/a b\"' \
             && git config --add credential.helper 'store' \
             && git config http.proxy 'http://proxy:3128'",
        );
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        assert_eq!(head(&slot), local);
        let config = std::fs::read_to_string(slot.join(".git/config")).unwrap();
        assert!(config.starts_with(QUEUE_CLONE_MARKER), "{config}");
        assert!(
            config.contains("url = \"/nonexistent/origin.git\""),
            "{config}"
        );
        assert!(
            config.contains("sshCommand = \"ssh -i \\\"/keys/a b\\\"\""),
            "{config}"
        );
        assert!(config.contains("helper = \"store\""), "{config}");
        assert!(config.contains("proxy = \"http://proxy:3128\""), "{config}");
        assert_eq!(config_quote("a\nb"), None);
        assert_eq!(config_quote("a\"b\\c").as_deref(), Some("\"a\\\"b\\\\c\""));
        // The clone's config is the queue's: nothing of the source's other
        // settings (hooks, filters, includes) is carried over.
        assert!(!config.contains("hooksPath = /operator"), "{config}");
    }

    /// #1170: a config planted in the clone's git directory (or any file
    /// replaced there) is overwritten from the source on the next call, so a
    /// filter driver or a changed remote cannot persist.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_config_replaced_in_the_clone_is_rewritten_from_the_source() {
        let (dir, origin, work) = repos();
        let workspace = dir.path().join("queue");
        let slot = ensure_queue_worktree(&workspace, &work).unwrap();
        std::fs::write(
            slot.join(".git/config"),
            format!(
                "{QUEUE_CLONE_MARKER}[filter \"evil\"]\n\tsmudge = /bin/true\n\
                 [remote \"origin\"]\n\turl = /elsewhere\n"
            ),
        )
        .unwrap();
        std::fs::write(
            slot.join(".git/objects/info/alternates"),
            "/elsewhere/objects\n",
        )
        .unwrap();
        assert_eq!(ensure_queue_worktree(&workspace, &work).unwrap(), slot);
        let config = std::fs::read_to_string(slot.join(".git/config")).unwrap();
        assert!(
            !config.contains("evil") && !config.contains("/elsewhere"),
            "{config}"
        );
        assert!(config.contains(&origin.display().to_string()), "{config}");
        let alternates =
            std::fs::read_to_string(slot.join(".git/objects/info/alternates")).unwrap();
        assert_eq!(alternates, format!("{}/objects\n", common_dir(&work)));
        // A git directory without the queue's marker is not a queue clone.
        std::fs::write(slot.join(".git/config"), "[core]\n\tbare = false\n").unwrap();
        let error = pin_queue_worktree(&workspace, &slot).err().unwrap();
        assert!(error.contains("not a queue clone"), "{error}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_launcher_without_a_workspace_keeps_the_entry_checkout() {
        let (dir, _origin, work) = repos();
        let sha = commit_file(&work, "a", "a", "a");
        let log = dir.path().join("lander.log");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let launcher = recording_lander(dir.path(), &log, &sha, dir.path());
        let launcher = LanderLauncher::new(launcher.binary().to_path_buf());
        queued(&store, &work, &sha, "k").await;
        run_next(&store, &launcher).await.unwrap().unwrap();
        assert_eq!(
            logged(&log, "cwd"),
            vec![work.canonicalize().unwrap().display().to_string()]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn run_next_passes_the_source_filters_and_the_wall_time_regate_budget() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args.log");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let launcher = fake_lander(
            dir.path(),
            &format!(
                "echo \"$@\" > {log}; echo \"budget=$RSI_LANDER_REGATE_BUDGET_SECS\" >> {log}; \
                 echo \"count=$RSI_LANDER_MAX_REGATED_STALE_RETRIES\" >> {log}; \
                 echo \"exact=$RSI_LANDER_EXACT_GATE\" >> {log}; \
                 echo 'test rolling_queue::red ... FAILED'; exit 1",
                log = log.display()
            ),
        );
        let commit = "b".repeat(40);
        let id = queued(&store, dir.path(), &commit, "k").await;
        let (_, result) = run_next(&store, &launcher).await.unwrap().unwrap();
        assert_eq!(result.state, RollingQueueEntryState::Refused);
        assert_eq!(result.outcome.failing_tests, vec!["rolling_queue::red"]);
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(logged.contains(&format!("--accepted {commit}")), "{logged}");
        assert!(
            logged.contains("--test-filter rsid=rolling_queue"),
            "{logged}"
        );
        // #1940: the re-gates are bounded by the wall time the batch has left
        // (the default gate timeout, an hour), never by a count.
        let budget: u64 = logged
            .lines()
            .find_map(|line| line.strip_prefix("budget="))
            .and_then(|secs| secs.parse().ok())
            .unwrap_or_default();
        assert!(
            (3000..=3600).contains(&budget),
            "the lander gets the batch's remaining wall time: {logged}"
        );
        assert!(
            logged.lines().any(|line| line == "count="),
            "no fixed count of re-gates: {logged}"
        );
        assert!(
            logged.trim_end().ends_with("exact=1"),
            "the queue runs the lander with the exact gate: {logged}"
        );
        let entry = store
            .lock()
            .await
            .get_rolling_queue_entry(id)
            .unwrap()
            .unwrap();
        assert_eq!(entry.state, RollingQueueEntryState::Refused);
    }

    /// #2167: an entry's claimed fixes reach the lander as `--fixes-test`
    /// (never as a filter), and its `claimed_fix_still_red` refusal settles
    /// the entry with that code and the still-red tests.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_claimed_fix_reaches_the_lander_and_its_refusal_names_the_tests() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args.log");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let launcher = fake_lander(
            dir.path(),
            &format!(
                "echo \"$@\" > {log}; echo 'publication_status=not_published'; \
                 echo 'failing_test=demo::red'; \
                 echo 'rsi-rolling-land: claimed_fix_still_red: demo::red; the claimed fix still fails on the candidate' >&2; exit 1",
                log = log.display()
            ),
        );
        let commit = "c".repeat(40);
        let new = crate::store::rolling_queue::NewQueueEntry {
            project_id: None,
            repo_path: dir.path().display().to_string(),
            source_commit: commit.clone(),
            source_session_id: Uuid::new_v4(),
            owner_epic_id: None,
            binding: RollingQueueBinding::Unbound,
            work_key: None,
            migration_version: None,
            hot_files: vec![],
            test_filters: vec!["rsid=rolling_queue".into()],
            fixes_tests: vec!["demo::red".into()],
            idempotency_key: "claimed".into(),
        };
        let (entry, _) = store
            .lock()
            .await
            .enqueue_rolling_queue_source(&new, Utc::now())
            .unwrap();
        assert_eq!(entry.fixes_tests, vec!["demo::red".to_string()]);
        assert_eq!(entry.test_filters, vec!["rsid=rolling_queue".to_string()]);
        let (_, result) = run_next(&store, &launcher).await.unwrap().unwrap();
        assert_eq!(result.state, RollingQueueEntryState::Refused);
        assert_eq!(
            result.outcome.refusal.as_deref(),
            Some(QUEUE_CLAIMED_FIX_STILL_RED)
        );
        assert_eq!(result.outcome.failing_tests, vec!["demo::red"]);
        let logged = std::fs::read_to_string(&log).unwrap();
        assert!(
            logged.contains("--test-filter rsid=rolling_queue --fixes-test demo::red"),
            "{logged}"
        );
        assert!(is_bisectable(&result), "a batch bisects to the claimant");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_unavailable_lander_fails_the_entry_and_wakes_the_owner() {
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let launcher = LanderLauncher::new(PathBuf::from("/nonexistent/rsi-rolling-land"));
        let dir = tempfile::tempdir().unwrap();
        queued(&store, dir.path(), &"c".repeat(40), "k").await;
        let (_, result) = run_next(&store, &launcher).await.unwrap().unwrap();
        assert_eq!(result.state, RollingQueueEntryState::Failed);
        assert_eq!(
            result.outcome.refusal.as_deref(),
            Some("lander_unavailable")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_killed_ancestry_probe_is_unknown_never_a_negative_verdict() {
        use std::os::unix::process::ExitStatusExt;
        let status = |code: i32| Ok(std::process::ExitStatus::from_raw(code << 8));
        assert_eq!(classify_ancestry(status(0)), Ancestry::Ancestor);
        assert_eq!(classify_ancestry(status(1)), Ancestry::NotAncestor);
        assert_eq!(classify_ancestry(status(128)), Ancestry::Unknown);
        // Death by SIGKILL (the shutdown kill of #2595): raw status = signal.
        assert_eq!(
            classify_ancestry(Ok(std::process::ExitStatus::from_raw(9))),
            Ancestry::Unknown
        );
        assert_eq!(
            classify_ancestry(Err(std::io::Error::other("spawn"))),
            Ancestry::Unknown
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_not_ancestor_is_rechecked_against_the_refetched_tip_before_failing() {
        let sources = vec!["a".to_string(), "b".to_string()];
        let calls = std::cell::RefCell::new(Vec::new());
        let fetches = std::cell::Cell::new(0);
        let verdicts = verify_ancestry_with(
            &sources,
            2,
            |source| {
                calls.borrow_mut().push(source.to_string());
                // `a` is only visible after the second fetch; `b` never lands.
                match (source, fetches.get()) {
                    ("a", 2) => Ancestry::Ancestor,
                    ("b", _) => Ancestry::NotAncestor,
                    _ => Ancestry::NotAncestor,
                }
            },
            || {
                fetches.set(fetches.get() + 1);
                true
            },
        );
        assert_eq!(verdicts, vec![Ancestry::Ancestor, Ancestry::NotAncestor]);
        assert_eq!(fetches.get(), 2);
        assert_eq!(*calls.borrow(), vec!["a", "b", "a", "b"]);
    }

    /// #2599: a failed refresh leaves `origin/rolling` possibly stale, so a "no"
    /// against it is Unknown, never a verified negative; a "yes" still holds.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_failed_fetch_cannot_produce_a_negative_ancestry_verdict() {
        let sources = vec!["a".to_string(), "b".to_string()];
        let verdicts = verify_ancestry_with(
            &sources,
            2,
            |source| match source {
                "a" => Ancestry::Ancestor,
                _ => Ancestry::NotAncestor,
            },
            || false,
        );
        assert_eq!(verdicts, vec![Ancestry::Ancestor, Ancestry::Unknown]);
    }

    /// #2599: a definite negative from a refreshed attempt survives a later
    /// attempt whose refresh failed, and a later successful refresh still wins.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_failed_retry_fetch_keeps_a_fresh_negative_and_a_later_fetch_can_confirm() {
        let sources = vec!["a".to_string()];
        let fetches = std::cell::Cell::new(0);
        let verdicts = verify_ancestry_with(
            &sources,
            2,
            |_| Ancestry::NotAncestor,
            || {
                fetches.set(fetches.get() + 1);
                fetches.get() == 1
            },
        );
        assert_eq!(verdicts, vec![Ancestry::NotAncestor]);
        let fetches = std::cell::Cell::new(0);
        let verdicts = verify_ancestry_with(
            &sources,
            2,
            |_| {
                if fetches.get() == 2 {
                    Ancestry::Ancestor
                } else {
                    Ancestry::NotAncestor
                }
            },
            || {
                fetches.set(fetches.get() + 1);
                fetches.get() == 2
            },
        );
        assert_eq!(verdicts, vec![Ancestry::Ancestor]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_shutdown_killed_ancestry_probe_leaves_the_entry_gating_and_reconcile_publishes_it() {
        let (_dir, _origin, work) = repos();
        let source = head(&work);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let id = queued(&store, &work, &source, "1").await;
        let batch_id = Uuid::new_v4();
        {
            let guard = store.lock().await;
            guard
                .conn
                .execute(
                    "INSERT INTO rolling_queue_batches(id,state,member_ids_json,member_count,created_at,row_version) \
                     VALUES (?1,'gating','[]',1,'2026-09-29T00:00:00.000000000Z',1)",
                    [batch_id.to_string()],
                )
                .unwrap();
            guard
                .conn
                .execute(
                    "UPDATE rolling_queue_entries SET state='gating', batch_id=?2 WHERE id=?1",
                    [id.to_string(), batch_id.to_string()],
                )
                .unwrap();
        }
        let group = vec![entry_of(&store, id).await];
        let result = RunResult {
            state: RollingQueueEntryState::Published,
            outcome: RollingQueueOutcome {
                landed_sha: Some(source.clone()),
                ..RollingQueueOutcome::default()
            },
        };
        let run = GroupRun {
            result: result.clone(),
            stdout: String::new(),
            own_tip: Some(source.clone()),
            conflict_source: None,
            log_path: None,
            cause: None,
            already_integrated: false,
        };
        let mut settled = Vec::new();
        // The shutdown SIGKILLed git: every probe is a signal exit.
        publish_group_with(
            &store,
            &work,
            batch_id,
            &group,
            &run,
            &result,
            &mut settled,
            |_, sources| vec![Ancestry::Unknown; sources.len()],
        )
        .await
        .unwrap();
        assert!(settled.is_empty());
        assert_eq!(
            entry_of(&store, id).await.state,
            RollingQueueEntryState::Gating
        );
        // Boot recovery re-checks against the remote tip and publishes it.
        assert_eq!(reconcile_gating(&store).await.unwrap(), (1, 0));
        let entry = entry_of(&store, id).await;
        assert_eq!(entry.state, RollingQueueEntryState::Published);
        assert_eq!(
            entry.outcome.unwrap().landed_sha.as_deref(),
            Some(source.as_str())
        );
    }

    /// #2698: a gate that publishes over a base red no snapshot record owns
    /// files one qa Issue (test, shard command, base SHA) and names it in the
    /// published outcome; a replay or a second gate files nothing more.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_published_gate_over_an_unowned_base_red_files_one_qa_issue_and_names_it() {
        let (_dir, _origin, work) = repos();
        let source = head(&work);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let project = Uuid::new_v4();
        {
            let now = Utc::now();
            store
                .lock()
                .await
                .insert_project(&rsi_common::types::Project {
                    id: project,
                    name: "base-red".into(),
                    path: None,
                    description: None,
                    color: rsi_common::types::Project::DEFAULT_COLOR.into(),
                    context_files: None,
                    created_at: now,
                    updated_at: now,
                })
                .unwrap();
        }
        let id =
            queued_in_project(&store, &work, &source, "1", Uuid::new_v4(), Some(project)).await;
        let batch_id = Uuid::new_v4();
        {
            let guard = store.lock().await;
            guard
                .conn
                .execute(
                    "INSERT INTO rolling_queue_batches(id,state,member_ids_json,member_count,created_at,row_version) \
                     VALUES (?1,'gating','[]',1,'2026-09-29T00:00:00.000000000Z',1)",
                    [batch_id.to_string()],
                )
                .unwrap();
            guard
                .conn
                .execute(
                    "UPDATE rolling_queue_entries SET state='gating', batch_id=?2 WHERE id=?1",
                    [id.to_string(), batch_id.to_string()],
                )
                .unwrap();
        }
        let group = vec![entry_of(&store, id).await];
        let result = RunResult {
            state: RollingQueueEntryState::Published,
            outcome: RollingQueueOutcome {
                landed_sha: Some(source.clone()),
                detail: Some("gate_timing=shards=1".into()),
                ..RollingQueueOutcome::default()
            },
        };
        let base = "b".repeat(40);
        let run = GroupRun {
            result: result.clone(),
            stdout: format!(
                "fetched_target_id={base}\nbase_red=research_corpus::every_sidecar_validates\n\
                 base_red=demo::owned\nknown_red=research_corpus::every_sidecar_validates:new\n\
                 known_red=demo::owned:#77:regression\n\
                 base_red_command=research_corpus::every_sidecar_validates\tcargo test -p rsi-common --test research_corpus\n"
            ),
            own_tip: Some(source.clone()),
            conflict_source: None,
            log_path: Some(PathBuf::from("/tmp/gate-2698.log")),
            cause: None,
            already_integrated: false,
        };
        let mut settled = Vec::new();
        publish_group_with(
            &store,
            &work,
            batch_id,
            &group,
            &run,
            &result,
            &mut settled,
            |_, sources| vec![Ancestry::Ancestor; sources.len()],
        )
        .await
        .unwrap();
        let detail = settled[0].1.outcome.detail.clone().unwrap();
        assert!(detail.starts_with("gate_timing=shards=1\n"), "{detail}");
        assert!(
            detail.contains(
                "base_red_issue=#1 test=research_corpus::every_sidecar_validates (filed)"
            ),
            "{detail}"
        );
        assert!(!detail.contains("demo::owned"), "{detail}");
        let issues = store
            .lock()
            .await
            .list_issues(&rsi_common::types::IssueFilter {
                project_id: Some(project),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].labels.contains(&"qa".to_string()));
        for fact in [
            "research_corpus::every_sidecar_validates",
            "cargo test -p rsi-common --test research_corpus",
            base.as_str(),
            "/tmp/gate-2698.log",
        ] {
            assert!(issues[0].body.contains(fact), "{fact}: {}", issues[0].body);
        }
        assert!(!issues[0].body.contains("unrecorded"), "{}", issues[0].body);
        // A replay (restart) of the same gate dedupes into the open Issue.
        let lines = gate_base_red::file_base_reds(
            &store,
            batch_id,
            &group,
            &run.stdout,
            run.log_path.as_deref(),
        )
        .await;
        assert_eq!(
            lines,
            ["base_red_issue=#1 test=research_corpus::every_sidecar_validates (already open)"]
        );
    }

    /// #2705 + #2703: a gate that observes an unowned base red (A) and then
    /// fails on a new candidate red (B) still files exactly one qa Issue, for
    /// A alone, with the shard command that observed it, and names it in the
    /// failed outcome.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_gate_over_an_unowned_base_red_files_one_qa_issue_for_the_base_red() {
        let (dir, _origin, work) = repos();
        let source = head(&work);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let project = Uuid::new_v4();
        {
            let now = Utc::now();
            store
                .lock()
                .await
                .insert_project(&rsi_common::types::Project {
                    id: project,
                    name: "base-red-failed".into(),
                    path: None,
                    description: None,
                    color: rsi_common::types::Project::DEFAULT_COLOR.into(),
                    context_files: None,
                    created_at: now,
                    updated_at: now,
                })
                .unwrap();
        }
        let id =
            queued_in_project(&store, &work, &source, "1", Uuid::new_v4(), Some(project)).await;
        let batch_id = Uuid::new_v4();
        {
            let guard = store.lock().await;
            guard
                .conn
                .execute(
                    "INSERT INTO rolling_queue_batches(id,state,member_ids_json,member_count,created_at,row_version) \
                     VALUES (?1,'gating','[]',1,'2026-09-29T00:00:00.000000000Z',1)",
                    [batch_id.to_string()],
                )
                .unwrap();
            guard
                .conn
                .execute(
                    "UPDATE rolling_queue_entries SET state='gating', batch_id=?2 WHERE id=?1",
                    [id.to_string(), batch_id.to_string()],
                )
                .unwrap();
        }
        let group = vec![entry_of(&store, id).await];
        let mut launcher = fake_lander(
            dir.path(),
            r#"printf 'publication_status=not_published\nlanding_outcome=not_published\n'
printf 'fetched_target_id=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n'
printf 'base_red=demo::base_red\nknown_red=demo::base_red:new\n'
printf 'base_red_command=demo::base_red\tcargo test -p demo --lib\n'
echo 'candidate_only_failures={"demo::candidate_red"}' >&2
exit 1"#,
        );
        launcher.workspace = Some(dir.path().join("queue"));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let run = run_group(&store, &launcher, batch_id, &work, &group, None, deadline).await;
        assert_ne!(run.result.state, RollingQueueEntryState::Published);
        let detail = run.result.outcome.detail.clone().unwrap_or_default();
        assert!(
            detail.contains("base_red_issue=#1 test=demo::base_red (filed)"),
            "{detail}"
        );
        assert!(!detail.contains("base_red_issue=#1 test=demo::candidate_red"));
        let issues = store
            .lock()
            .await
            .list_issues(&rsi_common::types::IssueFilter {
                project_id: Some(project),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].labels.contains(&"qa".to_string()));
        for fact in ["demo::base_red", "cargo test -p demo --lib"] {
            assert!(issues[0].body.contains(fact), "{fact}: {}", issues[0].body);
        }
        assert!(
            !issues[0].body.contains("candidate_red"),
            "{}",
            issues[0].body
        );
        assert!(!issues[0].body.contains("unrecorded"), "{}", issues[0].body);
        // #2712: the gate did not publish, so the Issue must not say it did.
        assert!(
            !issues[0].body.contains("published its candidate anyway"),
            "{}",
            issues[0].body
        );
        assert!(
            issues[0]
                .body
                .contains("observed `demo::base_red` red on the base"),
            "{}",
            issues[0].body
        );
        // A retried run (the next bisect round) dedupes into the open Issue.
        let again = run_group(&store, &launcher, batch_id, &work, &group, None, deadline).await;
        let detail = again.result.outcome.detail.clone().unwrap_or_default();
        assert!(
            detail.contains("base_red_issue=#1 test=demo::base_red (already open)"),
            "{detail}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn restart_reconcile_settles_a_landed_entry_once_and_requeues_the_rest() {
        let (_dir, _origin, work) = repos();
        let landed_commit = head(&work);
        sh(
            &work,
            "git checkout -q -b feature && echo x > x && git add x && git commit -qm x",
        );
        let pending_commit = head(&work);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let landed = queued(&store, &work, &landed_commit, "1").await;
        let pending = queued(&store, &work, &pending_commit, "2").await;
        {
            let guard = store.lock().await;
            // Both were mid-run when the daemon stopped.
            for id in [landed, pending] {
                let batch = Uuid::new_v4().to_string();
                guard
                    .conn
                    .execute(
                        "INSERT INTO rolling_queue_batches(id,state,member_ids_json,member_count,created_at,row_version) \
                         VALUES (?1,'gating','[]',1,'2026-09-29T00:00:00.000000000Z',1)",
                        [&batch],
                    )
                    .unwrap();
                guard
                    .conn
                    .execute(
                        "UPDATE rolling_queue_entries SET state='gating', batch_id=?2 WHERE id=?1",
                        [id.to_string(), batch],
                    )
                    .unwrap();
            }
        }
        assert_eq!(reconcile_gating(&store).await.unwrap(), (1, 1));
        // A second boot finds nothing left to settle: the wake is not repeated.
        assert_eq!(reconcile_gating(&store).await.unwrap(), (0, 0));
        let guard = store.lock().await;
        assert_eq!(
            guard
                .get_rolling_queue_entry(landed)
                .unwrap()
                .unwrap()
                .state,
            RollingQueueEntryState::Published
        );
        assert_eq!(
            guard
                .get_rolling_queue_entry(pending)
                .unwrap()
                .unwrap()
                .state,
            RollingQueueEntryState::Queued
        );
        let wakes = guard
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .count();
        assert_eq!(wakes, 1);
    }

    /// #3480: an entry that is `gating` at two daemon stops is held as
    /// suspect with an operator-visible refusal, not gated a third time; the
    /// first stop still re-drives it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn an_entry_gating_at_two_daemon_stops_is_held_as_suspect_3480() {
        let (_dir, _origin, work) = repos();
        sh(
            &work,
            "git checkout -q -b feature && echo x > x && git add x && git commit -qm x",
        );
        let commit = head(&work);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let entry = queued(&store, &work, &commit, "1").await;
        async fn gate_again(store: &Arc<rsid_core::store_lock::Mutex<Store>>, id: Uuid) {
            let guard = store.lock().await;
            let batch = Uuid::new_v4().to_string();
            guard
                .conn
                .execute(
                    "INSERT INTO rolling_queue_batches(id,state,member_ids_json,member_count,created_at,row_version) \
                     VALUES (?1,'gating','[]',1,'2026-09-29T00:00:00.000000000Z',1)",
                    [&batch],
                )
                .unwrap();
            guard
                .conn
                .execute(
                    "UPDATE rolling_queue_entries SET state='gating', batch_id=?2 WHERE id=?1",
                    [id.to_string(), batch],
                )
                .unwrap();
        }
        // First stop while gating: re-driven, not yet suspect.
        gate_again(&store, entry).await;
        assert_eq!(reconcile_gating(&store).await.unwrap(), (0, 1));
        assert_eq!(
            store
                .lock()
                .await
                .get_rolling_queue_entry(entry)
                .unwrap()
                .unwrap()
                .state,
            RollingQueueEntryState::Queued
        );
        // Re-gated after the restart and the daemon stopped again: suspect.
        gate_again(&store, entry).await;
        assert_eq!(reconcile_gating(&store).await.unwrap(), (1, 0));
        let guard = store.lock().await;
        let settled = guard.get_rolling_queue_entry(entry).unwrap().unwrap();
        assert_eq!(settled.state, RollingQueueEntryState::Refused);
        let outcome = settled.outcome.expect("a refusal outcome");
        assert_eq!(outcome.refusal.as_deref(), Some(QUEUE_SUSPECT_CRASH_LOOP));
        assert!(
            outcome
                .detail
                .unwrap_or_default()
                .contains("stopped 2 times"),
            "the refusal names the stops"
        );
        let wakes = guard
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .count();
        assert_eq!(wakes, 1, "the enqueuer is told once");
        drop(guard);
        // A third boot has nothing left to reconcile.
        assert_eq!(reconcile_gating(&store).await.unwrap(), (0, 0));
    }

    /// A kill before terminal settlement leaves a gating entry for the next
    /// daemon to re-admit in place. Cached ancestry after a failed fetch is
    /// insufficient publication proof.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn interrupted_gating_restart_preserves_identity_and_fifo_and_fails_closed() {
        for (on_rolling, fetch_ok) in [(true, true), (false, true), (true, false)] {
            let (dir, _origin, work) = repos();
            let source = if on_rolling {
                head(&work)
            } else {
                commit_file(&work, "pending", "pending", "pending")
            };
            let database = dir.path().join("restart.db");
            let store = Arc::new(rsid_core::store_lock::Mutex::new(
                Store::open_test_fixture(&database).unwrap(),
            ));
            let session = Uuid::new_v4();
            let id = queued_by(&store, &work, &source, "interrupted", session).await;
            let before = entry_of(&store, id).await;
            let later_source = commit_file(&work, "later", "later", "later");
            let later = queued(&store, &work, &later_source, "later").await;
            let claim = store
                .lock()
                .await
                .claim_next_rolling_queue_entry(Utc::now())
                .unwrap()
                .unwrap();
            assert_eq!(claim.entry.id, id);
            assert_eq!(
                entry_of(&store, id).await.state,
                RollingQueueEntryState::Gating
            );
            assert_eq!(wake_count(&store).await, 0);
            // The daemon/lander dies here, before writing any terminal state.
            drop(store);
            if !fetch_ok {
                sh(
                    &work,
                    "git remote set-url origin /nonexistent/rsi-unreachable-origin.git",
                );
            }
            let store = Arc::new(rsid_core::store_lock::Mutex::new(
                Store::open_test_fixture(&database).unwrap(),
            ));
            let published = on_rolling && fetch_ok;
            assert_eq!(
                reconcile_gating(&store).await.unwrap(),
                if published { (1, 0) } else { (0, 1) }
            );
            assert_eq!(reconcile_gating(&store).await.unwrap(), (0, 0));
            let recovered = entry_of(&store, id).await;
            assert_eq!(recovered.id, before.id);
            assert_eq!(recovered.sequence, before.sequence);
            assert_eq!(recovered.source_commit, source);
            assert_eq!(recovered.source_session_id, session);
            assert_eq!(recovered.owner_epic_id, before.owner_epic_id);
            assert_eq!(recovered.binding, before.binding);
            assert_eq!(recovered.work_key, before.work_key);
            assert_eq!(recovered.enqueued_at, before.enqueued_at);
            assert_eq!(
                recovered.state,
                if published {
                    RollingQueueEntryState::Published
                } else {
                    RollingQueueEntryState::Queued
                }
            );
            assert_eq!(wake_count(&store).await, usize::from(published));
            if published {
                assert_eq!(
                    recovered.outcome.unwrap().landed_sha.as_deref(),
                    Some(source.as_str())
                );
            } else {
                assert_eq!(recovered.finished_at, None);
                assert_eq!(recovered.outcome, None);
                assert!(recovered.sequence < entry_of(&store, later).await.sequence);
                let claim = store
                    .lock()
                    .await
                    .claim_next_rolling_queue_entry(Utc::now())
                    .unwrap()
                    .unwrap();
                assert_eq!(claim.entry.id, id);
                assert_eq!(claim.entry.sequence, before.sequence);
            }
            // The replay key still finds the original entry after reopening
            // the database and re-admitting it, with no duplicate enqueue.
            assert_eq!(
                queued_by(&store, &work, &source, "interrupted", session).await,
                id
            );
            let entries = store
                .lock()
                .await
                .list_rolling_queue_entries(None, 10)
                .unwrap();
            assert_eq!(
                entries
                    .iter()
                    .map(|(entry, _)| entry.id)
                    .collect::<Vec<_>>(),
                vec![id, later]
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_interrupted_batch_requeues_the_same_entries_without_terminal_wakes() {
        let (dir, _origin, work) = repos();
        let first = commit_file(&work, "one", "one", "one");
        let second = commit_file(&work, "two", "two", "two");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = [
            queued(&store, &work, &first, "one").await,
            queued(&store, &work, &second, "two").await,
        ];
        let launcher = fake_lander(
            dir.path(),
            "echo publication_status=unknown; echo 'landing interrupted' >&2; exit 3",
        );
        for _ in 0..2 {
            let results = run_next_batch(&store, &launcher, 2).await.unwrap();
            assert_eq!(results.iter().map(|(id, _)| *id).collect::<Vec<_>>(), ids);
            let guard = store.lock().await;
            let entries = guard.list_rolling_queue_entries(None, 10).unwrap();
            assert_eq!(entries.len(), 2);
            for ((entry, _), id) in entries.iter().zip(ids) {
                assert_eq!(entry.id, id);
                assert_eq!(entry.state, RollingQueueEntryState::Queued);
            }
            assert_eq!(guard.list_scheduled_jobs().unwrap().len(), 0);
        }
    }

    const REMOTE_READ_LANDER: &str = "echo publication_status=not_published; \
         echo failure_class=remote_read_unavailable; \
         echo 'rsi-rolling-land: cannot read remote rolling tip: git ls-remote exited 128' >&2; \
         exit 1";

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_remote_read_failure_classifies_as_a_requeue_and_a_red_gate_still_refuses() {
        let remote = classify_run(
            Some(1),
            "publication_status=not_published\nfailure_class=remote_read_unavailable\n",
            "rsi-rolling-land: cannot read remote rolling tip: git ls-remote exited 128",
            || None,
        );
        assert_eq!(remote.state, RollingQueueEntryState::Queued);
        assert!(is_remote_read_unavailable(&remote));
        assert!(!is_bisectable(&remote));
        let detail = remote.outcome.detail.unwrap();
        assert!(detail.contains("cause=remote_read_unavailable"), "{detail}");
        let red = classify_run(
            Some(1),
            "publication_status=not_published\n",
            "test q::red ... FAILED",
            || None,
        );
        assert_eq!(red.state, RollingQueueEntryState::Refused);
        assert!(!is_remote_read_unavailable(&red));
        assert!(is_bisectable(&red));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_remote_read_failure_after_a_green_gate_requeues_the_batch_without_bisecting() {
        let (dir, _origin, work) = repos();
        let first = commit_file(&work, "one", "one", "one");
        let second = commit_file(&work, "two", "two", "two");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = [
            queued(&store, &work, &first, "one").await,
            queued(&store, &work, &second, "two").await,
        ];
        let launcher = fake_lander(dir.path(), REMOTE_READ_LANDER);
        let results = run_next_batch(&store, &launcher, 2).await.unwrap();
        assert_eq!(results.iter().map(|(id, _)| *id).collect::<Vec<_>>(), ids);
        for (_, result) in &results {
            assert!(is_remote_read_unavailable(result), "{result:?}");
        }
        for id in ids {
            let entry = entry_of(&store, id).await;
            assert_eq!(entry.state, RollingQueueEntryState::Queued);
            assert!(entry.outcome.is_none());
        }
        assert_eq!(wake_count(&store).await, 0);
        let guard = store.lock().await;
        assert_eq!(events_of(&guard, "requeued_remote_read").len(), 2);
        assert_eq!(events_of(&guard, "remote_read_unavailable").len(), 1);
        assert!(events_of(&guard, "bisect_started").is_empty());
        assert!(events_of(&guard, "bisect_isolated").is_empty());
        drop(guard);
        // The batch keeps its positions: it is claimed again in the same order.
        let again = run_next_batch(&store, &launcher, 2).await.unwrap();
        assert_eq!(again.iter().map(|(id, _)| *id).collect::<Vec<_>>(), ids);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_permanent_remote_read_outage_fails_typed_after_the_requeue_limit() {
        let (dir, _origin, work) = repos();
        let sha = commit_file(&work, "one", "one", "one");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let id = queued(&store, &work, &sha, "one").await;
        let launcher = fake_lander(dir.path(), REMOTE_READ_LANDER);
        for _ in 0..QUEUE_REMOTE_READ_REQUEUE_LIMIT {
            run_next_batch(&store, &launcher, 1).await.unwrap();
            assert_eq!(
                entry_of(&store, id).await.state,
                RollingQueueEntryState::Queued
            );
        }
        let last = run_next_batch(&store, &launcher, 1).await.unwrap();
        assert_eq!(last.len(), 1);
        let entry = entry_of(&store, id).await;
        assert_eq!(entry.state, RollingQueueEntryState::Failed);
        let outcome = entry.outcome.unwrap();
        assert_eq!(
            outcome.refusal.as_deref(),
            Some(QUEUE_REMOTE_READ_UNAVAILABLE)
        );
        let detail = outcome.detail.unwrap();
        assert!(detail.contains("cause=remote_read_unavailable"), "{detail}");
        assert_eq!(wake_count(&store).await, 1);
        assert!(
            run_next_batch(&store, &launcher, 1)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_remote_read_failure_in_a_bisect_round_requeues_the_batch_and_isolates_nobody() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let mark = dir.path().join("first-run-done");
        // The first run is red and names two members' files (a real bisect);
        // every later run cannot read the remote.
        let guard = format!(
            "if [ ! -f {mark} ]; then touch {mark}; echo 'error[E0425]: boom' >&2; \
             echo ' --> a:1:1' >&2; echo ' --> d:1:1' >&2; \
             echo publication_status=not_published; exit 1; fi\n{REMOTE_READ_LANDER}",
            mark = mark.display()
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 4, "every member is accounted for once");
        for id in ids {
            let entry = entry_of(&store, id).await;
            assert_eq!(entry.state, RollingQueueEntryState::Queued, "{entry:?}");
            assert!(entry.outcome.is_none());
        }
        assert_eq!(lander_runs(&log).len(), 2, "one gate, one bisect round");
        assert_eq!(wake_count(&store).await, 0);
        let guard = store.lock().await;
        assert_eq!(events_of(&guard, "bisect_started").len(), 1);
        assert!(events_of(&guard, "bisect_isolated").is_empty());
        assert_eq!(events_of(&guard, "requeued_remote_read").len(), 4);
    }

    fn member(id: u128, derived: Option<u32>, declared: bool) -> MigrationMember {
        MigrationMember {
            id: Uuid::from_u128(id),
            derived,
            declared,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn declared_migrations_are_numbered_tip_plus_one_in_queue_order() {
        let plan = plan_migration_order(
            148,
            &[
                member(1, Some(149), true),
                member(2, None, false),
                member(3, Some(149), true),
            ],
        );
        assert_eq!(
            plan.assigned,
            vec![(Uuid::from_u128(1), 149), (Uuid::from_u128(3), 150)]
        );
        assert!(plan.out_of_order.is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_fixed_migration_number_taken_by_a_predecessor_is_out_of_order() {
        let plan = plan_migration_order(
            148,
            &[member(1, Some(149), false), member(2, Some(149), false)],
        );
        assert_eq!(plan.assigned, vec![(Uuid::from_u128(1), 149)]);
        assert_eq!(plan.out_of_order, vec![Uuid::from_u128(2)]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_batch_gates_the_union_of_filters_and_a_rust_member_without_filters_widens_it() {
        let mut a = queued_entry_view("a", &["rsid=x", "rsi=y"]);
        let b = queued_entry_view("b", &["rsid=x", "rsi-common=z"]);
        assert_eq!(
            union_filters(&[a.clone(), b.clone()], |_| false),
            vec!["rsid=x", "rsi=y", "rsi-common=z"]
        );
        a.test_filters.clear();
        assert!(union_filters(&[a, b], |_| false).is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_rust_free_member_without_filters_does_not_widen_the_batch() {
        let a = queued_entry_view("a", &["rsid=x", "rsi=y"]);
        let script = queued_entry_view("b", &[]);
        let rust = queued_entry_view("c", &[]);
        let script_commit = script.source_commit.clone();
        let rust_free = |entry: &RollingQueueEntryV1| entry.source_commit == script_commit;
        assert_eq!(
            union_filters(&[a.clone(), script.clone()], rust_free),
            vec!["rsid=x", "rsi=y"]
        );
        // A member with Rust changes and no filters still widens the batch.
        assert!(union_filters(&[a, script, rust], rust_free).is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn only_rust_cargo_and_migration_paths_affect_the_gate() {
        for path in [
            "crates/rsid/src/lib.rs",
            "crates/rsid-store/src/store/migrations/v090.rs",
            "crates/rsid/build.rs",
            "Cargo.toml",
            "crates/rsid/Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            ".cargo/config.toml",
        ] {
            assert!(path_affects_rust_gate(path), "{path}");
        }
        for path in [
            "scripts/check-touched-shards",
            "docs/agents/routing.md",
            "thoughts/shared/notes/x.md",
            "tools/provisional-migrations/v091.json",
            ".claude/commands/foo.md",
        ] {
            assert!(!path_affects_rust_gate(path), "{path}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn source_rust_freedom_reads_the_diff_against_rolling() {
        let (_dir, repo) = rust_free_repo();
        let git_in = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "{args:?}");
        };
        let head = |branch: &str| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["rev-parse", branch])
                .output()
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        git_in(&["checkout", "-q", "-b", "docs", "rolling"]);
        std::fs::create_dir_all(repo.join("scripts")).unwrap();
        std::fs::write(repo.join("scripts/tool.sh"), "echo hi\n").unwrap();
        git_in(&["add", "scripts/tool.sh"]);
        git_in(&["commit", "-q", "-m", "script"]);
        git_in(&["checkout", "-q", "-b", "code", "rolling"]);
        std::fs::write(repo.join("lib.rs"), "fn a() {}\n").unwrap();
        git_in(&["add", "lib.rs"]);
        git_in(&["commit", "-q", "-m", "code"]);
        assert!(source_is_rust_free(&repo, &head("docs")));
        assert!(!source_is_rust_free(&repo, &head("code")));
        // An unknown revision never narrows the gate.
        assert!(!source_is_rust_free(&repo, &"0".repeat(40)));
    }

    /// #2814: a docs-only source with two merge bases (it merged an earlier
    /// rolling commit that rolling then moved on from) is Rust-free by its
    /// merge result, not by the merge-base diff that re-counts rolling's Rust.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_source_with_several_merge_bases_is_judged_by_its_merge_result() {
        let (_dir, repo) = rust_free_repo();
        let git_in = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "{args:?}");
        };
        let commit_file = |path: &str, text: &str, message: &str| {
            let full = repo.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, text).unwrap();
            git_in(&["add", path]);
            git_in(&["commit", "-q", "-m", message]);
        };
        let rev = |name: &str| git(&repo, &["rev-parse", name]).unwrap().trim().to_string();
        git_in(&["checkout", "-q", "-b", "worker", "rolling"]);
        commit_file("lib.rs", "fn a() {}\n", "worker first");
        let worker_first = rev("HEAD");
        git_in(&["checkout", "-q", "rolling"]);
        commit_file("other.rs", "fn b() {}\n", "rolling first");
        let rolling_first = rev("HEAD");
        git_in(&[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "rolling takes worker",
            &worker_first,
        ]);
        git_in(&["checkout", "-q", "worker"]);
        git_in(&[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "worker takes rolling",
            &rolling_first,
        ]);
        commit_file("docs/note.md", "# note\n", "worker docs");
        let source = rev("HEAD");
        git_in(&["checkout", "-q", "rolling"]);
        commit_file("lib.rs", "fn a() { let _x = 1; }\n", "rolling moves on");
        let bases = git(&repo, &["merge-base", "--all", &source, "rolling"]).unwrap();
        assert_eq!(bases.split_whitespace().count(), 2, "{bases}");
        assert_eq!(
            source_changed_paths(&repo, &source),
            DeltaRead::Paths(vec!["docs/note.md".to_string()])
        );
        assert!(source_is_rust_free(&repo, &source));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn derived_filters_come_from_the_checker_normalized_and_fall_back_to_none() {
        let (_dir, repo) = rust_free_repo();
        let head = git(&repo, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_owned();
        // No checker script: the unfiltered gate.
        assert!(derive_test_filters(&repo, &head).await.is_empty());
        std::fs::create_dir_all(repo.join("scripts")).unwrap();
        let mut suggested: Vec<String> = (0..40).map(|i| format!("rsi=module_{i}")).collect();
        suggested.push("rsid=shard:store-01:test(same)".into());
        suggested.push("rsid-store=shard:store-02:test(same)".into());
        std::fs::write(
            repo.join("scripts/check-touched-shards"),
            format!(
                "import json, sys\nassert '--filters-only' in sys.argv and '--json' in sys.argv\nprint(json.dumps({{'suggested_filters': {}}}))\n",
                serde_json::to_string(&suggested).unwrap().replace('"', "'")
            ),
        )
        .unwrap();
        let derived = derive_test_filters(&repo, &head).await;
        assert_eq!(derived.len(), 41);
        assert_eq!(derived[40], "rsid=shard:store-01:test(same)");
        assert!(parse_suggested_filters("not json").is_empty());
        // One malformed entry voids the whole set (the unfiltered gate).
        assert!(
            parse_suggested_filters(r#"{"suggested_filters": ["rsi=a", null, "rsi=b"]}"#)
                .is_empty()
        );
        assert!(parse_suggested_filters(r#"{"suggested_filters": ["rsi=a", "bad"]}"#).is_empty());
        assert_eq!(
            parse_suggested_filters(r#"{"suggested_filters": ["rsi=a", "rsi=b"]}"#),
            vec!["rsi=a", "rsi=b"]
        );
    }

    fn rust_free_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().to_path_buf();
        for args in [
            vec!["init", "-q", "-b", "rolling"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(&args)
                .status()
                .unwrap();
            assert!(status.success());
        }
        std::fs::write(repo.join("README"), "x\n").unwrap();
        for args in [vec!["add", "README"], vec!["commit", "-q", "-m", "base"]] {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(&args)
                .status()
                .unwrap();
            assert!(status.success());
        }
        (dir, repo)
    }

    fn queued_entry_view(commit: &str, filters: &[&str]) -> RollingQueueEntryV1 {
        RollingQueueEntryV1 {
            issue: None,
            sequence: 1,
            id: Uuid::new_v4(),
            source_commit: commit.repeat(40),
            source_session_id: Uuid::new_v4(),
            owner_epic_id: None,
            binding: RollingQueueBinding::Unbound,
            work_key: None,
            migration_version: None,
            hot_files: vec![],
            derived_test_filters: Vec::new(),
            test_filters: filters.iter().map(|f| (*f).to_string()).collect(),
            fixes_tests: Vec::new(),
            filters_state: None,
            expedite_reason: None,
            state: RollingQueueEntryState::Queued,
            outcome: None,
            enqueued_at: String::new(),
            finished_at: None,
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_merge_conflict_is_typed_and_not_a_red_gate() {
        let result = classify_run(
            Some(1),
            "publication_status=not_published\n",
            "rsi-rolling-land: integration refused: Conflict { paths: [\"f\"] }",
            || None,
        );
        assert_eq!(result.state, RollingQueueEntryState::Refused);
        assert_eq!(
            result.outcome.refusal.as_deref(),
            Some(QUEUE_BATCH_MERGE_CONFLICT)
        );
        let policy = as_batch_result(classify_run(
            Some(8),
            "policy_fence=migration_number\n",
            "refused",
            || None,
        ));
        assert_eq!(
            policy.outcome.refusal.as_deref(),
            Some(QUEUE_BATCH_POLICY_REFUSED)
        );
        assert!(
            policy
                .outcome
                .detail
                .unwrap()
                .contains("policy_fence=migration_number")
        );
    }

    /// A fake lander that really merges its `--accepted` sources onto
    /// `origin/rolling` in `work` and pushes once, like the real one on green.
    /// `guard` is shell run first (it sees `"$@"`); `@SKIP@` names a source the
    /// lander reports landed without merging.
    fn merging_lander(dir: &Path, work: &Path, log: &Path, guard: &str) -> LanderLauncher {
        let body = r#"
echo "run $*" >> @LOG@
echo "budget $RSI_LANDER_REGATE_BUDGET_SECS" >> @LOG@
@GUARD@
cd @WORK@ || exit 9
git fetch -q origin rolling
git checkout -q -B land origin/rolling
fetched=$(git rev-parse HEAD)
while [ $# -gt 0 ]; do
  if [ "$1" = --accepted ]; then
    if [ "$2" != "$SKIP" ]; then
      git merge -q --no-edit "$2" >/dev/null 2>&1 || { git merge --abort; echo "integration refused: Conflict { paths: [\"f\"] }; conflict_source=$2" >&2; echo publication_status=not_published; exit 1; }
    fi
    shift
  fi
  shift
done
git push -q origin HEAD:rolling && echo push >> @LOG@
echo publication_status=published
echo fetched_target_id=$fetched
echo candidate_id=$(git rev-parse HEAD)
echo published_target_id=$(git rev-parse HEAD)
"#
        .replace("@LOG@", &log.display().to_string())
        .replace("@WORK@", &work.display().to_string())
        .replace("@GUARD@", guard);
        fake_lander(dir, &body)
    }

    fn commit_file(work: &Path, branch: &str, file: &str, content: &str) -> String {
        sh(
            work,
            &format!(
                "git checkout -q -B {branch} origin/rolling && printf '{content}\\n' > {file} \
                 && git add {file} && git commit -qm {branch}"
            ),
        );
        head(work)
    }

    async fn queue_all(
        store: &Arc<rsid_core::store_lock::Mutex<Store>>,
        work: &Path,
        shas: &[String],
        migration: Option<u32>,
    ) -> Vec<Uuid> {
        let mut ids = Vec::new();
        for (index, sha) in shas.iter().enumerate() {
            let id = queued(store, work, sha, &format!("k{index}")).await;
            if let Some(version) = migration {
                store
                    .lock()
                    .await
                    .conn
                    .execute(
                        "UPDATE rolling_queue_entries SET migration_version=?2 WHERE id=?1",
                        rusqlite::params![id.to_string(), version],
                    )
                    .unwrap();
            }
            ids.push(id);
        }
        ids
    }

    async fn entry_of(
        store: &Arc<rsid_core::store_lock::Mutex<Store>>,
        id: Uuid,
    ) -> RollingQueueEntryV1 {
        store
            .lock()
            .await
            .get_rolling_queue_entry(id)
            .unwrap()
            .unwrap()
    }

    fn log_lines(log: &Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn is_ancestor_of_origin(work: &Path, sha: &str) -> bool {
        let _ = git(work, &["fetch", "--quiet", "origin", "rolling"]);
        git(
            work,
            &["merge-base", "--is-ancestor", sha, "origin/rolling"],
        )
        .is_some()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_green_batch_publishes_every_source_with_one_push_and_verifies_ancestry() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 4);
        // FIFO: settled in enqueue order.
        assert_eq!(results.iter().map(|r| r.0).collect::<Vec<_>>(), ids);
        assert!(
            results
                .iter()
                .all(|(_, r)| r.state == RollingQueueEntryState::Published)
        );
        let lines = log_lines(&log);
        assert_eq!(
            lines.iter().filter(|l| l.starts_with("run ")).count(),
            1,
            "one lander run for the whole batch: {lines:?}"
        );
        assert_eq!(lines.iter().filter(|l| *l == "push").count(), 1);
        let run = &lines[0];
        for sha in &shas {
            assert!(run.contains(&format!("--accepted {sha}")), "{run}");
            assert!(is_ancestor_of_origin(&work, sha));
        }
        // The batch is one row; every owner is woken once.
        let guard = store.lock().await;
        let (state, members): (String, i64) = guard
            .conn
            .query_row(
                "SELECT state, member_count FROM rolling_queue_batches",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((state.as_str(), members), ("published", 4));
        let wakes = guard
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .count();
        assert_eq!(wakes, 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn handwritten_filter_in_any_member_keeps_derived_empty_fallback_strict() {
        let mut derived = queued_entry_view("a", &["rsid=shard:other-02:test(fixture)"]);
        derived.derived_test_filters = derived.test_filters.clone();
        assert_eq!(
            union_derived_filters(&[derived.clone()]),
            derived.test_filters
        );
        let explicit = queued_entry_view("b", &["rsid=shard:other-02:test(fixture)"]);
        assert_eq!(
            union_derived_filters(&[derived, explicit]),
            Vec::<String>::new()
        );
    }

    /// A detached lander run for the tests: output and status in files under
    /// the queue workspace, no systemd scope.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn detached_lander(dir: &Path, body: &str) -> LanderLauncher {
        fake_lander(dir, body)
            .with_workspace(dir.join("queue"))
            .with_detached_runs()
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn run_dir_entries(dir: &Path) -> usize {
        std::fs::read_dir(dir.join("queue").join(detached_run::RUNS_SUBDIR))
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn process_is_gone(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            // `pid (comm) S ...`: a zombie is already dead, only unreaped.
            Ok(stat) => stat
                .rsplit_once(") ")
                .is_some_and(|(_, rest)| rest.starts_with('Z')),
            Err(_) => true,
        }
    }

    /// #3435: the detached path returns what the attached one did.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_detached_lander_run_reports_its_output_and_exit_status_and_leaves_no_files() {
        let dir = crate::test_support::disk_backed_tempdir("detached-lander-output");
        let launcher = detached_lander(dir.path(), "echo gate_out; echo gate_err >&2; exit 3");
        let output = run_lander(
            &launcher,
            dir.path(),
            &[queued_entry_view("a", &[])],
            None,
            tokio::time::Instant::now() + Duration::from_secs(60),
            None,
        )
        .await
        .unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stdout, b"gate_out\n");
        assert_eq!(output.stderr, b"gate_err\n");
        assert_eq!(run_dir_entries(dir.path()), 0);
    }

    /// #3435: a daemon restart in the middle of a gate does not restart the
    /// gate. The first daemon's future is dropped mid-run (its process is
    /// gone), the next daemon asks for the same gate and gets the first run's
    /// result; the lander ran once.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn the_next_daemon_adopts_the_lander_run_a_restart_interrupted() {
        let dir = crate::test_support::disk_backed_tempdir("detached-lander-adopt");
        let starts = dir.path().join("starts");
        let body = format!("echo start >> {}\nsleep 2\necho gated", starts.display());
        let launcher = detached_lander(dir.path(), &body);
        let entry = queued_entry_view("a", &[]);

        let interrupted = tokio::time::timeout(
            Duration::from_millis(500),
            run_lander(
                &launcher,
                dir.path(),
                &[entry.clone()],
                None,
                tokio::time::Instant::now() + Duration::from_secs(60),
                None,
            ),
        )
        .await;
        assert!(interrupted.is_err(), "the daemon went away mid-run");
        assert_eq!(
            run_dir_entries(dir.path()),
            1,
            "the run stays for its successor"
        );

        let mut next = launcher.clone();
        next.run_epoch = Some("the-next-daemon".into());
        let output = run_lander(
            &next,
            dir.path(),
            &[entry],
            None,
            tokio::time::Instant::now() + Duration::from_secs(60),
            None,
        )
        .await
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"gated\n");
        assert_eq!(
            std::fs::read_to_string(&starts).unwrap().lines().count(),
            1,
            "adopting must not gate the batch again"
        );
        assert_eq!(run_dir_entries(dir.path()), 0);
    }

    /// #3435: a different gate never inherits another's result, and the
    /// inherited run is stopped so two landers do not share one worktree.
    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_different_gate_stops_the_inherited_run_and_starts_fresh() {
        let dir = crate::test_support::disk_backed_tempdir("detached-lander-other");
        let pids = dir.path().join("pids");
        let body = format!("echo $$ >> {}\nsleep 30\necho gated", pids.display());
        let launcher = detached_lander(dir.path(), &body);
        let interrupted = tokio::time::timeout(
            Duration::from_millis(500),
            run_lander(
                &launcher,
                dir.path(),
                &[queued_entry_view("a", &[])],
                None,
                tokio::time::Instant::now() + Duration::from_secs(60),
                None,
            ),
        )
        .await;
        assert!(interrupted.is_err());
        let inherited: u32 = std::fs::read_to_string(&pids)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(!process_is_gone(inherited));

        let mut next = launcher.clone();
        next.run_epoch = Some("the-next-daemon".into());
        // The new run is bounded by its deadline; it only has to start.
        let result = run_lander(
            &next,
            dir.path(),
            &[queued_entry_view("b", &[])],
            None,
            tokio::time::Instant::now() + Duration::from_millis(700),
            None,
        )
        .await;
        assert!(matches!(result, Err(LanderError::TimedOut)));
        assert_eq!(std::fs::read_to_string(&pids).unwrap().lines().count(), 2);
        for _ in 0..50 {
            if process_is_gone(inherited) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            process_is_gone(inherited),
            "the inherited lander was stopped"
        );
        assert_eq!(run_dir_entries(dir.path()), 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    /// More than 128 KiB of derived filters (#3615): one environment string
    /// that large fails `execve` with E2BIG, so the set must arrive intact
    /// through the file, with no inline copy.
    fn oversize_derived_entry() -> (RollingQueueEntryV1, Vec<String>) {
        let filters: Vec<String> = (0..1800)
            .map(|index| {
                format!(
                    "rsid=shard:other-02:test(fixture_{index:05}_{})",
                    "x".repeat(90)
                )
            })
            .collect();
        let mut entry = queued_entry_view("a", &[]);
        entry.test_filters = filters.clone();
        entry.derived_test_filters = filters.clone();
        let inline = serde_json::to_string(&filters).unwrap();
        assert!(
            inline.len() > 128 * 1024,
            "the fixture must pass MAX_ARG_STRLEN"
        );
        (entry, filters)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    const READ_DERIVED_FILE: &str = "test -z \"${RSI_LANDER_DERIVED_FILTERS+set}\" || exit 7\n\
         cat \"$RSI_LANDER_DERIVED_FILTERS_FILE\"";

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn lander_receives_more_than_128_kib_of_derived_filters_through_a_file() {
        let dir = crate::test_support::disk_backed_tempdir("derived-filter-file");
        let mut launcher = fake_lander(dir.path(), READ_DERIVED_FILE);
        launcher.inherited_env.push((
            rsi_common::rolling_queue::DERIVED_FILTERS_INLINE_ENV.into(),
            "[\"stale-outer-gate\"]".into(),
        ));
        let (entry, filters) = oversize_derived_entry();
        let output = run_lander(
            &launcher,
            dir.path(),
            &[entry],
            None,
            tokio::time::Instant::now() + Duration::from_secs(60),
            None,
        )
        .await
        .unwrap();
        assert!(output.status.success(), "{output:?}");
        let received: Vec<String> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(received, filters);
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_detached_lander_receives_more_than_128_kib_of_derived_filters() {
        let dir = crate::test_support::disk_backed_tempdir("derived-filter-file-detached");
        let mut launcher = detached_lander(dir.path(), READ_DERIVED_FILE);
        launcher.inherited_env.push((
            rsi_common::rolling_queue::DERIVED_FILTERS_INLINE_ENV.into(),
            "[\"stale-outer-gate\"]".into(),
        ));
        let (entry, filters) = oversize_derived_entry();
        let output = run_lander(
            &launcher,
            dir.path(),
            &[entry],
            None,
            tokio::time::Instant::now() + Duration::from_secs(60),
            None,
        )
        .await
        .unwrap();
        assert!(output.status.success(), "{output:?}");
        let received: Vec<String> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(received, filters);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_derived_filter_file_is_bounded_and_typed() {
        use rsi_common::rolling_queue::{DERIVED_FILTERS_FILE_MAX_BYTES, read_derived_filters};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("derived.json");
        std::fs::write(&path, r#"["a=b","c=d"]"#).unwrap();
        let read = read_derived_filters(Some(path.as_os_str()), Some("[\"ignored\"]")).unwrap();
        assert_eq!(read.into_iter().collect::<Vec<_>>(), ["a=b", "c=d"]);
        std::fs::write(&path, "not json").unwrap();
        let error = read_derived_filters(Some(path.as_os_str()), None).unwrap_err();
        assert!(
            error.starts_with("queue_derived_filters_invalid: "),
            "{error}"
        );
        std::fs::write(
            &path,
            vec![b' '; DERIVED_FILTERS_FILE_MAX_BYTES as usize + 1],
        )
        .unwrap();
        let error = read_derived_filters(Some(path.as_os_str()), None).unwrap_err();
        assert!(error.contains("over the"), "{error}");
        let missing = dir.path().join("missing.json");
        assert!(read_derived_filters(Some(missing.as_os_str()), None).is_err());
        assert!(read_derived_filters(None, None).unwrap().is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn lander_receives_derived_filter_provenance() {
        let dir = crate::test_support::disk_backed_tempdir("derived-filter-transport");
        let mut launcher = fake_lander(dir.path(), READ_DERIVED_FILE);
        launcher.inherited_env.push((
            rsi_common::rolling_queue::DERIVED_FILTERS_INLINE_ENV.into(),
            "[\"stale-outer-gate\"]".into(),
        ));
        let mut entry = queued_entry_view("a", &["rsid=shard:other-02:test(fixture)"]);
        entry.derived_test_filters = entry.test_filters.clone();
        let output = run_lander(
            &launcher,
            dir.path(),
            &[entry.clone()],
            None,
            tokio::time::Instant::now() + Duration::from_secs(60),
            None,
        )
        .await
        .unwrap();
        assert!(output.status.success());
        let received: Vec<String> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(received, entry.test_filters);
    }

    /// #2112: the lander reports queue-derived filters as explicit
    /// (`selection=closure reason=explicit_filters`), so the queue records the
    /// async full-library obligation itself on publish. Caller-named filters
    /// stay exempt; an unfiltered gate is left to the lander's own evidence.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_derived_filter_publish_owes_the_full_library_run_and_explicit_filters_do_not() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        // `b` was enqueued with operator-named filters; `c` gates unfiltered.
        note_explicit_filters(ids[1]);
        store
            .lock()
            .await
            .conn
            .execute(
                "UPDATE rolling_queue_entries SET test_filters_json='[]' WHERE id=?1",
                [ids[2].to_string()],
            )
            .unwrap();
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 3);
        assert!(
            results
                .iter()
                .all(|(_, r)| r.state == RollingQueueEntryState::Published)
        );
        let owed = store.lock().await.bounded_qa_landings().unwrap();
        assert_eq!(
            owed.iter().map(|w| w.entry.id).collect::<Vec<_>>(),
            [ids[0]]
        );
        assert_eq!(owed[0].job_id, None, "pending until the QA task dispatches");
        assert!(
            !has_explicit_filters(ids[1]),
            "settled entries are forgotten"
        );
    }

    /// A rolling base with two workspace packages, `tui` and `daemon`, where
    /// `daemon` depends on `common` and `tui` does not; one source per path.
    fn lane_workspace() -> (tempfile::TempDir, PathBuf, Vec<String>) {
        let (dir, _origin, work) = repos();
        sh(
            &work,
            "mkdir -p crates/common/src crates/daemon/src crates/tui/src \
             && printf '[workspace]\\nmembers = []\\n' > Cargo.toml \
             && printf '[package]\\nname = \"common\"\\n' > crates/common/Cargo.toml \
             && printf '[package]\\nname = \"daemon\"\\n[dependencies]\\ncommon = { path = \"../common\" }\\n' > crates/daemon/Cargo.toml \
             && printf '[package]\\nname = \"tui\"\\n' > crates/tui/Cargo.toml \
             && git add -A && git commit -qm workspace && git push -q origin rolling \
             && git fetch -q origin rolling",
        );
        let mut sources = Vec::new();
        for (branch, file) in [
            ("daemon-one", "crates/daemon/src/lib.rs"),
            ("tui-one", "crates/tui/src/lib.rs"),
            ("common-one", "crates/common/src/lib.rs"),
            ("manifest", "crates/tui/Cargo.toml"),
        ] {
            sh(
                &work,
                &format!(
                    "git checkout -q -B {branch} origin/rolling && printf '// {branch}\\n' >> {file} \
                     && git add {file} && git commit -qm {branch}"
                ),
            );
            sources.push(head(&work));
        }
        (dir, work, sources)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_disjoint_source_claims_a_second_lane_beside_a_gating_batch_1224() {
        let (_dir, work, sources) = lane_workspace();
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &sources, None).await;
        // The daemon source gates first (nothing else is gating: the plain claim).
        let first = claim_batch_in_lanes(&store, 1).await.unwrap().unwrap();
        assert_eq!(first.entries[0].id, ids[0]);
        assert!(!first.parallel_lane);
        // The TUI source shares no package with it: a second lane.
        let second = claim_batch_in_lanes(&store, 1).await.unwrap().unwrap();
        assert_eq!(second.entries[0].id, ids[1]);
        assert!(second.parallel_lane);
        // Two lanes at most.
        assert!(claim_batch_in_lanes(&store, 1).await.unwrap().is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_overlapping_or_unattributable_source_waits_for_the_gating_batch_1224() {
        let (_dir, work, sources) = lane_workspace();
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        // daemon gates; `common` is a dependency of it and `manifest` is a
        // Cargo.toml change: neither may share the gate, and the manifest
        // entry behind `common` stays behind it.
        let queue = [sources[0].clone(), sources[2].clone(), sources[3].clone()];
        let ids = queue_all(&store, &work, &queue, None).await;
        let first = claim_batch_in_lanes(&store, 1).await.unwrap().unwrap();
        assert_eq!(first.entries[0].id, ids[0]);
        assert!(claim_batch_in_lanes(&store, 1).await.unwrap().is_none());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn enqueue_warns_when_the_source_conflicts_with_an_entry_ahead_but_not_a_disjoint_one_2118() {
        let (_dir, _origin, work) = repos();
        let ahead = commit_file(&work, "ahead", "shared", "ahead");
        let disjoint = commit_file(&work, "disjoint", "elsewhere", "x");
        let conflicting = commit_file(&work, "conflicting", "shared", "other");
        let queue = [(Uuid::new_v4(), 7, ahead), (Uuid::new_v4(), 8, disjoint)];
        let warnings = queue_conflict_warnings(&work, &conflicting, &queue);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(
            warnings[0].code,
            rsi_common::rolling_queue::QUEUE_CONFLICTS_WITH_ENTRY
        );
        assert_eq!(warnings[0].entry_id, Some(queue[0].0));
        assert_eq!(warnings[0].entry_sequence, Some(7));
        assert_eq!(warnings[0].files, ["shared"]);
        assert!(queue_conflict_warnings(&work, &queue[1].2, &queue[..1]).is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn enqueue_warns_when_the_source_conflicts_with_the_rolling_tip_2118() {
        let (_dir, _origin, work) = repos();
        let stale = commit_file(&work, "stale", "shared", "stale");
        sh(
            &work,
            "git checkout -q -B advance origin/rolling && printf 'tip\\n' > shared \
             && git add shared && git commit -qm advance && git push -q origin HEAD:rolling \
             && git fetch -q origin rolling",
        );
        let warnings = queue_conflict_warnings(&work, &stale, &[]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(
            warnings[0].code,
            rsi_common::rolling_queue::QUEUE_CONFLICTS_WITH_ROLLING
        );
        assert_eq!(warnings[0].entry_id, None);
        assert_eq!(warnings[0].files, ["shared"]);
    }

    /// Push a commit that writes `files` (path, content) to the remote
    /// `rolling` and refresh the work clone's `origin/rolling`.
    fn advance_rolling(work: &Path, files: &[(&str, &str)]) {
        let mut script = String::from("git checkout -q -B advance origin/rolling");
        for (path, content) in files {
            script.push_str(&format!(
                " && mkdir -p $(dirname {path}) && printf '{content}\\n' > {path} && git add {path}"
            ));
        }
        script.push_str(
            " && git commit -qm advance && git push -q origin HEAD:rolling \
             && git fetch -q origin rolling",
        );
        sh(work, &script);
    }

    async fn sweep_queue(store: &Arc<rsid_core::store_lock::Mutex<Store>>) -> Vec<Uuid> {
        tip_conflict::sweep(store, Duration::ZERO).await.unwrap()
    }

    fn refusal_of(entry: &RollingQueueEntryV1) -> (Option<&str>, Option<&str>) {
        let outcome = entry.outcome.as_ref();
        (
            outcome.and_then(|outcome| outcome.refusal.as_deref()),
            outcome.and_then(|outcome| outcome.detail.as_deref()),
        )
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_queued_source_conflicting_with_the_rolling_tip_is_refused_at_once_3293() {
        let (_dir, _origin, work) = repos();
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let stale = commit_file(&work, "stale", "shared", "stale");
        let clean = commit_file(&work, "clean", "elsewhere", "clean");
        let ids = queue_all(&store, &work, &[stale, clean], None).await;
        assert!(
            sweep_queue(&store).await.is_empty(),
            "nothing conflicts with the tip it was cut from"
        );

        // Rolling advances onto the stale source's lines.
        advance_rolling(&work, &[("shared", "tip")]);
        assert_eq!(sweep_queue(&store).await, [ids[0]]);
        let refused = entry_of(&store, ids[0]).await;
        assert_eq!(refused.state, RollingQueueEntryState::Refused);
        let (code, detail) = refusal_of(&refused);
        assert_eq!(
            code,
            Some(rsi_common::rolling_queue::QUEUE_CONFLICTS_WITH_ROLLING)
        );
        assert!(
            detail.unwrap().starts_with("conflict_paths=shared;"),
            "{detail:?}"
        );
        assert_eq!(
            entry_of(&store, ids[1]).await.state,
            RollingQueueEntryState::Queued,
            "a source that merges cleanly stays queued"
        );
        let wakes = store
            .lock()
            .await
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name == format!("merge-queue-{}", ids[0]))
            .count();
        assert_eq!(wakes, 1, "its enqueuer is woken once");
        assert!(sweep_queue(&store).await.is_empty(), "refused once");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_claimed_source_is_not_refused_by_the_rolling_conflict_sweep_3293() {
        let (_dir, _origin, work) = repos();
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let stale = commit_file(&work, "stale", "shared", "stale");
        let ids = queue_all(&store, &work, &[stale], None).await;
        advance_rolling(&work, &[("shared", "tip")]);
        let claimed = claim_batch_in_lanes(&store, 1).await.unwrap().unwrap();
        assert_eq!(claimed.entries[0].id, ids[0]);
        assert!(sweep_queue(&store).await.is_empty());
        assert_eq!(
            entry_of(&store, ids[0]).await.state,
            RollingQueueEntryState::Gating,
            "the lander decides a gating entry"
        );
    }

    /// A rolling tip that defines `old_api`, for the compile-probe tests (the
    /// stand-in checker treats a `CALL name` without a `fn name` as a break).
    fn compile_probe_workspace() -> (tempfile::TempDir, PathBuf) {
        let (dir, _origin, work) = repos();
        sh(
            &work,
            "printf 'fn old_api\\n' > lib.rs && git add lib.rs && git commit -qm api \
             && git push -q origin rolling && git fetch -q origin rolling",
        );
        (dir, work)
    }

    /// A wide source cut from the tip: `files` plus forty-five Rust files.
    fn wide_source(work: &Path, branch: &str, files: &[(&str, &str)]) -> String {
        let mut script = format!(
            "git checkout -q -B {branch} origin/rolling \
             && for n in $(seq 1 45); do printf 'fn {branch}_noise%s\\n' $n > {branch}_wide$n.rs; done"
        );
        for (path, content) in files {
            script.push_str(&format!(" && printf '{content}\\n' > {path}"));
        }
        script.push_str(&format!(" && git add -A && git commit -qm {branch}"));
        sh(work, &script);
        head(work)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_wide_entry_holds_the_claim_until_its_compile_probe_has_a_verdict_3394() {
        use compile_probe::tests::FakeChecker;
        let (dir, work) = compile_probe_workspace();
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let wide = wide_source(&work, "wide", &[]);
        let narrow = commit_file(&work, "narrow", "notes.txt", "n");
        let ids = queue_all(&store, &work, &[wide.clone(), narrow], None).await;

        let held = compile_probe::held_head_for_claim(&store, 32).await;
        assert_eq!(held.as_deref(), Some(wide.as_str()));
        assert!(
            claim_batch_in_lanes_held(&store, 4, held)
                .await
                .unwrap()
                .is_none(),
            "neither the wide entry nor the one behind it is claimed"
        );

        let checker = Arc::new(FakeChecker::new());
        let refused = compile_probe::sweep_with(&store, &dir.path().join("probe"), checker.clone())
            .await
            .unwrap();
        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(
            checker.runs(),
            1,
            "one bounded check, and only for the wide entry"
        );
        assert_eq!(compile_probe::held_head_for_claim(&store, 32).await, None);
        let claimed = claim_batch_in_lanes_held(&store, 4, None)
            .await
            .unwrap()
            .unwrap();
        let claimed_ids: Vec<Uuid> = claimed.entries.iter().map(|entry| entry.id).collect();
        assert_eq!(claimed_ids, ids, "a clear entry batches as before");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_wide_entry_that_breaks_the_entry_queued_ahead_is_refused_naming_it_3394() {
        use compile_probe::tests::FakeChecker;
        let (dir, work) = compile_probe_workspace();
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        // A calls the API B renames: every merge is textually clean.
        let a = commit_file(&work, "caller", "caller.rs", "CALL old_api");
        let b = wide_source(&work, "refactor", &[("lib.rs", "fn new_api")]);
        let ids = queue_all(&store, &work, &[a, b.clone()], None).await;

        let checker = Arc::new(FakeChecker::new());
        let probe_root = dir.path().join("probe");
        let refused = compile_probe::sweep_with(&store, &probe_root, checker.clone())
            .await
            .unwrap();
        assert_eq!(refused, [ids[1]]);
        let entry = entry_of(&store, ids[1]).await;
        assert_eq!(entry.state, RollingQueueEntryState::Refused);
        let (code, detail) = refusal_of(&entry);
        assert_eq!(
            code,
            Some(rsi_common::rolling_queue::QUEUE_COMPILE_CONFLICT)
        );
        let detail = detail.unwrap();
        assert!(detail.contains(&ids[0].to_string()), "names A: {detail}");
        assert!(detail.contains("E0425"), "the first error: {detail}");
        assert_eq!(
            entry_of(&store, ids[0]).await.state,
            RollingQueueEntryState::Queued,
            "the entry ahead is not blamed"
        );
        let wakes = store
            .lock()
            .await
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name == format!("merge-queue-{}", ids[1]))
            .count();
        assert_eq!(wakes, 1, "its enqueuer is woken once");
        assert!(
            compile_probe::sweep_with(&store, &probe_root, checker)
                .await
                .unwrap()
                .is_empty(),
            "refused once"
        );
        // Nothing the refusal left behind holds the queue.
        assert_eq!(compile_probe::held_head_for_claim(&store, 32).await, None);
    }

    /// Write a provisional declaration naming `declared` as renumber sites and
    /// commit it with `files` on a branch cut from the rolling tip.
    fn commit_declaring(
        work: &Path,
        branch: &str,
        files: &[(&str, &str)],
        declared: &[&str],
    ) -> String {
        commit_declaring_with(work, branch, files, declared, &[])
    }

    /// [`commit_declaring`], plus `named` paths the declaration lists without
    /// making them renumber sites (#3362).
    fn commit_declaring_with(
        work: &Path,
        branch: &str,
        files: &[(&str, &str)],
        declared: &[&str],
        named: &[&str],
    ) -> String {
        let entries = declared
            .iter()
            .map(|path| format!("{{\"path\":\"{path}\",\"site\":true}}"))
            .chain(named.iter().map(|path| format!("{{\"path\":\"{path}\"}}")))
            .collect::<Vec<_>>()
            .join(",");
        let mut script = format!(
            "git checkout -q -B {branch} origin/rolling && mkdir -p tools/provisional-migrations \
             && printf '{{\"version\":901,\"files\":[{entries}]}}\\n' > tools/provisional-migrations/{branch}.json \
             && git add tools/provisional-migrations/{branch}.json"
        );
        for (path, content) in files {
            script.push_str(&format!(
                " && mkdir -p $(dirname {path}) && printf '{content}\\n' > {path} && git add {path}"
            ));
        }
        script.push_str(&format!(" && git commit -qm {branch}"));
        sh(work, &script);
        head(work)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_conflict_only_on_declared_renumber_sites_stays_queued_3293() {
        let (_dir, _origin, work) = repos();
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let renumberable = commit_declaring(
            &work,
            "renumberable",
            &[("v901", "mine"), ("tests", "mine")],
            &["v901", "tests"],
        );
        // The same declaration, but one conflict is not a declared site.
        let mixed = commit_declaring(
            &work,
            "mixed",
            &[("v901", "mine"), ("other", "mine")],
            &["v901"],
        );
        // No declaration at all.
        let undeclared = commit_file(&work, "undeclared", "v901", "mine");
        let ids = queue_all(&store, &work, &[renumberable, mixed, undeclared], None).await;
        advance_rolling_with_stub_tool(&work);
        advance_rolling(
            &work,
            &[("v901", "theirs"), ("tests", "theirs"), ("other", "theirs")],
        );
        assert_eq!(sweep_queue(&store).await, [ids[1], ids[2]]);
        assert_eq!(
            entry_of(&store, ids[0]).await.state,
            RollingQueueEntryState::Queued,
            "the renumber tool proves the declared migration's version clash"
        );
        let mixed = entry_of(&store, ids[1]).await;
        let (_, detail) = refusal_of(&mixed);
        assert!(
            detail.is_some_and(|d| d.starts_with("conflict_paths=other,v901;")
                || d.starts_with("conflict_paths=v901,other;")),
            "{detail:?}"
        );
        assert_eq!(
            entry_of(&store, ids[2]).await.state,
            RollingQueueEntryState::Refused
        );
    }

    /// A renumber-guard stand-in the probe starts as its trusted executable
    /// (`<exe> rolling-migration-renumber ...`). It reads the
    /// source's declaration the way the real tool's validation does in
    /// miniature: `--transform` refuses (exit 2) an unparsable declaration and
    /// otherwise reports the versions; `--build` refuses a source whose
    /// `marker` file says `bad` (like the real tool's `unproved migration
    /// conflict`) and a merge with a conflict on any file the declaration does
    /// not mark `"site": true` (the real tool's `nonmigration conflict`: a path
    /// merely NAMED in the declaration is no renumber site), and crashes
    /// (exit 1) on `crash`. `reject_unpublished` models the real tool's refusal
    /// of a target whose provisional inventory is not published yet (#3406).
    const STUB_GUARD_SCRIPT: &str = r#"
shift 1
repo=; mode=; base=; source=; target=; unit=
while [ $# -gt 0 ]; do
  case $1 in
    --repo) repo=$2; shift 2;;
    --base) base=$2; shift 2;;
    --source) source=$2; shift 2;;
    --target) target=$2; shift 2;;
    --unit-file) unit=$2; shift 2;;
    --scratch) shift 2;;
    --transform) mode=transform; shift;;
    --build) mode=build; shift;;
    *) shift;;
  esac
done
g() { git -C "$repo" "$@"; }
refuse() { echo "provisional migration refused: $1" >&2; exit 2; }
declaration() {
  first=$(g diff --name-only --diff-filter=A "$1" "$2" -- tools/provisional-migrations/ | head -n 1)
  [ -n "$first" ] || refuse "invalid provisional site declaration"
  text=$(g show "$2:$first")
  case $text in
    '{"version"'*'}') ;;
    *) refuse "invalid provisional site declaration";;
  esac
  printf '%s\n' "$text"
}
if [ "$mode" = transform ]; then
  if [ "$reject_unpublished" = 1 ] && [ -n "$(g diff --name-only --diff-filter=A "$base" "$target" -- tools/provisional-migrations/)" ]; then
    refuse "invalid migration inventory at $target: provisional inventory does not match its source: migration blocks V901; LATEST_SCHEMA_VERSION"
  fi
  declaration "$base" "$source" >/dev/null
  printf '{"base":"%s","source":"%s","target":"%s","old_version":901,"new_version":902,"renumber_noop":false}\n' "$base" "$source" "$target"
  exit 0
fi
field() { sed -n "s/.*\"$1\":\"\([0-9a-f]*\)\".*/\1/p" "$unit"; }
ubase=$(field base); usource=$(field source); utarget=$(field target)
marker=$(g show "$usource:marker" 2>/dev/null)
case $marker in
  *crash*) echo "tool bug" >&2; exit 1;;
  *bad*) refuse "unproved migration conflict: v901";;
esac
sites=$(declaration "$ubase" "$usource" | grep -o '"path":"[^"]*","site":true' | sed 's/"path":"\([^"]*\)".*/\1/')
merged=$(g merge-tree --write-tree --name-only --no-messages "$utarget" "$usource"); rc=$?
if [ "$rc" = 1 ]; then
  outside=
  for path in $(printf '%s\n' "$merged" | tail -n +2); do
    case "
$sites
" in
      *"
$path
"*) ;;
      *) outside="${outside:+$outside, }$path";;
    esac
  done
  [ -z "$outside" ] || refuse "nonmigration conflict: $outside"
fi
printf '{"candidate": "%s"}\n' 0000000000000000000000000000000000000000
"#;

    /// Make `work` answer the probe's trusted-executable lookup with the stub
    /// and move the remote `rolling` tip once, as installing a tool did.
    fn advance_rolling_with_stub_tool(work: &Path) {
        install_stub_guard(work, false);
    }

    fn install_stub_guard(work: &Path, reject_unpublished: bool) {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rsi-stub-guard-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("rsi-rolling-land");
        std::fs::write(
            &exe,
            format!(
                "#!/bin/sh\nreject_unpublished={}\n{STUB_GUARD_SCRIPT}",
                u8::from(reject_unpublished)
            ),
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        crate::repo_guard_call::register_test_tool(work, exe);
        sh(
            work,
            "git checkout -q -B advance origin/rolling && printf 'stub guard\\n' > stub-guard-marker \
             && git add stub-guard-marker && git commit -qm stub-guard \
             && git push -q origin HEAD:rolling && git fetch -q origin rolling",
        );
    }

    /// #361: the probe's trusted executable is a killable child. A guard that
    /// hangs, with a descendant, ends at the shared deadline with nothing
    /// proved, and nothing it started outlives the kill.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_hanging_guard_is_killed_at_the_probe_deadline_and_proves_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, _origin, work) = repos();
        let source = commit_declaring(&work, "hangs", &[("marker", "ok")], &["marker"]);
        let started_file = dir.path().join("guard-started");
        let survivor = dir.path().join("guard-descendant-survived");
        let exe = dir.path().join("hanging-guard");
        std::fs::write(
            &exe,
            format!(
                "#!/bin/sh\n: > '{}'\n(sleep 4; : > '{}') &\nsleep 300\n",
                started_file.display(),
                survivor.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        crate::repo_guard_call::register_test_tool(&work, exe);
        let started = std::time::Instant::now();
        let proof = provisional_probe::prove_against_tip(
            &work,
            "origin/rolling",
            &source,
            started + std::time::Duration::from_millis(2500),
        );
        // #3545 replaced Proof::Unknown with a typed Inconclusive cause.
        assert!(
            matches!(proof, provisional_probe::Proof::Inconclusive { .. }),
            "{proof:?}"
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert!(started_file.exists(), "the guard was never started");
        std::thread::sleep(std::time::Duration::from_millis(4500));
        assert!(!survivor.exists(), "a guard descendant outlived the kill");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn missing_provisional_guard_preserves_best_effort_conflict_classification_3545() {
        use provisional_probe::{InconclusiveCause, Proof, Renumber};
        let (_dir, _origin, work) = repos();
        let source = commit_declaring(&work, "source", &[("marker", "ok")], &["marker"]);
        let tip = git(&work, &["rev-parse", "origin/rolling"]).unwrap();
        let deadline = || std::time::Instant::now() + Duration::from_secs(30);
        // #465: the guard is a trusted installed executable, not a file of the
        // tip. With none resolvable (no guard is registered for this repo) the
        // probe is Inconclusive, never Clear: an unproven source is not
        // admitted (hub #3545). The best-effort sweep classifier keeps its
        // separate unknown outcome rather than inventing a conflict.
        assert_eq!(
            provisional_probe::prove_against_tip(&work, tip.trim(), &source, deadline()),
            Proof::Inconclusive {
                cause: InconclusiveCause::Infrastructure(
                    "no trusted rolling-migration-renumber guard executable".into()
                )
            }
        );
        assert_eq!(
            provisional_probe::classify_conflict(&work, tip.trim(), &source, deadline()),
            Renumber::Unknown
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_source_the_renumber_tool_refuses_is_refused_at_once_3346() {
        let (_dir, _origin, work) = repos();
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let provable = commit_declaring(&work, "provable", &[("marker", "ok")], &["marker"]);
        let unprovable = commit_declaring(&work, "unprovable", &[("marker", "bad")], &["marker"]);
        let crashes = commit_declaring(&work, "crashes", &[("marker", "crash")], &["marker"]);
        // The same marker, but it declares no migration: not the tool's business.
        let undeclared = commit_file(&work, "undeclared", "marker", "bad");
        let ids = queue_all(
            &store,
            &work,
            &[provable, unprovable.clone(), crashes, undeclared],
            None,
        )
        .await;
        advance_rolling_with_stub_tool(&work);

        assert_eq!(sweep_queue(&store).await, [ids[1]]);
        let refused = entry_of(&store, ids[1]).await;
        assert_eq!(refused.state, RollingQueueEntryState::Refused);
        assert_eq!(refused.source_commit, unprovable);
        let (code, detail) = refusal_of(&refused);
        assert_eq!(
            code,
            Some(rsi_common::rolling_queue::PROVISIONAL_UNPROVABLE)
        );
        let detail = detail.unwrap();
        assert!(
            detail.starts_with("unproved migration conflict: v901;"),
            "the tool's own reason leads the detail: {detail}"
        );
        for kept in [ids[0], ids[2], ids[3]] {
            assert_eq!(
                entry_of(&store, kept).await.state,
                RollingQueueEntryState::Queued,
                "only a refusal by the tool refuses: a proved source, a crash and an undeclared source stay queued"
            );
        }
        let wakes = store
            .lock()
            .await
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name == format!("merge-queue-{}", ids[1]))
            .count();
        assert_eq!(wakes, 1, "its enqueuer is woken once");
        assert!(sweep_queue(&store).await.is_empty(), "refused once");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_conflict_free_unprovable_source_is_refused_when_rolling_advances_3346() {
        let (_dir, _origin, work) = repos();
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        advance_rolling_with_stub_tool(&work);
        let unprovable = commit_declaring(&work, "unprovable", &[("marker", "bad")], &["marker"]);
        let ids = queue_all(&store, &work, &[unprovable.clone()], None).await;
        // Nothing conflicts and the tool refuses: refused on the first pass.
        assert_eq!(sweep_queue(&store).await, [ids[0]]);
        assert_eq!(entry_of(&store, ids[0]).await.source_commit, unprovable);
        assert_eq!(
            refusal_of(&entry_of(&store, ids[0]).await).0,
            Some(rsi_common::rolling_queue::PROVISIONAL_UNPROVABLE)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_path_named_in_a_declaration_but_not_a_renumber_site_exempts_nothing_3362() {
        let (_dir, _origin, work) = repos();
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        // `unrelated` is named in the declaration but is no renumber site.
        let named = commit_declaring_with(
            &work,
            "named",
            &[("v901", "mine"), ("unrelated", "mine")],
            &["v901"],
            &["unrelated"],
        );
        // An invalid declaration: the tool refuses it, so it exempts nothing.
        sh(
            &work,
            "git checkout -q -B broken origin/rolling && mkdir -p tools/provisional-migrations \
             && printf '{' > tools/provisional-migrations/broken.json \
             && printf 'mine\\n' > v901 && git add tools v901 && git commit -qm broken",
        );
        let broken = head(&work);
        let ids = queue_all(&store, &work, &[named, broken], None).await;
        advance_rolling_with_stub_tool(&work);
        advance_rolling(&work, &[("v901", "theirs"), ("unrelated", "theirs")]);

        assert_eq!(sweep_queue(&store).await, [ids[0], ids[1]]);
        let refused = entry_of(&store, ids[0]).await;
        let (code, detail) = refusal_of(&refused);
        assert_eq!(
            code,
            Some(rsi_common::rolling_queue::QUEUE_CONFLICTS_WITH_ROLLING)
        );
        let detail = detail.unwrap();
        assert!(
            detail.starts_with("conflict_paths=unrelated,v901;"),
            "{detail}"
        );
        assert!(
            detail.contains("does not cover them: nonmigration conflict: unrelated"),
            "the tool's reason is named: {detail}"
        );
        let invalid = entry_of(&store, ids[1]).await;
        let (code, detail) = refusal_of(&invalid);
        assert_eq!(
            code,
            Some(rsi_common::rolling_queue::QUEUE_CONFLICTS_WITH_ROLLING)
        );
        assert!(
            detail.is_some_and(|detail| detail.starts_with("conflict_paths=v901;")
                && detail.contains("invalid provisional site declaration")),
            "an invalid declaration exempts nothing"
        );
    }

    /// The paths of the Issue's case: the declared migration file and the store
    /// test module's tail.
    const MIGRATION_FILE: &str = "crates/rsid-store/src/store/migrations/v901.rs";
    const MIGRATION_TESTS: &str = "crates/rsid-store/src/store/tests.rs";

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_declared_migration_version_clash_stays_queued_and_is_not_a_conflict_3336() {
        use rsi_common::rolling_queue::{
            ENTRY_CONFLICTS_WITH_ROLLING, ENTRY_RENUMBERABLE_WITH_ROLLING,
            QUEUE_CONFLICTS_WITH_ENTRY, QUEUE_CONFLICTS_WITH_ROLLING,
            QUEUE_RENUMBERABLE_WITH_ENTRY, QUEUE_RENUMBERABLE_WITH_ROLLING,
        };

        let (_dir, _origin, work) = repos();
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        advance_rolling_with_stub_tool(&work);
        let clash = [(MIGRATION_FILE, "mine"), (MIGRATION_TESTS, "mine")];
        let entry_files = [(MIGRATION_FILE, "entry"), (MIGRATION_TESTS, "entry")];
        let sites = [MIGRATION_FILE, MIGRATION_TESTS];
        let entry = commit_declaring(&work, "entry", &entry_files, &sites);
        let source = commit_declaring(&work, "source", &clash, &sites);
        // The same clash from a declaration that does not mark the files sites.
        let unsited = commit_declaring_with(
            &work,
            "unsited",
            &clash,
            &[],
            &[MIGRATION_FILE, MIGRATION_TESTS],
        );
        // Not declared at all.
        sh(
            &work,
            &format!(
                "git checkout -q -B undeclared origin/rolling && mkdir -p $(dirname {MIGRATION_FILE}) \
                 && printf 'mine\\n' > {MIGRATION_FILE} && git add {MIGRATION_FILE} \
                 && git commit -qm undeclared"
            ),
        );
        let undeclared = head(&work);
        let ahead = [(Uuid::new_v4(), 7, entry.clone())];
        let codes = |warnings: &[rsi_common::rolling_queue::QueueConflictWarningV1]| {
            warnings
                .iter()
                .map(|warning| warning.code.clone())
                .collect::<Vec<_>>()
        };

        // The source and the entry ahead both add the migration file.
        let warnings = queue_conflict_warnings(&work, &source, &ahead);
        assert_eq!(
            codes(&warnings),
            [QUEUE_RENUMBERABLE_WITH_ENTRY],
            "{warnings:?}"
        );
        assert_eq!(warnings[0].entry_id, Some(ahead[0].0));
        assert_eq!(warnings[0].files, [MIGRATION_FILE, MIGRATION_TESTS]);
        assert!(
            warnings[0]
                .detail
                .contains("declared migration v901 will be renumbered to v902 at landing"),
            "{warnings:?}"
        );
        // Without a declaration the tool proves, it stays a conflict.
        for stays in [&unsited, &undeclared] {
            let warnings = queue_conflict_warnings(&work, stays, &ahead);
            assert_eq!(
                codes(&warnings),
                [QUEUE_CONFLICTS_WITH_ENTRY],
                "{warnings:?}"
            );
        }
        let warnings = queue_conflict_warnings(&work, &unsited, &ahead);
        assert!(
            warnings[0]
                .detail
                .contains("does not cover them: nonmigration conflict"),
            "{warnings:?}"
        );

        // Rolling advances onto the same files.
        advance_rolling(
            &work,
            &[(MIGRATION_FILE, "theirs"), (MIGRATION_TESTS, "theirs")],
        );
        let warnings = queue_conflict_warnings(&work, &source, &ahead);
        assert_eq!(
            codes(&warnings),
            [
                QUEUE_RENUMBERABLE_WITH_ROLLING,
                ENTRY_RENUMBERABLE_WITH_ROLLING
            ],
            "{warnings:?}"
        );
        assert_eq!(warnings[0].entry_id, None);
        assert_eq!(warnings[1].entry_id, Some(ahead[0].0));
        let warnings = queue_conflict_warnings(&work, &undeclared, &ahead);
        assert_eq!(
            codes(&warnings),
            [
                QUEUE_CONFLICTS_WITH_ROLLING,
                ENTRY_RENUMBERABLE_WITH_ROLLING
            ],
            "{warnings:?}"
        );
        assert!(!codes(&warnings).contains(&ENTRY_CONFLICTS_WITH_ROLLING.to_owned()));

        // The sweep keeps the declared source queued and refuses the others.
        let ids = queue_all(&store, &work, &[source, unsited, undeclared], None).await;
        assert_eq!(sweep_queue(&store).await, [ids[1], ids[2]]);
        assert_eq!(
            entry_of(&store, ids[0]).await.state,
            RollingQueueEntryState::Queued,
            "the lander renumbers a declared migration's version clash"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_probe_that_cannot_run_never_refuses_a_queued_source_3293() {
        let (_dir, _origin, work) = repos();
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        // A source commit the repository does not hold.
        let ids = queue_all(&store, &work, &["1".repeat(40)], None).await;
        advance_rolling(&work, &[("shared", "tip")]);
        for _ in 0..4 {
            assert!(sweep_queue(&store).await.is_empty());
        }
        assert_eq!(
            entry_of(&store, ids[0]).await.state,
            RollingQueueEntryState::Queued
        );

        // A repository that does not exist: no tip, nothing refused.
        let gone = work.join("no-such-checkout");
        let id = queued(&store, &gone, &"2".repeat(40), "k-gone").await;
        assert!(sweep_queue(&store).await.is_empty());
        assert_eq!(
            entry_of(&store, id).await.state,
            RollingQueueEntryState::Queued
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn enqueue_attributes_existing_rolling_conflicts_to_the_ahead_entry_3271() {
        use rsi_common::rolling_queue::{ENTRY_CONFLICTS_WITH_ROLLING, QUEUE_CONFLICTS_WITH_ENTRY};

        let (_dir, _origin, work) = repos();
        commit_file(&work, "ahead", "shared", "ahead");
        std::fs::write(work.join("genuine"), "ahead\n").unwrap();
        git(&work, &["add", "genuine"]).unwrap();
        git(&work, &["commit", "-qm", "ahead-genuine"]).unwrap();
        let ahead = head(&work);

        git(
            &work,
            &["checkout", "-q", "-b", "advance", "origin/rolling"],
        )
        .unwrap();
        std::fs::write(work.join("shared"), "rolling\n").unwrap();
        git(&work, &["add", "shared"]).unwrap();
        git(&work, &["commit", "-qm", "advance"]).unwrap();
        git(&work, &["push", "-q", "origin", "HEAD:rolling"]).unwrap();
        git(&work, &["fetch", "-q", "origin", "rolling"]).unwrap();

        // The source contains rolling and leaves its conflicting file alone.
        let source = commit_file(&work, "source", "unrelated", "source");
        git(
            &work,
            &["merge-base", "--is-ancestor", "origin/rolling", &source],
        )
        .unwrap();
        assert_eq!(
            git(&work, &["diff", "origin/rolling", &source, "--", "shared"]).unwrap(),
            ""
        );
        let id = Uuid::new_v4();
        let queue = [(id, 7, ahead)];
        let warnings = queue_conflict_warnings(&work, &source, &queue);
        // Exactly the entry's own warning: the source is attributed no conflict.
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].code, ENTRY_CONFLICTS_WITH_ROLLING);
        assert_eq!(warnings[0].entry_id, Some(id));
        assert_eq!(warnings[0].entry_sequence, Some(7));
        assert_eq!(warnings[0].files, ["shared"]);

        // An independent source-vs-entry conflict still gets its own warning,
        // even when the same entry also has a conflict with rolling.
        std::fs::write(work.join("genuine"), "source\n").unwrap();
        git(&work, &["add", "genuine"]).unwrap();
        git(&work, &["commit", "-qm", "source-genuine"]).unwrap();
        let warnings = queue_conflict_warnings(&work, &head(&work), &queue);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert_eq!(warnings[0].code, ENTRY_CONFLICTS_WITH_ROLLING);
        assert_eq!(warnings[0].files, ["shared"]);
        assert_eq!(warnings[1].code, QUEUE_CONFLICTS_WITH_ENTRY);
        assert_eq!(warnings[1].entry_id, Some(id));
        assert_eq!(warnings[1].entry_sequence, Some(7));
        assert_eq!(warnings[1].files, ["genuine"]);
    }

    /// #3406: the pair probe cannot treat a queued target's provisional
    /// inventory as released. Model the real tool's target-inventory refusal;
    /// the real publication-order proof lives in the renumber tool's tests.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn two_provisional_units_with_the_same_version_have_unknown_pair_classification_3406() {
        use provisional_probe::{Proof, Renumber};
        use rsi_common::rolling_queue::QUEUE_CONFLICT_SCAN_INCOMPLETE;
        let (_dir, _origin, work) = repos();
        // The stand-in otherwise proves renumbers, but rejects an unpublished
        // target exactly as the real tool does before publication pins it.
        install_stub_guard(&work, true);
        let entry = commit_declaring(
            &work,
            "entry",
            &[(MIGRATION_FILE, "entry")],
            &[MIGRATION_FILE],
        );
        let source = commit_declaring(
            &work,
            "source",
            &[(MIGRATION_FILE, "source")],
            &[MIGRATION_FILE],
        );
        let tip = git(&work, &["rev-parse", "origin/rolling"]).unwrap();
        let deadline = || std::time::Instant::now() + Duration::from_secs(30);
        for commit in [&entry, &source] {
            assert!(matches!(
                provisional_probe::classify_conflict(&work, tip.trim(), commit, deadline()),
                Renumber::Renumberable(_)
            ));
        }
        assert!(matches!(
            provisional_probe::prove_against_tip(&work, &entry, &source, deadline()),
            Proof::Unprovable(reason)
                if reason.starts_with(&format!("invalid migration inventory at {entry}:"))
        ));
        assert_eq!(
            provisional_probe::classify_conflict(&work, &entry, &source, deadline()),
            Renumber::Unknown
        );
        let id = Uuid::new_v4();
        let warnings = queue_conflict_warnings(&work, &source, &[(id, 7, entry)]);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings
                .iter()
                .all(|w| w.code == QUEUE_CONFLICT_SCAN_INCOMPLETE)
        );
        assert_eq!(warnings[0].entry_id, Some(id));
        assert_eq!(warnings[0].entry_sequence, Some(7));
        assert_eq!(warnings[0].files, [MIGRATION_FILE]);
        assert!(warnings[0].detail.contains("classification is unknown"));
    }

    /// An exhausted renumber proof cannot advise repairing an entry. Inject
    /// the outcome instead of spending the real four-second scan budget.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn entry_renumber_classification_preserves_unknown_and_the_shared_deadline_3406() {
        use provisional_probe::Renumber;
        use rsi_common::rolling_queue::{
            ENTRY_CONFLICTS_WITH_ROLLING, ENTRY_RENUMBERABLE_WITH_ROLLING,
            QUEUE_CONFLICT_SCAN_INCOMPLETE,
        };
        let (_dir, _origin, work) = repos();
        let entry = commit_declaring(
            &work,
            "ahead",
            &[(MIGRATION_FILE, "entry")],
            &[MIGRATION_FILE],
        );
        let declared = commit_declaring(
            &work,
            "declared",
            &[(MIGRATION_FILE, "source")],
            &[MIGRATION_FILE],
        );
        let ordinary = commit_file(&work, "ordinary", "unrelated", "source");
        let ahead = [(Uuid::new_v4(), 7, entry.clone())];
        for source in [&declared, &ordinary] {
            for (kind, expected) in [
                (
                    Renumber::Renumberable("v901 -> v902".into()),
                    ENTRY_RENUMBERABLE_WITH_ROLLING,
                ),
                (Renumber::Unknown, QUEUE_CONFLICT_SCAN_INCOMPLETE),
                (
                    Renumber::Conflict(Some("nonmigration conflict".into())),
                    ENTRY_CONFLICTS_WITH_ROLLING,
                ),
            ] {
                let mut proofs = 0;
                let warnings = queue_conflict_warnings_with_probe(
                    &work,
                    source,
                    &ahead,
                    ENQUEUE_CONFLICT_SCAN_BUDGET,
                    |_, theirs, _| {
                        if theirs == entry {
                            MergeConflictProbe::Conflicts(vec![MIGRATION_FILE.into()])
                        } else {
                            MergeConflictProbe::Clean
                        }
                    },
                    |_, theirs, deadline| {
                        assert_eq!(theirs, entry);
                        assert!(
                            deadline.saturating_duration_since(std::time::Instant::now())
                                <= ENQUEUE_CONFLICT_SCAN_BUDGET
                        );
                        proofs += 1;
                        kind.clone()
                    },
                );
                assert_eq!(proofs, 1);
                assert_eq!(warnings[0].code, expected, "{warnings:?}");
                assert_eq!(warnings[0].entry_id, Some(ahead[0].0));
                assert_eq!(warnings[0].entry_sequence, Some(7));
                assert_eq!(warnings[0].files, [MIGRATION_FILE]);
                assert_eq!(
                    warnings.len(),
                    if kind == Renumber::Unknown { 2 } else { 1 }
                );
                if kind == Renumber::Unknown {
                    assert!(
                        warnings
                            .iter()
                            .all(|warning| warning.code == QUEUE_CONFLICT_SCAN_INCOMPLETE)
                    );
                    assert!(warnings[0].detail.contains("classification is unknown"));
                }
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_unknown_entry_rolling_probe_reports_incomplete_without_attributing_conflicts_3311() {
        use rsi_common::rolling_queue::QUEUE_CONFLICT_SCAN_INCOMPLETE;

        let (_dir, _origin, work) = repos();
        let entry = commit_file(&work, "ahead", "shared", "ahead");
        let source = commit_file(&work, "source", "shared", "source");
        let tip = git(&work, &["rev-parse", "origin/rolling"]).unwrap();
        let tip = tip.trim();
        let queue = [
            (Uuid::new_v4(), 7, entry.clone()),
            (Uuid::new_v4(), 8, entry.clone()),
        ];
        let mut baseline_probes = 0;
        let mut successful_pairwise_probes = 0;
        let warnings = queue_conflict_warnings_with_probe(
            &work,
            &source,
            &queue,
            Duration::from_secs(3600),
            |ours, theirs, timeout| {
                if ours == tip && theirs == entry {
                    baseline_probes += 1;
                    return MergeConflictProbe::Unknown {
                        reason: "injected failure",
                    };
                }
                let result = merge_conflict_paths_with(&work, ours, theirs, &["git"], timeout);
                if ours == entry && theirs == source {
                    assert_eq!(result, MergeConflictProbe::Conflicts(vec!["shared".into()]));
                    successful_pairwise_probes += 1;
                }
                result
            },
            |_, _, _| provisional_probe::Renumber::Conflict(None),
        );
        assert_eq!(
            baseline_probes, 1,
            "unknown results are cached per entry commit"
        );
        assert_eq!(successful_pairwise_probes, 2);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].code, QUEUE_CONFLICT_SCAN_INCOMPLETE);
        assert_eq!(warnings[0].entry_id, None);
        assert_eq!(warnings[0].files, Vec::<String>::new());
        assert!(
            warnings[0].detail.contains("1 probe(s) not completed"),
            "{warnings:?}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn unknown_source_probes_report_incomplete_before_the_deadline_3311() {
        use rsi_common::rolling_queue::{
            QUEUE_CONFLICT_SCAN_INCOMPLETE, QUEUE_CONFLICTS_WITH_ENTRY,
        };

        let (_dir, _origin, work) = repos();
        let entry = commit_file(&work, "ahead", "shared", "ahead");
        let source = commit_file(&work, "source", "shared", "source");
        let queue = [(Uuid::new_v4(), 7, entry.clone())];
        for fail_pairwise in [false, true] {
            let warnings = queue_conflict_warnings_with_probe(
                &work,
                &source,
                &queue,
                Duration::from_secs(3600),
                |ours, theirs, timeout| {
                    if theirs == source && (ours == entry) == fail_pairwise {
                        MergeConflictProbe::Unknown {
                            reason: "injected failure",
                        }
                    } else {
                        merge_conflict_paths_with(&work, ours, theirs, &["git"], timeout)
                    }
                },
                |_, _, _| provisional_probe::Renumber::Conflict(None),
            );
            let codes: Vec<_> = warnings
                .iter()
                .map(|warning| warning.code.as_str())
                .collect();
            let expected = if fail_pairwise {
                vec![QUEUE_CONFLICT_SCAN_INCOMPLETE]
            } else {
                vec![QUEUE_CONFLICTS_WITH_ENTRY, QUEUE_CONFLICT_SCAN_INCOMPLETE]
            };
            assert_eq!(codes, expected);
            assert!(
                warnings
                    .last()
                    .unwrap()
                    .detail
                    .contains("1 probe(s) not completed")
            );
        }
    }

    /// #3036 (the hub's #3022 case): a refused source that edits files a
    /// queued entry ahead deletes is held by exactly that entry. A disjoint
    /// entry, and an entry that only carries a rolling change the source
    /// conflicts with, hold nothing; a spent budget holds nothing either.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_refused_source_is_held_only_by_an_entry_ahead_whose_own_change_conflicts_3036() {
        let (_dir, _origin, work) = repos();
        let advance = |file: &str, content: &str| {
            sh(
                &work,
                &format!(
                    "git checkout -q -B advance origin/rolling && printf '{content}\\n' > {file} \
                     && git add {file} && git commit -qm advance && git push -q origin HEAD:rolling \
                     && git fetch -q origin rolling"
                ),
            );
        };
        advance("dist", "built");
        sh(
            &work,
            "git checkout -q -B untrack origin/rolling && git rm -q dist && git commit -qm untrack",
        );
        let untrack = head(&work);
        let disjoint = commit_file(&work, "disjoint", "elsewhere", "x");
        let source = commit_file(&work, "source", "dist", "rebuilt");
        let stale = commit_file(&work, "stale", "shared", "stale");
        advance("shared", "tip");
        let carrier = commit_file(&work, "carrier", "unrelated", "y");
        let queue = [
            (Uuid::new_v4(), 1, untrack),
            (Uuid::new_v4(), 2, disjoint),
            (Uuid::new_v4(), 3, carrier),
        ];
        assert_eq!(
            conflicting_entries_ahead(&work, &source, &queue, LAND_CONFLICT_SCAN_BUDGET),
            vec![queue[0].0]
        );
        // `stale` conflicts with the rolling tip only; `carrier` carries that
        // change but does not make it.
        assert!(
            conflicting_entries_ahead(&work, &stale, &queue, LAND_CONFLICT_SCAN_BUDGET).is_empty()
        );
        assert!(conflicting_entries_ahead(&work, &source, &queue, Duration::ZERO).is_empty());
    }

    /// #3345: an entry past the probe cap was never inspected; the scan says so.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn entries_past_the_probe_cap_make_the_scan_incomplete_3345() {
        use rsi_common::rolling_queue::QUEUE_CONFLICT_SCAN_INCOMPLETE;
        let (_dir, _origin, work) = repos();
        let source = commit_file(&work, "source", "mine", "mine");
        let queue = |len: usize| -> Vec<(Uuid, i64, String)> {
            (0..len)
                .map(|index| {
                    (
                        Uuid::new_v4(),
                        i64::try_from(index).unwrap(),
                        format!("{index:040x}"),
                    )
                })
                .collect()
        };
        let scan = |ahead: &[(Uuid, i64, String)]| {
            queue_conflict_warnings_with_probe(
                &work,
                &source,
                ahead,
                Duration::from_secs(30),
                |_, _, _| MergeConflictProbe::Clean,
                |_, _, _| provisional_probe::Renumber::Conflict(None),
            )
        };
        // At the cap every entry is probed: no gap is reported.
        assert!(scan(&queue(CONFLICT_PROBE_MAX_ENTRIES)).is_empty());
        // One entry past it was never inspected, and the scan says so.
        let warnings = scan(&queue(CONFLICT_PROBE_MAX_ENTRIES + 1));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].code, QUEUE_CONFLICT_SCAN_INCOMPLETE);
        assert!(
            warnings[0].detail.contains("1 probe(s)"),
            "{}",
            warnings[0].detail
        );
        // #3741: the farthest entry (sequence 0) is the one skipped, and named.
        assert!(
            warnings[0]
                .detail
                .contains("queue sequences not fully checked: 0)"),
            "{}",
            warnings[0].detail
        );
    }

    /// #3741: a queue deeper than the old 32-entry cap whose conflicting entry
    /// is directly ahead of the new position (the highest sequence) is found,
    /// both for a 35-deep queue and for one past the current cap.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_conflict_with_the_entry_directly_ahead_is_found_in_a_deep_queue_3741() {
        use rsi_common::rolling_queue::{
            QUEUE_CONFLICT_SCAN_INCOMPLETE, QUEUE_CONFLICTS_WITH_ENTRY,
        };
        let (_dir, _origin, work) = repos();
        let source = commit_file(&work, "source", "mine", "mine");
        for depth in [35_usize, CONFLICT_PROBE_MAX_ENTRIES + 3] {
            let queue: Vec<(Uuid, i64, String)> = (0..depth)
                .map(|index| {
                    (
                        Uuid::new_v4(),
                        i64::try_from(index).unwrap() + 100,
                        format!("{index:040x}"),
                    )
                })
                .collect();
            let directly_ahead = queue.last().unwrap().clone();
            let entry_commit = directly_ahead.2.clone();
            let warnings = queue_conflict_warnings_with_probe(
                &work,
                &source,
                &queue,
                Duration::from_secs(30),
                |ours, theirs, _| {
                    if ours == entry_commit && theirs == source {
                        MergeConflictProbe::Conflicts(vec!["shared".into()])
                    } else {
                        MergeConflictProbe::Clean
                    }
                },
                |_, _, _| provisional_probe::Renumber::Conflict(None),
            );
            let found: Vec<_> = warnings
                .iter()
                .filter(|w| w.code == QUEUE_CONFLICTS_WITH_ENTRY)
                .collect();
            assert_eq!(found.len(), 1, "depth {depth}: {warnings:?}");
            assert_eq!(found[0].entry_id, Some(directly_ahead.0));
            assert_eq!(found[0].entry_sequence, Some(directly_ahead.1));
            // Past the cap the farthest entries are skipped, and named.
            let skipped = depth.saturating_sub(CONFLICT_PROBE_MAX_ENTRIES);
            let incomplete = warnings
                .iter()
                .find(|w| w.code == QUEUE_CONFLICT_SCAN_INCOMPLETE);
            assert_eq!(incomplete.is_some(), skipped > 0, "depth {depth}");
            let Some(incomplete) = incomplete else {
                continue;
            };
            assert!(
                incomplete.detail.contains(&format!("{skipped} probe(s)"))
                    && incomplete
                        .detail
                        .contains("queue sequences not fully checked: 102, 101, 100)"),
                "{}",
                incomplete.detail
            );
        }
    }

    /// #3741: when the wall budget runs out mid-scan, the entries nearest the
    /// new position were already probed and the incomplete warning names the
    /// farther sequences it never reached.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_spent_budget_skips_the_far_entries_and_names_their_sequences_3741() {
        use rsi_common::rolling_queue::{
            QUEUE_CONFLICT_SCAN_INCOMPLETE, QUEUE_CONFLICTS_WITH_ENTRY,
        };
        let (_dir, _origin, work) = repos();
        let source = commit_file(&work, "source", "mine", "mine");
        let queue: Vec<(Uuid, i64, String)> = (1..=6)
            .map(|index| (Uuid::new_v4(), index, format!("{index:040x}")))
            .collect();
        let nearest = queue[5].2.clone();
        let mut pair_probes = 0_usize;
        let warnings = queue_conflict_warnings_with_probe(
            &work,
            &source,
            &queue,
            Duration::from_millis(300),
            |ours, _, _| {
                if ours == nearest {
                    return MergeConflictProbe::Conflicts(vec!["shared".into()]);
                }
                // Only the pairwise probe of another entry burns the budget;
                // the tip-baseline probes stay fast.
                if !queue.iter().any(|entry| entry.2 == ours) {
                    return MergeConflictProbe::Clean;
                }
                pair_probes += 1;
                std::thread::sleep(Duration::from_millis(400));
                MergeConflictProbe::Clean
            },
            |_, _, _| provisional_probe::Renumber::Conflict(None),
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.code == QUEUE_CONFLICTS_WITH_ENTRY && w.entry_sequence == Some(6)),
            "{warnings:?}"
        );
        let incomplete = warnings
            .iter()
            .find(|w| w.code == QUEUE_CONFLICT_SCAN_INCOMPLETE)
            .expect("an incomplete warning");
        assert_eq!(pair_probes, 1, "one probe spends the budget");
        assert!(
            incomplete
                .detail
                .contains("queue sequences not fully checked: 4, 3, 2, 1)"),
            "{}",
            incomplete.detail
        );
    }

    /// #2907: a synthetic queue of 32 entries that all conflict with the new
    /// source. An unbounded scan runs 32 probes (each several git processes);
    /// the bounded one stops at its budget and says so.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_conflict_scan_of_a_long_conflicting_queue_stops_at_its_budget_2907() {
        use rsi_common::rolling_queue::{
            QUEUE_CONFLICT_SCAN_INCOMPLETE, QUEUE_CONFLICTS_WITH_ENTRY,
        };
        let (_dir, _origin, work) = repos();
        let queue: Vec<(Uuid, i64, String)> = (0..32)
            .map(|index| {
                let sha = commit_file(
                    &work,
                    &format!("ahead{index}"),
                    "shared",
                    &format!("v{index}"),
                );
                (Uuid::new_v4(), i64::try_from(index).unwrap(), sha)
            })
            .collect();
        let source = commit_file(&work, "source", "shared", "mine");

        // Budget spent before the first probe: nothing is probed, the call
        // returns at once and names the gap.
        let started = std::time::Instant::now();
        let warnings = queue_conflict_warnings_within(&work, &source, &queue, Duration::ZERO);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a spent budget must not probe: {:?}",
            started.elapsed()
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].code, QUEUE_CONFLICT_SCAN_INCOMPLETE);
        assert!(
            warnings[0]
                .detail
                .contains(&format!("{} probe(s)", queue.len() + 1)),
            "{}",
            warnings[0].detail
        );

        // The production budget bounds the whole scan however slow git is, and
        // a scan that finished carries no incomplete marker.
        let started = std::time::Instant::now();
        let warnings =
            queue_conflict_warnings_within(&work, &source, &queue, ENQUEUE_CONFLICT_SCAN_BUDGET);
        assert!(
            started.elapsed() < ENQUEUE_CONFLICT_SCAN_BUDGET + Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        let entry_warnings = warnings
            .iter()
            .filter(|warning| warning.code == QUEUE_CONFLICTS_WITH_ENTRY)
            .count();
        let incomplete = warnings
            .iter()
            .any(|warning| warning.code == QUEUE_CONFLICT_SCAN_INCOMPLETE);
        assert!(
            incomplete || entry_warnings == queue.len(),
            "complete scan names every conflicting entry: {entry_warnings}"
        );
    }

    /// #2912: the conflict scan resolves its rolling ref under the scan's
    /// deadline: a stalled `rev-parse` is killed at the budget and reported as
    /// an incomplete scan, never as "no rolling ref".
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_stalled_ref_resolve_ends_the_conflict_scan_at_its_budget_2912() {
        let (_dir, _origin, work) = repos();
        let source = commit_file(&work, "source", "shared", "mine");
        stall_git(&work, "rev-parse");
        let budget = Duration::from_millis(600);
        let started = std::time::Instant::now();
        let warnings = queue_conflict_warnings_within(&work, &source, &[], budget);
        assert!(
            started.elapsed() < budget + Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(
            warnings[0].code,
            rsi_common::rolling_queue::QUEUE_CONFLICT_SCAN_INCOMPLETE
        );
        assert!(
            stalled_git_was_killed(&work),
            "the stalled git must be killed"
        );
    }

    /// #2912: every git read under `with_git_deadline` (`git` and `git grep`)
    /// is killed at the deadline and the bound reports that it was hit.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn git_reads_under_a_deadline_are_killed_when_it_passes_2912() {
        let (_dir, _origin, work) = repos();
        for subcommand in ["cat-file", "diff", "grep"] {
            stall_git(&work, subcommand);
            let started = std::time::Instant::now();
            let ((answer, grep), hit) =
                with_git_deadline(started + Duration::from_millis(500), || {
                    (
                        git(&work, &[subcommand, "-e", "x"]),
                        git_grep_hits(&work, &["-e", "x"]),
                    )
                });
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "{subcommand}: {:?}",
                started.elapsed()
            );
            assert!(hit, "{subcommand}: the deadline hit must be reported");
            assert_eq!(answer, None, "{subcommand}");
            assert_eq!(grep, None, "{subcommand}");
            assert!(
                stalled_git_was_killed(&work),
                "{subcommand}: the stalled git must be killed"
            );
            std::fs::remove_file(work.join(".rsi-stall")).unwrap();
        }
        // No bound, no hit: a read that finishes is never reported as cut short.
        let (answer, hit) =
            with_git_deadline(std::time::Instant::now() + Duration::from_secs(30), || {
                git(&work, &["rev-parse", "HEAD"])
            });
        assert!(answer.is_some() && !hit);
    }

    /// #2912: the rolling tip read behind the derivation wait is part of the
    /// wait: a stalled `rev-parse` answers "not done" at the bound, killed.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_stalled_tip_read_ends_the_derivation_wait_at_its_bound_2912() {
        let (_dir, _origin, work) = repos();
        let sha = head(&work);
        stall_git(&work, "rev-parse");
        let started = std::time::Instant::now();
        let answer =
            derive_test_filters_shared(&work, &sha, Some(Duration::from_millis(500))).await;
        assert_eq!(
            answer,
            SharedDerivation::TipUnreadable,
            "a stalled tip read is not a derivation in progress (#2993)"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert!(
            stalled_git_was_killed(&work),
            "the stalled git must be killed"
        );
    }

    /// #2907: a slow derivation outlives the call that stopped waiting, the
    /// retry joins it instead of starting another, and a finished one is reused.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_slow_filter_derivation_is_joined_by_the_retry_and_runs_once_2907() {
        let (dir, repo) = rust_free_repo();
        let head = git(&repo, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_owned();
        let runs = dir.path().join("derive-runs");
        std::fs::create_dir_all(repo.join("scripts")).unwrap();
        std::fs::write(
            repo.join("scripts/check-touched-shards"),
            format!(
                "import json, time\nopen({runs:?}, 'a').write('x')\ntime.sleep(1.5)\nprint(json.dumps({{'suggested_filters': ['rsi=slow']}}))\n",
                runs = runs.display().to_string()
            ),
        )
        .unwrap();

        let started = std::time::Instant::now();
        let first =
            derive_test_filters_shared(&repo, &head, Some(Duration::from_millis(200))).await;
        assert_eq!(
            first,
            SharedDerivation::Running,
            "still deriving at the bound"
        );
        assert!(
            started.elapsed() < Duration::from_millis(1200),
            "the bound held: {:?}",
            started.elapsed()
        );
        let joined = derive_test_filters_shared(&repo, &head, Some(Duration::from_secs(30))).await;
        assert_eq!(
            joined,
            SharedDerivation::Ready(vec!["rsi=slow".to_string()])
        );
        let reused = derive_test_filters_shared(&repo, &head, Some(Duration::from_secs(5))).await;
        assert_eq!(
            reused,
            SharedDerivation::Ready(vec!["rsi=slow".to_string()]),
            "a finished derivation is reused"
        );
        assert_eq!(
            std::fs::read_to_string(&runs).unwrap(),
            "x",
            "the checker ran once"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_missing_entry_commit_reports_an_incomplete_conflict_scan_2118() {
        let (_dir, _origin, work) = repos();
        let sha = commit_file(&work, "solo", "solo", "s");
        let missing = "0".repeat(40);
        let warnings = queue_conflict_warnings(&work, &sha, &[(Uuid::new_v4(), 1, missing)]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(
            warnings[0].code,
            rsi_common::rolling_queue::QUEUE_CONFLICT_SCAN_INCOMPLETE
        );
        assert_eq!(warnings[0].entry_id, None);
        assert_eq!(warnings[0].files, Vec::<String>::new());
        assert!(warnings[0].detail.contains("2 probe(s) not completed"));
    }

    /// A source repository with a merge driver configured (config and
    /// attributes) over `shared`, whose driver writes `marker`.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn driver_repo(base: &str) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let (dir, origin, work) = repos();
        let marker = dir.path().join("driver-ran");
        sh(
            &work,
            &format!(
                "printf '{base}' > shared && git add shared && git commit -qm base \
                 && git push -q origin HEAD:rolling && git fetch -q origin rolling \
                 && git config merge.review.driver 'printf invoked > {} ; exit 1' \
                 && printf 'shared merge=review\\n' > .git/info/attributes \
                 && printf 'shared merge=review\\n' > .gitattributes",
                marker.display()
            ),
        );
        (dir, origin, work)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn branch_edit(work: &Path, branch: &str, script: &str) -> String {
        sh(
            work,
            &format!(
                "git checkout -q -B {branch} origin/rolling && {script} && git add shared && git commit -qm {branch}"
            ),
        );
        head(work)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    fn status_of(work: &Path) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(work)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_merge_probe_never_runs_a_configured_merge_driver_on_a_conflict_2706() {
        let (dir, _origin, work) = driver_repo("one\\n");
        let a = branch_edit(&work, "a", "printf 'two\\n' > shared");
        let b = branch_edit(&work, "b", "printf 'three\\n' > shared");
        let before = status_of(&work);
        let warnings = queue_conflict_warnings(&work, &b, &[(Uuid::new_v4(), 1, a)]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].files, ["shared"]);
        assert!(
            !dir.path().join("driver-ran").exists(),
            "the configured merge driver ran during the probe"
        );
        assert_eq!(status_of(&work), before, "the probe changed the source");
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_merge_probe_uses_builtin_semantics_for_a_clean_merge_under_a_driver_2706() {
        let (dir, _origin, work) = driver_repo("a\\nb\\nc\\nd\\ne\\nf\\ng\\n");
        let a = branch_edit(&work, "a", "sed -i '1s/a/A/' shared");
        let b = branch_edit(&work, "b", "sed -i '7s/g/G/' shared");
        let before = status_of(&work);
        assert!(queue_conflict_warnings(&work, &b, &[(Uuid::new_v4(), 1, a)]).is_empty());
        assert!(!dir.path().join("driver-ran").exists());
        assert_eq!(status_of(&work), before);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_rolling_merge_probe_tells_clean_from_conflict_from_unknown_348() {
        let (_dir, _origin, work) = repos();
        let conflicting = commit_file(&work, "a", "shared", "one");
        let clean = commit_file(&work, "c", "other", "x");
        sh(
            &work,
            "git checkout -q rolling && printf 'two\\n' > shared && git add shared \
             && git commit -qm advance && git push -q origin rolling && git fetch -q origin",
        );
        assert_eq!(
            rolling_merge_probe(&work, &conflicting),
            RollingMergeProbe::Conflict(vec!["shared".to_owned()])
        );
        assert_eq!(rolling_merge_probe(&work, &clean), RollingMergeProbe::Clean);
        assert_eq!(
            rolling_merge_probe(&work, &"0".repeat(40)),
            RollingMergeProbe::Unknown
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn a_merge_probe_past_its_timeout_is_killed_and_yields_no_warning_2706() {
        let (dir, _origin, work) = repos();
        let a = commit_file(&work, "a", "shared", "one");
        let b = commit_file(&work, "b", "shared", "two");
        let pid_file = dir.path().join("probe-pid");
        let script = format!("echo $$ > {} ; exec sleep 60", pid_file.display());
        let started = std::time::Instant::now();
        let paths = merge_conflict_paths_with(
            &work,
            &a,
            &b,
            &["sh", "-c", &script, "hung-git"],
            Duration::from_millis(500),
        );
        assert!(
            matches!(paths, MergeConflictProbe::Unknown { .. }),
            "{paths:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{:?}",
            started.elapsed()
        );
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signal 0 only checks that the process exists.
        let alive = unsafe { nix::libc::kill(pid, 0) } == 0;
        assert!(!alive, "the timed-out probe child was left running");
    }

    /// #2714: resolving a commit the source lacks must not lazy-fetch from a
    /// promisor remote, which would run the configured upload-pack helper.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_merge_probe_preflight_never_runs_a_promisor_helper_for_a_missing_commit_2714() {
        let (dir, _origin, work) = repos();
        let present = commit_file(&work, "present", "shared", "one");
        let marker = dir.path().join("helper-ran");
        let helper = dir.path().join("helper.sh");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\necho ran > {}\nexec sleep 60\n",
                marker.display()
            ),
        )
        .unwrap();
        sh(
            &work,
            &format!(
                "chmod +x {helper} && git config remote.origin.promisor true \
                 && git config remote.origin.partialclonefilter blob:none \
                 && git config remote.origin.uploadpack {helper}",
                helper = helper.display()
            ),
        );
        let missing = "1".repeat(40);
        let started = std::time::Instant::now();
        // No launcher is injected: the default `git` runs, so the preflight
        // reads are the only thing that could reach the helper.
        let paths =
            merge_conflict_paths_with(&work, &present, &missing, &["git"], Duration::from_secs(5));
        assert!(
            matches!(paths, MergeConflictProbe::Unknown { .. }),
            "{paths:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5) + Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        assert!(
            !marker.exists(),
            "the probe preflight ran the configured upload-pack helper"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_green_batch_from_one_owner_wakes_that_owner_once_naming_every_entry() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let owner = Uuid::new_v4();
        let mut ids = Vec::new();
        for (index, sha) in shas.iter().enumerate() {
            ids.push(queued_by(&store, &work, sha, &format!("k{index}"), owner).await);
        }
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 3);
        let wakes: Vec<_> = store
            .lock()
            .await
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .collect();
        assert_eq!(wakes.len(), 1, "one owner, one batch, one wake");
        assert_eq!(wakes[0].wake_session_id, Some(owner));
        for (sha, id) in shas.iter().zip(&ids) {
            assert!(wakes[0].message.contains(sha), "{}", wakes[0].message);
            assert!(wakes[0].message.contains(&id.to_string()));
            assert_eq!(
                entry_of(&store, *id).await.state,
                RollingQueueEntryState::Published
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_reported_source_missing_from_the_published_tip_fails_alone() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let guard = format!("export SKIP={}", shas[1]);
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(
            entry_of(&store, ids[0]).await.state,
            RollingQueueEntryState::Published
        );
        let lost = entry_of(&store, ids[1]).await;
        assert_eq!(lost.state, RollingQueueEntryState::Failed);
        assert_eq!(
            lost.outcome.unwrap().refusal.as_deref(),
            Some(QUEUE_BATCH_ANCESTRY_UNVERIFIED)
        );
        assert_eq!(
            entry_of(&store, ids[2]).await.state,
            RollingQueueEntryState::Published
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_merge_conflict_refuses_only_the_conflicting_entry() {
        let (dir, _origin, work) = repos();
        let a = commit_file(&work, "a", "shared", "one");
        let b = commit_file(&work, "b", "shared", "two");
        let c = commit_file(&work, "c", "other", "three");
        let shas = vec![a.clone(), b.clone(), c.clone()];
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        // The conflicting middle entry is refused; the batch retries without it
        // and publishes the other two with one push.
        assert_eq!(lander_runs(&log).len(), 2);
        assert_eq!(log_lines(&log).iter().filter(|l| *l == "push").count(), 1);
        let retry = lander_runs(&log).pop().unwrap();
        assert!(retry.contains(&a) && retry.contains(&c) && !retry.contains(&b));
        for id in [ids[0], ids[2]] {
            assert_eq!(
                entry_of(&store, id).await.state,
                RollingQueueEntryState::Published
            );
        }
        let refused = entry_of(&store, ids[1]).await;
        assert_eq!(refused.state, RollingQueueEntryState::Refused);
        let outcome = refused.outcome.unwrap();
        assert_eq!(outcome.refusal.as_deref(), Some(QUEUE_BATCH_MERGE_CONFLICT));
        assert!(outcome.detail.unwrap().contains("paths: [\"f\"]"));
        assert!(is_ancestor_of_origin(&work, &a));
        assert!(!is_ancestor_of_origin(&work, &b));
        assert!(is_ancestor_of_origin(&work, &c));
        assert_eq!(wake_count(&store).await, 3);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_lander_names_the_conflicting_source() {
        let marked = mark_conflict_source(
            "integration refused: Conflict { paths: [\"f\"] }".to_string(),
            "abc123",
        );
        assert_eq!(conflict_source(&marked).as_deref(), Some("abc123"));
        let other = mark_conflict_source("gate failed".to_string(), "abc123");
        assert_eq!(other, "gate failed");
        assert_eq!(conflict_source(&other), None);
    }

    /// Guard shell for the merging fake lander: any run whose `--accepted`
    /// list contains a `bad` source prints that source's failing test and
    /// exits red, like a red gate.
    fn red_when_any(bad: &[(&str, &str)]) -> String {
        let mut guard = String::from("red=0\n");
        for (sha, test) in bad {
            guard.push_str(&format!(
                "case \" $* \" in *\" {sha} \"*) echo 'test {test} ... FAILED' >&2; red=1;; esac\n"
            ));
        }
        guard.push_str("if [ $red = 1 ]; then echo publication_status=not_published; exit 1; fi");
        guard
    }

    fn lander_runs(log: &Path) -> Vec<String> {
        log_lines(log)
            .into_iter()
            .filter(|l| l.starts_with("run "))
            .collect()
    }

    async fn wake_count(store: &Arc<rsid_core::store_lock::Mutex<Store>>) -> usize {
        store
            .lock()
            .await
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .count()
    }

    /// A batch of `count` sources where `bad` (0-based position -> failing
    /// test) are red. Returns the shas, ids, runs and the work tree.
    async fn bisected(
        count: usize,
        bad: &[(usize, &str)],
    ) -> (
        Arc<rsid_core::store_lock::Mutex<Store>>,
        Vec<String>,
        Vec<Uuid>,
        Vec<String>,
        PathBuf,
        tempfile::TempDir,
    ) {
        let (dir, _origin, work) = repos();
        let names = ["a", "b", "c", "d", "e", "f", "g", "h"];
        let shas: Vec<String> = names[..count]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let bad_shas: Vec<(&str, &str)> = bad
            .iter()
            .map(|(index, test)| (shas[*index].as_str(), *test))
            .collect();
        let launcher = merging_lander(dir.path(), &work, &log, &red_when_any(&bad_shas));
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, count).await.unwrap();
        assert_eq!(results.len(), count, "every member settles exactly once");
        let runs = lander_runs(&log);
        (store, shas, ids, runs, work, dir)
    }

    async fn assert_isolated_red(
        store: &Arc<rsid_core::store_lock::Mutex<Store>>,
        id: Uuid,
        test: &str,
    ) {
        let entry = entry_of(store, id).await;
        assert_eq!(entry.state, RollingQueueEntryState::Refused);
        let outcome = entry.outcome.unwrap();
        assert_eq!(outcome.refusal.as_deref(), Some("gate_failed"));
        assert_eq!(outcome.failing_tests, vec![test.to_string()]);
        assert_eq!(outcome.landed_sha, None);
    }

    async fn assert_landed(
        store: &Arc<rsid_core::store_lock::Mutex<Store>>,
        work: &Path,
        id: Uuid,
        sha: &str,
    ) {
        let entry = entry_of(store, id).await;
        assert_eq!(entry.state, RollingQueueEntryState::Published);
        let landed = entry.outcome.unwrap().landed_sha.unwrap();
        assert_eq!(landed.len(), 40);
        assert!(is_ancestor_of_origin(work, sha));
    }

    fn events_of(store: &Store, kind: &str) -> Vec<serde_json::Value> {
        store
            .conn
            .prepare("SELECT detail_json FROM rolling_queue_events WHERE kind=?1 ORDER BY id")
            .unwrap()
            .query_map([kind], |row| row.get::<_, Option<String>>(0))
            .unwrap()
            .map(|detail| serde_json::from_str(&detail.unwrap().unwrap()).unwrap())
            .collect()
    }

    /// A sandbox worktree of `work`'s repository holding one new commit, like
    /// the per-worker sandbox an Issue worker lands from (#1945).
    fn sandbox_commit(work: &Path, name: &str) -> (PathBuf, String) {
        let tree = work.parent().unwrap().join(format!("sandbox-{name}"));
        sh(
            work,
            &format!(
                "git worktree add -q -b {name} {} origin/rolling",
                tree.display()
            ),
        );
        sh(
            &tree,
            &format!("printf '{name}\\n' > {name} && git add {name} && git commit -qm {name}"),
        );
        let sha = head(&tree);
        (tree, sha)
    }

    async fn queue_sandboxes(
        store: &Arc<rsid_core::store_lock::Mutex<Store>>,
        sandboxes: &[(PathBuf, String)],
        project: Option<Uuid>,
    ) -> Vec<Uuid> {
        let mut ids = Vec::new();
        for (index, (tree, sha)) in sandboxes.iter().enumerate() {
            ids.push(
                queued_in_project(
                    store,
                    tree,
                    sha,
                    &format!("k{index}"),
                    Uuid::new_v4(),
                    project,
                )
                .await,
            );
        }
        ids
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn sandboxes_of_one_repository_gate_as_one_batch_and_report_its_size() {
        let (dir, _origin, work) = repos();
        let sandboxes: Vec<(PathBuf, String)> = ["a", "b", "c"]
            .iter()
            .map(|name| sandbox_commit(&work, name))
            .collect();
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(
            dir.path(),
            &work,
            &log,
            "echo 'gate_timing=shards=1 wall_secs=3'",
        );
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_sandboxes(&store, &sandboxes, Some(Uuid::from_u128(0x1945))).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        // One gate for three sandboxes, settled in queue order.
        assert_eq!(results.iter().map(|r| r.0).collect::<Vec<_>>(), ids);
        let runs = lander_runs(&log);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(log_lines(&log).iter().filter(|l| *l == "push").count(), 1);
        for (_, sha) in &sandboxes {
            assert!(
                runs[0].contains(&format!("--accepted {sha}")),
                "{}",
                runs[0]
            );
            assert!(is_ancestor_of_origin(&work, sha));
        }
        for id in ids {
            let entry = entry_of(&store, id).await;
            assert_eq!(entry.state, RollingQueueEntryState::Published);
            assert_eq!(
                entry.outcome.unwrap().detail.as_deref(),
                Some("gate_timing=shards=1 wall_secs=3 batch_size=3")
            );
        }
        let guard = store.lock().await;
        let members: i64 = guard
            .conn
            .query_row("SELECT member_count FROM rolling_queue_batches", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(members, 3);
        let formed = events_of(&guard, "batch_formed");
        assert_eq!(formed.len(), 1);
        assert_eq!(formed[0]["members"], 3);
        assert_eq!(formed[0]["checkouts"], 3);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn other_projects_and_other_repositories_never_join_a_batch() {
        let (dir, _origin, work) = repos();
        let (_other_dir, _other_origin, other_work) = repos();
        let (a, b, c) = (
            sandbox_commit(&work, "a"),
            sandbox_commit(&work, "b"),
            sandbox_commit(&work, "c"),
        );
        let foreign = commit_file(&other_work, "x", "x", "x");
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let (project, other_project) = (Some(Uuid::from_u128(0x1945)), Some(Uuid::from_u128(0x2)));
        let first = queue_sandboxes(&store, &[a.clone()], project).await[0];
        let foreign_id =
            queued_in_project(&store, &other_work, &foreign, "kf", Uuid::new_v4(), project).await;
        let second = queued_in_project(&store, &b.0, &b.1, "kb", Uuid::new_v4(), project).await;
        let third =
            queued_in_project(&store, &c.0, &c.1, "kc", Uuid::new_v4(), other_project).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        // Only the head's project and repository gated together.
        assert_eq!(
            results.iter().map(|r| r.0).collect::<Vec<_>>(),
            [first, second]
        );
        let runs = lander_runs(&log);
        assert_eq!(runs.len(), 1);
        assert!(runs[0].contains(&a.1) && runs[0].contains(&b.1));
        assert!(!runs[0].contains(&c.1) && !runs[0].contains(&foreign));
        for id in [foreign_id, third] {
            assert_eq!(
                entry_of(&store, id).await.state,
                RollingQueueEntryState::Queued
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_red_sandbox_member_is_isolated_and_the_others_still_publish() {
        let (dir, _origin, work) = repos();
        let sandboxes: Vec<(PathBuf, String)> = ["a", "b", "c"]
            .iter()
            .map(|name| sandbox_commit(&work, name))
            .collect();
        let log = dir.path().join("lander.log");
        let guard = red_when_any(&[(sandboxes[1].1.as_str(), "q::red_b")]);
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_sandboxes(&store, &sandboxes, Some(Uuid::from_u128(0x1945))).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 3, "every member settles exactly once");
        assert_isolated_red(&store, ids[1], "q::red_b").await;
        assert_landed(&store, &work, ids[0], &sandboxes[0].1).await;
        assert_landed(&store, &work, ids[2], &sandboxes[2].1).await;
        assert!(!is_ancestor_of_origin(&work, &sandboxes[1].1));
        assert_eq!(wake_count(&store).await, 3);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_member_whose_commit_is_gone_is_refused_typed_and_the_rest_gate() {
        let (dir, _origin, work) = repos();
        let live = sandbox_commit(&work, "a");
        let (second_tree, _) = sandbox_commit(&work, "b");
        let pruned = "1".repeat(40);
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let project = Some(Uuid::from_u128(0x1945));
        let kept = queued_in_project(&store, &live.0, &live.1, "k0", Uuid::new_v4(), project).await;
        let gone =
            queued_in_project(&store, &second_tree, &pruned, "k1", Uuid::new_v4(), project).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 2, "no member is dropped");
        let refused = entry_of(&store, gone).await;
        assert_eq!(refused.state, RollingQueueEntryState::Refused);
        let outcome = refused.outcome.unwrap();
        assert_eq!(outcome.refusal.as_deref(), Some(QUEUE_SOURCE_UNREACHABLE));
        let detail = outcome.detail.unwrap();
        assert!(
            detail.contains(&pruned) && detail.contains("re-enqueue"),
            "{detail}"
        );
        assert_landed(&store, &work, kept, &live.1).await;
        let runs = lander_runs(&log);
        assert_eq!(runs.len(), 1);
        assert!(!runs[0].contains(&pruned), "{}", runs[0]);
        assert_eq!(wake_count(&store).await, 2);
        let guard = store.lock().await;
        assert_eq!(events_of(&guard, "source_unreachable").len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_batch_from_one_checkout_is_left_to_the_lander() {
        let (dir, _origin, work) = repos();
        let sha = commit_file(&work, "a", "a", "a");
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let id = queued(&store, &work, &sha, "k").await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            entry_of(&store, id).await.state,
            RollingQueueEntryState::Published
        );
        let guard = store.lock().await;
        assert!(events_of(&guard, "batch_formed").is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn checkouts_of_one_repository_share_an_identity_and_a_vanished_one_keeps_its_path() {
        let (dir, _origin, work) = repos();
        let (first, _) = sandbox_commit(&work, "a");
        let (second, _) = sandbox_commit(&work, "b");
        let (_other_dir, _other_origin, other_work) = repos();
        let key = |path: &Path| repository_key(&path.display().to_string());
        assert_eq!(key(&first), key(&second));
        assert_eq!(key(&first), key(&work));
        assert_ne!(key(&first), key(&other_work));
        assert!(key(&first).starts_with("git:"));
        let gone = dir.path().join("reclaimed");
        assert_eq!(key(&gone), format!("path:{}", gone.display()));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_gate_timing_line_carries_the_batch_size_and_other_details_are_untouched() {
        let detail = "gate_timing=shards=2 wall_secs=30\nfailing tests: q::red".to_string();
        assert_eq!(
            note_batch_size(Some(detail), 4).as_deref(),
            Some("gate_timing=shards=2 wall_secs=30 batch_size=4\nfailing tests: q::red")
        );
        let untimed = "lander refused".to_string();
        assert_eq!(note_batch_size(Some(untimed.clone()), 4), Some(untimed));
        assert_eq!(note_batch_size(None, 4), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_batch_workspace_failure_keeps_the_first_checkouts_typed_refusal() {
        let (dir, _origin, work) = repos();
        let (first, _) = sandbox_commit(&work, "a");
        let (second, _) = sandbox_commit(&work, "b");
        let mut launcher = fake_lander(dir.path(), "exit 0");
        launcher.workspace = Some(dir.path().join("queue"));
        launcher.fence_abi = || Ok(2);
        let checkouts = [first.display().to_string(), second.display().to_string()];
        let error = queue_repo_for_batch(&launcher, &checkouts)
            .await
            .unwrap_err();
        assert!(error.starts_with(write_fence::UNSUPPORTED_CODE), "{error}");
    }

    /// A head whose sandbox was reclaimed does not fail the batch: the queue
    /// checkout is made from the next member's checkout. Skipped on a host
    /// without the write fence the queue worktree needs (#1160).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_reclaimed_head_checkout_does_not_stop_the_batch_workspace() {
        if write_fence::require_abi(WriteFence::kernel_abi()).is_err() {
            return;
        }
        let (dir, _origin, work) = repos();
        let (live, _) = sandbox_commit(&work, "a");
        let mut launcher = fake_lander(dir.path(), "exit 0");
        launcher.workspace = Some(dir.path().join("queue"));
        let gone = dir.path().join("reclaimed-sandbox").display().to_string();
        let checkouts = [gone, live.display().to_string()];
        let repo = queue_repo_for_batch(&launcher, &checkouts).await.unwrap();
        assert!(
            repo.starts_with(dir.path().join("queue/worktrees")),
            "{repo:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_red_gate_event_names_the_failing_tests_and_a_log_that_exists() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(
            dir.path(),
            &work,
            &log,
            &red_when_any(&[(shas[1].as_str(), "q::red_b")]),
        )
        .with_workspace(dir.path().join("queue"));
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 3).await.unwrap();
        let guard = store.lock().await;
        let mut details = events_of(&guard, "bisect_started");
        details.extend(events_of(&guard, "bisect_gate"));
        let red: Vec<&serde_json::Value> = details
            .iter()
            .filter(|detail| detail["outcome"] != "green")
            .collect();
        assert!(!red.is_empty(), "{details:?}");
        for detail in red {
            let cause = detail["cause"].as_str().unwrap();
            assert!(cause.contains("gate_failed"), "{cause}");
            assert!(cause.contains("failing_tests=q::red_b"), "{cause}");
            let path = detail["log"].as_str().unwrap();
            let text = std::fs::read_to_string(path).unwrap();
            assert!(text.contains("q::red_b ... FAILED"), "{text}");
        }
        // The settled entry points at a log too.
        let red_entry = guard
            .conn
            .query_row(
                "SELECT outcome_json FROM rolling_queue_entries WHERE state='refused'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert!(red_entry.contains("lander_log="), "{red_entry}");
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_conflict_refusal_event_names_the_conflicting_path() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let body = format!(
            "case \" $* \" in *\" {} \"*) echo 'integration refused: Conflict {{ paths: [\"src/x.rs\", \"y.rs\"] }}; conflict_source={}' >&2; exit 1;; esac\nexit 1",
            shas[0], shas[0]
        );
        let launcher = fake_lander(dir.path(), &body).with_workspace(dir.path().join("queue"));
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 2).await.unwrap();
        let guard = store.lock().await;
        let events = events_of(&guard, "conflict_isolated");
        assert_eq!(events.len(), 1, "{events:?}");
        let cause = events[0]["cause"].as_str().unwrap();
        assert!(cause.contains("conflict_paths=src/x.rs,y.rs"), "{cause}");
        assert!(std::path::Path::new(events[0]["log"].as_str().unwrap()).exists());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_run_log_directory_keeps_only_the_newest_bounded_files() {
        let dir = tempfile::tempdir().unwrap();
        let big = "x".repeat(RUN_LOG_STREAM_BYTES + 4096);
        let batch = Uuid::new_v4();
        let mut last = None;
        for _ in 0..(RUN_LOG_KEEP_FILES + 7) {
            last = write_run_log(dir.path(), batch, "hdr", &big, "err");
        }
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let logs = names.iter().filter(|name| name.ends_with(".log")).count();
        let sidecars = names
            .iter()
            .filter(|name| name.ends_with(".timings.tsv"))
            .count();
        assert_eq!((logs, sidecars), (RUN_LOG_KEEP_FILES, RUN_LOG_KEEP_FILES));
        let last = last.unwrap();
        assert!(last.exists(), "the newest log survives rotation");
        let size = std::fs::metadata(&last).unwrap().len() as usize;
        assert!(size < RUN_LOG_STREAM_BYTES + 512, "{size}");
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn each_gate_receives_a_timing_sidecar_next_to_its_log() {
        let dir = tempfile::tempdir().unwrap();
        let launcher = fake_lander(
            dir.path(),
            "printf 'candidate-0\\trsid\\ta::test\\t1.234\\tPASS\\n' >> \"$RSI_LANDER_TEST_TIMINGS\"",
        )
        .with_workspace(dir.path().join("queue"));
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let run = run_group(
            &store,
            &launcher,
            Uuid::new_v4(),
            dir.path(),
            &[],
            None,
            tokio::time::Instant::now() + Duration::from_secs(10),
        )
        .await;
        let log = run.log_path.unwrap();
        assert!(log.is_file());
        assert_eq!(
            std::fs::read_to_string(log.with_extension("timings.tsv")).unwrap(),
            concat!(
                "run\tbinary\ttest\tseconds\tstatus\n",
                "candidate-0\trsid\ta::test\t1.234\tPASS\n"
            )
        );
        assert!(
            run.result
                .outcome
                .detail
                .unwrap()
                .contains(&log.display().to_string())
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_bad_first_source_is_isolated_in_two_extra_gates_and_the_innocents_publish() {
        let (store, shas, ids, runs, work, _dir) = bisected(4, &[(0, "q::red_a")]).await;
        // batch (red), [a,b] (red), [a] (red: isolated), then [b,c,d] (green).
        assert_eq!(runs.len(), 4, "{runs:?}");
        let counts: Vec<usize> = runs
            .iter()
            .map(|r| r.matches("--accepted").count())
            .collect();
        assert_eq!(counts, vec![4, 2, 1, 3]);
        assert_isolated_red(&store, ids[0], "q::red_a").await;
        for index in 1..4 {
            assert_landed(&store, &work, ids[index], &shas[index]).await;
        }
        assert!(!is_ancestor_of_origin(&work, &shas[0]));
        assert_eq!(wake_count(&store).await, 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_bad_last_source_is_isolated_in_two_extra_gates_and_the_innocents_publish() {
        let (store, shas, ids, runs, work, _dir) = bisected(4, &[(3, "q::red_d")]).await;
        // batch (red), [a,b] (green, published), [c] (green, published), d isolated.
        let counts: Vec<usize> = runs
            .iter()
            .map(|r| r.matches("--accepted").count())
            .collect();
        assert_eq!(counts, vec![4, 2, 1], "{runs:?}");
        assert_isolated_red(&store, ids[3], "q::red_d").await;
        for index in 0..3 {
            assert_landed(&store, &work, ids[index], &shas[index]).await;
        }
        assert!(!is_ancestor_of_origin(&work, &shas[3]));
        assert_eq!(wake_count(&store).await, 4);
        // The bisect trail is on the batch, in order.
        let guard = store.lock().await;
        let kinds: Vec<String> = guard
            .conn
            .prepare("SELECT kind FROM rolling_queue_events ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|kind| kind.unwrap())
            .collect();
        let trail: Vec<&str> = kinds
            .iter()
            .map(String::as_str)
            .filter(|kind| kind.starts_with("bisect"))
            .collect();
        assert_eq!(
            trail,
            vec![
                "bisect_started",
                "bisect_gate",
                "bisect_gate",
                "bisect_isolated"
            ]
        );
        let state: String = guard
            .conn
            .query_row("SELECT state FROM rolling_queue_batches", [], |r| r.get(0))
            .unwrap();
        assert_eq!(state, "published");
    }

    /// A batch of four where source `d` is red and the failure output names
    /// `named` files (rustc `-->` lines). Returns the store, shas, ids, runs.
    async fn red_naming(
        named: &[&str],
    ) -> (
        Arc<rsid_core::store_lock::Mutex<Store>>,
        Vec<String>,
        Vec<Uuid>,
        Vec<usize>,
        PathBuf,
        tempfile::TempDir,
    ) {
        red_naming_at(3, named).await
    }

    /// Like `red_naming`, with member `red` (index) the red one.
    async fn red_naming_at(
        red: usize,
        named: &[&str],
    ) -> (
        Arc<rsid_core::store_lock::Mutex<Store>>,
        Vec<String>,
        Vec<Uuid>,
        Vec<usize>,
        PathBuf,
        tempfile::TempDir,
    ) {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let locations: String = named
            .iter()
            .map(|file| format!("echo ' --> {file}:1:1' >&2\n"))
            .collect();
        let guard = format!(
            "case \" $* \" in *\" {} \"*) echo 'error[E0425]: boom' >&2\n{locations}\
             echo publication_status=not_published; exit 1;; esac",
            shas[red]
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 4, "every member settles exactly once");
        let counts = lander_runs(&log)
            .iter()
            .map(|r| r.matches("--accepted").count())
            .collect();
        (store, shas, ids, counts, work, dir)
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_failure_in_files_only_one_member_changes_refuses_it_and_regates_the_rest_whole() {
        let (store, shas, ids, counts, work, _dir) = red_naming(&["d"]).await;
        // batch (red, names only d's file), then [a,b,c] as one batch (green):
        // that confirms d, and nothing is bisected.
        assert_eq!(counts, vec![4, 3]);
        let refused = entry_of(&store, ids[3]).await;
        assert_eq!(refused.state, RollingQueueEntryState::Refused);
        let outcome = refused.outcome.unwrap();
        assert_eq!(outcome.refusal.as_deref(), Some("gate_failed"));
        let detail = outcome.detail.unwrap();
        assert!(
            detail.contains("outcome_reason=attributed_confirmed"),
            "{detail}"
        );
        assert!(detail.contains("bisect_rounds=1"), "{detail}");
        for index in 0..3 {
            assert_landed(&store, &work, ids[index], &shas[index]).await;
            let detail = entry_of(&store, ids[index]).await.outcome.unwrap().detail;
            let detail = detail.unwrap();
            assert!(detail.contains("bisect_rounds=1"), "{detail}");
            assert!(
                detail.contains("outcome_reason=published_after_bisect"),
                "{detail}"
            );
        }
        assert!(!is_ancestor_of_origin(&work, &shas[3]));
        let guard = store.lock().await;
        assert_eq!(events_of(&guard, "bisect_attributed").len(), 1);
        assert_eq!(events_of(&guard, "bisect_attribution_confirmed").len(), 1);
        assert!(events_of(&guard, "bisect_attribution_rejected").is_empty());
        assert!(events_of(&guard, "bisect_isolated").is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_wrong_attribution_is_rejected_by_a_red_complement_and_the_member_stays_in_play() {
        // b is the red source, but the failure output names only d's file.
        let (store, shas, ids, counts, work, _dir) = red_naming_at(1, &["d"]).await;
        // batch (red, names d), complement [a,b,c] (red: rejected), then the
        // halving bisect: [a,b] (red), [a] (green), b isolated, tail [c,d] green.
        assert_eq!(counts, vec![4, 3, 2, 1, 2]);
        let refused = entry_of(&store, ids[1]).await;
        assert_eq!(refused.state, RollingQueueEntryState::Refused);
        let detail = refused.outcome.unwrap().detail.unwrap();
        assert!(
            detail.contains("outcome_reason=attribution_rejected"),
            "{detail}"
        );
        for index in [0, 2, 3] {
            assert_landed(&store, &work, ids[index], &shas[index]).await;
        }
        // The wrongly attributed member was not refused: it landed.
        assert!(is_ancestor_of_origin(&work, &shas[3]));
        assert!(!is_ancestor_of_origin(&work, &shas[1]));
        let guard = store.lock().await;
        assert_eq!(events_of(&guard, "bisect_attributed").len(), 1);
        assert_eq!(events_of(&guard, "bisect_attribution_rejected").len(), 1);
        assert!(events_of(&guard, "bisect_attribution_confirmed").is_empty());
        assert_eq!(events_of(&guard, "bisect_isolated").len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_failure_naming_files_two_members_change_is_bisected_and_reports_its_rounds() {
        // The output names a's and d's files: no single member owns it.
        let (store, shas, ids, counts, work, _dir) = red_naming(&["a", "d"]).await;
        // batch (red), [a,b] (green), [c] (green), d isolated: the bisect ran.
        assert_eq!(counts, vec![4, 2, 1]);
        let refused = entry_of(&store, ids[3]).await;
        assert_eq!(refused.state, RollingQueueEntryState::Refused);
        let detail = refused.outcome.unwrap().detail.unwrap();
        assert!(
            detail.contains("outcome_reason=bisect_isolated"),
            "{detail}"
        );
        assert!(detail.contains("bisect_rounds=2"), "{detail}");
        for index in 0..3 {
            assert_landed(&store, &work, ids[index], &shas[index]).await;
        }
        let guard = store.lock().await;
        assert!(events_of(&guard, "bisect_attributed").is_empty());
        assert_eq!(events_of(&guard, "bisect_isolated").len(), 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn two_bad_sources_in_a_batch_of_four_both_end_red_with_their_own_failing_tests() {
        let (store, shas, ids, _runs, work, _dir) =
            bisected(4, &[(0, "q::red_a"), (2, "q::red_c")]).await;
        assert_isolated_red(&store, ids[0], "q::red_a").await;
        assert_isolated_red(&store, ids[2], "q::red_c").await;
        assert_landed(&store, &work, ids[1], &shas[1]).await;
        assert_landed(&store, &work, ids[3], &shas[3]).await;
        assert!(!is_ancestor_of_origin(&work, &shas[0]));
        assert!(!is_ancestor_of_origin(&work, &shas[2]));
        assert_eq!(wake_count(&store).await, 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_all_red_batch_settles_every_source_red_and_never_loops() {
        let (store, _shas, ids, runs, _work, _dir) =
            bisected(3, &[(0, "q::r0"), (1, "q::r1"), (2, "q::r2")]).await;
        assert!(runs.len() <= 3 * 4 + 4, "{runs:?}");
        for (index, id) in ids.iter().enumerate() {
            assert_isolated_red(&store, *id, &format!("q::r{index}")).await;
        }
        assert_eq!(wake_count(&store).await, 3);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_second_settle_of_a_bisected_entry_writes_nothing_and_wakes_nobody() {
        let (store, _shas, ids, _runs, _work, _dir) = bisected(4, &[(3, "q::red_d")]).await;
        assert_eq!(wake_count(&store).await, 4);
        let again = RunResult {
            state: RollingQueueEntryState::Refused,
            outcome: RollingQueueOutcome {
                refusal: Some("gate_failed".into()),
                ..RollingQueueOutcome::default()
            },
        };
        for id in &ids {
            settle(&store, *id, &again).await.unwrap();
        }
        assert_eq!(wake_count(&store).await, 4);
        // The first outcome is untouched: the published entries stay published.
        assert_eq!(
            entry_of(&store, ids[0]).await.state,
            RollingQueueEntryState::Published
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_single_source_the_lander_reports_landed_but_the_tip_lacks_fails_typed() {
        let (dir, _origin, work) = repos();
        let sha = commit_file(&work, "a", "a", "a");
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, &format!("export SKIP={sha}"));
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let id = queued(&store, &work, &sha, "k").await;
        run_next(&store, &launcher).await.unwrap().unwrap();
        let entry = entry_of(&store, id).await;
        assert_eq!(entry.state, RollingQueueEntryState::Failed);
        assert_eq!(
            entry.outcome.unwrap().refusal.as_deref(),
            Some(QUEUE_BATCH_ANCESTRY_UNVERIFIED)
        );
        assert_eq!(wake_count(&store).await, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_bisect_segment_is_ancestry_verified_before_it_settles_published() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // d is red; the lander lies about b in every published segment.
        let guard = format!(
            "export SKIP={}\n{}",
            shas[1],
            red_when_any(&[(shas[3].as_str(), "q::red_d")])
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        let lost = entry_of(&store, ids[1]).await;
        assert_eq!(lost.state, RollingQueueEntryState::Failed);
        assert_eq!(
            lost.outcome.unwrap().refusal.as_deref(),
            Some(QUEUE_BATCH_ANCESTRY_UNVERIFIED)
        );
        for index in [0, 2] {
            assert_landed(&store, &work, ids[index], &shas[index]).await;
        }
        assert_isolated_red(&store, ids[3], "q::red_d").await;
        assert_eq!(wake_count(&store).await, 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_published_segment_records_the_lander_final_migration_number() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // a is red; b is renumbered to 903 by the lander once a is out.
        let guard = format!(
            "echo migration_0_source_id={b}\necho migration_0_final_version=903\n{red}",
            b = shas[1],
            red = red_when_any(&[(shas[0].as_str(), "q::red_a")])
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_isolated_red(&store, ids[0], "q::red_a").await;
        let published = entry_of(&store, ids[1]).await;
        assert_eq!(published.state, RollingQueueEntryState::Published);
        assert_eq!(published.migration_version, Some(903));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn every_run_of_a_bisect_gets_its_own_wall_time_not_a_regate_count() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // The tip keeps moving: every run reports one re-gated stale retry.
        let guard = format!(
            "echo stale_retry_1=aa..bb:regated\n{}",
            red_when_any(&[(shas[3].as_str(), "q::red_d")])
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        // #1940, #3267: the tip keeps moving, yet no run is refused for a spent
        // count, and a run is not shortened by the runs before it: each is given
        // the full per-run wall time.
        let budgets = budgets(&log);
        assert!(budgets.len() >= 3, "{budgets:?}");
        assert!(
            budgets.iter().all(|secs| (3500..=3600).contains(secs)),
            "{budgets:?}"
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_batch_ceiling_scales_with_the_bisect_depth() {
        let per_run = Duration::from_secs(60 * 60);
        let ceiling = |members: usize| {
            let budget = GateBudget::new(per_run, members);
            budget.ceiling - tokio::time::Instant::now()
        };
        let runs = |members: usize| (ceiling(members).as_secs_f64() / 3600.0).round() as u32;
        // A full gate, ceil(log2 n) halvings and the extra runs a non-attributable
        // red needs: a batch of four can isolate its culprit.
        assert_eq!(runs(1), BATCH_EXTRA_RUNS);
        assert_eq!(runs(2), BATCH_EXTRA_RUNS + 1);
        assert_eq!(runs(4), BATCH_EXTRA_RUNS + 2);
        assert_eq!(runs(8), BATCH_EXTRA_RUNS + 3);
        assert!(runs(4) >= 1 + 2 + 1, "full run + bisect depth + tail");
    }

    /// A merging fake lander that also takes `pause` seconds per run, and hangs
    /// (stuck gate) on a run whose sources include `hang_with` but not
    /// `hang_without`.
    fn slow_lander(
        dir: &Path,
        work: &Path,
        log: &Path,
        red: &[(&str, &str)],
        pause: &str,
        hang: Option<(&str, &str)>,
    ) -> LanderLauncher {
        let mut guard = format!("sleep {pause}\n");
        if let Some((with, without)) = hang {
            guard.push_str(&format!(
                "case \" $* \" in *\" {with} \"*) case \" $* \" in *\" {without} \"*) ;; \
                 *) sleep 120;; esac;; esac\n"
            ));
        }
        guard.push_str(&red_when_any(red));
        merging_lander(dir, work, log, &guard)
    }

    #[cfg(target_os = "linux")]
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_non_attributable_red_in_a_batch_of_four_is_isolated_though_the_runs_outlast_one_budget()
     {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // Four runs of about 1.3 s each: past the 3 s a single shared budget
        // would give the batch, inside the 3 s each run gets (#3267).
        let launcher = slow_lander(
            dir.path(),
            &work,
            &log,
            &[(shas[1].as_str(), "q::red_b")],
            "1.3",
            None,
        )
        .with_gate_timeout(Duration::from_secs(3))
        .with_workspace(dir.path().join("queue"));
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        let started = std::time::Instant::now();
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 4);
        assert!(
            started.elapsed() > Duration::from_secs(3),
            "the batch's runs together outlast one budget: {:?}",
            started.elapsed()
        );
        // batch (red), [a,b] (red), [a] (green), b isolated with NO run of its
        // own, tail [c,d] (green).
        let counts: Vec<usize> = lander_runs(&log)
            .iter()
            .map(|r| r.matches("--accepted").count())
            .collect();
        assert_eq!(counts, vec![4, 2, 1, 2]);
        for index in [0, 2, 3] {
            assert_landed(&store, &work, ids[index], &shas[index]).await;
        }
        // The suspect window narrowed to one member: it is refused with the
        // failing test named, and the outcome points at the bisect's run logs.
        let refused = entry_of(&store, ids[1]).await;
        assert_eq!(refused.state, RollingQueueEntryState::Refused);
        let outcome = refused.outcome.unwrap();
        assert_eq!(outcome.refusal.as_deref(), Some("gate_failed"));
        assert_eq!(outcome.failing_tests, vec!["q::red_b".to_string()]);
        let detail = outcome.detail.unwrap();
        assert!(
            detail.contains("outcome_reason=bisect_isolated"),
            "{detail}"
        );
        let logs = detail
            .lines()
            .find_map(|line| line.strip_prefix("bisect_logs="))
            .unwrap_or_else(|| panic!("the outcome names the bisect evidence: {detail}"));
        assert_eq!(logs.split(',').count(), 3, "{logs}");
        for path in logs.split(',') {
            assert!(Path::new(path).is_file(), "{path}");
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_timeout_mid_bisect_requeues_members_a_red_run_left_out_and_refuses_only_the_suspects()
     {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // b is red. The batch (red) and [a,b] (red, which leaves c and d out
        // while the red persists) are fast; the gate of [a] alone then hangs.
        let launcher = slow_lander(
            dir.path(),
            &work,
            &log,
            &[(shas[1].as_str(), "q::red_b")],
            "0",
            Some((shas[0].as_str(), shas[1].as_str())),
        )
        .with_gate_timeout(Duration::from_secs(4));
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        let started = std::time::Instant::now();
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(60));
        // Only the members still in the suspect window are settled.
        assert_eq!(results.len(), 2);
        for index in [0, 1] {
            let entry = entry_of(&store, ids[index]).await;
            assert_eq!(entry.state, RollingQueueEntryState::Refused);
            assert_eq!(
                entry.outcome.unwrap().refusal.as_deref(),
                Some(QUEUE_GATE_TIMEOUT)
            );
        }
        // c and d are not refused: they are back in the queue, in order.
        let mut sequences = Vec::new();
        for index in [2, 3] {
            let entry = entry_of(&store, ids[index]).await;
            assert_eq!(entry.state, RollingQueueEntryState::Queued);
            assert!(entry.outcome.is_none());
            sequences.push(entry.sequence);
        }
        assert!(sequences[0] < sequences[1], "{sequences:?}");
        for sha in &shas {
            assert!(!is_ancestor_of_origin(&work, sha));
        }
        assert_eq!(wake_count(&store).await, 2, "only the refused are woken");
        let guard = store.lock().await;
        let timeout = events_of(&guard, "gate_timeout");
        assert_eq!(timeout.len(), 1);
        assert_eq!(timeout[0]["refused"], 2);
        assert_eq!(timeout[0]["requeued"], 2);
        assert_eq!(events_of(&guard, "requeued_after_bisect").len(), 2);
        let state: String = guard
            .conn
            .query_row("SELECT state FROM rolling_queue_batches", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            state, "refused",
            "the batch closes with its last open member"
        );
        drop(guard);
        // They are claimed again at their original position, ahead of anything
        // enqueued later.
        let later = commit_file(&work, "e", "e", "e");
        let later_id = queued(&store, &work, &later, "k-late").await;
        let again = claim_batch(&store, 4).await.unwrap().unwrap();
        let claimed: Vec<Uuid> = again.entries.iter().map(|entry| entry.id).collect();
        assert_eq!(claimed, vec![ids[2], ids[3], later_id]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_timeout_with_nothing_proved_still_refuses_every_member() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // The very first gate hangs: no run proved anything about anyone.
        let launcher = slow_lander(
            dir.path(),
            &work,
            &log,
            &[],
            "0",
            Some((shas[0].as_str(), "none")),
        )
        .with_gate_timeout(Duration::from_secs(2));
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 4);
        for id in ids {
            let entry = entry_of(&store, id).await;
            assert_eq!(entry.state, RollingQueueEntryState::Refused);
            assert_eq!(
                entry.outcome.unwrap().refusal.as_deref(),
                Some(QUEUE_GATE_TIMEOUT)
            );
        }
        let guard = store.lock().await;
        assert!(events_of(&guard, "requeued_after_bisect").is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn restart_in_the_middle_of_a_bisect_settles_each_entry_once_without_a_second_push() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        let claimed = store
            .lock()
            .await
            .claim_rolling_queue_batch(Utc::now(), 4)
            .unwrap()
            .unwrap();
        store
            .lock()
            .await
            .begin_rolling_queue_bisect(claimed.batch_id, "gate_failed", Utc::now())
            .unwrap();
        // The bisect had published [a,b] and settled a; the daemon died before
        // it could settle b, and c/d were still being bisected.
        sh(
            &work,
            &format!(
                "git checkout -q -B land origin/rolling && git merge -q --no-edit {} {} \
                 && git push -q origin HEAD:rolling",
                shas[0], shas[1]
            ),
        );
        let landed = RunResult {
            state: RollingQueueEntryState::Published,
            outcome: RollingQueueOutcome {
                landed_sha: Some(head(&work)),
                ..RollingQueueOutcome::default()
            },
        };
        settle(&store, ids[0], &landed).await.unwrap();
        assert_eq!(wake_count(&store).await, 1);
        // Boot: b landed (settled once), c and d go back to the queue.
        assert_eq!(reconcile_gating(&store).await.unwrap(), (1, 2));
        assert_eq!(reconcile_gating(&store).await.unwrap(), (0, 0));
        assert_eq!(wake_count(&store).await, 2);
        for id in &ids[..2] {
            assert_eq!(
                entry_of(&store, *id).await.state,
                RollingQueueEntryState::Published
            );
        }
        for id in &ids[2..] {
            assert_eq!(
                entry_of(&store, *id).await.state,
                RollingQueueEntryState::Queued
            );
        }
        // Re-driven: c and d publish once each, nothing republished.
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(lander_runs(&log).len(), 1);
        for sha in &shas {
            assert!(is_ancestor_of_origin(&work, sha));
        }
        assert_eq!(wake_count(&store).await, 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_lander_policy_refusal_propagates_as_the_batch_policy_code() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(
            dir.path(),
            &work,
            &log,
            "echo policy_fence=migration_number; echo 'migration must be tip + 1' >&2; exit 8",
        );
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        for id in ids {
            let entry = entry_of(&store, id).await;
            assert_eq!(entry.state, RollingQueueEntryState::Refused);
            let outcome = entry.outcome.unwrap();
            assert_eq!(outcome.refusal.as_deref(), Some(QUEUE_BATCH_POLICY_REFUSED));
            assert!(
                outcome
                    .detail
                    .unwrap()
                    .contains("policy_fence=migration_number")
            );
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn the_batch_size_caps_the_claim_and_later_entries_run_next() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        let first = run_next_batch(&store, &launcher, 2).await.unwrap();
        assert_eq!(first.iter().map(|r| r.0).collect::<Vec<_>>(), ids[..2]);
        assert_eq!(
            entry_of(&store, ids[2]).await.state,
            RollingQueueEntryState::Queued
        );
        let second = run_next_batch(&store, &launcher, 2).await.unwrap();
        assert_eq!(second.iter().map(|r| r.0).collect::<Vec<_>>(), ids[2..]);
        assert!(
            run_next_batch(&store, &launcher, 2)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_second_source_claiming_a_taken_migration_number_is_refused_in_order() {
        let (dir, _origin, work) = repos();
        // Both sources add migration 901 on top of tip 900, neither declares
        // a provisional migration: only the first can take 901.
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, Some(901)).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        let first = entry_of(&store, ids[0]).await;
        assert_eq!(first.state, RollingQueueEntryState::Published);
        assert_eq!(first.migration_version, Some(901));
        let second = entry_of(&store, ids[1]).await;
        assert_eq!(second.state, RollingQueueEntryState::Refused);
        assert_eq!(
            second.outcome.unwrap().refusal.as_deref(),
            Some(QUEUE_MIGRATION_OUT_OF_ORDER)
        );
        let runs = log_lines(&log)
            .into_iter()
            .filter(|l| l.starts_with("run "))
            .collect::<Vec<_>>();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].contains(&shas[0]) && !runs[0].contains(&shas[1]));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn restart_reconcile_settles_every_member_of_a_landed_batch_once() {
        let (_dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        sh(
            &work,
            &format!(
                "git checkout -q -B land origin/rolling && git merge -q --no-edit {} {} \
                 && git push -q origin HEAD:rolling",
                shas[0], shas[1]
            ),
        );
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        // The daemon claimed the batch, the lander published, then the daemon died.
        store
            .lock()
            .await
            .claim_rolling_queue_batch(Utc::now(), 4)
            .unwrap()
            .unwrap();
        assert_eq!(reconcile_gating(&store).await.unwrap(), (2, 0));
        assert_eq!(reconcile_gating(&store).await.unwrap(), (0, 0));
        for id in ids {
            assert_eq!(
                entry_of(&store, id).await.state,
                RollingQueueEntryState::Published
            );
        }
        let wakes = store
            .lock()
            .await
            .list_scheduled_jobs()
            .unwrap()
            .into_iter()
            .filter(|job| job.name.starts_with("merge-queue-"))
            .count();
        assert_eq!(wakes, 2);
    }

    const EXTERNAL_PUSH: &str = "ext() { ( cd @WORKDIR@ && git fetch -q origin rolling \
        && git checkout -q -B ext origin/rolling && printf x > \"$1\" && git add \"$1\" \
        && git commit -qm \"$1\" && git push -q origin HEAD:rolling ) >/dev/null 2>&1; }\n\
        n=$(grep -c '^run ' @LOG@)\ngit fetch -q origin rolling\nred=0\n";

    fn external_push_guard(work: &Path, log: &Path, rest: &str) -> String {
        format!(
            "{}{rest}",
            EXTERNAL_PUSH
                .replace("@WORKDIR@", &work.display().to_string())
                .replace("@LOG@", &log.display().to_string())
        )
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_lander_receipt_failing_test_lines_beat_the_raw_libtest_lines() {
        let stdout = "publication_status=not_published\nfailing_test=q::a\nfailing_test=q::b\n";
        let stderr = "rsi-rolling-land: new test failures relative to rolling abc: q::a [new], q::b [new]; base reds: ";
        let result = classify_run(Some(1), stdout, stderr, || None);
        assert_eq!(result.state, RollingQueueEntryState::Refused);
        assert_eq!(result.outcome.failing_tests, vec!["q::a", "q::b"]);
        // Raw libtest lines still work when no receipt line exists.
        let raw = classify_run(Some(1), "", "test q::c ... FAILED", || None);
        assert_eq!(raw.outcome.failing_tests, vec!["q::c"]);
        // A receipt line is the authority even when raw lines are present.
        let both = classify_run(
            Some(1),
            "failing_test=q::d\n",
            "test q::e ... FAILED",
            || None,
        );
        assert_eq!(both.outcome.failing_tests, vec!["q::d"]);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_missing_source_with_migration_evidence_never_settles_published() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // The lander reports b landed with migration evidence but never merged it.
        let guard = format!(
            "export SKIP={b}\necho migration_0_source_id={b}\necho migration_0_final_version=901\n",
            b = shas[1]
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_landed(&store, &work, ids[0], &shas[0]).await;
        let lost = entry_of(&store, ids[1]).await;
        assert_eq!(lost.state, RollingQueueEntryState::Failed);
        assert_eq!(
            lost.outcome.unwrap().refusal.as_deref(),
            Some(QUEUE_BATCH_ANCESTRY_UNVERIFIED)
        );
        assert_eq!(wake_count(&store).await, 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_external_fix_before_a_green_prefix_regates_the_now_green_remainder() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // b is red until an external commit adds `fix`; the fix lands while the
        // [a] gate runs, so the [a,b] red is stale by the time b is next.
        let guard = external_push_guard(
            &work,
            &log,
            &format!(
                "if ! git cat-file -e origin/rolling:fix 2>/dev/null; then \
                 case \" $* \" in *\" {b} \"*) echo 'test q::b ... FAILED' >&2; red=1;; esac; fi\n\
                 if [ $n = 2 ]; then ext fix; fi\n\
                 if [ $red = 1 ]; then echo publication_status=not_published; exit 1; fi",
                b = shas[1]
            ),
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_landed(&store, &work, ids[0], &shas[0]).await;
        assert_landed(&store, &work, ids[1], &shas[1]).await;
        assert_eq!(lander_runs(&log).len(), 3, "batch, [a], then b re-gated");
        assert_eq!(wake_count(&store).await, 2);
    }

    /// #2023: the red names only b's file, so b is held back and the complement
    /// [a] is gated. An external fix lands while it gates, so [a] is green on a
    /// different base than the red: that must not confirm the stale attribution.
    /// b goes back to pending, is gated on the current tip, and publishes.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_external_fix_during_the_complement_gate_regates_the_attributed_member() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let guard = external_push_guard(
            &work,
            &log,
            &format!(
                "if ! git cat-file -e origin/rolling:fix 2>/dev/null; then \
                 case \" $* \" in *\" {b} \"*) echo 'error[E0425]: boom' >&2; \
                 echo ' --> b:1:1' >&2; red=1;; esac; fi\n\
                 if [ $n = 2 ]; then ext fix; fi\n\
                 if [ $red = 1 ]; then echo publication_status=not_published; exit 1; fi",
                b = shas[1]
            ),
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 2, "every member settles exactly once");
        assert_landed(&store, &work, ids[0], &shas[0]).await;
        assert_landed(&store, &work, ids[1], &shas[1]).await;
        assert_eq!(lander_runs(&log).len(), 3, "batch, [a], then b re-gated");
        assert_eq!(wake_count(&store).await, 2);
        let guard = store.lock().await;
        assert_eq!(events_of(&guard, "bisect_attributed").len(), 1);
        assert_eq!(events_of(&guard, "bisect_attribution_stale").len(), 1);
        assert!(events_of(&guard, "bisect_attribution_confirmed").is_empty());
        assert!(events_of(&guard, "bisect_isolated").is_empty());
    }

    /// #2024: the held member itself reaches rolling out of band while the
    /// complement gates. It settles published, once, and is never refused.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_held_member_that_lands_out_of_band_during_the_complement_gate_settles_published() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let guard = external_push_guard(
            &work,
            &log,
            &format!(
                "case \" $* \" in *\" {b} \"*) echo 'error[E0425]: boom' >&2; \
                 echo ' --> b:1:1' >&2; red=1;; esac\n\
                 if [ $n = 2 ]; then ( cd {work} && git fetch -q origin rolling \
                 && git checkout -q -B ext origin/rolling && git merge -q --no-edit {b} \
                 && git push -q origin HEAD:rolling ) >/dev/null 2>&1; fi\n\
                 if [ $red = 1 ]; then echo publication_status=not_published; exit 1; fi",
                b = shas[1],
                work = work.display()
            ),
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 2, "every member settles exactly once");
        assert_landed(&store, &work, ids[0], &shas[0]).await;
        let held = entry_of(&store, ids[1]).await;
        assert_eq!(held.state, RollingQueueEntryState::Published);
        assert!(is_ancestor_of_origin(&work, &shas[1]));
        assert_eq!(wake_count(&store).await, 2);
        let guard = store.lock().await;
        assert_eq!(events_of(&guard, "already_landed").len(), 1);
        assert!(events_of(&guard, "bisect_attribution_confirmed").is_empty());
    }

    /// #1940: an external push after each of the first two runs no longer
    /// exhausts a count of one re-gate. Each stale red is gated again within
    /// the batch's wall time, and the third run, on the final tip, publishes.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn two_external_advances_between_runs_are_regated_within_the_wall_time_budget() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // The first two runs are red and someone else pushes right after each.
        let guard = external_push_guard(
            &work,
            &log,
            "if [ $n -le 2 ]; then echo 'test q::x ... FAILED' >&2; red=1; fi\n\
             if [ $red = 1 ]; then echo fetched_target_id=$(git rev-parse origin/rolling); \
             ext e$n; echo publication_status=not_published; exit 1; fi",
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 4);
        assert_eq!(lander_runs(&log).len(), 3, "two stale reds, then green");
        for (id, sha) in ids.iter().zip(&shas) {
            assert_landed(&store, &work, *id, sha).await;
        }
        let budgets = budgets(&log);
        assert!(
            budgets.iter().all(|secs| (3000..=3600).contains(secs)),
            "{budgets:?}"
        );
        assert_eq!(wake_count(&store).await, 4);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_interaction_only_red_ends_typed_and_never_publishes_a_red_tip() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // a and b each pass alone; together they fail.
        let guard = format!(
            "red=0\ncase \" $* \" in *\" {a} \"*) case \" $* \" in *\" {b} \"*) \
             echo 'test q::interact ... FAILED' >&2; red=1;; esac;; esac\n\
             if [ $red = 1 ]; then echo publication_status=not_published; exit 1; fi",
            a = shas[0],
            b = shas[1]
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_landed(&store, &work, ids[0], &shas[0]).await;
        assert_isolated_red(&store, ids[1], "q::interact").await;
        assert!(!is_ancestor_of_origin(&work, &shas[1]));
        let pushes = log_lines(&log).iter().filter(|l| *l == "push").count();
        assert_eq!(pushes, 1, "only the green prefix was ever pushed");
        assert_eq!(wake_count(&store).await, 2);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn restart_after_a_bisected_member_published_restores_its_planned_migration() {
        let (dir, _origin, work) = repos();
        let a = commit_file(&work, "a", "a", "a");
        // b declares a provisional migration: the lander renumbers it to tip + 1
        // once a is out of its group.
        sh(
            &work,
            "git checkout -q -B b origin/rolling && mkdir -p tools/provisional-migrations \
             && printf '{}\\n' > tools/provisional-migrations/b.json && printf b > b \
             && git add -A && git commit -qm b",
        );
        let b = head(&work);
        let _ = dir;
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &[a.clone(), b.clone()], None).await;
        store
            .lock()
            .await
            .conn
            .execute(
                "UPDATE rolling_queue_entries SET migration_version=905 WHERE id=?1",
                rusqlite::params![ids[1].to_string()],
            )
            .unwrap();
        let claimed = store
            .lock()
            .await
            .claim_rolling_queue_batch(Utc::now(), 4)
            .unwrap()
            .unwrap();
        store
            .lock()
            .await
            .begin_rolling_queue_bisect(claimed.batch_id, "gate_failed", Utc::now())
            .unwrap();
        // The bisect is about to run [b] alone; the plan is durable first.
        let group: Vec<RollingQueueEntryV1> = vec![entry_of(&store, ids[1]).await];
        record_migration_plan(&store, &work, claimed.batch_id, &group)
            .await
            .unwrap();
        // The lander pushed b, then the daemon died before recording its report.
        sh(
            &work,
            &format!(
                "git checkout -q -B land origin/rolling && git merge -q --no-edit {b} \
                 && git push -q origin HEAD:rolling"
            ),
        );
        assert_eq!(entry_of(&store, ids[1]).await.migration_version, Some(905));
        assert_eq!(reconcile_gating(&store).await.unwrap(), (1, 1));
        let restored = entry_of(&store, ids[1]).await;
        assert_eq!(restored.state, RollingQueueEntryState::Published);
        assert_eq!(restored.migration_version, Some(901));
        assert_eq!(
            entry_of(&store, ids[0]).await.state,
            RollingQueueEntryState::Queued
        );
    }

    /// A lander that merges its sources, pushes once, and (`late`) reports an
    /// uncertain publication after an external commit landed on top of its
    /// push. It mirrors the real lander's `--expected-tip` handling.
    fn racing_lander(
        dir: &Path,
        work: &Path,
        log: &Path,
        pre_fetch: &str,
        red_sources: &str,
        late: &str,
    ) -> LanderLauncher {
        let body = r#"
echo "run $*" >> @LOG@
echo "budget $RSI_LANDER_REGATE_BUDGET_SECS" >> @LOG@
n=$(grep -c '^run ' @LOG@)
ext() { ( cd @WORK@ && git fetch -q origin rolling \
    && git checkout -q -B ext origin/rolling && printf x > "$1" && git add "$1" \
    && git commit -qm "$1" && git push -q origin HEAD:rolling ) >/dev/null 2>&1; }
expected=""; prev=""
for arg in "$@"; do
  if [ "$prev" = --expected-tip ]; then expected=$arg; echo "expected $arg" >> @LOG@; fi
  prev=$arg
done
@PRE_FETCH@
cd @WORK@ || exit 9
git fetch -q origin rolling
fetched=$(git rev-parse origin/rolling)
stale=""
if [ -n "$expected" ] && [ "$expected" != "$fetched" ]; then
  stale="stale_retry_1=$expected..$fetched:regated"
fi
red=0
if ! git cat-file -e origin/rolling:fix 2>/dev/null; then
  for red_source in @RED@; do case " $* " in *" $red_source "*) red=1;; esac; done
fi
if [ $red = 1 ]; then
  echo 'test q::red ... FAILED' >&2
  echo publication_status=not_published
  echo fetched_target_id=$fetched
  [ -n "$stale" ] && echo "$stale"
  exit 1
fi
git checkout -q -B land origin/rolling
while [ $# -gt 0 ]; do
  if [ "$1" = --accepted ]; then
    git merge -q --no-edit "$2" >/dev/null 2>&1 || { git merge --abort; echo 'Conflict { paths: ["f"] }' >&2; echo publication_status=not_published; exit 1; }
    shift
  fi
  shift
done
git push -q origin HEAD:rolling && echo push >> @LOG@
cand=$(git rev-parse HEAD)
@LATE@
echo publication_status=published
echo fetched_target_id=$fetched
echo candidate_id=$cand
echo published_target_id=$cand
[ -n "$stale" ] && echo "$stale"
exit 0
"#
        .replace("@LOG@", &log.display().to_string())
        .replace("@WORK@", &work.display().to_string())
        .replace("@PRE_FETCH@", pre_fetch)
        .replace("@RED@", red_sources)
        .replace("@LATE@", late);
        fake_lander(dir, &body)
    }

    /// The wall-time re-gate budget (seconds) each lander run was given.
    fn budgets(log: &Path) -> Vec<u64> {
        log_lines(log)
            .into_iter()
            .filter_map(|line| line.strip_prefix("budget ")?.parse().ok())
            .collect()
    }

    /// #1007 S4 races, finding 1: the observed remote head after an uncertain
    /// publication is not the queue's own push when an external descendant of
    /// the published candidate sits on top of it. That external commit fixed
    /// the remaining red, so the red evidence is stale: the remainder is
    /// re-gated (charging the regate) and published, never refused.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_external_descendant_of_an_uncertain_publication_is_external_not_own() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // Run 2 (`[a]`) pushes its candidate, then an external commit adding
        // `fix` lands on top before the lander reports; the report is uncertain.
        let launcher = racing_lander(
            dir.path(),
            &work,
            &log,
            "",
            &shas[1],
            "if [ $n = 2 ]; then ext fix; echo publication_status=unknown; \
             echo fetched_target_id=$fetched; echo candidate_id=$cand; \
             echo published_target_id=$cand; exit 1; fi",
        );
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_landed(&store, &work, ids[0], &shas[0]).await;
        assert_landed(&store, &work, ids[1], &shas[1]).await;
        assert_eq!(lander_runs(&log).len(), 3, "batch, [a], then b re-gated");
        assert!(
            budgets(&log).iter().all(|secs| *secs > 0),
            "the external descendant is re-gated within the wall time"
        );
        assert_eq!(wake_count(&store).await, 2);
    }

    /// Finding 3: a push after the queue's probe but before the lander's
    /// initial fetch is reported through `--expected-tip`. #1940: it is gated
    /// on the fetched tip within the batch's wall time, never refused for a
    /// spent count, so the green remainder publishes.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_push_between_the_probe_and_the_lander_fetch_is_regated_not_refused() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // Run 2 is hit by a push right after the queue probed; run 4
        // (`[b, c]`) is hit by a second one.
        let launcher = racing_lander(
            dir.path(),
            &work,
            &log,
            "if [ $n = 2 ]; then ext e1; fi\n\
             if [ $n = 4 ]; then ext e2; fi",
            &shas[0],
            "",
        );
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        run_next_batch(&store, &launcher, 4).await.unwrap();
        let runs = lander_runs(&log);
        assert_eq!(runs.len(), 4, "{runs:?}");
        assert!(!runs[0].contains("--expected-tip"), "{runs:?}");
        for run in &runs[1..] {
            assert!(run.contains("--expected-tip"), "{runs:?}");
        }
        assert!(budgets(&log).iter().all(|secs| *secs > 0));
        assert_isolated_red(&store, ids[0], "q::red").await;
        assert_landed(&store, &work, ids[1], &shas[1]).await;
        assert_landed(&store, &work, ids[2], &shas[2]).await;
    }

    /// A provisional source published while an external migration landed
    /// between the queue's plan fetch and the lander's fetch: the plan says
    /// 901, the published provisional commit carries 902.
    async fn crashed_provisional_run(
        work: &Path,
    ) -> (Arc<rsid_core::store_lock::Mutex<Store>>, Uuid, Uuid, String) {
        let a = commit_file(work, "a", "a", "a");
        sh(
            work,
            "git checkout -q -B b origin/rolling && mkdir -p tools/provisional-migrations \
             && printf '{}\\n' > tools/provisional-migrations/b.json && printf b > b \
             && git add -A && git commit -qm b",
        );
        let b = head(work);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, work, &[a, b.clone()], None).await;
        store
            .lock()
            .await
            .conn
            .execute(
                "UPDATE rolling_queue_entries SET migration_version=905 WHERE id=?1",
                rusqlite::params![ids[1].to_string()],
            )
            .unwrap();
        let claimed = store
            .lock()
            .await
            .claim_rolling_queue_batch(Utc::now(), 4)
            .unwrap()
            .unwrap();
        store
            .lock()
            .await
            .begin_rolling_queue_bisect(claimed.batch_id, "gate_failed", Utc::now())
            .unwrap();
        let group: Vec<RollingQueueEntryV1> = vec![entry_of(&store, ids[1]).await];
        record_migration_plan(&store, work, claimed.batch_id, &group)
            .await
            .unwrap();
        // An external migration lands, then the lander publishes b's
        // provisional commit (parents: the tip and b) renumbered to 902.
        sh(
            work,
            &format!(
                "git checkout -q -B ext origin/rolling && mkdir -p crates/rsid-store/src/store/migrations \
                 && printf 'impl Store {{}}\\n' > crates/rsid-store/src/store/migrations/v901.rs \
                 && git add -A && git commit -qm ext901 && git push -q origin HEAD:rolling \
                 && git checkout -q -B land origin/rolling && git merge -q --no-commit --no-ff {b} \
                 && printf 'impl Store {{}}\\n' > crates/rsid-store/src/store/migrations/v902.rs \
                 && git add -A && git commit -qm provisional && git push -q origin HEAD:rolling"
            ),
        );
        (store, ids[1], claimed.batch_id, b)
    }

    /// Finding 2: the restart recovers the number from the published
    /// provisional commit (902), not the stale plan (901).
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn restart_recovers_the_actual_migration_number_from_the_published_commit() {
        let (_dir, _origin, work) = repos();
        let (store, id, _batch, b) = crashed_provisional_run(&work).await;
        assert_eq!(
            planned_number(&store, id).await,
            Some(901),
            "the plan was made before the external migration"
        );
        assert_eq!(published_migration_number(&work, &b), Some(902));
        assert_eq!(reconcile_gating(&store).await.unwrap(), (1, 1));
        let restored = entry_of(&store, id).await;
        assert_eq!(restored.state, RollingQueueEntryState::Published);
        assert_eq!(restored.migration_version, Some(902));
    }

    /// Finding 2: an actual receipt number saved before the crash is never
    /// overwritten on restart, by the plan or by the commit scan.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn restart_never_overwrites_a_saved_receipt_migration_number() {
        let (_dir, _origin, work) = repos();
        let (store, id, batch, _b) = crashed_provisional_run(&work).await;
        // The queue saved the lander's receipt (903), then died before settling.
        bisect_event(
            &store,
            batch,
            "migration_receipt",
            Some(id),
            serde_json::json!({ "version": 903 }),
        )
        .await
        .unwrap();
        store
            .lock()
            .await
            .record_rolling_queue_assigned_migration(id, 903)
            .unwrap();
        assert_eq!(reconcile_gating(&store).await.unwrap(), (1, 1));
        assert_eq!(entry_of(&store, id).await.migration_version, Some(903));
    }

    async fn planned_number(
        store: &Arc<rsid_core::store_lock::Mutex<Store>>,
        id: Uuid,
    ) -> Option<u32> {
        store
            .lock()
            .await
            .planned_rolling_queue_migration(id)
            .unwrap()
    }

    // #1208: a source already on rolling is published, never gated.

    async fn assert_published_out_of_band(
        store: &Arc<rsid_core::store_lock::Mutex<Store>>,
        work: &Path,
        id: Uuid,
        sha: &str,
    ) {
        let entry = entry_of(store, id).await;
        assert_eq!(entry.state, RollingQueueEntryState::Published, "{entry:?}");
        let outcome = entry.outcome.unwrap();
        assert_eq!(outcome.refusal, None);
        let tip = outcome.landed_sha.unwrap();
        assert!(
            git(work, &["merge-base", "--is-ancestor", sha, &tip]).is_some(),
            "the landed tip {tip} contains {sha}"
        );
        assert!(
            outcome
                .detail
                .unwrap_or_default()
                .contains("landed outside the queue")
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_content_equivalent_source_settles_published_at_the_containing_tip() {
        let (dir, _origin, work) = repos();
        let source = commit_file(&work, "source", "same", "accepted");
        let tip = commit_file(&work, "equivalent", "same", "accepted");
        assert_ne!(source, tip);
        sh(
            &work,
            &format!("git push -q origin {tip}:refs/heads/rolling"),
        );
        assert_eq!(
            verify_ancestry(&work, std::slice::from_ref(&source)),
            vec![Ancestry::NotAncestor]
        );
        let launcher = fake_lander(
            dir.path(),
            &format!(
                "echo publication_status=published\necho landing_outcome=already_on_rolling\necho fetched_target_id={tip}\necho candidate_id={tip}\necho published_target_id={tip}\n"
            ),
        );
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let id = queued(&store, &work, &source, "already-contained").await;
        let (settled, result) = run_next(&store, &launcher).await.unwrap().unwrap();
        assert_eq!(settled, id);
        assert_eq!(result.state, RollingQueueEntryState::Published);
        let entry = entry_of(&store, id).await;
        assert_eq!(entry.state, RollingQueueEntryState::Published);
        let outcome = entry.outcome.unwrap();
        assert_eq!(outcome.landed_sha, Some(tip));
        assert_eq!(outcome.refusal, None);
        assert!(outcome.detail.unwrap().starts_with("already_on_rolling"));
        assert_eq!(wake_count(&store).await, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_already_on_rolling_receipt_still_requires_its_tip_on_rolling() {
        let (dir, _origin, work) = repos();
        let source = commit_file(&work, "source", "same", "accepted");
        let launcher = fake_lander(
            dir.path(),
            &format!(
                "echo publication_status=published\necho landing_outcome=already_on_rolling\necho published_target_id={source}\n"
            ),
        );
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let id = queued(&store, &work, &source, "unconfirmed-tip").await;
        run_next(&store, &launcher).await.unwrap().unwrap();
        let entry = entry_of(&store, id).await;
        assert_eq!(entry.state, RollingQueueEntryState::Failed);
        assert_eq!(
            entry.outcome.unwrap().refusal.as_deref(),
            Some(QUEUE_BATCH_ANCESTRY_UNVERIFIED)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_member_already_on_rolling_is_published_without_a_gate() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        // The operator lands b directly, after it was enqueued.
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        sh(
            &work,
            &format!("git push -q origin {}:refs/heads/rolling", shas[1]),
        );
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 3, "every member settles exactly once");
        assert_published_out_of_band(&store, &work, ids[1], &shas[1]).await;
        let runs = lander_runs(&log);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert!(
            !runs[0].contains(&format!("--accepted {}", shas[1])),
            "the landed source is not gated: {runs:?}"
        );
        for (id, sha) in [(ids[0], &shas[0]), (ids[2], &shas[2])] {
            assert_landed(&store, &work, id, sha).await;
        }
        let guard = store.lock().await;
        assert_eq!(events_of(&guard, "already_landed").len(), 1);
    }

    /// Land `source` on the shared `rolling` as `email` would, with `subject`
    /// (a `--no-ff` merge, or a `commit-tree` commit when `renumber`).
    fn publish_as(work: &Path, source: &str, email: &str, subject: &str, renumber: bool) -> String {
        let script = if renumber {
            format!(
                "tree=$(git rev-parse {source}^{{tree}}) && c=$(git commit-tree $tree \
                 -p origin/rolling -p {source} -m '{subject}') \
                 && git push -q origin $c:refs/heads/rolling && git update-ref refs/heads/landed $c"
            )
        } else {
            format!(
                "git checkout -q -B landed origin/rolling && git merge -q --no-ff {source} \
                 -m '{subject}' && git push -q origin HEAD:rolling"
            )
        };
        sh(
            work,
            &format!("export GIT_COMMITTER_EMAIL={email} GIT_AUTHOR_EMAIL={email}; {script}"),
        );
        git(work, &["rev-parse", "landed"])
            .unwrap()
            .trim()
            .to_string()
    }

    /// A daemon restart that found the entry `gating`: claim a batch, then
    /// requeue its entry the way startup reconciliation does.
    async fn restart_while_gating(store: &Arc<rsid_core::store_lock::Mutex<Store>>, id: Uuid) {
        let guard = store.lock().await;
        guard
            .claim_rolling_queue_batch(Utc::now(), 4)
            .unwrap()
            .unwrap();
        assert!(
            guard
                .requeue_gating_rolling_queue_entry(id, Utc::now())
                .unwrap()
        );
    }

    async fn published_detail(
        store: &Arc<rsid_core::store_lock::Mutex<Store>>,
        id: Uuid,
    ) -> (String, String) {
        let entry = entry_of(store, id).await;
        assert_eq!(entry.state, RollingQueueEntryState::Published, "{entry:?}");
        let outcome = entry.outcome.unwrap();
        (
            outcome.landed_sha.unwrap(),
            outcome.detail.unwrap_or_default(),
        )
    }

    const LANDER_EMAIL: &str = "rsi-rolling-land@rsi.invalid";

    /// #4009/#4040: the lander published the entry but the daemon restarted
    /// before the settle, so the re-driven entry finds its source on rolling.
    /// The lander's own commit (its committer identity names the source) is
    /// reported as the queue's publication, naming the restart the entry's
    /// events show, never as an outside landing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_lander_published_entry_found_after_a_restart_is_not_called_an_outside_landing() {
        let (dir, _origin, work) = repos();
        let source = commit_file(&work, "a", "a", "a");
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let id = queued(&store, &work, &source, "lander-published").await;
        restart_while_gating(&store, id).await;
        let integration = publish_as(
            &work,
            &source,
            LANDER_EMAIL,
            &format!("rsi integration: merge {source} into rolling"),
            false,
        );
        assert_eq!(
            lander_publication_commit(&work, &source).as_deref(),
            Some(integration.as_str())
        );
        assert_eq!(lander_publication_commit(&work, "0000"), None);
        run_next(&store, &launcher).await.unwrap().unwrap();
        assert!(lander_runs(&log).is_empty(), "nothing is gated");
        let (landed, detail) = published_detail(&store, id).await;
        assert_eq!(landed, integration);
        assert!(
            detail.contains("published to origin/rolling by the queue's lander")
                && detail.contains(&integration)
                && detail.contains("requeued_after_restart"),
            "{detail}"
        );
        assert!(!detail.contains("landed outside the queue"), "{detail}");
    }

    /// #4040 item 1: a commit with the lander's subject but another writer's
    /// identity (the manager `Integrate` executor, a hand push) is not the
    /// queue's landing, even after a restart found nothing else to say.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn an_integration_subject_from_another_writer_is_still_an_outside_landing() {
        let (dir, _origin, work) = repos();
        let source = commit_file(&work, "a", "a", "a");
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let id = queued(&store, &work, &source, "manager-integrated").await;
        let integration = publish_as(
            &work,
            &source,
            "integration@rsi.local",
            &format!("rsi integration: merge {source} into rolling"),
            false,
        );
        assert_eq!(lander_publication_commit(&work, &source), None);
        run_next(&store, &launcher).await.unwrap().unwrap();
        let (landed, detail) = published_detail(&store, id).await;
        assert!(
            detail.contains("(landed outside the queue); settled without a gate"),
            "{detail}"
        );
        assert!(
            git(
                &work,
                &["merge-base", "--is-ancestor", &integration, &landed]
            )
            .is_some(),
            "the tip {landed} contains the manager's commit {integration}"
        );
    }

    /// #4040 item 2: a renumbered provisional-migration publication (the
    /// lander's commit, parents target and source, another subject) is found
    /// by the same identity evidence, with no restart claimed when none ran.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_renumbered_lander_publication_is_attributed_to_the_lander_without_a_restart() {
        let (dir, _origin, work) = repos();
        let source = commit_file(&work, "a", "a", "a");
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let id = queued(&store, &work, &source, "renumbered").await;
        let renumbered = publish_as(
            &work,
            &source,
            LANDER_EMAIL,
            "rsi landing: renumber provisional migration",
            true,
        );
        run_next(&store, &launcher).await.unwrap().unwrap();
        let (landed, detail) = published_detail(&store, id).await;
        assert_eq!(landed, renumbered);
        assert!(
            detail.contains("by the queue's lander")
                && detail.contains("gate receipt")
                && !detail.contains("restart"),
            "{detail}"
        );
        assert!(!detail.contains("landed outside the queue"), "{detail}");
    }

    /// #4040 item 2: a fast-forward publication writes no commit; an entry the
    /// queue was gating when the daemon restarted, found on rolling afterwards,
    /// is attributed to the queue's earlier run, not to an outside landing.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_fast_forwarded_entry_found_after_a_restart_is_not_called_an_outside_landing() {
        let (dir, _origin, work) = repos();
        let source = commit_file(&work, "a", "a", "a");
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let id = queued(&store, &work, &source, "fast-forwarded").await;
        restart_while_gating(&store, id).await;
        sh(
            &work,
            &format!("git push -q origin {source}:refs/heads/rolling"),
        );
        run_next(&store, &launcher).await.unwrap().unwrap();
        assert!(lander_runs(&log).is_empty(), "nothing is gated");
        let (landed, detail) = published_detail(&store, id).await;
        assert_eq!(landed, source);
        assert!(
            detail.contains("re-driven after a daemon restart")
                && detail.contains("requeued_after_restart")
                && detail.contains("gate_timing"),
            "{detail}"
        );
        assert!(!detail.contains("landed outside the queue"), "{detail}");
    }

    /// #4040 item 3: the restart is named only when the entry's events show
    /// one; a lander commit found on a path that is not a restart says only
    /// that the receipt is not on the entry.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_missing_receipt_text_names_a_restart_only_when_one_happened() {
        let none = already_landed_outcome("s", "tip".into(), Some("lander".into()), 0).1;
        assert!(
            none.contains("gate receipt") && !none.contains("restart"),
            "{none}"
        );
        let some = already_landed_outcome("s", "tip".into(), Some("lander".into()), 2).1;
        assert!(some.contains("requeued_after_restart x2"), "{some}");
        let outside = already_landed_outcome("s", "tip".into(), None, 0);
        assert_eq!(outside.0, "tip");
        assert!(
            outside.1.contains("landed outside the queue"),
            "{}",
            outside.1
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_batch_whose_sources_all_landed_runs_no_lander_and_settles_published() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let launcher = merging_lander(dir.path(), &work, &log, "");
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        sh(
            &work,
            &format!(
                "git checkout -q -B landed origin/rolling && git merge -q --no-edit {} {} \
                 && git push -q origin HEAD:rolling",
                shas[0], shas[1]
            ),
        );
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(lander_runs(&log).is_empty(), "nothing is gated");
        for (id, sha) in ids.iter().zip(&shas) {
            assert_published_out_of_band(&store, &work, *id, sha).await;
        }
        let state: String = store
            .lock()
            .await
            .conn
            .query_row("SELECT state FROM rolling_queue_batches", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(state, "published");
        assert_eq!(wake_count(&store).await, 2);
    }

    /// Guard for the merging lander: the first run that carries `sha` lands it
    /// on rolling first (as an operator would) and then refuses it like the
    /// real lander does an integrated source.
    fn lands_then_refuses(work: &Path, marker: &Path, sha: &str) -> String {
        format!(
            "case \" $* \" in *\" {sha} \"*) if [ ! -f {marker} ]; then touch {marker}; \
             git -C {work} push -q origin {sha}:refs/heads/rolling; \
             echo 'rsi-rolling-land: accepted source {sha} is already integrated; supply \
             --accepted BASE:{sha} with its historical accepted base' >&2; \
             echo publication_status=not_published; exit 1; fi;; esac",
            marker = marker.display(),
            work = work.display(),
        )
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_source_the_lander_calls_already_integrated_settles_published_not_gate_failed() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let guard = lands_then_refuses(&work, &dir.path().join("landed"), &shas[1]);
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 3);
        assert_published_out_of_band(&store, &work, ids[1], &shas[1]).await;
        let runs = lander_runs(&log);
        assert_eq!(runs.len(), 2, "{runs:?}");
        assert!(
            !runs[1].contains(&format!("--accepted {}", shas[1])),
            "the retry drops the landed source"
        );
        for (id, sha) in [(ids[0], &shas[0]), (ids[2], &shas[2])] {
            assert_landed(&store, &work, id, sha).await;
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_lone_source_the_lander_calls_already_integrated_settles_published() {
        let (dir, _origin, work) = repos();
        let sha = commit_file(&work, "a", "a", "a");
        let log = dir.path().join("lander.log");
        let guard = lands_then_refuses(&work, &dir.path().join("landed"), &sha);
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let id = queued(&store, &work, &sha, "k").await;
        let (settled, result) = run_next(&store, &launcher).await.unwrap().unwrap();
        assert_eq!(settled, id);
        assert_eq!(result.state, RollingQueueEntryState::Published);
        assert_published_out_of_band(&store, &work, id, &sha).await;
        assert_eq!(lander_runs(&log).len(), 1);
        assert_eq!(wake_count(&store).await, 1);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn an_already_integrated_report_is_published_from_ancestry_or_typed_never_a_red_gate() {
        let stderr = "rsi-rolling-land: accepted source abc is already integrated; supply --accepted BASE:abc";
        let landed = classify_run(
            Some(1),
            "publication_status=not_published\n",
            stderr,
            || Some("f".repeat(40)),
        );
        assert_eq!(landed.state, RollingQueueEntryState::Published);
        assert_eq!(landed.outcome.landed_sha, Some("f".repeat(40)));
        let unconfirmed = classify_run(Some(1), "", stderr, || None);
        assert_eq!(unconfirmed.state, RollingQueueEntryState::Refused);
        assert_eq!(
            unconfirmed.outcome.refusal.as_deref(),
            Some(QUEUE_SOURCE_ALREADY_INTEGRATED)
        );
        assert!(!is_bisectable(&unconfirmed));
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_member_landed_out_of_band_during_a_bisect_is_published_and_never_regated() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b", "c"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        // The first (whole-batch) run lands c out of band, then goes red on b.
        let marker = dir.path().join("landed");
        let guard = format!(
            "if [ ! -f {marker} ]; then touch {marker}; git -C {work} push -q origin {c}:refs/heads/rolling; fi\n{red}",
            marker = marker.display(),
            work = work.display(),
            c = shas[2],
            red = red_when_any(&[(shas[1].as_str(), "q::b_breaks")]),
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard);
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert_eq!(results.len(), 3, "every member settles exactly once");
        assert_published_out_of_band(&store, &work, ids[2], &shas[2]).await;
        assert_landed(&store, &work, ids[0], &shas[0]).await;
        assert_isolated_red(&store, ids[1], "q::b_breaks").await;
        let runs = lander_runs(&log);
        assert!(
            runs[1..]
                .iter()
                .all(|run| !run.contains(&format!("--accepted {}", shas[2]))),
            "the landed member is never gated again: {runs:?}"
        );
        let guard = store.lock().await;
        assert!(events_of(&guard, "regate_spent").is_empty());
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[tokio::test]
    async fn a_batch_past_its_gate_wall_time_is_refused_typed_and_the_lander_stopped() {
        let (dir, _origin, work) = repos();
        let shas: Vec<String> = ["a", "b"]
            .iter()
            .map(|name| commit_file(&work, name, name, name))
            .collect();
        let log = dir.path().join("lander.log");
        let pid_file = dir.path().join("sleeper.pid");
        let term_file = dir.path().join("lander-term");
        let guard = format!(
            "trap 'touch {term}; while :; do sleep 1; done' TERM; sleep 60 & echo $! > {pid}; wait",
            pid = pid_file.display(),
            term = term_file.display(),
        );
        let launcher = merging_lander(dir.path(), &work, &log, &guard)
            .with_gate_timeout(Duration::from_secs(2));
        let store = Arc::new(rsid_core::store_lock::Mutex::new(
            Store::open_in_memory().unwrap(),
        ));
        let ids = queue_all(&store, &work, &shas, None).await;
        let started = std::time::Instant::now();
        let results = run_next_batch(&store, &launcher, 4).await.unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "bounded by the budget"
        );
        assert_eq!(results.len(), 2);
        for id in &ids {
            let entry = entry_of(&store, *id).await;
            assert_eq!(entry.state, RollingQueueEntryState::Refused);
            let outcome = entry.outcome.unwrap();
            assert_eq!(outcome.refusal.as_deref(), Some(QUEUE_GATE_TIMEOUT));
            assert!(
                outcome
                    .detail
                    .unwrap()
                    .contains("rolling_queue_gate_timeout_mins")
            );
        }
        for sha in &shas {
            assert!(!is_ancestor_of_origin(&work, sha));
        }
        // The lander's process group, its children included, was stopped.
        assert!(
            term_file.exists(),
            "the lander received SIGTERM before escalation"
        );
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let pid = nix::unistd::Pid::from_raw(pid.trim().parse().unwrap());
        let mut alive = true;
        for _ in 0..50 {
            alive = nix::sys::signal::kill(pid, None).is_ok();
            if !alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(!alive, "the lander's child outlived timeout escalation");
        let guard = store.lock().await;
        assert_eq!(events_of(&guard, "gate_timeout").len(), 1);
        drop(guard);
        assert_eq!(wake_count(&store).await, 2);
    }

    /// #1940: a stuck gate holds the queue for an hour at most by default, and
    /// the lander learns the batch's remaining wall time as its re-gate budget.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
    #[test]
    fn the_default_gate_timeout_is_an_hour() {
        assert_eq!(ROLLING_QUEUE_DEFAULT_GATE_TIMEOUT_MINS, 60);
        let launcher = LanderLauncher::new(PathBuf::from("rsi-rolling-land"));
        assert_eq!(launcher.gate_timeout(), Duration::from_secs(60 * 60));
        assert!(
            ROLLING_QUEUE_DEFAULT_GATE_TIMEOUT_MINS as u64
                >= rsi_common::rolling_queue::ROLLING_QUEUE_MIN_GATE_TIMEOUT_MINS
        );
    }
}
