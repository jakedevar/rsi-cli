//! The run journal (#3683 R5, design #3628 "Native Harness trial and recovery" and "Outcomes,
//! records, provenance and budgets").
//!
//! One `journal.json` per run holds a record per trial attempt `(run, task, slot, retry)`: the
//! step reached, the draft outcome so far, the container and project identities, the **absolute**
//! (epoch millisecond) agent and verifier deadlines, the daemon task id and its last status (so
//! interrupted cost and result survive), and cleanup status. Every change is one atomic
//! temp-write, fsync, rename; an `flock`-style exclusive lock on `journal.lock` makes a run owned
//! by one runner at a time and is released by the kernel if that runner dies.
//!
//! This module is pure bookkeeping and policy: the file store ([`JournalStore`]), the retry
//! policy ([`retry_decision`]) and the recovery plans ([`plan_recovery`], [`reconcile_harness`]).
//! The lifecycle that drives trials through it is [`super::lifecycle`].
//!
//! Recovery rules, from the design:
//!
//! * A durable terminal outcome is reused, never replayed. A valid slot is never run twice.
//! * A crash before the agent started costs nothing: the half-built environment is removed and
//!   the same attempt starts again (bounded by [`MAX_PRE_AGENT_RESTARTS`]).
//! * A crash while the oracle ran, or while the verifier ran, makes the attempt `infra_interrupted`
//!   and invalid: the container may have been modified, so a verifier is never re-run in it and a
//!   reward is never read after the fact. Evidence is collected and the environment removed.
//! * Only a confirmed `infra_interrupted` earns **one** fresh replacement; an agent or verifier
//!   timeout, a bad reward, a route or spend refusal, an unknown protocol error and an operator
//!   cancellation never do.
//! * Resume requires the plan, runner build and dataset to be the ones that opened the journal.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write as _,
    path::{Path, PathBuf},
    sync::Mutex,
};

use rsi_common::{
    bench::{BenchTaskState, BenchTaskStatusV1},
    bench_runner::{
        PlannedTrial, TrialException, TrialExceptionKind, TrialIdentity, TrialOutcomeV1, trial_key,
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::trial::{EnvFacts, Resume, Step, fresh_outcome};

/// Schema version of `journal.json`.
pub const JOURNAL_SCHEMA_VERSION: u32 = 1;
pub const JOURNAL_FILE: &str = "journal.json";
pub const LOCK_FILE: &str = "journal.lock";
/// A slot gets one fresh replacement after a confirmed infrastructure interruption (design).
pub const MAX_INFRA_REPLACEMENTS: u32 = 1;
/// How often an attempt that never reached its agent may be started again after a crash.
pub const MAX_PRE_AGENT_RESTARTS: u32 = 2;

// ---- identity ------------------------------------------------------------------------------

/// What must be identical for a journal to be resumed (design: "Require plan/binary/dataset
/// hashes to match for resume").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunIdentity {
    pub plan_sha256: String,
    /// The runner build's evaluator digest (`stamp`), so a changed evaluator cannot resume.
    pub runner_digest: String,
    pub dataset_sha256: String,
}

/// Lowercase hex sha256, prefixed `sha256:`.
#[must_use]
pub fn sha256_of(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut text = String::with_capacity(71);
    text.push_str("sha256:");
    for byte in digest {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

// ---- records -------------------------------------------------------------------------------

/// Absolute deadlines in epoch milliseconds, written when the phase's intent step is journaled.
/// A restarted runner judges the phase by these, never by a fresh clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Deadlines {
    pub agent_started_ms: Option<u64>,
    pub agent_deadline_ms: Option<u64>,
    pub verifier_started_ms: Option<u64>,
    pub verifier_deadline_ms: Option<u64>,
}

/// The daemon's side of a Harness trial: its trial id (set before `BenchTaskStart` is sent, so an
/// unknown Start outcome can still be cancelled) and the last status seen, which carries the
/// cost that must survive an interruption.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonTrial {
    pub trial: String,
    #[serde(default)]
    pub last_status: Option<BenchTaskStatusV1>,
    #[serde(default)]
    pub cancel_requested: bool,
}

/// What the artifact manifest found, kept in the journal beside its path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestSummary {
    pub path: String,
    pub files: usize,
    pub skipped: usize,
    pub total_bytes: u64,
    pub truncated: bool,
}

/// One trial attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrialRecord {
    pub planned: PlannedTrial,
    pub step: Step,
    /// The outcome so far; the final one once `step` is [`Step::Terminal`].
    pub draft: TrialOutcomeV1,
    #[serde(default)]
    pub env: Option<EnvFacts>,
    #[serde(default)]
    pub deadlines: Deadlines,
    #[serde(default)]
    pub daemon: Option<DaemonTrial>,
    /// Times this attempt was started again after a crash before its agent ran.
    #[serde(default)]
    pub restarts: u32,
    /// The `retry` this attempt replaces, when it is a replacement.
    #[serde(default)]
    pub replaces: Option<u32>,
    /// The `retry` that replaced this attempt, once decided and journaled.
    #[serde(default)]
    pub replaced_by: Option<u32>,
    #[serde(default)]
    pub outcome_path: Option<String>,
    #[serde(default)]
    pub manifest: Option<ManifestSummary>,
    pub updated_ms: u64,
}

impl TrialRecord {
    #[must_use]
    pub fn new(
        planned: PlannedTrial,
        mode: rsi_common::bench_runner::RunnerMode,
        now_ms: u64,
    ) -> Self {
        let draft = fresh_outcome(&planned, mode);
        Self {
            planned,
            step: Step::Prepared,
            draft,
            env: None,
            deadlines: Deadlines::default(),
            daemon: None,
            restarts: 0,
            replaces: None,
            replaced_by: None,
            outcome_path: None,
            manifest: None,
            updated_ms: now_ms,
        }
    }

    #[must_use]
    pub const fn identity(&self) -> &TrialIdentity {
        &self.planned.identity
    }

    #[must_use]
    pub fn is_terminal(&self) -> bool {
        self.step == Step::Terminal
    }

    /// The daemon trial id this attempt uses: the join key of the runner outcome and the daemon
    /// row (R3a), one definition shared with the Harness client.
    #[must_use]
    pub fn trial_id(&self) -> String {
        trial_key(&self.planned.identity)
    }
}

/// The whole journal file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunJournalV1 {
    pub schema_version: u32,
    pub run_id: String,
    pub identity: RunIdentity,
    pub created_ms: u64,
    /// The operator cancelled the run (journaled before any session is cancelled).
    #[serde(default)]
    pub cancelled: bool,
    pub records: Vec<TrialRecord>,
}

impl RunJournalV1 {
    #[must_use]
    pub fn record(&self, identity: &TrialIdentity) -> Option<&TrialRecord> {
        self.records.iter().find(|r| r.identity() == identity)
    }

    pub fn record_mut(&mut self, identity: &TrialIdentity) -> Option<&mut TrialRecord> {
        self.records.iter_mut().find(|r| r.identity() == identity)
    }

    /// The attempts of one statistical slot, in retry order.
    #[must_use]
    pub fn slot(&self, task: &str, slot: u32) -> Vec<&TrialRecord> {
        let mut found: Vec<&TrialRecord> = self
            .records
            .iter()
            .filter(|r| r.identity().task == task && r.identity().slot == slot)
            .collect();
        found.sort_by_key(|r| r.identity().retry);
        found
    }
}

// ---- errors and fault injection -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JournalError {
    /// Another runner holds the run.
    Owned(String),
    /// The journal on disk was written for another plan, runner build or dataset.
    Mismatch(String),
    /// The journal file is unreadable or malformed.
    Corrupt(String),
    Io(String),
    /// A test-injected crash: the runner is dead and persists nothing more.
    InjectedCrash(usize),
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Owned(why) => write!(f, "run journal is owned by another runner: {why}"),
            Self::Mismatch(why) => write!(f, "run journal does not match this run: {why}"),
            Self::Corrupt(why) => write!(f, "run journal is unreadable: {why}"),
            Self::Io(why) => write!(f, "run journal i/o: {why}"),
            Self::InjectedCrash(n) => write!(f, "injected crash at journal write {n}"),
        }
    }
}

impl std::error::Error for JournalError {}

/// Crash injection for tests: the Nth journal write (zero-based) either never happens
/// (`before`) or happens and is the last one (`after`). From then on the store is dead, like a
/// process that was killed: no later write reaches the disk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Faults {
    pub crash_before: Option<usize>,
    pub crash_after: Option<usize>,
}

struct Inner {
    journal: RunJournalV1,
    writes: usize,
    dead: bool,
}

/// The file-backed journal of one run, shared by the trial threads of one runner process.
pub struct JournalStore {
    path: PathBuf,
    faults: Faults,
    // Held for the lifetime of the store; the kernel drops it if the process dies.
    _lock: File,
    inner: Mutex<Inner>,
}

fn io_error(path: &Path, error: &std::io::Error) -> JournalError {
    JournalError::Io(format!("{}: {error}", path.display()))
}

impl JournalStore {
    /// Open (or create) the journal of `run_id` under `dir` and take its lock.
    ///
    /// # Errors
    /// [`JournalError::Owned`] when another runner holds the lock, [`JournalError::Mismatch`]
    /// when the journal on disk is another plan's, build's or dataset's, [`JournalError::Corrupt`]
    /// or an I/O failure.
    pub fn open(
        dir: &Path,
        run_id: &str,
        identity: &RunIdentity,
        now_ms: u64,
    ) -> Result<Self, JournalError> {
        Self::open_with(dir, run_id, identity, now_ms, Faults::default())
    }

    /// [`Self::open`] with crash injection.
    ///
    /// # Errors
    /// As [`Self::open`].
    pub fn open_with(
        dir: &Path,
        run_id: &str,
        identity: &RunIdentity,
        now_ms: u64,
        faults: Faults,
    ) -> Result<Self, JournalError> {
        fs::create_dir_all(dir).map_err(|e| io_error(dir, &e))?;
        let lock_path = dir.join(LOCK_FILE);
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| io_error(&lock_path, &e))?;
        lock.try_lock().map_err(|e| match e {
            fs::TryLockError::WouldBlock => {
                JournalError::Owned(format!("{} is locked", lock_path.display()))
            }
            fs::TryLockError::Error(error) => io_error(&lock_path, &error),
        })?;
        let path = dir.join(JOURNAL_FILE);
        let journal = match fs::read(&path) {
            Ok(bytes) => {
                let journal: RunJournalV1 = serde_json::from_slice(&bytes)
                    .map_err(|e| JournalError::Corrupt(format!("{}: {e}", path.display())))?;
                if journal.schema_version != JOURNAL_SCHEMA_VERSION {
                    return Err(JournalError::Mismatch(format!(
                        "schema_version {}",
                        journal.schema_version
                    )));
                }
                if journal.run_id != run_id {
                    return Err(JournalError::Mismatch(format!(
                        "journal is for run {}",
                        journal.run_id
                    )));
                }
                if &journal.identity != identity {
                    return Err(JournalError::Mismatch(
                        "the plan, runner build or dataset differ from the ones that opened the journal"
                            .into(),
                    ));
                }
                journal
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => RunJournalV1 {
                schema_version: JOURNAL_SCHEMA_VERSION,
                run_id: run_id.to_string(),
                identity: identity.clone(),
                created_ms: now_ms,
                cancelled: false,
                records: Vec::new(),
            },
            Err(e) => return Err(io_error(&path, &e)),
        };
        let store = Self {
            path,
            faults,
            _lock: lock,
            inner: Mutex::new(Inner {
                journal,
                writes: 0,
                dead: false,
            }),
        };
        // A fresh journal is durable from the start, so "owned run" exists on disk.
        if !store.path.exists() {
            store.update(|_| ())?;
        }
        Ok(store)
    }

    /// The current journal.
    #[must_use]
    pub fn snapshot(&self) -> RunJournalV1 {
        self.guard().journal.clone()
    }

    #[must_use]
    pub fn record(&self, identity: &TrialIdentity) -> Option<TrialRecord> {
        self.guard().journal.record(identity).cloned()
    }

    /// Whether the operator cancelled the run.
    #[must_use]
    pub fn cancelled(&self) -> bool {
        self.guard().journal.cancelled
    }

    /// How many journal writes this store performed (the crash-injection index space).
    #[must_use]
    pub fn writes(&self) -> usize {
        self.guard().writes
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Apply `change` and make the result durable, atomically: the new journal replaces the old
    /// one by rename, so a reader sees all of the change or none of it. If the write fails the
    /// in-memory journal is restored.
    ///
    /// # Errors
    /// The write failed, or the store is dead after an injected crash.
    pub fn update<R>(
        &self,
        change: impl FnOnce(&mut RunJournalV1) -> R,
    ) -> Result<R, JournalError> {
        let mut inner = self.guard();
        let index = inner.writes;
        if inner.dead {
            return Err(JournalError::InjectedCrash(index));
        }
        let before = inner.journal.clone();
        let result = change(&mut inner.journal);
        if self.faults.crash_before == Some(index) {
            inner.dead = true;
            inner.journal = before;
            return Err(JournalError::InjectedCrash(index));
        }
        if let Err(error) = self.persist(&inner.journal) {
            inner.journal = before;
            return Err(error);
        }
        inner.writes += 1;
        if self.faults.crash_after == Some(index) {
            inner.dead = true;
            return Err(JournalError::InjectedCrash(index));
        }
        Ok(result)
    }

    fn persist(&self, journal: &RunJournalV1) -> Result<(), JournalError> {
        let bytes = serde_json::to_vec_pretty(journal)
            .map_err(|e| JournalError::Io(format!("serialize: {e}")))?;
        write_atomic(&self.path, &bytes).map_err(|e| JournalError::Io(e.to_string()))
    }
}

/// Write `bytes` to `path` so a reader sees the old file or the whole new one: temp file in the
/// same directory, fsync, rename, fsync of the directory.
///
/// # Errors
/// The first I/O failure.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = parent.join(format!(".{name}.tmp"));
    {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    // Persist the rename itself. Directories cannot be opened for sync everywhere (Windows).
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

// ---- retry policy ----------------------------------------------------------------------------

/// Why a slot is not replaced, or that it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    /// One fresh container (and session) in the same statistical slot.
    Replace,
    NoRetry(&'static str),
}

/// The retry policy (design parity row on retries and "Outcomes" table): one replacement, only
/// for a confirmed infrastructure interruption that left no valid reward, and never when the
/// attempt also carries a task timeout, a reward failure, a route or spend refusal, a protocol
/// error or an operator cancellation.
#[must_use]
pub fn retry_decision(
    outcome: &TrialOutcomeV1,
    replacements_so_far: u32,
    run_cancelled: bool,
) -> RetryDecision {
    if run_cancelled {
        return RetryDecision::NoRetry("the run was cancelled");
    }
    if outcome.reward_valid {
        return RetryDecision::NoRetry("the attempt has a valid reward");
    }
    if outcome.exceptions.is_empty() {
        return RetryDecision::NoRetry("the attempt names no infrastructure cause");
    }
    for exception in &outcome.exceptions {
        use TrialExceptionKind as K;
        let why = match exception.kind {
            K::InfraInterrupted => continue,
            K::AgentTimeout | K::VerifierTimeout => "task timeouts are not retried",
            K::RewardMissing | K::RewardEmpty | K::RewardParse | K::RewardNonFinite => {
                "reward failures are not retried"
            }
            K::RouteRejected => "route rejections are not retried",
            K::SpendCapRejected => "spend refusals are not retried",
            K::Cancelled => "cancellation is not retried",
            K::InvalidPlan => "unknown or invalid plan errors are not retried",
            K::EnvironmentStartFailed
            | K::EnvironmentStartTimeout
            | K::SolutionUploadFailed
            | K::AgentExecFailed
            | K::AgentSettleFailed
            | K::VerifierUploadFailed
            | K::VerifierExecFailed => "only a confirmed infrastructure interruption is retried",
        };
        return RetryDecision::NoRetry(why);
    }
    if replacements_so_far >= MAX_INFRA_REPLACEMENTS {
        return RetryDecision::NoRetry("the slot already used its one replacement");
    }
    RetryDecision::Replace
}

// ---- recovery ----------------------------------------------------------------------------

/// What a restarted runner does with one journaled attempt.
#[derive(Debug, Clone)]
pub enum Recovery {
    /// A durable terminal outcome: reuse it, replay nothing.
    Terminal,
    /// Continue (or start) the attempt.
    Run(RecoveryPlan),
}

#[derive(Debug, Clone)]
pub struct RecoveryPlan {
    /// The first step still to run, and the outcome so far.
    pub resume: Resume,
    /// Remove the half-built environment of the dead runner before starting again.
    pub stale_cleanup: bool,
    /// The attempt's `restarts` after this recovery.
    pub restarts: u32,
}

/// The `*Done` step that precedes a first-to-run step: persisting it makes the plan idempotent
/// (planning it again yields the same plan).
#[must_use]
pub const fn settled_before(from: Step) -> Step {
    match from {
        Step::Prepared | Step::EnvironmentStarting => Step::Prepared,
        Step::EnvironmentDone | Step::AgentRunning => Step::EnvironmentDone,
        Step::AgentDone | Step::Verifying => Step::AgentDone,
        Step::VerifyDone | Step::Collecting => Step::VerifyDone,
        Step::CollectDone | Step::CleaningUp => Step::CollectDone,
        Step::CleanupDone | Step::Terminal => Step::CleanupDone,
    }
}

fn interrupted(draft: &mut TrialOutcomeV1, why: &str) {
    draft.exceptions.push(TrialException {
        kind: TrialExceptionKind::InfraInterrupted,
        message: why.to_string(),
    });
}

/// Decide how to continue an attempt from its last durable step (oracle mode). Never reads a
/// clock: the decision follows from the step alone, so it is the same however late the restart.
#[must_use]
pub fn plan_recovery(record: &TrialRecord) -> Recovery {
    let mut draft = record.draft.clone();
    let mut restarts = record.restarts;
    let mut stale_cleanup = false;
    let from = match record.step {
        Step::Terminal => return Recovery::Terminal,
        // Nothing was created: start the attempt.
        Step::Prepared => Step::EnvironmentStarting,
        // Something may exist already (project, directories, container) under a name we know.
        // Remove it, then start again; a crash loop stops being retried.
        Step::EnvironmentStarting => {
            restarts += 1;
            if restarts > MAX_PRE_AGENT_RESTARTS {
                interrupted(
                    &mut draft,
                    "the runner died while starting the environment too many times",
                );
                Step::CleaningUp
            } else {
                stale_cleanup = true;
                Step::EnvironmentStarting
            }
        }
        Step::EnvironmentDone => Step::AgentRunning,
        // The solution may have run and changed the container; its exit is unknown. Invalid,
        // never verified. Evidence is still collected, then the environment removed.
        Step::AgentRunning => {
            interrupted(
                &mut draft,
                "the runner restarted while the oracle was running; the container may have been \
                 modified, so it is not verified",
            );
            Step::Collecting
        }
        Step::AgentDone => Step::Verifying,
        // A verifier that was interrupted is never re-run in an already-modified container, and
        // a reward written after the fact is never read.
        Step::Verifying => {
            interrupted(
                &mut draft,
                "the runner restarted during verification; the verifier is not re-run and no \
                 reward is read",
            );
            Step::Collecting
        }
        Step::VerifyDone => Step::Collecting,
        Step::Collecting => Step::Collecting,
        Step::CollectDone => Step::CleaningUp,
        Step::CleaningUp => Step::CleaningUp,
        Step::CleanupDone => Step::Terminal,
    };
    Recovery::Run(RecoveryPlan {
        resume: Resume { from, draft },
        stale_cleanup,
        restarts,
    })
}

/// The facts a stale cleanup assumes: the environment start died somewhere, so every
/// deterministic resource of the project may exist.
#[must_use]
pub fn stale_env_facts(project: &str) -> EnvFacts {
    EnvFacts {
        project: project.to_string(),
        container: None,
        dirs_created: true,
        compose_attempted: true,
        collected: false,
    }
}

// ---- Harness reconciliation ----------------------------------------------------------------

/// What the daemon says about a journaled Harness trial.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionView {
    /// The daemon never saw the trial (Start was not delivered).
    NoSession,
    Live(BenchTaskStatusV1),
    Settled(BenchTaskStatusV1),
}

/// How a restarted runner continues a Harness attempt that was in `agent_running`.
#[derive(Debug, Clone, PartialEq)]
pub enum HarnessRecovery {
    /// No session exists and nothing was spent: start the same attempt again.
    RestartAttempt,
    /// The session is live and its original deadline has not passed: keep polling, on the
    /// original clock (`deadline_ms` is the journaled one).
    Reconnect { deadline_ms: u64 },
    /// The session is live and past its deadline: cancel, settle, then grade.
    CancelAndSettle,
    /// The daemon settled it: take Finish and the trace, grade normally.
    Settled(BenchTaskStatusV1),
    /// The daemon restarted under it. Not a resumable session: the row and its cost are kept,
    /// the attempt is invalid and earns the one replacement.
    InfraInvalid(BenchTaskStatusV1),
}

/// Reconcile a Harness attempt with the daemon's view. `now_ms` is only compared to the
/// journaled absolute deadline; a missing deadline is treated as already passed.
#[must_use]
pub fn reconcile_harness(record: &TrialRecord, view: SessionView, now_ms: u64) -> HarnessRecovery {
    match view {
        SessionView::NoSession => HarnessRecovery::RestartAttempt,
        SessionView::Settled(status) => {
            if status.state == BenchTaskState::InfraInterrupted {
                HarnessRecovery::InfraInvalid(status)
            } else {
                HarnessRecovery::Settled(status)
            }
        }
        SessionView::Live(status) => {
            if status.state == BenchTaskState::InfraInterrupted {
                return HarnessRecovery::InfraInvalid(status);
            }
            match record.deadlines.agent_deadline_ms {
                Some(deadline_ms) if now_ms < deadline_ms => {
                    HarnessRecovery::Reconnect { deadline_ms }
                }
                _ => HarnessRecovery::CancelAndSettle,
            }
        }
    }
}

// ---- replacement ---------------------------------------------------------------------------

/// The trial that replaces `failed`: same slot, `retry + 1`, same pinned task, a fresh container.
#[must_use]
pub fn replacement_of(failed: &PlannedTrial) -> PlannedTrial {
    let mut next = failed.clone();
    next.identity.retry += 1;
    next
}

/// How many replacements the slot of `identity` has already used.
#[must_use]
pub fn replacements_used(journal: &RunJournalV1, identity: &TrialIdentity) -> u32 {
    let used = journal
        .slot(&identity.task, identity.slot)
        .iter()
        .filter(|r| r.replaces.is_some())
        .count();
    u32::try_from(used).unwrap_or(u32::MAX)
}

/// Terminal records keyed by their directory key, for summaries.
#[must_use]
pub fn terminal_outcomes(journal: &RunJournalV1) -> BTreeMap<String, TrialOutcomeV1> {
    journal
        .records
        .iter()
        .filter(|r| r.is_terminal())
        .map(|r| (r.trial_id(), r.draft.clone()))
        .collect()
}
