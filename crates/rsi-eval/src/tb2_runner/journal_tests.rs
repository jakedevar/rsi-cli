//! Sourc-derived tests of the journal, recovery, retries and artifact settlement (#3683 R5;
//! design #3628 "Native Harness trial and recovery" and "Outcomes, records, provenance and
//! budgets"). Fake infrastructure only: a fake environment over a shared fake "Docker" world, a
//! virtual wall clock, a fake session port and the real journal on a temp directory. A crash is
//! injected at **every** journal write, before and after it, and the run is resumed by a new
//! journal store (a new process) until it ends.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use rsi_common::{
    bench::{BenchTaskState, BenchTaskStatusV1, BenchUsage},
    bench_runner::{
        PlannedTrial, RunnerMode, TimeoutPolicy, TrialException, TrialExceptionKind, TrialIdentity,
        TrialOutcomeV1, TrialState, trial_key,
    },
};

use super::{
    artifacts::{
        ArtifactManifest, Limits, SkipReason, build_manifest, is_safe_relative, verify_manifest,
    },
    client::Pacer,
    journal::{
        Faults, HarnessRecovery, JournalError, JournalStore, MAX_PRE_AGENT_RESTARTS, Recovery,
        RetryDecision, RunIdentity, RunJournalV1, SessionView, TrialRecord, plan_recovery,
        reconcile_harness, retry_decision, settled_before,
    },
    lifecycle::{
        Adopt, CancelFlag, EnvFactory, Lifecycle, LifecycleError, MANIFEST_FILE, OUTCOME_FILE,
        SessionPort, TrialPaths,
    },
    reward::{REWARD_JSON_PATH, REWARD_TEXT_PATH},
    transport::OpError,
    trial::{
        Clock, EnvError, EnvFacts, ExecSpec, ORACLE_LOG_PATH, Step, TrialEnvironment,
        VERIFIER_LOG_PATH,
    },
};

// ---- virtual wall clock ----------------------------------------------------------------------

/// Epoch milliseconds; sleeping and fake operations advance it.
struct Wall(AtomicU64);

impl Wall {
    fn at(ms: u64) -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(ms)))
    }

    fn advance(&self, secs: u64) {
        self.0.fetch_add(secs * 1000, Ordering::SeqCst);
    }

    fn ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl Clock for Wall {
    fn now(&self) -> Duration {
        Duration::from_millis(self.ms())
    }
}

impl Pacer for Wall {
    fn sleep(&self, duration: Duration) {
        self.0.fetch_add(
            u64::try_from(duration.as_millis()).unwrap(),
            Ordering::SeqCst,
        );
    }
}

// ---- the fake Docker world, shared by every "process" ------------------------------------------

#[derive(Default)]
struct World {
    /// Every external action, in order: `"<op> <key>"`.
    events: Vec<String>,
    /// Environments that exist (created and not yet removed).
    alive: BTreeSet<String>,
    /// Verifier runs that wrote a reward.
    rewarded: BTreeSet<String>,
    reward: &'static str,
    oracle_secs: u64,
    verify_secs: u64,
    /// Keys whose environment start fails as a confirmed infrastructure interruption.
    start_infra: BTreeSet<String>,
    /// Keys whose oracle run is cut by an infrastructure interruption.
    oracle_infra: BTreeSet<String>,
    /// Stale cleanup fails for these keys.
    cleanup_fails: BTreeSet<String>,
    /// Surviving execs can still mutate logs until settlement succeeds.
    active_execs: BTreeSet<String>,
    settle_fails: bool,
    settle_secs: u64,
    /// Set the shared cancel flag when this key's oracle starts.
    cancel_on_oracle: Option<(String, Arc<CancelFlag>)>,
    wall: Option<Arc<Wall>>,
}

impl World {
    fn count(&self, op: &str, key: &str) -> usize {
        let wanted = format!("{op} {key}");
        self.events.iter().filter(|e| **e == wanted).count()
    }
}

type Shared = Arc<Mutex<World>>;

struct FakeEnv {
    world: Shared,
    key: String,
    collected: bool,
}

impl FakeEnv {
    fn log(&self, op: &str) {
        self.world
            .lock()
            .unwrap()
            .events
            .push(format!("{op} {}", self.key));
    }

    fn wall(&self) -> Arc<Wall> {
        self.world.lock().unwrap().wall.clone().unwrap()
    }
}

impl TrialEnvironment for FakeEnv {
    fn start(&mut self, _timeout: Duration) -> Result<(), EnvError> {
        self.log("start");
        let mut world = self.world.lock().unwrap();
        world.alive.insert(self.key.clone());
        if world.start_infra.contains(&self.key) {
            return Err(EnvError::Infra("docker tunnel lost".into()));
        }
        Ok(())
    }

    fn upload_dir(
        &mut self,
        _local: &Path,
        remote: &str,
        _timeout: Duration,
    ) -> Result<(), EnvError> {
        self.log(&format!("upload:{remote}"));
        Ok(())
    }

    fn exec(&mut self, spec: &ExecSpec<'_>) -> Result<i32, EnvError> {
        if spec.script.starts_with("chmod") {
            return Ok(0);
        }
        let (secs, ran) = if spec.script.contains(ORACLE_LOG_PATH) {
            self.log("oracle");
            self.world
                .lock()
                .unwrap()
                .active_execs
                .insert(self.key.clone());
            let (flag, infra, secs) = {
                let world = self.world.lock().unwrap();
                (
                    world.cancel_on_oracle.clone(),
                    world.oracle_infra.contains(&self.key),
                    world.oracle_secs,
                )
            };
            if let Some((key, cancel)) = flag
                && key == self.key
            {
                cancel.set();
            }
            if infra {
                return Err(EnvError::Infra("ssh connection reset".into()));
            }
            (secs, "oracle")
        } else if spec.script.contains(VERIFIER_LOG_PATH) {
            self.log("verify");
            self.world
                .lock()
                .unwrap()
                .active_execs
                .insert(self.key.clone());
            let secs = self.world.lock().unwrap().verify_secs;
            self.world.lock().unwrap().rewarded.insert(self.key.clone());
            (secs, "verify")
        } else {
            (0, "other")
        };
        let _ = ran;
        self.wall().advance(secs);
        Ok(0)
    }

    fn read_file(&mut self, remote: &str, _timeout: Duration) -> Result<Option<Vec<u8>>, EnvError> {
        if remote == REWARD_TEXT_PATH {
            self.log("read_reward");
            let world = self.world.lock().unwrap();
            if world.rewarded.contains(&self.key) {
                return Ok(Some(world.reward.as_bytes().to_vec()));
            }
        }
        let _ = REWARD_JSON_PATH;
        Ok(None)
    }

    fn settle_execs(&mut self, _timeout: Duration) -> Result<(), EnvError> {
        self.log("settle");
        let mut world = self.world.lock().unwrap();
        world.wall.as_ref().unwrap().advance(world.settle_secs);
        if world.settle_fails {
            return Err(EnvError::Failed("exec still running".into()));
        }
        world.active_execs.remove(&self.key);
        Ok(())
    }

    fn collect(&mut self, dest: &Path, _timeout: Duration) -> Result<(), EnvError> {
        self.log("collect");
        assert!(
            !self.world.lock().unwrap().active_execs.contains(&self.key),
            "evidence must be stable before collection"
        );
        std::fs::create_dir_all(dest.join("agent")).unwrap();
        std::fs::write(
            dest.join("agent/oracle.txt"),
            format!("log of {}", self.key),
        )
        .unwrap();
        self.collected = true;
        Ok(())
    }

    fn cleanup(&mut self, _timeout: Duration) -> Result<(), EnvError> {
        self.log("cleanup");
        let mut world = self.world.lock().unwrap();
        if world.cleanup_fails.contains(&self.key) {
            return Err(EnvError::Failed("compose down failed".into()));
        }
        world.alive.remove(&self.key);
        world.active_execs.remove(&self.key);
        Ok(())
    }

    fn facts(&self) -> Option<EnvFacts> {
        Some(EnvFacts {
            project: self.key.clone(),
            container: Some(format!("c-{}", self.key)),
            dirs_created: true,
            compose_attempted: true,
            collected: self.collected,
        })
    }
}

struct Factory(Shared);

impl EnvFactory for Factory {
    fn open<'e>(
        &'e self,
        trial: &'e PlannedTrial,
        adopt: Adopt<'_>,
    ) -> Result<Box<dyn TrialEnvironment + 'e>, OpError> {
        let key = trial_key(&trial.identity);
        let collected = matches!(adopt, Adopt::Facts(f) if f.collected);
        if matches!(adopt, Adopt::Stale) {
            self.0
                .lock()
                .unwrap()
                .events
                .push(format!("adopt_stale {key}"));
        }
        Ok(Box::new(FakeEnv {
            world: Arc::clone(&self.0),
            key,
            collected,
        }))
    }
}

#[derive(Default)]
struct Sessions {
    log: Arc<Mutex<Vec<String>>>,
    /// The fake Docker world: session cancels land in its event log too, so the order of a
    /// cancellation against a teardown is observable.
    world: Option<Shared>,
    views: Mutex<BTreeMap<String, SessionView>>,
    cancel_fails: bool,
}

impl SessionPort for Sessions {
    fn status(&self, trial: &str) -> Result<SessionView, String> {
        Ok(self
            .views
            .lock()
            .unwrap()
            .get(trial)
            .cloned()
            .unwrap_or(SessionView::NoSession))
    }

    fn cancel(&self, trial: &str, _window: Duration) -> Result<(), String> {
        self.log
            .lock()
            .unwrap()
            .push(format!("session_cancel {trial}"));
        if let Some(world) = &self.world {
            world
                .lock()
                .unwrap()
                .events
                .push(format!("session_cancel {trial}"));
        }
        if self.cancel_fails {
            Err("daemon unreachable".into())
        } else {
            Ok(())
        }
    }
}

// ---- the rig -----------------------------------------------------------------------------------

fn trial(task: &str, slot: u32, retry: u32) -> PlannedTrial {
    PlannedTrial {
        identity: TrialIdentity {
            run_id: "run-1".into(),
            task: task.into(),
            slot,
            retry,
        },
        image: format!("registry/{task}:1"),
        image_digest: format!("sha256:{}", "a".repeat(64)),
        cpus: 1,
        memory_mb: 2048,
        agent_timeout_sec: 900,
        verifier_timeout_sec: 600,
        solution_env: BTreeMap::new(),
        verifier_env: BTreeMap::new(),
        agent_user: None,
        verifier_user: None,
    }
}

fn identity() -> RunIdentity {
    RunIdentity {
        plan_sha256: "sha256:plan".into(),
        runner_digest: "sha256:runner".into(),
        dataset_sha256: "sha256:data".into(),
    }
}

struct Rig {
    dir: tempfile::TempDir,
    world: Shared,
    wall: Arc<Wall>,
    cancel: Arc<CancelFlag>,
    trials: Vec<PlannedTrial>,
    timeouts: TimeoutPolicy,
    stop_file: Option<PathBuf>,
}

impl Rig {
    fn new(tasks: &[&str]) -> Self {
        let wall = Wall::at(1_000_000);
        let cancel = Arc::new(CancelFlag::new());
        let world = Arc::new(Mutex::new(World {
            reward: "1.0",
            oracle_secs: 5,
            verify_secs: 10,
            wall: Some(Arc::clone(&wall)),
            ..World::default()
        }));
        Self {
            dir: tempfile::tempdir().unwrap(),
            world,
            wall,
            cancel,
            trials: tasks.iter().map(|t| trial(t, 0, 0)).collect(),
            timeouts: TimeoutPolicy::default(),
            stop_file: None,
        }
    }

    fn run_dir(&self) -> PathBuf {
        self.dir.path().join("run")
    }

    fn open(&self, faults: Faults) -> Result<JournalStore, JournalError> {
        JournalStore::open_with(
            &self.run_dir(),
            "run-1",
            &identity(),
            self.wall.ms(),
            faults,
        )
    }

    fn work_dir(&self, t: &PlannedTrial) -> PathBuf {
        self.run_dir().join("trials").join(trial_key(&t.identity))
    }

    fn with_lifecycle<R>(
        &self,
        store: &JournalStore,
        mode: RunnerMode,
        sessions: Option<&Sessions>,
        body: impl FnOnce(&Lifecycle<'_>) -> R,
    ) -> R {
        let factory = Factory(Arc::clone(&self.world));
        let root = self.run_dir();
        let paths = move |t: &PlannedTrial| TrialPaths {
            solution_dir: root.join("solution"),
            tests_dir: root.join("tests"),
            work_dir: root.join("trials").join(trial_key(&t.identity)),
        };
        let lifecycle = Lifecycle {
            journal: store,
            envs: &factory,
            wall: &*self.wall,
            sessions: sessions.map(|s| s as &dyn SessionPort),
            cancel: &self.cancel,
            mode,
            timeouts: &self.timeouts,
            paths: &paths,
            stop_file: self.stop_file.as_deref(),
        };
        body(&lifecycle)
    }

    /// Run every slot to its end. The first process gets `faults`; after a crash a new store
    /// (a new process, no faults) resumes. Returns the crashes survived and the final journal.
    fn drive_all(&self, faults: Faults) -> (u32, RunJournalV1) {
        let mut faults = faults;
        let mut crashes = 0;
        loop {
            let store = match self.open(faults) {
                Ok(store) => store,
                Err(JournalError::InjectedCrash(_)) => {
                    crashes += 1;
                    faults = Faults::default();
                    continue;
                }
                Err(other) => panic!("open: {other}"),
            };
            faults = Faults::default();
            let mut crashed = false;
            for planned in &self.trials {
                let result =
                    self.with_lifecycle(&store, RunnerMode::Oracle, None, |l| l.run_slot(planned));
                match result {
                    Ok(_) => {}
                    Err(
                        LifecycleError::Halted(_)
                        | LifecycleError::Journal(JournalError::InjectedCrash(_)),
                    ) => {
                        crashed = true;
                        break;
                    }
                    Err(other) => panic!("run_slot: {other}"),
                }
            }
            if !crashed {
                return (crashes, store.snapshot());
            }
            crashes += 1;
            assert!(crashes < 12, "the run does not converge");
        }
    }

    fn events(&self) -> Vec<String> {
        self.world.lock().unwrap().events.clone()
    }

    fn count(&self, op: &str, key: &str) -> usize {
        self.world.lock().unwrap().count(op, key)
    }
}

fn last_of<'a>(journal: &'a RunJournalV1, task: &str) -> &'a TrialRecord {
    journal
        .slot(task, 0)
        .last()
        .copied()
        .expect("slot has records")
}

fn exception(kind: TrialExceptionKind) -> TrialException {
    TrialException {
        kind,
        message: format!("{kind:?}"),
    }
}

fn outcome_with(kinds: &[TrialExceptionKind], valid: bool) -> TrialOutcomeV1 {
    let mut outcome = super::trial::fresh_outcome(&trial("t", 0, 0), RunnerMode::Oracle);
    outcome.exceptions = kinds.iter().copied().map(exception).collect();
    outcome.reward_valid = valid;
    outcome.reward = valid.then_some(1.0);
    outcome
}

/// The first crash point after which the durable journal satisfies `wanted` (a new process then
/// finds exactly that on disk).
fn crash_point(before: bool, tasks: &[&str], wanted: impl Fn(&RunJournalV1) -> bool) -> usize {
    for index in 0..400 {
        let rig = Rig::new(tasks);
        let faults = if before {
            Faults {
                crash_before: Some(index),
                ..Faults::default()
            }
        } else {
            Faults {
                crash_after: Some(index),
                ..Faults::default()
            }
        };
        let store = match rig.open(faults) {
            Ok(store) => store,
            Err(_) => continue,
        };
        for planned in &rig.trials {
            let _ = rig.with_lifecycle(&store, RunnerMode::Oracle, None, |l| l.run_slot(planned));
        }
        drop(store);
        let bytes = std::fs::read(rig.run_dir().join("journal.json")).unwrap();
        let journal: RunJournalV1 = serde_json::from_slice(&bytes).unwrap();
        if wanted(&journal) {
            return index;
        }
    }
    panic!("no crash point matches");
}

// ---- the journal file --------------------------------------------------------------------------

#[test]
fn the_journal_is_owned_by_one_runner_and_bound_to_its_plan_build_and_dataset() {
    let rig = Rig::new(&["alpha"]);
    let first = rig.open(Faults::default()).unwrap();
    // A live second runner cannot take the run.
    assert!(matches!(
        rig.open(Faults::default()),
        Err(JournalError::Owned(_))
    ));
    drop(first);
    // A dead one's lock is gone: the run can be resumed, by the same identity only.
    let reopened = rig.open(Faults::default()).unwrap();
    drop(reopened);
    for changed in [
        RunIdentity {
            plan_sha256: "sha256:other-plan".into(),
            ..identity()
        },
        RunIdentity {
            runner_digest: "sha256:other-build".into(),
            ..identity()
        },
        RunIdentity {
            dataset_sha256: "sha256:other-data".into(),
            ..identity()
        },
    ] {
        let refused = JournalStore::open(&rig.run_dir(), "run-1", &changed, 0);
        assert!(
            matches!(refused, Err(JournalError::Mismatch(_))),
            "{changed:?}"
        );
    }
    let wrong_run = JournalStore::open(&rig.run_dir(), "run-2", &identity(), 0);
    assert!(matches!(wrong_run, Err(JournalError::Mismatch(_))));
}

#[test]
fn a_corrupt_journal_is_refused_not_replaced() {
    let rig = Rig::new(&["alpha"]);
    drop(rig.open(Faults::default()).unwrap());
    std::fs::write(rig.run_dir().join("journal.json"), "{ not json").unwrap();
    assert!(matches!(
        rig.open(Faults::default()),
        Err(JournalError::Corrupt(_))
    ));
    assert_eq!(
        std::fs::read_to_string(rig.run_dir().join("journal.json")).unwrap(),
        "{ not json"
    );
}

#[test]
fn a_journal_write_is_all_or_nothing_and_leaves_no_temp_file() {
    let rig = Rig::new(&["alpha"]);
    let store = rig.open(Faults::default()).unwrap();
    store.update(|j| j.cancelled = true).unwrap();
    let bytes = std::fs::read(rig.run_dir().join("journal.json")).unwrap();
    let on_disk: RunJournalV1 = serde_json::from_slice(&bytes).unwrap();
    assert!(on_disk.cancelled);
    let names: Vec<String> = std::fs::read_dir(rig.run_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().all(|n| !n.ends_with(".tmp")),
        "no temp file is left: {names:?}"
    );
    // A crash before a write leaves the previous journal exactly, and kills the store: nothing
    // later reaches the disk, like a process that was killed.
    drop(store);
    let doomed = rig
        .open(Faults {
            crash_before: Some(0),
            ..Faults::default()
        })
        .unwrap();
    assert_eq!(
        doomed.update(|j| j.cancelled = false),
        Err(JournalError::InjectedCrash(0))
    );
    assert_eq!(
        doomed.update(|j| j.cancelled = false),
        Err(JournalError::InjectedCrash(0))
    );
    let after: RunJournalV1 =
        serde_json::from_slice(&std::fs::read(rig.run_dir().join("journal.json")).unwrap())
            .unwrap();
    assert!(after.cancelled, "the previous journal is intact");
}

// ---- retry policy ------------------------------------------------------------------------------

#[test]
fn only_a_confirmed_infrastructure_interruption_earns_one_replacement() {
    use TrialExceptionKind as K;
    let infra = [K::InfraInterrupted];
    assert_eq!(
        retry_decision(&outcome_with(&infra, false), 0, false),
        RetryDecision::Replace
    );
    // The one replacement is spent.
    assert!(matches!(
        retry_decision(&outcome_with(&infra, false), 1, false),
        RetryDecision::NoRetry(_)
    ));
    // Task timeouts, reward failures, route and spend refusals, protocol errors and cancellation
    // are never retried, even when an infrastructure interruption is also recorded.
    for never in [
        K::AgentTimeout,
        K::VerifierTimeout,
        K::RewardMissing,
        K::RewardEmpty,
        K::RewardParse,
        K::RewardNonFinite,
        K::RouteRejected,
        K::SpendCapRejected,
        K::InvalidPlan,
        K::Cancelled,
        K::EnvironmentStartFailed,
        K::EnvironmentStartTimeout,
        K::AgentSettleFailed,
        K::VerifierExecFailed,
    ] {
        assert!(
            matches!(
                retry_decision(&outcome_with(&[never], false), 0, false),
                RetryDecision::NoRetry(_)
            ),
            "{never:?} alone"
        );
        assert!(
            matches!(
                retry_decision(
                    &outcome_with(&[never, K::InfraInterrupted], false),
                    0,
                    false
                ),
                RetryDecision::NoRetry(_)
            ),
            "{never:?} then infra"
        );
    }
    // A valid reward is never replaced, and a cancelled run replaces nothing.
    assert!(matches!(
        retry_decision(&outcome_with(&infra, true), 0, false),
        RetryDecision::NoRetry(_)
    ));
    assert!(matches!(
        retry_decision(&outcome_with(&infra, false), 0, true),
        RetryDecision::NoRetry(_)
    ));
    assert!(matches!(
        retry_decision(&outcome_with(&[], false), 0, false),
        RetryDecision::NoRetry(_)
    ));
}

#[test]
fn a_task_timeout_a_bad_reward_or_a_verifier_timeout_is_graded_once_and_never_replaced() {
    // Agent timeout: still graded by the verifier, valid, one attempt.
    let rig = Rig::new(&["slow"]);
    rig.world.lock().unwrap().oracle_secs = 901;
    let (crashes, journal) = rig.drive_all(Faults::default());
    assert_eq!(crashes, 0);
    assert_eq!(journal.slot("slow", 0).len(), 1);
    let record = last_of(&journal, "slow");
    assert!(record.draft.reward_valid, "{:?}", record.draft.exceptions);
    assert!(
        record
            .draft
            .exceptions
            .iter()
            .any(|e| e.kind == TrialExceptionKind::AgentTimeout)
    );
    assert_eq!(rig.count("oracle", "slow-s0-r0"), 1);

    // Verifier timeout: the reward that arrived late is not accepted, and nothing is retried.
    let rig = Rig::new(&["late"]);
    rig.world.lock().unwrap().verify_secs = 601;
    let (_, journal) = rig.drive_all(Faults::default());
    assert_eq!(journal.slot("late", 0).len(), 1, "no replacement");
    let record = last_of(&journal, "late");
    assert!(!record.draft.reward_valid && record.draft.reward.is_none());
    assert_eq!(
        record.draft.exceptions.last().map(|e| e.kind),
        Some(TrialExceptionKind::VerifierTimeout)
    );
    assert_eq!(rig.count("verify", "late-s0-r0"), 1);

    // A reward that never appears: invalid, not a zero, not retried.
    let rig = Rig::new(&["missing"]);
    rig.world.lock().unwrap().reward = "";
    let (_, journal) = rig.drive_all(Faults::default());
    assert_eq!(journal.slot("missing", 0).len(), 1);
    assert!(!last_of(&journal, "missing").draft.reward_valid);
}

// ---- recovery plans ------------------------------------------------------------------------------

fn record_at(step: Step, restarts: u32) -> TrialRecord {
    let mut record = TrialRecord::new(trial("t", 0, 0), RunnerMode::Oracle, 0);
    record.step = step;
    record.restarts = restarts;
    record.draft.states = vec![
        TrialState::Prepared,
        TrialState::EnvironmentReady,
        TrialState::AgentRunning,
        TrialState::AgentSettled,
    ];
    record
}

fn plan_of(record: &TrialRecord) -> super::journal::RecoveryPlan {
    match plan_recovery(record) {
        Recovery::Run(plan) => plan,
        Recovery::Terminal => panic!("expected a plan for {:?}", record.step),
    }
}

#[test]
fn every_step_has_one_recovery_and_planning_it_twice_gives_the_same_plan() {
    let steps = [
        Step::Prepared,
        Step::EnvironmentStarting,
        Step::EnvironmentDone,
        Step::AgentRunning,
        Step::AgentDone,
        Step::Verifying,
        Step::VerifyDone,
        Step::Collecting,
        Step::CollectDone,
        Step::CleaningUp,
        Step::CleanupDone,
    ];
    let expect_from = [
        Step::EnvironmentStarting,
        Step::EnvironmentStarting,
        Step::AgentRunning,
        Step::Collecting,
        Step::Verifying,
        Step::Collecting,
        Step::Collecting,
        Step::Collecting,
        Step::CleaningUp,
        Step::CleaningUp,
        Step::Terminal,
    ];
    for (step, from) in steps.into_iter().zip(expect_from) {
        let record = record_at(step, 0);
        let plan = plan_of(&record);
        assert_eq!(plan.resume.from, from, "{step:?}");
        // Persisting the plan (as the lifecycle does) and planning again changes nothing,
        // except that a stale-cleanup restart keeps its intent step and counts the next crash.
        if !plan.stale_cleanup {
            let mut persisted = record.clone();
            persisted.draft = plan.resume.draft.clone();
            persisted.step = settled_before(plan.resume.from);
            persisted.restarts = plan.restarts;
            let again = plan_of(&persisted);
            assert_eq!(again.resume.from, plan.resume.from, "{step:?}");
            assert_eq!(
                again.resume.draft.exceptions, plan.resume.draft.exceptions,
                "{step:?}"
            );
        }
    }
    assert!(matches!(
        plan_recovery(&record_at(Step::Terminal, 0)),
        Recovery::Terminal
    ));
}

#[test]
fn an_interrupted_oracle_or_verifier_is_infrastructure_invalid_and_never_rerun() {
    for step in [Step::AgentRunning, Step::Verifying] {
        let plan = plan_of(&record_at(step, 0));
        assert_eq!(
            plan.resume.from,
            Step::Collecting,
            "{step:?}: collect, then clean up"
        );
        assert!(!plan.stale_cleanup);
        let last = plan.resume.draft.exceptions.last().unwrap();
        assert_eq!(last.kind, TrialExceptionKind::InfraInterrupted, "{step:?}");
        assert!(plan.resume.draft.reward.is_none());
    }
    // A crash while starting removes the half-built environment first; a crash loop stops.
    let plan = plan_of(&record_at(Step::EnvironmentStarting, 0));
    assert!(plan.stale_cleanup && plan.restarts == 1);
    let plan = plan_of(&record_at(
        Step::EnvironmentStarting,
        MAX_PRE_AGENT_RESTARTS,
    ));
    assert!(!plan.stale_cleanup, "the crash loop is cut");
    assert_eq!(plan.resume.from, Step::CleaningUp);
    assert_eq!(
        plan.resume.draft.exceptions.last().unwrap().kind,
        TrialExceptionKind::InfraInterrupted
    );
}

// ---- crash at every journal write ----------------------------------------------------------------

fn assert_converged(rig: &Rig, journal: &RunJournalV1, label: &str) {
    for planned in &rig.trials {
        let task = planned.identity.task.as_str();
        let attempts = journal.slot(task, 0);
        assert!(!attempts.is_empty(), "{label}: slot {task} has no record");
        assert!(attempts.len() <= 2, "{label}: more than one replacement");
        for (index, attempt) in attempts.iter().enumerate() {
            assert_eq!(attempt.identity().retry as usize, index, "{label}");
            assert!(
                attempt.is_terminal(),
                "{label}: {task} r{index} not terminal"
            );
            // Evidence is on the hub for every attempt.
            assert!(
                rig.work_dir(&attempt.planned).join(OUTCOME_FILE).is_file(),
                "{label}: no outcome file for {task} r{index}"
            );
            // The oracle and the verifier never ran twice in one container.
            let key = trial_key(attempt.identity());
            assert!(
                rig.count("oracle", &key) <= 1,
                "{label}: oracle reran in {key}"
            );
            assert!(
                rig.count("verify", &key) <= 1,
                "{label}: verifier reran in {key}"
            );
            // Every container this attempt created is gone.
            assert!(
                !rig.world.lock().unwrap().alive.contains(&key),
                "{label}: {key} leaked"
            );
        }
        let (last, earlier) = attempts.split_last().unwrap();
        // A replaced attempt is an infrastructure row with no valid reward.
        for old in earlier {
            assert!(
                !old.draft.reward_valid,
                "{label}: a valid slot was replaced"
            );
            assert_eq!(
                old.draft.exceptions.last().unwrap().kind,
                TrialExceptionKind::InfraInterrupted,
                "{label}"
            );
            assert_eq!(old.replaced_by, Some(last.identity().retry), "{label}");
        }
        // The slot ends valid, or - only if its one replacement was also interrupted - invalid
        // for an infrastructure cause.
        if !last.draft.reward_valid {
            assert_eq!(
                attempts.len(),
                2,
                "{label}: invalid slot without its replacement"
            );
            assert_eq!(
                last.draft.exceptions.last().unwrap().kind,
                TrialExceptionKind::InfraInterrupted,
                "{label}"
            );
        } else {
            assert!(last.draft.passed(), "{label}");
        }
    }
}

#[test]
fn a_crash_before_or_after_every_journal_write_converges_without_duplicating_a_valid_slot() {
    let mut cases = 0;
    for before in [true, false] {
        for index in 0..400 {
            let rig = Rig::new(&["alpha", "beta"]);
            let faults = if before {
                Faults {
                    crash_before: Some(index),
                    ..Faults::default()
                }
            } else {
                Faults {
                    crash_after: Some(index),
                    ..Faults::default()
                }
            };
            let (crashes, journal) = rig.drive_all(faults);
            let label = format!(
                "crash {} write {index}",
                if before { "before" } else { "after" }
            );
            assert_converged(&rig, &journal, &label);
            // The journal on disk is the journal in memory.
            let disk: RunJournalV1 =
                serde_json::from_slice(&std::fs::read(rig.run_dir().join("journal.json")).unwrap())
                    .unwrap();
            assert_eq!(disk, journal, "{label}");
            cases += 1;
            if crashes == 0 {
                // The index is past the last write: every earlier write was covered.
                break;
            }
        }
    }
    assert!(cases > 40, "the sweep covered {cases} crash points only");
}

#[test]
fn a_clean_run_journals_absolute_deadlines_and_every_state_in_order() {
    let rig = Rig::new(&["alpha"]);
    let start = rig.wall.ms();
    let (crashes, journal) = rig.drive_all(Faults::default());
    assert_eq!(crashes, 0);
    let record = last_of(&journal, "alpha");
    let agent_start = record.deadlines.agent_started_ms.unwrap();
    assert!(agent_start >= start);
    assert_eq!(
        record.deadlines.agent_deadline_ms,
        Some(agent_start + 900_000)
    );
    let verify_start = record.deadlines.verifier_started_ms.unwrap();
    assert_eq!(
        record.deadlines.verifier_deadline_ms,
        Some(verify_start + 600_000)
    );
    assert!(
        verify_start >= agent_start + 5_000,
        "the verifier's clock starts after the agent's"
    );
    assert_eq!(
        record.draft.states,
        [
            TrialState::Prepared,
            TrialState::EnvironmentReady,
            TrialState::AgentRunning,
            TrialState::AgentSettled,
            TrialState::Verifying,
            TrialState::Collected,
            TrialState::Cleaned,
            TrialState::Terminal,
        ]
    );
    assert_eq!(record.step, Step::Terminal);
    assert_eq!(record.env.as_ref().map(|e| e.collected), Some(true));
}

// ---- resume --------------------------------------------------------------------------------------

#[test]
fn resuming_a_finished_run_reuses_every_durable_result_and_touches_nothing() {
    let rig = Rig::new(&["alpha", "beta"]);
    let (_, first) = rig.drive_all(Faults::default());
    let events = rig.events().len();
    // The world would now fail the oracle - a replay would show.
    rig.world.lock().unwrap().reward = "0.0";
    let (crashes, second) = rig.drive_all(Faults::default());
    assert_eq!(crashes, 0);
    assert_eq!(rig.events().len(), events, "no external action on resume");
    assert_eq!(
        first.records, second.records,
        "results are reused, not replayed"
    );
    for task in ["alpha", "beta"] {
        assert!(last_of(&second, task).draft.passed());
        assert_eq!(second.slot(task, 0).len(), 1, "no duplicate slot");
    }
}

#[test]
fn a_missing_outcome_file_of_a_durable_terminal_result_is_rewritten_not_rerun() {
    let rig = Rig::new(&["alpha"]);
    let (_, first) = rig.drive_all(Faults::default());
    let path = rig.work_dir(&first.records[0].planned).join(OUTCOME_FILE);
    std::fs::remove_file(&path).unwrap();
    let events = rig.events().len();
    let (_, second) = rig.drive_all(Faults::default());
    assert_eq!(rig.events().len(), events);
    let written: TrialOutcomeV1 = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(written, last_of(&second, "alpha").draft);
}

#[test]
fn a_crash_before_the_agent_costs_nothing_and_restarts_the_same_attempt_after_removing_leftovers() {
    // Die after "environment starting" was journaled, before the environment existed.
    let index = crash_point(false, &["alpha"], |j| {
        j.records
            .first()
            .is_some_and(|r| r.step == Step::EnvironmentStarting)
    });
    let rig = Rig::new(&["alpha"]);
    let (crashes, journal) = rig.drive_all(Faults {
        crash_after: Some(index),
        ..Faults::default()
    });
    assert_eq!(crashes, 1);
    assert_eq!(
        journal.slot("alpha", 0).len(),
        1,
        "no replacement: nothing was lost"
    );
    let record = last_of(&journal, "alpha");
    assert!(record.draft.passed());
    assert_eq!(record.restarts, 1);
    let events = rig.events();
    let stale = events
        .iter()
        .position(|e| e == "adopt_stale alpha-s0-r0")
        .unwrap();
    let cleanup = events
        .iter()
        .position(|e| e == "cleanup alpha-s0-r0")
        .unwrap();
    let start = events
        .iter()
        .position(|e| e == "start alpha-s0-r0")
        .unwrap();
    assert!(
        stale < cleanup && cleanup < start,
        "leftovers are removed before the restart"
    );
}

#[test]
fn leftovers_that_cannot_be_removed_end_the_attempt_instead_of_colliding() {
    let index = crash_point(false, &["alpha"], |j| {
        j.records
            .first()
            .is_some_and(|r| r.step == Step::EnvironmentStarting)
    });
    let rig = Rig::new(&["alpha"]);
    rig.world
        .lock()
        .unwrap()
        .cleanup_fails
        .insert("alpha-s0-r0".into());
    let (_, journal) = rig.drive_all(Faults {
        crash_after: Some(index),
        ..Faults::default()
    });
    let first = journal.slot("alpha", 0)[0];
    assert!(!first.draft.reward_valid);
    assert_eq!(
        first.draft.exceptions.last().unwrap().kind,
        TrialExceptionKind::InfraInterrupted
    );
    assert!(
        first.draft.cleanup.error.is_some(),
        "the cleanup failure is reported apart"
    );
    assert_eq!(
        rig.count("start", "alpha-s0-r0"),
        0,
        "no environment was started over leftovers"
    );
}

#[test]
fn an_oracle_interrupted_by_a_crash_is_invalid_keeps_its_row_and_gets_one_fresh_replacement() {
    // The oracle ran to its end in the container, but the runner died before journaling it.
    let index = crash_point(true, &["alpha"], |j| {
        j.records
            .first()
            .is_some_and(|r| r.step == Step::AgentRunning)
    });
    let rig = Rig::new(&["alpha"]);
    let (crashes, journal) = rig.drive_all(Faults {
        crash_before: Some(index),
        ..Faults::default()
    });
    assert_eq!(crashes, 1);
    let attempts = journal.slot("alpha", 0);
    assert_eq!(
        attempts.len(),
        2,
        "the infrastructure row and its one replacement"
    );
    let (old, new) = (attempts[0], attempts[1]);
    // The old row is kept: invalid, typed, with its evidence collected and its container gone.
    assert!(!old.draft.reward_valid && old.draft.reward.is_none());
    assert_eq!(
        old.draft.exceptions.last().unwrap().kind,
        TrialExceptionKind::InfraInterrupted
    );
    assert!(old.draft.collection.collected && old.draft.cleanup.cleaned);
    assert_eq!(old.replaced_by, Some(1));
    // Never verified: no verifier and no reward read in the modified container.
    assert_eq!(rig.count("verify", "alpha-s0-r0"), 0);
    assert_eq!(rig.count("read_reward", "alpha-s0-r0"), 0);
    // The replacement is a fresh container, in the same slot, and it passed.
    assert_eq!(new.replaces, Some(0));
    assert_eq!(new.identity().slot, old.identity().slot);
    assert_ne!(trial_key(new.identity()), trial_key(old.identity()));
    assert!(new.draft.passed());
    assert_eq!(rig.count("oracle", "alpha-s0-r0"), 1);
    assert_eq!(rig.count("oracle", "alpha-s0-r1"), 1);
}

#[test]
fn no_reward_is_read_after_a_restart_even_when_the_deadline_passed_and_a_reward_file_exists() {
    // Die while the verifier ran: the reward file may well exist by the time we are back.
    let index = crash_point(true, &["alpha"], |j| {
        j.records.first().is_some_and(|r| r.step == Step::Verifying)
    });
    let rig = Rig::new(&["alpha"]);
    // The restart happens long after the original verifier deadline.
    let wall = Arc::clone(&rig.wall);
    let reads_before_restart;
    let (crashes, journal) = {
        let store = rig
            .open(Faults {
                crash_before: Some(index),
                ..Faults::default()
            })
            .unwrap();
        let first = rig.with_lifecycle(&store, RunnerMode::Oracle, None, |l| {
            l.run_slot(&rig.trials[0])
        });
        assert!(first.is_err());
        drop(store);
        wall.advance(3 * 3600);
        reads_before_restart = rig.count("read_reward", "alpha-s0-r0");
        let (more, journal) = rig.drive_all(Faults::default());
        (more + 1, journal)
    };
    assert_eq!(crashes, 1);
    let attempts = journal.slot("alpha", 0);
    assert_eq!(attempts.len(), 2);
    assert!(
        !attempts[0].draft.reward_valid,
        "the late reward is not accepted"
    );
    assert_eq!(
        rig.count("verify", "alpha-s0-r0"),
        1,
        "the verifier is not re-run"
    );
    assert_eq!(
        rig.count("read_reward", "alpha-s0-r0"),
        reads_before_restart,
        "and nothing is read after the fact"
    );
    assert!(
        attempts[1].draft.passed(),
        "the replacement runs on its own fresh clocks"
    );
}

#[test]
fn a_crash_after_a_valid_reward_keeps_it_and_only_finishes_collection_and_cleanup() {
    let index = crash_point(false, &["alpha"], |j| {
        j.records
            .first()
            .is_some_and(|r| r.step == Step::VerifyDone)
    });
    let rig = Rig::new(&["alpha"]);
    let (crashes, journal) = rig.drive_all(Faults {
        crash_after: Some(index),
        ..Faults::default()
    });
    assert_eq!(crashes, 1);
    assert_eq!(
        journal.slot("alpha", 0).len(),
        1,
        "a valid slot is never replaced"
    );
    assert!(last_of(&journal, "alpha").draft.passed());
    assert_eq!(rig.count("verify", "alpha-s0-r0"), 1);
    assert_eq!(rig.count("oracle", "alpha-s0-r0"), 1);
    assert_eq!(rig.count("collect", "alpha-s0-r0"), 1);
}

#[test]
fn an_infrastructure_start_failure_is_replaced_once_and_the_second_is_final() {
    let rig = Rig::new(&["alpha"]);
    {
        let mut world = rig.world.lock().unwrap();
        world.start_infra.insert("alpha-s0-r0".into());
        world.start_infra.insert("alpha-s0-r1".into());
    }
    let (_, journal) = rig.drive_all(Faults::default());
    let attempts = journal.slot("alpha", 0);
    assert_eq!(attempts.len(), 2, "one replacement only");
    for attempt in &attempts {
        assert!(!attempt.draft.reward_valid);
        assert_eq!(
            attempt.draft.exceptions.last().unwrap().kind,
            TrialExceptionKind::InfraInterrupted
        );
        assert!(attempt.draft.cleanup.cleaned, "both containers are removed");
    }
    assert_eq!(rig.count("start", "alpha-s0-r0"), 1);
    assert_eq!(rig.count("start", "alpha-s0-r1"), 1);
}

#[test]
fn the_daemon_stop_marker_starts_no_replacement_but_keeps_the_infrastructure_row() {
    let mut rig = Rig::new(&["alpha"]);
    let marker = rig.dir.path().join("stop.json");
    rig.stop_file = Some(marker.clone());
    rig.world
        .lock()
        .unwrap()
        .start_infra
        .insert("alpha-s0-r0".into());
    // Written before the replacement would start: the run's budget is spent.
    std::fs::write(&marker, b"{}").unwrap();
    let (_, journal) = rig.drive_all(Faults::default());
    let attempts = journal.slot("alpha", 0);
    assert_eq!(attempts.len(), 1, "the stop marker forbids the replacement");
    assert!(!attempts[0].draft.reward_valid);
    assert_eq!(
        attempts[0].draft.exceptions.last().unwrap().kind,
        TrialExceptionKind::InfraInterrupted
    );
    assert!(
        attempts[0].draft.cleanup.cleaned,
        "its container is removed"
    );
    assert_eq!(rig.count("start", "alpha-s0-r1"), 0);
}

// ---- cancellation ------------------------------------------------------------------------------

#[test]
fn recovery_and_cancellation_settle_surviving_execs_before_collecting_evidence() {
    for step in [Step::AgentRunning, Step::Verifying] {
        for cancel in [false, true] {
            let rig = Rig::new(&["alpha"]);
            let store = rig.open(Faults::default()).unwrap();
            seed(&store, &rig.trials[0], step, false, true);
            store
                .update(|j| {
                    j.records[0].draft.mode = RunnerMode::Oracle;
                    j.records[0].draft.states.push(TrialState::EnvironmentReady);
                })
                .unwrap();
            rig.world
                .lock()
                .unwrap()
                .active_execs
                .insert("alpha-s0-r0".into());
            rig.with_lifecycle(&store, RunnerMode::Oracle, None, |l| {
                if cancel {
                    l.cancel_run().map(|_| ())
                } else {
                    l.run_slot(&rig.trials[0]).map(|_| ())
                }
            })
            .unwrap();
            let events = rig.events();
            let settle = events
                .iter()
                .position(|e| e == "settle alpha-s0-r0")
                .unwrap();
            let collect = events
                .iter()
                .position(|e| e == "collect alpha-s0-r0")
                .unwrap();
            let cleanup = events
                .iter()
                .position(|e| e == "cleanup alpha-s0-r0")
                .unwrap();
            assert!(
                settle < collect && collect < cleanup,
                "{step:?} cancel={cancel}: {events:?}"
            );
            assert!(store.snapshot().records[0].draft.collection.collected);
        }
    }
}

#[test]
fn failed_or_late_settlement_skips_evidence_collection_and_still_removes_resources() {
    for cancel in [false, true] {
        for late in [false, true] {
            let rig = Rig::new(&["alpha"]);
            let store = rig.open(Faults::default()).unwrap();
            seed(&store, &rig.trials[0], Step::Verifying, false, true);
            store
                .update(|j| {
                    j.records[0].draft.mode = RunnerMode::Oracle;
                    j.records[0].draft.states.push(TrialState::EnvironmentReady);
                })
                .unwrap();
            {
                let mut world = rig.world.lock().unwrap();
                world.settle_fails = !late;
                world.settle_secs = if late {
                    rsi_common::bench_runner::CANCEL_SETTLE_TIMEOUT_SEC + 1
                } else {
                    0
                };
                world.active_execs.insert("alpha-s0-r0".into());
            }
            rig.with_lifecycle(&store, RunnerMode::Oracle, None, |l| {
                if cancel {
                    l.cancel_run().map(|_| ())
                } else {
                    l.run_slot(&rig.trials[0]).map(|_| ())
                }
            })
            .unwrap();
            let record = store.snapshot().records[0].clone();
            assert!(record.draft.collection.attempted && !record.draft.collection.collected);
            assert!(
                record
                    .draft
                    .collection
                    .error
                    .as_deref()
                    .unwrap()
                    .contains("exec settlement")
            );
            assert!(record.draft.cleanup.cleaned);
            assert_eq!(rig.count("collect", "alpha-s0-r0"), 0);
            assert_eq!(rig.count("cleanup", "alpha-s0-r0"), 1);
        }
    }
}

#[test]
fn a_cancelled_run_stops_at_the_next_step_then_tears_down_without_a_replacement() {
    let rig = Rig::new(&["alpha"]);
    rig.world.lock().unwrap().cancel_on_oracle =
        Some(("alpha-s0-r0".into(), Arc::clone(&rig.cancel)));
    let store = rig.open(Faults::default()).unwrap();
    let result = rig.with_lifecycle(&store, RunnerMode::Oracle, None, |l| {
        l.run_slot(&rig.trials[0])
    });
    assert!(
        matches!(result, Err(LifecycleError::Halted(_))),
        "{result:?}"
    );
    // Nothing ran after the cancel: no verifier, no collection, no cleanup by the trial thread.
    assert_eq!(rig.count("verify", "alpha-s0-r0"), 0);
    assert_eq!(rig.count("cleanup", "alpha-s0-r0"), 0);
    let report = rig
        .with_lifecycle(&store, RunnerMode::Oracle, None, |l| l.cancel_run())
        .unwrap();
    assert_eq!(report.torn_down.len(), 1);
    let journal = store.snapshot();
    assert!(journal.cancelled);
    assert_eq!(
        journal.slot("alpha", 0).len(),
        1,
        "cancellation is never replaced"
    );
    let record = last_of(&journal, "alpha");
    assert!(record.is_terminal() && !record.draft.reward_valid);
    assert_eq!(
        record.draft.exceptions.last().unwrap().kind,
        TrialExceptionKind::Cancelled
    );
    assert!(record.draft.cleanup.cleaned && record.draft.collection.collected);
    // Evidence was collected before the environment was removed.
    let events = rig.events();
    let collect = events
        .iter()
        .position(|e| e == "collect alpha-s0-r0")
        .unwrap();
    let cleanup = events
        .iter()
        .position(|e| e == "cleanup alpha-s0-r0")
        .unwrap();
    assert!(collect < cleanup);
    assert!(rig.world.lock().unwrap().alive.is_empty());
}

fn status(trial: &str, state: BenchTaskState, cost: f64) -> BenchTaskStatusV1 {
    BenchTaskStatusV1 {
        run_id: "run-1".into(),
        task: "alpha".into(),
        trial: trial.into(),
        attempt: 1,
        state,
        stop_reason: None,
        session_id: Some("sess-1".into()),
        usage: BenchUsage {
            cost_usd: Some(cost),
            prompt_tokens: 100,
            cached_tokens: 0,
            output_tokens: 10,
            reasoning_tokens: 0,
            wall_ms: 1000,
            calls: 2,
        },
        route_verdict: None,
        route_violations: Vec::new(),
        harness_build_digest: None,
        trace: None,
    }
}

fn seed(store: &JournalStore, planned: &PlannedTrial, step: Step, daemon: bool, container: bool) {
    let mut record = TrialRecord::new(planned.clone(), RunnerMode::Harness, 1_000_000);
    record.step = step;
    if container {
        record.env = Some(EnvFacts {
            project: trial_key(&planned.identity),
            container: Some("c1".into()),
            dirs_created: true,
            compose_attempted: true,
            collected: false,
        });
    }
    if daemon {
        record.daemon = Some(super::journal::DaemonTrial {
            trial: trial_key(&planned.identity),
            last_status: None,
            cancel_requested: false,
        });
    }
    store.update(|j| j.records.push(record)).unwrap();
}

#[test]
fn cancelling_a_run_cancels_every_live_session_before_any_container_is_torn_down() {
    let rig = Rig::new(&["alpha", "beta", "gamma"]);
    let store = rig.open(Faults::default()).unwrap();
    // alpha: running with a known session. beta: the Start reply was lost (no daemon record) but
    // its agent step was journaled. gamma: never started.
    seed(&store, &rig.trials[0], Step::AgentRunning, true, true);
    seed(&store, &rig.trials[1], Step::AgentRunning, false, true);
    seed(&store, &rig.trials[2], Step::Prepared, false, false);
    for key in ["alpha-s0-r0", "beta-s0-r0"] {
        rig.world.lock().unwrap().alive.insert(key.into());
    }
    let sessions = Sessions {
        world: Some(Arc::clone(&rig.world)),
        ..Sessions::default()
    };
    let report = rig
        .with_lifecycle(&store, RunnerMode::Harness, Some(&sessions), |l| {
            l.cancel_run()
        })
        .unwrap();
    let cancels = sessions.log.lock().unwrap().clone();
    assert_eq!(
        cancels,
        ["session_cancel alpha-s0-r0", "session_cancel beta-s0-r0"],
        "an unknown Start outcome is cancelled by its journaled trial id"
    );
    assert_eq!(report.cancelled_sessions.len(), 2);
    assert_eq!(report.torn_down.len(), 3);
    // Ordering: every session cancel was issued before the first collection or teardown.
    let events = rig.events();
    let last_cancel = events
        .iter()
        .rposition(|e| e.starts_with("session_cancel"))
        .unwrap();
    let first_teardown = events
        .iter()
        .position(|e| e.starts_with("collect") || e.starts_with("cleanup"))
        .unwrap();
    assert!(last_cancel < first_teardown, "{events:?}");
    let journal = store.snapshot();
    assert!(journal.cancelled);
    for task in ["alpha", "beta", "gamma"] {
        let record = last_of(&journal, task);
        assert!(record.is_terminal());
        assert_eq!(
            record.draft.exceptions.last().unwrap().kind,
            TrialExceptionKind::Cancelled
        );
    }
    // gamma never touched Docker.
    assert_eq!(rig.count("cleanup", "gamma-s0-r0"), 0);
    assert_eq!(rig.count("cleanup", "alpha-s0-r0"), 1);
    assert_eq!(rig.count("cleanup", "beta-s0-r0"), 1);
    assert!(rig.world.lock().unwrap().alive.is_empty());
}

#[test]
fn a_session_that_cannot_be_cancelled_is_reported_and_its_container_is_still_removed() {
    let rig = Rig::new(&["alpha"]);
    let store = rig.open(Faults::default()).unwrap();
    seed(&store, &rig.trials[0], Step::AgentRunning, true, true);
    rig.world.lock().unwrap().alive.insert("alpha-s0-r0".into());
    let sessions = Sessions {
        cancel_fails: true,
        ..Sessions::default()
    };
    let report = rig
        .with_lifecycle(&store, RunnerMode::Harness, Some(&sessions), |l| {
            l.cancel_run()
        })
        .unwrap();
    assert!(report.cancelled_sessions.is_empty());
    assert_eq!(report.session_errors.len(), 1);
    assert!(report.session_errors[0].contains("daemon unreachable"));
    assert_eq!(rig.count("cleanup", "alpha-s0-r0"), 1);
}

#[test]
fn a_valid_reward_already_read_survives_a_later_cancellation() {
    let rig = Rig::new(&["alpha"]);
    let store = rig.open(Faults::default()).unwrap();
    seed(&store, &rig.trials[0], Step::VerifyDone, false, true);
    store
        .update(|j| {
            let r = &mut j.records[0];
            r.draft.reward = Some(1.0);
            r.draft.reward_valid = true;
        })
        .unwrap();
    rig.world.lock().unwrap().alive.insert("alpha-s0-r0".into());
    rig.with_lifecycle(&store, RunnerMode::Oracle, None, |l| l.cancel_run())
        .unwrap();
    let record = last_of(&store.snapshot(), "alpha").clone();
    assert!(record.draft.passed(), "{:?}", record.draft.exceptions);
    assert!(record.draft.exceptions.is_empty());
    assert!(record.draft.cleanup.cleaned);
}

#[test]
fn a_slot_not_started_when_the_run_is_cancelled_becomes_an_explicit_not_run_row() {
    let rig = Rig::new(&["alpha"]);
    let store = rig.open(Faults::default()).unwrap();
    rig.cancel.set();
    let result = rig
        .with_lifecycle(&store, RunnerMode::Oracle, None, |l| {
            l.run_slot(&rig.trials[0])
        })
        .unwrap();
    assert_eq!(result.attempts.len(), 1);
    let outcome = &result.attempts[0];
    assert!(!outcome.reward_valid);
    assert_eq!(
        outcome.exceptions.last().unwrap().kind,
        TrialExceptionKind::Cancelled
    );
    assert_eq!(rig.events().len(), 0, "nothing was started");
}

// ---- Harness reconciliation (the daemon's view) --------------------------------------------------

#[test]
fn a_restarted_runner_reconciles_a_harness_attempt_with_the_daemon_on_the_original_clock() {
    let mut record = TrialRecord::new(trial("alpha", 0, 0), RunnerMode::Harness, 0);
    record.step = Step::AgentRunning;
    record.deadlines.agent_started_ms = Some(1_000_000);
    record.deadlines.agent_deadline_ms = Some(1_900_000);
    let id = "alpha-s0-r0";
    let live = status(id, BenchTaskState::Running, 0.5);
    // No such trial at the daemon: Start was never delivered. Nothing was spent.
    assert_eq!(
        reconcile_harness(&record, SessionView::NoSession, 1_500_000),
        HarnessRecovery::RestartAttempt
    );
    // Live inside the original window: keep polling against the JOURNALED deadline - a restart
    // never extends it.
    assert_eq!(
        reconcile_harness(&record, SessionView::Live(live.clone()), 1_500_000),
        HarnessRecovery::Reconnect {
            deadline_ms: 1_900_000
        }
    );
    // Live past it: cancel and settle; the late session is never given more time.
    assert_eq!(
        reconcile_harness(&record, SessionView::Live(live.clone()), 1_900_000),
        HarnessRecovery::CancelAndSettle
    );
    assert_eq!(
        reconcile_harness(&record, SessionView::Live(live.clone()), 9_000_000),
        HarnessRecovery::CancelAndSettle
    );
    // No journaled deadline is treated as passed.
    let mut no_clock = record.clone();
    no_clock.deadlines = super::journal::Deadlines::default();
    assert_eq!(
        reconcile_harness(&no_clock, SessionView::Live(live.clone()), 0),
        HarnessRecovery::CancelAndSettle
    );
    // Settled by the daemon: take Finish and the trace.
    let finished = status(id, BenchTaskState::Finished, 1.25);
    assert_eq!(
        reconcile_harness(&record, SessionView::Settled(finished.clone()), 9_000_000),
        HarnessRecovery::Settled(finished)
    );
    // A daemon restart is not a resumable session: the row and its cost are kept as an invalid
    // infrastructure attempt.
    let interrupted = status(id, BenchTaskState::InfraInterrupted, 0.75);
    assert_eq!(
        reconcile_harness(
            &record,
            SessionView::Settled(interrupted.clone()),
            1_500_000
        ),
        HarnessRecovery::InfraInvalid(interrupted.clone())
    );
    assert_eq!(
        reconcile_harness(&record, SessionView::Live(interrupted.clone()), 1_500_000),
        HarnessRecovery::InfraInvalid(interrupted)
    );
}

#[test]
fn the_daemons_status_with_its_cost_is_journaled_before_the_runner_acts_on_it() {
    let rig = Rig::new(&["alpha"]);
    let store = rig.open(Faults::default()).unwrap();
    seed(&store, &rig.trials[0], Step::AgentRunning, false, true);
    let sessions = Sessions::default();
    let interrupted = status("alpha-s0-r0", BenchTaskState::InfraInterrupted, 0.75);
    sessions.views.lock().unwrap().insert(
        "alpha-s0-r0".into(),
        SessionView::Settled(interrupted.clone()),
    );
    let recovery = rig
        .with_lifecycle(&store, RunnerMode::Harness, Some(&sessions), |l| {
            l.reconcile(&rig.trials[0].identity)
        })
        .unwrap();
    assert_eq!(recovery, HarnessRecovery::InfraInvalid(interrupted.clone()));
    let kept = last_of(&store.snapshot(), "alpha").daemon.clone().unwrap();
    assert_eq!(
        kept.last_status,
        Some(interrupted),
        "interrupted cost survives in the journal"
    );
}

// ---- artifacts -----------------------------------------------------------------------------------

fn tree(root: &Path) {
    std::fs::create_dir_all(root.join("agent")).unwrap();
    std::fs::create_dir_all(root.join("verifier")).unwrap();
    std::fs::write(root.join("agent/oracle.txt"), "oracle log").unwrap();
    std::fs::write(root.join("verifier/test-stdout.txt"), "tests ok").unwrap();
    std::fs::write(root.join("verifier/reward.txt"), "1.0").unwrap();
}

#[cfg(unix)]
#[test]
fn a_manifest_lists_regular_files_by_digest_and_never_follows_planted_links_or_special_files() {
    use std::os::unix::{fs::symlink, net::UnixListener};
    let secrets = tempfile::tempdir().unwrap();
    let secret = secrets.path().join("secret.txt");
    std::fs::write(&secret, "hub secret").unwrap();
    let evidence = tempfile::tempdir().unwrap();
    let root = evidence.path();
    tree(root);
    symlink(&secret, root.join("agent/link-to-secret")).unwrap();
    symlink(secrets.path(), root.join("agent/link-to-dir")).unwrap();
    symlink("../../../etc", root.join("verifier/up")).unwrap();
    let _socket = UnixListener::bind(root.join("agent/sock")).unwrap();

    let manifest = build_manifest(root, Limits::default()).unwrap();
    let files: Vec<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        files,
        [
            "agent/oracle.txt",
            "verifier/reward.txt",
            "verifier/test-stdout.txt"
        ]
    );
    let secret_digest = super::journal::sha256_of(b"hub secret");
    let hex = secret_digest.trim_start_matches("sha256:");
    assert!(
        manifest.files.iter().all(|f| f.sha256 != hex),
        "the file behind a symlink was never read"
    );
    let skipped: BTreeMap<&str, SkipReason> = manifest
        .skipped
        .iter()
        .map(|s| (s.path.as_str(), s.reason))
        .collect();
    assert_eq!(skipped["agent/link-to-secret"], SkipReason::Symlink);
    assert_eq!(skipped["agent/link-to-dir"], SkipReason::Symlink);
    assert_eq!(skipped["verifier/up"], SkipReason::Symlink);
    assert_eq!(skipped["agent/sock"], SkipReason::SpecialFile);
    assert!(!manifest.truncated);
    assert_eq!(
        manifest.total_bytes,
        ("oracle log".len() + "tests ok".len() + "1.0".len()) as u64
    );
}

#[cfg(unix)]
#[test]
fn a_root_that_is_a_symlink_is_never_walked() {
    let real = tempfile::tempdir().unwrap();
    tree(real.path());
    let holder = tempfile::tempdir().unwrap();
    let link = holder.path().join("logs");
    std::os::unix::fs::symlink(real.path(), &link).unwrap();
    assert!(build_manifest(&link, Limits::default()).is_err());
    assert!(build_manifest(&real.path().join("agent/oracle.txt"), Limits::default()).is_err());
    assert!(build_manifest(&real.path().join("missing"), Limits::default()).is_err());
}

#[cfg(unix)]
#[test]
fn unsafe_names_oversize_files_depth_and_counts_are_bounded_and_reported() {
    use std::os::unix::ffi::OsStrExt;
    let evidence = tempfile::tempdir().unwrap();
    let root = evidence.path();
    tree(root);
    // A name that is not UTF-8, and one with a control character.
    std::fs::write(
        root.join(std::ffi::OsStr::from_bytes(b"bad-\xff-name")),
        "x",
    )
    .unwrap();
    std::fs::write(root.join("ctl\u{7}name"), "x").unwrap();
    std::fs::write(root.join("big.bin"), vec![0_u8; 4096]).unwrap();
    let mut deep = root.join("d");
    for _ in 0..6 {
        deep = deep.join("d");
    }
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("far.txt"), "far").unwrap();

    let limits = Limits {
        max_file_bytes: 1024,
        max_depth: 3,
        ..Limits::default()
    };
    let manifest = build_manifest(root, limits).unwrap();
    let reasons: BTreeSet<SkipReason> = manifest.skipped.iter().map(|s| s.reason).collect();
    assert!(reasons.contains(&SkipReason::UnsafeName));
    assert!(reasons.contains(&SkipReason::OversizeFile));
    assert!(reasons.contains(&SkipReason::TooDeep));
    assert!(
        manifest
            .files
            .iter()
            .all(|f| f.path != "big.bin" && !f.path.ends_with("far.txt"))
    );
    assert!(manifest.files.iter().all(|f| is_safe_relative(&f.path)));

    // A file count past the limit stops the walk and says so.
    let capped = build_manifest(
        root,
        Limits {
            max_files: 2,
            ..Limits::default()
        },
    )
    .unwrap();
    assert_eq!(capped.files.len(), 2);
    assert!(capped.truncated);
    let by_size = build_manifest(
        root,
        Limits {
            max_total_bytes: 10,
            ..Limits::default()
        },
    )
    .unwrap();
    assert!(by_size.truncated && by_size.total_bytes <= 10);
}

#[cfg(unix)]
#[test]
fn verification_detects_changed_removed_and_replaced_by_link_evidence() {
    let evidence = tempfile::tempdir().unwrap();
    let root = evidence.path();
    tree(root);
    let manifest = build_manifest(root, Limits::default()).unwrap();
    assert!(verify_manifest(root, &manifest).is_empty());

    std::fs::write(root.join("agent/oracle.txt"), "tampered").unwrap();
    std::fs::remove_file(root.join("verifier/reward.txt")).unwrap();
    std::fs::remove_file(root.join("verifier/test-stdout.txt")).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", root.join("verifier/test-stdout.txt")).unwrap();
    let problems = verify_manifest(root, &manifest);
    assert_eq!(problems.len(), 3, "{problems:?}");
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("agent/oracle.txt") && p.contains("changed"))
    );
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("verifier/reward.txt") && p.contains("missing"))
    );
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("verifier/test-stdout.txt") && p.contains("regular file"))
    );

    // A manifest that names a path outside the evidence root is not trusted.
    let mut hostile = ArtifactManifest::default();
    hostile.files.push(super::artifacts::ArtifactEntry {
        path: "../outside".into(),
        bytes: 0,
        sha256: String::new(),
    });
    assert!(verify_manifest(root, &hostile)[0].contains("unsafe path"));
}

#[test]
fn collection_writes_a_manifest_beside_the_outcome_and_the_journal_summarises_it() {
    let rig = Rig::new(&["alpha"]);
    let (_, journal) = rig.drive_all(Faults::default());
    let record = last_of(&journal, "alpha");
    let summary = record
        .manifest
        .clone()
        .expect("a manifest summary is journaled");
    assert_eq!(summary.files, 1);
    assert_eq!(summary.skipped, 0);
    assert!(!summary.truncated);
    let work = rig.work_dir(&record.planned);
    assert_eq!(summary.path, work.join(MANIFEST_FILE).display().to_string());
    let manifest: ArtifactManifest =
        serde_json::from_slice(&std::fs::read(work.join(MANIFEST_FILE)).unwrap()).unwrap();
    assert_eq!(manifest.files[0].path, "agent/oracle.txt");
    assert!(verify_manifest(&work.join("logs"), &manifest).is_empty());
}

// ---- the ownership of the run by one process ------------------------------------------------------

#[test]
fn two_lifecycles_over_one_journal_never_duplicate_a_slot() {
    let rig = Rig::new(&["alpha"]);
    let store = rig.open(Faults::default()).unwrap();
    let again = rig.with_lifecycle(&store, RunnerMode::Oracle, None, |l| {
        l.run_slot(&rig.trials[0]).unwrap();
        l.run_slot(&rig.trials[0]).unwrap()
    });
    assert_eq!(again.attempts.len(), 1);
    assert_eq!(rig.count("start", "alpha-s0-r0"), 1);
    assert_eq!(store.snapshot().records.len(), 1);
}
