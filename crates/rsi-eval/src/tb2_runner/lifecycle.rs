//! The journaled trial lifecycle (#3683 R5, design #3628 "Native Harness trial and recovery"):
//! drives R1's trial state machine with every step made durable, resumes a dead runner's run,
//! applies the one-replacement retry policy, and cancels live sessions before any teardown.
//!
//! The journal ([`super::journal`]) decides *what* to do after a crash; this module does it.
//! Everything outside the process is injected: the environment ([`EnvFactory`]), the daemon
//! sessions ([`SessionPort`]) and the clock ([`Pacer`], epoch-based so absolute deadlines survive
//! a restart). Nothing here talks to Docker, SSH or a socket itself.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use rsi_common::bench_runner::{
    CANCEL_SETTLE_TIMEOUT_SEC, PlannedTrial, RunnerMode, TimeoutPolicy, TrialException,
    TrialExceptionKind, TrialIdentity, TrialOutcomeV1, TrialState,
};

use super::{
    artifacts::{Limits, build_manifest},
    client::{
        AgentClock, BenchClient, CallError, HarnessTrialResult, Pacer, SystemPacer, resume_trial,
    },
    journal::{
        DaemonTrial, HarnessRecovery, JournalError, JournalStore, ManifestSummary, Recovery,
        RetryDecision, SessionView, TrialRecord, plan_recovery, reconcile_harness, replacement_of,
        replacements_used, retry_decision, settled_before, write_atomic,
    },
    transport::OpError,
    trial::{
        EnvError, EnvFacts, Halt, ModelPort, Step, TrialEnvironment, TrialInputs, TrialObserver,
        run_trial_observed, settle_for_collection,
    },
};

/// File names inside a trial's work directory.
pub const OUTCOME_FILE: &str = "outcome.json";
pub const MANIFEST_FILE: &str = "artifacts.manifest.json";

/// An epoch-based clock: `now()` is the time since the Unix epoch, so a journaled deadline means
/// the same instant in the process that wrote it and in the one that resumes it.
#[derive(Debug, Default)]
pub struct EpochPacer;

impl super::trial::Clock for EpochPacer {
    fn now(&self) -> Duration {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
    }
}

impl Pacer for EpochPacer {
    fn sleep(&self, duration: Duration) {
        SystemPacer::new().sleep(duration);
    }
}

/// How an environment is opened for an attempt.
pub enum Adopt<'a> {
    /// A new attempt; nothing exists yet.
    Fresh,
    /// A dead runner died while starting: every deterministic resource of the project may exist.
    Stale,
    /// Take over the journaled environment.
    Facts(&'a EnvFacts),
}

/// Builds the environment of an attempt. R2's remote Docker environment implements it; tests
/// inject a fake.
pub trait EnvFactory: Sync {
    /// # Errors
    /// The environment cannot be built (an unusable remote path, say).
    fn open<'e>(
        &'e self,
        trial: &'e PlannedTrial,
        adopt: Adopt<'_>,
    ) -> Result<Box<dyn TrialEnvironment + 'e>, OpError>;
}

/// The daemon side of the sessions a cancellation must stop.
pub trait SessionPort: Sync {
    fn client(&self) -> Option<&BenchClient> {
        None
    }
    /// What the daemon says about `trial` (`NoSession` when it never saw it).
    ///
    /// # Errors
    /// The daemon cannot be asked.
    fn status(&self, trial: &str) -> Result<SessionView, String>;
    /// Cancel `trial` within `window`. A trial the daemon does not know is not an error.
    ///
    /// # Errors
    /// The cancel could not be delivered.
    fn cancel(&self, trial: &str, window: Duration) -> Result<(), String>;
}

/// Session reconciliation and cancellation use the same run-local capability as trial calls.
pub struct BenchSessions<'a> {
    pub client: &'a BenchClient,
    pub wall: &'a (dyn Pacer + Sync),
}

impl SessionPort for BenchSessions<'_> {
    fn client(&self) -> Option<&BenchClient> {
        Some(self.client)
    }

    fn status(&self, trial: &str) -> Result<SessionView, String> {
        match self.client.status(self.wall, trial, None) {
            Ok(status) if status.state.is_live() => Ok(SessionView::Live(status)),
            Ok(status) => Ok(SessionView::Settled(status)),
            Err(CallError::Refused(r)) if r.code == rsi_common::bench::BENCH_TRIAL_UNKNOWN => {
                Ok(SessionView::NoSession)
            }
            Err(error) => Err(format!("{error:?}")),
        }
    }

    fn cancel(&self, trial: &str, window: Duration) -> Result<(), String> {
        match self.client.cancel(self.wall, trial, window) {
            Ok(_) => Ok(()),
            Err(CallError::Refused(r)) if r.code == rsi_common::bench::BENCH_TRIAL_UNKNOWN => {
                Ok(())
            }
            Err(error) => Err(format!("{error:?}")),
        }
    }
}

/// The operator's cancellation, shared by every trial thread.
#[derive(Debug, Default)]
pub struct CancelFlag(AtomicBool);

impl CancelFlag {
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicBool::new(false))
    }

    pub fn set(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Hub-side paths of one attempt.
#[derive(Debug, Clone)]
pub struct TrialPaths {
    pub solution_dir: PathBuf,
    pub tests_dir: PathBuf,
    /// Per-attempt directory: outcome, manifest, compose file, collected `logs/`.
    pub work_dir: PathBuf,
}

impl TrialPaths {
    #[must_use]
    pub fn collect_dir(&self) -> PathBuf {
        self.work_dir.join("logs")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleError {
    /// The runner stopped (a crash, a journal that could not be written, an operator cancel at a
    /// step boundary). The journal says where; a restart continues.
    Halted(String),
    Journal(JournalError),
    Env(OpError),
}

impl std::fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Halted(why) => write!(f, "runner halted: {why}"),
            Self::Journal(e) => write!(f, "{e}"),
            Self::Env(e) => write!(f, "{}: {}", e.kind.as_str(), e.detail),
        }
    }
}

impl std::error::Error for LifecycleError {}

impl From<JournalError> for LifecycleError {
    fn from(e: JournalError) -> Self {
        Self::Journal(e)
    }
}

impl From<OpError> for LifecycleError {
    fn from(e: OpError) -> Self {
        Self::Env(e)
    }
}

/// A journaled Harness client; the oracle never calls its model boundary.
struct HarnessModel<'a> {
    client: Option<&'a BenchClient>,
    wall: &'a dyn Pacer,
    journal: &'a JournalStore,
    identity: TrialIdentity,
}

impl ModelPort for HarnessModel<'_> {
    fn start_session(&mut self, _trial: &PlannedTrial) -> Result<String, String> {
        Err("the oracle runner never starts a model session".to_string())
    }

    fn harness_available(&self) -> bool {
        self.client.is_some()
    }

    fn drive_harness(
        &mut self,
        start: &rsi_common::bench::BenchTaskStartParams,
    ) -> Result<HarnessTrialResult, Halt> {
        let client = self
            .client
            .ok_or_else(|| Halt("no run-local Harness client".into()))?;
        let record = self
            .journal
            .record(&self.identity)
            .ok_or_else(|| Halt("missing attempt".into()))?;
        let clock = AgentClock {
            started: record
                .deadlines
                .agent_started_ms
                .or_else(|| {
                    record.deadlines.agent_deadline_ms.map(|deadline| {
                        deadline.saturating_sub(start.agent_timeout_sec.saturating_mul(1000))
                    })
                })
                .map_or_else(
                    || {
                        if record
                            .daemon
                            .as_ref()
                            .is_some_and(|d| d.last_status.is_some())
                        {
                            // A known session with no durable deadline is already expired.
                            Duration::ZERO
                        } else {
                            self.wall.now()
                        }
                    },
                    Duration::from_millis,
                ),
            timeout: Duration::from_secs(start.agent_timeout_sec),
        };
        // Persist the trial capability and original clock before Start can create a session.
        let trial = start.trial.clone();
        self.journal
            .update(|journal| {
                if let Some(record) = journal.record_mut(&self.identity) {
                    record.deadlines.agent_started_ms = Some(millis(clock.started));
                    record.deadlines.agent_deadline_ms = Some(millis(clock.deadline()));
                    if record.daemon.is_none() {
                        record.daemon = Some(DaemonTrial {
                            trial,
                            last_status: None,
                            cancel_requested: false,
                        });
                    }
                }
            })
            .map_err(|e| Halt(e.to_string()))?;
        let status = record.daemon.and_then(|d| d.last_status);
        let result = resume_trial(client, self.wall, start, Some(clock), status);
        self.journal
            .update(|journal| {
                if let Some(record) = journal.record_mut(&self.identity) {
                    record.daemon = Some(DaemonTrial {
                        trial: start.trial.clone(),
                        last_status: result.status.clone(),
                        cancel_requested: result.cancel_requested,
                    });
                }
            })
            .map_err(|e| Halt(e.to_string()))?;
        Ok(result)
    }
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// What one slot produced: every attempt in retry order. The last one is the slot's result;
/// earlier ones are infrastructure rows kept as evidence.
#[derive(Debug, Clone)]
pub struct SlotResult {
    pub attempts: Vec<TrialOutcomeV1>,
}

/// What a cancellation did, in order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CancelReport {
    /// Daemon trials a cancel was issued for.
    pub cancelled_sessions: Vec<String>,
    /// Cancels the daemon could not be given (the trial is still torn down).
    pub session_errors: Vec<String>,
    /// Attempts whose environment was torn down and which were finalised as cancelled.
    pub torn_down: Vec<TrialIdentity>,
}

pub struct Lifecycle<'a> {
    pub journal: &'a JournalStore,
    pub envs: &'a dyn EnvFactory,
    /// Epoch-based; see [`EpochPacer`].
    pub wall: &'a (dyn Pacer + Sync),
    pub sessions: Option<&'a dyn SessionPort>,
    pub cancel: &'a CancelFlag,
    pub mode: RunnerMode,
    pub timeouts: &'a TimeoutPolicy,
    pub paths: &'a (dyn Fn(&PlannedTrial) -> TrialPaths + Sync),
    /// The daemon's run stop marker (R6): while it exists no replacement attempt is started.
    pub stop_file: Option<&'a Path>,
}

struct Observer<'a> {
    journal: &'a JournalStore,
    wall: &'a dyn Pacer,
    cancel: &'a CancelFlag,
    identity: TrialIdentity,
    agent_timeout_ms: u64,
    verifier_timeout_ms: u64,
    paths: TrialPaths,
}

impl TrialObserver for Observer<'_> {
    fn step(
        &mut self,
        step: Step,
        draft: &TrialOutcomeV1,
        env: &dyn TrialEnvironment,
    ) -> Result<(), Halt> {
        // The operator's cancel stops the trial before its next action; the lifecycle then
        // cancels sessions and tears down (see `cancel_run`). Settled steps still persist.
        if self.cancel.is_set() && step.is_intent() {
            return Err(Halt("cancelled by the operator".into()));
        }
        let now = millis(self.wall.now());
        let facts = env.facts();
        let manifest = if step == Step::CollectDone && draft.collection.collected {
            Some(write_manifest(&self.paths)?)
        } else {
            None
        };
        let outcome_path = self.paths.work_dir.join(OUTCOME_FILE);
        let identity = self.identity.clone();
        let (agent_ms, verifier_ms) = (self.agent_timeout_ms, self.verifier_timeout_ms);
        self.journal
            .update(|journal| {
                if let Some(record) = journal.record_mut(&identity)
                    // A cancellation that finalised the attempt concurrently wins: a late step
                    // of the dying trial thread never reopens it.
                    && !record.is_terminal()
                {
                    record.step = step;
                    record.draft = draft.clone();
                    if facts.is_some() {
                        record.env = facts;
                    }
                    match step {
                        Step::AgentRunning => {
                            if draft.mode == RunnerMode::Oracle
                                && record.deadlines.agent_started_ms.is_none()
                            {
                                record.deadlines.agent_started_ms = Some(now);
                                record.deadlines.agent_deadline_ms =
                                    Some(now.saturating_add(agent_ms));
                            }
                        }
                        Step::Verifying => {
                            record.deadlines.verifier_started_ms = Some(now);
                            record.deadlines.verifier_deadline_ms =
                                Some(now.saturating_add(verifier_ms));
                        }
                        _ => {}
                    }
                    if manifest.is_some() {
                        record.manifest.clone_from(&manifest);
                    }
                    if step == Step::Terminal {
                        record.outcome_path = Some(outcome_path.display().to_string());
                    }
                    record.updated_ms = now;
                }
            })
            .map_err(|e| Halt(e.to_string()))?;
        if step == Step::Terminal {
            // The journal's draft, not ours: a concurrent cancellation may have finalised it.
            let durable = self
                .journal
                .record(&self.identity)
                .map_or_else(|| draft.clone(), |r| r.draft);
            write_outcome(&self.paths.work_dir, &durable).map_err(|e| Halt(e.to_string()))?;
        }
        Ok(())
    }
}

/// Write the artifact manifest of the collected evidence beside the outcome and summarise it.
/// A manifest that cannot be built is a halt only when the journal itself is at stake; an
/// unwalkable evidence directory is recorded as a truncated, empty manifest.
fn write_manifest(paths: &TrialPaths) -> Result<ManifestSummary, Halt> {
    let manifest = build_manifest(&paths.collect_dir(), Limits::default()).unwrap_or_else(|why| {
        let mut empty = super::artifacts::ArtifactManifest::default();
        empty.truncated = true;
        empty.skipped.push(super::artifacts::SkippedEntry {
            path: why,
            reason: super::artifacts::SkipReason::Unreadable,
        });
        empty
    });
    let path = paths.work_dir.join(MANIFEST_FILE);
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| Halt(e.to_string()))?;
    write_atomic(&path, &bytes).map_err(|e| Halt(format!("{}: {e}", path.display())))?;
    Ok(ManifestSummary {
        path: path.display().to_string(),
        files: manifest.files.len(),
        skipped: manifest.skipped.len(),
        total_bytes: manifest.total_bytes,
        truncated: manifest.truncated,
    })
}

fn write_outcome(work_dir: &Path, outcome: &TrialOutcomeV1) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(outcome).map_err(std::io::Error::other)?;
    write_atomic(&work_dir.join(OUTCOME_FILE), &bytes)
}

impl Lifecycle<'_> {
    fn now_ms(&self) -> u64 {
        millis(self.wall.now())
    }

    /// Make sure the attempt has a record; an existing one (a resumed run) is left alone, so a
    /// slot is never duplicated.
    fn ensure_record(&self, trial: &PlannedTrial) -> Result<(), JournalError> {
        let now = self.now_ms();
        let mode = self.mode;
        self.journal.update(|journal| {
            if journal.record(&trial.identity).is_none() {
                journal
                    .records
                    .push(TrialRecord::new(trial.clone(), mode, now));
            }
        })
    }

    /// The daemon's stop marker exists: start nothing new, let what runs finish.
    #[must_use]
    pub fn stop_requested(&self) -> bool {
        self.stop_file.is_some_and(Path::exists)
    }

    /// Run one slot to its end: resume the latest attempt journaled for it (or start `first`),
    /// and replace it at most once after a confirmed infrastructure interruption.
    ///
    /// # Errors
    /// [`LifecycleError::Halted`] when the runner stopped (the journal says where), or a
    /// journal/environment failure.
    pub fn run_slot(&self, first: &PlannedTrial) -> Result<SlotResult, LifecycleError> {
        let snapshot = self.journal.snapshot();
        let mut trial = snapshot
            .slot(&first.identity.task, first.identity.slot)
            .last()
            .map_or_else(|| first.clone(), |r| r.planned.clone());
        loop {
            self.ensure_record(&trial)?;
            if self.cancel.is_set() || self.journal.cancelled() {
                self.finalize_not_run(&trial)?;
            }
            let outcome = self.run_attempt(&trial)?;
            let snapshot = self.journal.snapshot();
            let decision = retry_decision(
                &outcome,
                replacements_used(&snapshot, &trial.identity),
                snapshot.cancelled || self.cancel.is_set(),
            );
            // The daemon's stop marker forbids a replacement as it forbids a new trial.
            let decision = if decision == RetryDecision::Replace && self.stop_requested() {
                RetryDecision::NoRetry("the daemon stopped the run")
            } else {
                decision
            };
            if decision != RetryDecision::Replace {
                let attempts = snapshot
                    .slot(&trial.identity.task, trial.identity.slot)
                    .iter()
                    .map(|r| r.draft.clone())
                    .collect();
                return Ok(SlotResult { attempts });
            }
            let next = replacement_of(&trial);
            let now = self.now_ms();
            let mode = self.mode;
            let from = trial.identity.clone();
            let replacement = next.clone();
            // One atomic write links the two attempts, so a crash leaves either no replacement
            // or a journaled one, never a duplicate.
            self.journal.update(move |journal| {
                if journal.record(&replacement.identity).is_none() {
                    let mut record = TrialRecord::new(replacement.clone(), mode, now);
                    record.replaces = Some(from.retry);
                    journal.records.push(record);
                }
                if let Some(old) = journal.record_mut(&from) {
                    old.replaced_by = Some(replacement.identity.retry);
                }
            })?;
            trial = next;
        }
    }

    /// The slot's attempt was never run because the run was cancelled: an explicit invalid row.
    fn finalize_not_run(&self, trial: &PlannedTrial) -> Result<(), LifecycleError> {
        let Some(record) = self.journal.record(&trial.identity) else {
            return Ok(());
        };
        if record.step != Step::Prepared {
            return Ok(());
        }
        let paths = (self.paths)(trial);
        let mut draft = record.draft;
        draft.exceptions.push(TrialException {
            kind: TrialExceptionKind::Cancelled,
            message: "the run was cancelled before this trial started".into(),
        });
        draft.states.push(TrialState::Terminal);
        self.set_terminal(&trial.identity, draft, &paths)
    }

    fn set_terminal(
        &self,
        identity: &TrialIdentity,
        draft: TrialOutcomeV1,
        paths: &TrialPaths,
    ) -> Result<(), LifecycleError> {
        let now = self.now_ms();
        let outcome_path = paths.work_dir.join(OUTCOME_FILE);
        let for_journal = draft.clone();
        let id = identity.clone();
        self.journal.update(move |journal| {
            if let Some(record) = journal.record_mut(&id) {
                record.step = Step::Terminal;
                record.draft = for_journal;
                record.outcome_path = Some(outcome_path.display().to_string());
                record.updated_ms = now;
            }
        })?;
        write_outcome(&paths.work_dir, &draft)
            .map_err(|e| LifecycleError::Halted(format!("{}: {e}", paths.work_dir.display())))
    }

    /// Run or resume one attempt and return its final outcome.
    fn run_attempt(&self, trial: &PlannedTrial) -> Result<TrialOutcomeV1, LifecycleError> {
        let paths = (self.paths)(trial);
        let Some(record) = self.journal.record(&trial.identity) else {
            return Err(LifecycleError::Halted("the attempt has no record".into()));
        };
        let mut plan = match plan_recovery(&record) {
            Recovery::Terminal => {
                // Reuse the durable outcome; only make sure the file the summary reads exists.
                let on_disk = paths.work_dir.join(OUTCOME_FILE).is_file();
                if !on_disk {
                    write_outcome(&paths.work_dir, &record.draft).map_err(|e| {
                        LifecycleError::Halted(format!("{}: {e}", paths.work_dir.display()))
                    })?;
                }
                return Ok(record.draft);
            }
            Recovery::Run(plan) => plan,
        };
        if self.mode == RunnerMode::Harness && record.step == Step::AgentRunning {
            // Oracle recovery deliberately invalidates an unknown exec; Harness has a durable
            // daemon identity and can reconnect without replaying the agent in the container.
            plan.resume.draft = record.draft.clone();
            match self.reconcile(&trial.identity)? {
                HarnessRecovery::RestartAttempt
                | HarnessRecovery::Reconnect { .. }
                | HarnessRecovery::CancelAndSettle
                | HarnessRecovery::Settled(_)
                | HarnessRecovery::InfraInvalid(_) => {
                    // The client resumes the status saved by reconcile: poll live, cancel
                    // expired, Finish settled, or preserve an infra row and its trace. Its
                    // typed result decides verification and the existing replacement policy.
                    plan.resume.from = Step::AgentRunning;
                }
            }
        }
        // Persist the recovery decision first, so planning it again gives the same plan. A
        // restart that must clean up keeps its intent step: if it dies again before the cleanup
        // finished, the next one cleans up again.
        let persisted_step = if plan.stale_cleanup {
            Step::EnvironmentStarting
        } else {
            settled_before(plan.resume.from)
        };
        let now = self.now_ms();
        let id = trial.identity.clone();
        let draft = plan.resume.draft.clone();
        let restarts = plan.restarts;
        self.journal.update(move |journal| {
            if let Some(record) = journal.record_mut(&id) {
                record.draft = draft;
                record.step = persisted_step;
                record.restarts = restarts;
                record.updated_ms = now;
            }
        })?;
        let mut resume = plan.resume;
        if plan.stale_cleanup {
            let mut stale = self.envs.open(trial, Adopt::Stale)?;
            let cleaned = stale.cleanup(Duration::from_secs(self.timeouts.cleanup_sec));
            if let Err(error) = cleaned {
                // The leftovers cannot be removed, so a new environment would collide with them:
                // the attempt is infrastructure-invalid and ends here.
                resume.draft.exceptions.push(TrialException {
                    kind: TrialExceptionKind::InfraInterrupted,
                    message: format!("stale environment could not be removed: {error:?}"),
                });
                resume.draft.cleanup.attempted = true;
                resume.draft.cleanup.error = Some(format!("{error:?}"));
                resume.draft.states.push(TrialState::Terminal);
                self.set_terminal(&trial.identity, resume.draft.clone(), &paths)?;
                return Ok(resume.draft);
            }
        }
        let adopt = match (&record.env, resume.from) {
            (Some(facts), from) if from > Step::EnvironmentStarting => Adopt::Facts(facts),
            (None, from) if from > Step::EnvironmentStarting => Adopt::Stale,
            _ => Adopt::Fresh,
        };
        let mut env = self.envs.open(trial, adopt)?;
        let mut observer = Observer {
            journal: self.journal,
            wall: self.wall,
            cancel: self.cancel,
            identity: trial.identity.clone(),
            agent_timeout_ms: trial.agent_timeout_sec.saturating_mul(1000),
            verifier_timeout_ms: trial.verifier_timeout_sec.saturating_mul(1000),
            paths: paths.clone(),
        };
        let collect_dir = paths.collect_dir();
        let inputs = TrialInputs {
            solution_dir: &paths.solution_dir,
            tests_dir: &paths.tests_dir,
            collect_dir: &collect_dir,
        };
        run_trial_observed(
            &mut *env,
            self.wall,
            &mut HarnessModel {
                client: self.sessions.and_then(SessionPort::client),
                wall: self.wall,
                journal: self.journal,
                identity: trial.identity.clone(),
            },
            self.mode,
            self.timeouts,
            trial,
            &inputs,
            &mut observer,
            Some(resume),
        )
        .map_err(|halt| LifecycleError::Halted(halt.0))
    }

    /// Ask the daemon about a journaled Harness attempt and decide how to continue it (R6 acts
    /// on the answer; the decision itself is the journal's pure function).
    ///
    /// # Errors
    /// The attempt is unknown, no session port is configured, or the daemon cannot be asked.
    pub fn reconcile(&self, identity: &TrialIdentity) -> Result<HarnessRecovery, LifecycleError> {
        let record = self
            .journal
            .record(identity)
            .ok_or_else(|| LifecycleError::Halted("unknown attempt".into()))?;
        let sessions = self
            .sessions
            .ok_or_else(|| LifecycleError::Halted("no daemon session port".into()))?;
        let trial = record.trial_id();
        let view = sessions
            .status(&trial)
            .map_err(|why| LifecycleError::Halted(format!("daemon status: {why}")))?;
        if view == SessionView::NoSession {
            self.journal.update(|journal| {
                if let Some(record) = journal.record_mut(identity) {
                    record.daemon = Some(DaemonTrial {
                        trial: trial.clone(),
                        last_status: None,
                        cancel_requested: false,
                    });
                }
            })?;
        }
        if let SessionView::Live(status) | SessionView::Settled(status) = &view {
            // Cost and result of an interrupted attempt are kept, whatever happens next.
            let kept = status.clone();
            let id = identity.clone();
            self.journal.update(move |journal| {
                if let Some(record) = journal.record_mut(&id) {
                    let cancel_requested =
                        record.daemon.as_ref().is_some_and(|d| d.cancel_requested);
                    record.daemon = Some(DaemonTrial {
                        trial,
                        last_status: Some(kept),
                        cancel_requested,
                    });
                }
            })?;
        }
        Ok(reconcile_harness(&record, view, self.now_ms()))
    }

    /// Cancel the run: journal the cancellation, **cancel every known live session first**, and
    /// only then tear each unfinished attempt down (collecting evidence before removal when its
    /// container exists) and finalise it as cancelled. A valid reward already read is kept.
    ///
    /// # Errors
    /// A journal failure. A session that cannot be cancelled is reported, not fatal: its
    /// environment is still removed.
    pub fn cancel_run(&self) -> Result<CancelReport, LifecycleError> {
        self.cancel.set();
        self.journal.update(|journal| journal.cancelled = true)?;
        let snapshot = self.journal.snapshot();
        let live: Vec<&TrialRecord> = snapshot
            .records
            .iter()
            .filter(|r| !r.is_terminal())
            .collect();
        let mut report = CancelReport::default();
        // 1. Sessions. In Harness mode an attempt that reached its agent step may have a session
        //    even when the Start reply was never seen; the journaled trial id still cancels it.
        if let Some(sessions) = self.sessions {
            let window = Duration::from_secs(CANCEL_SETTLE_TIMEOUT_SEC);
            for record in &live {
                let may_have_session = record.daemon.is_some()
                    || (self.mode == RunnerMode::Harness && record.step >= Step::AgentRunning);
                if !may_have_session {
                    continue;
                }
                let trial = record.trial_id();
                match sessions.cancel(&trial, window) {
                    Ok(()) => report.cancelled_sessions.push(trial),
                    Err(why) => report.session_errors.push(format!("{trial}: {why}")),
                }
            }
        }
        // 2. Teardown, only after every cancel was issued.
        for record in live {
            self.tear_down(record)?;
            report.torn_down.push(record.planned.identity.clone());
        }
        Ok(report)
    }

    fn tear_down(&self, record: &TrialRecord) -> Result<(), LifecycleError> {
        let trial = &record.planned;
        let paths = (self.paths)(trial);
        let mut draft = record.draft.clone();
        let started = record.step > Step::Prepared;
        if started {
            let adopt = record.env.as_ref().map_or(Adopt::Stale, Adopt::Facts);
            match self.envs.open(trial, adopt) {
                Ok(mut env) => {
                    let has_container = record.env.as_ref().is_some_and(|f| f.container.is_some());
                    if has_container && !draft.collection.collected {
                        draft.collection.attempted = true;
                        let collected =
                            settle_for_collection(&mut *env, self.wall).and_then(|()| {
                                env.collect(
                                    &paths.collect_dir(),
                                    Duration::from_secs(self.timeouts.collect_sec),
                                )
                            });
                        match collected {
                            Ok(()) => draft.collection.collected = true,
                            Err(error) => draft.collection.error = Some(describe(&error)),
                        }
                    }
                    draft.cleanup.attempted = true;
                    match env.cleanup(Duration::from_secs(self.timeouts.cleanup_sec)) {
                        Ok(()) => draft.cleanup.cleaned = true,
                        Err(error) => draft.cleanup.error = Some(describe(&error)),
                    }
                }
                Err(error) => draft.cleanup.error = Some(error.detail),
            }
        }
        if !draft.reward_valid {
            draft.exceptions.push(TrialException {
                kind: TrialExceptionKind::Cancelled,
                message: "the operator cancelled the run".into(),
            });
        }
        draft.states.push(TrialState::Terminal);
        self.set_terminal(&trial.identity, draft, &paths)
    }
}

fn describe(error: &EnvError) -> String {
    match error {
        EnvError::Timeout => "deadline exceeded".to_string(),
        EnvError::Infra(why) | EnvError::Failed(why) => why.clone(),
    }
}
