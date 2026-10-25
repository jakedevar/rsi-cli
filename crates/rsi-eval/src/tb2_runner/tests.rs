//! Source-derived tests of the oracle core (design #3628 "Comparability and validation"): reward
//! precedence and errors, oracle nonzero handling, independent clocks, timeouts, collection and
//! cleanup. A fake environment and a fake clock stand in for Docker and time; the fake model
//! port panics if called. No remote or paid execution.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::{
    planned_trial,
    reward::{REWARD_JSON_PATH, REWARD_TEXT_PATH, Reward, RewardError, read_reward},
    trial::{
        Clock, EnvError, ExecSpec, ModelPort, ORACLE_LOG_PATH, SOLUTION_DIR, TESTS_DIR,
        TrialEnvironment, TrialInputs, VERIFIER_LOG_PATH, run_trial,
    },
    whole_seconds,
};
use rsi_common::bench_runner::{
    PlannedTrial, RunnerMode, TimeoutPolicy, TrialExceptionKind, TrialIdentity, TrialOutcomeV1,
    TrialState,
};
use std::{cell::Cell, collections::BTreeMap, path::Path, rc::Rc, time::Duration};

const fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

// ---- rewards -------------------------------------------------------------------------------

fn json(text: &str) -> Result<Reward, RewardError> {
    read_reward(Some(text.as_bytes()), None)
}
fn text(text: &str) -> Result<Reward, RewardError> {
    read_reward(None, Some(text.as_bytes()))
}

#[test]
fn reward_json_takes_precedence_over_text() {
    let reward = read_reward(Some(br#"{"reward": 0.0}"#), Some(b"1.0")).unwrap();
    assert_eq!(reward.scalar, Some(0.0));
}

#[test]
fn present_but_invalid_json_blocks_the_text_fallback() {
    for bad in [
        "",
        "not json",
        "[1]",
        "{\"reward\": \"1\"}",
        "{\"reward\": null}",
    ] {
        let got = read_reward(Some(bad.as_bytes()), Some(b"1.0"));
        assert!(
            matches!(got, Err(RewardError::Parse(_))),
            "{bad:?}: {got:?}"
        );
    }
}

#[test]
fn reward_text_is_one_finite_float_with_surrounding_whitespace() {
    assert_eq!(text(" \n1.0\t\n").unwrap().scalar, Some(1.0));
    assert_eq!(text("0").unwrap().map["reward"], 0.0);
    assert_eq!(text("1e-1").unwrap().scalar, Some(0.1));
    assert_eq!(text(" \n").unwrap_err(), RewardError::EmptyText);
    assert_eq!(text("").unwrap_err(), RewardError::EmptyText);
    assert!(matches!(text("pass").unwrap_err(), RewardError::Parse(_)));
    assert!(matches!(text("1 2").unwrap_err(), RewardError::Parse(_)));
    for bad in ["nan", "NaN", "inf", "-inf", "infinity"] {
        assert!(
            matches!(text(bad).unwrap_err(), RewardError::NonFinite(_)),
            "{bad}"
        );
    }
    assert!(matches!(
        read_reward(None, Some(&[0xff, 0xfe])).unwrap_err(),
        RewardError::Parse(_)
    ));
}

#[test]
fn reward_json_is_a_map_of_finite_numbers_with_bools_normalized() {
    let reward = json(r#"{"reward": 0.5, "speed": 2, "ok": true, "bad": false}"#).unwrap();
    assert_eq!(reward.scalar, Some(0.5));
    assert_eq!(reward.map["speed"], 2.0);
    assert_eq!(reward.map["ok"], 1.0);
    assert_eq!(reward.map["bad"], 0.0);
}

#[test]
fn the_scalar_is_reward_else_the_lexicographically_first_value() {
    assert_eq!(
        json(r#"{"b": 2, "a": 7, "reward": 0}"#).unwrap().scalar,
        Some(0.0)
    );
    assert_eq!(json(r#"{"b": 2, "a": 7}"#).unwrap().scalar, Some(7.0));
    assert_eq!(json(r#"{"x": true}"#).unwrap().scalar, Some(1.0));
}

#[test]
fn an_empty_json_map_is_accepted_with_no_scalar() {
    let reward = json("{}").unwrap();
    assert!(reward.map.is_empty());
    assert_eq!(reward.scalar, None);
}

#[test]
fn json_values_must_be_finite_numbers() {
    assert!(matches!(
        json(r#"{"reward": "1"}"#).unwrap_err(),
        RewardError::Parse(_)
    ));
    assert!(matches!(
        json(r#"{"reward": [1]}"#).unwrap_err(),
        RewardError::Parse(_)
    ));
    // Out of f64 range is not a finite number: serde_json refuses it.
    assert!(json(r#"{"reward": 1e999}"#).is_err());
}

#[test]
fn no_reward_file_is_missing_not_a_zero() {
    assert_eq!(read_reward(None, None).unwrap_err(), RewardError::Missing);
    assert_eq!(
        RewardError::Missing.kind(),
        TrialExceptionKind::RewardMissing
    );
    assert_eq!(
        RewardError::EmptyText.kind(),
        TrialExceptionKind::RewardEmpty
    );
    assert_eq!(
        RewardError::NonFinite(String::new()).kind(),
        TrialExceptionKind::RewardNonFinite
    );
    assert_eq!(
        RewardError::Parse(String::new()).kind(),
        TrialExceptionKind::RewardParse
    );
}

// ---- trial fixtures ------------------------------------------------------------------------

#[derive(Clone)]
struct FakeClock(Rc<Cell<Duration>>);
impl Clock for FakeClock {
    fn now(&self) -> Duration {
        self.0.get()
    }
}

/// A scripted exec: matches when the script contains `needle`.
struct ExecRule {
    needle: &'static str,
    cost: Duration,
    result: Result<i32, EnvError>,
}

struct Fake {
    clock: Rc<Cell<Duration>>,
    log: Vec<String>,
    timeouts: Vec<(String, Duration)>,
    enforce: bool,
    start: (Duration, Result<(), EnvError>),
    upload_errors: Vec<(&'static str, EnvError)>,
    execs: Vec<ExecRule>,
    files: BTreeMap<String, (Duration, Vec<u8>)>,
    settle: Result<(), EnvError>,
    collect: Result<(), EnvError>,
    cleanup: Result<(), EnvError>,
    /// Time cleanup takes; it is spent even past the deadline (an environment that overruns and
    /// still reports success).
    cleanup_cost: Duration,
}

impl Fake {
    fn new(clock: &Rc<Cell<Duration>>) -> Self {
        Self {
            clock: clock.clone(),
            log: Vec::new(),
            timeouts: Vec::new(),
            enforce: true,
            start: (secs(1), Ok(())),
            upload_errors: Vec::new(),
            execs: Vec::new(),
            files: BTreeMap::new(),
            settle: Ok(()),
            collect: Ok(()),
            cleanup: Ok(()),
            cleanup_cost: Duration::ZERO,
        }
    }

    fn reward(mut self, json: &str) -> Self {
        self.files
            .insert(REWARD_JSON_PATH.into(), (secs(0), json.as_bytes().to_vec()));
        self
    }

    fn exec_rule(
        mut self,
        needle: &'static str,
        cost: Duration,
        result: Result<i32, EnvError>,
    ) -> Self {
        self.execs.push(ExecRule {
            needle,
            cost,
            result,
        });
        self
    }

    /// Spend `cost`, or the whole `limit` and fail when the operation would overrun it.
    fn spend(&mut self, name: &str, cost: Duration, limit: Duration) -> Result<(), EnvError> {
        self.timeouts.push((name.to_string(), limit));
        if self.enforce && cost > limit {
            self.clock.set(self.clock.get() + limit);
            return Err(EnvError::Timeout);
        }
        self.clock.set(self.clock.get() + cost);
        Ok(())
    }
}

impl TrialEnvironment for Fake {
    fn start(&mut self, timeout: Duration) -> Result<(), EnvError> {
        self.log.push("start".into());
        self.spend("start", self.start.0, timeout)?;
        self.start.1.clone()
    }
    fn upload_dir(
        &mut self,
        local: &Path,
        remote: &str,
        timeout: Duration,
    ) -> Result<(), EnvError> {
        self.log
            .push(format!("upload {} -> {remote}", local.display()));
        self.spend("upload", secs(1), timeout)?;
        match self.upload_errors.iter().find(|(r, _)| *r == remote) {
            Some((_, error)) => Err(error.clone()),
            None => Ok(()),
        }
    }
    fn exec(&mut self, spec: &ExecSpec<'_>) -> Result<i32, EnvError> {
        self.log.push(format!(
            "exec user={} env={:?}: {}",
            spec.user.unwrap_or("-"),
            spec.env,
            spec.script
        ));
        let rule = self
            .execs
            .iter()
            .position(|r| spec.script.contains(r.needle));
        let (cost, result) = rule.map_or((secs(1), Ok(0)), |i| {
            (self.execs[i].cost, self.execs[i].result.clone())
        });
        self.spend("exec", cost, spec.timeout)?;
        result
    }
    fn read_file(&mut self, remote: &str, timeout: Duration) -> Result<Option<Vec<u8>>, EnvError> {
        self.log.push(format!("read {remote}"));
        let (cost, bytes) = self
            .files
            .get(remote)
            .cloned()
            .map_or((secs(0), None), |(c, b)| (c, Some(b)));
        self.spend("read", cost, timeout)?;
        Ok(bytes)
    }
    fn settle_execs(&mut self, timeout: Duration) -> Result<(), EnvError> {
        self.log.push("settle".into());
        self.spend("settle", secs(1), timeout)?;
        self.settle.clone()
    }
    fn collect(&mut self, _dest: &Path, timeout: Duration) -> Result<(), EnvError> {
        self.log.push("collect".into());
        self.spend("collect", secs(2), timeout)?;
        self.collect.clone()
    }
    fn cleanup(&mut self, _timeout: Duration) -> Result<(), EnvError> {
        self.log.push("cleanup".into());
        self.clock.set(self.clock.get() + self.cleanup_cost);
        self.cleanup.clone()
    }
}

struct PanicModel;
impl ModelPort for PanicModel {
    fn start_session(&mut self, _trial: &PlannedTrial) -> Result<String, String> {
        panic!("the oracle must never open a model session");
    }
}

fn planned() -> PlannedTrial {
    PlannedTrial {
        identity: TrialIdentity {
            run_id: "run-1".into(),
            task: "fixture-task".into(),
            slot: 0,
            retry: 0,
        },
        image: "example/fixture:1".into(),
        image_digest: format!("sha256:{}", "b".repeat(64)),
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

fn drive(fake: &mut Fake, clock: &Rc<Cell<Duration>>, trial: &PlannedTrial) -> TrialOutcomeV1 {
    run_trial(
        fake,
        &FakeClock(clock.clone()),
        &mut PanicModel,
        RunnerMode::Oracle,
        &TimeoutPolicy::default(),
        trial,
        &TrialInputs {
            solution_dir: Path::new("/hub/solution"),
            tests_dir: Path::new("/hub/tests"),
            collect_dir: Path::new("/hub/out"),
        },
    )
}

fn passing(clock: &Rc<Cell<Duration>>) -> Fake {
    Fake::new(clock).reward(r#"{"reward": 1.0}"#)
}

fn kinds(outcome: &TrialOutcomeV1) -> Vec<TrialExceptionKind> {
    outcome.exceptions.iter().map(|e| e.kind).collect()
}

// ---- the happy path ------------------------------------------------------------------------

#[test]
fn an_oracle_trial_runs_every_step_in_order_and_never_touches_the_model() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = passing(&clock);
    let outcome = drive(&mut fake, &clock, &planned());

    assert_eq!(
        fake.log,
        vec![
            "start".to_string(),
            format!("upload /hub/solution -> {SOLUTION_DIR}"),
            "exec user=root env={}: chmod +x /solution/solve.sh".to_string(),
            format!(
                "exec user=- env={{\"DEBIAN_FRONTEND\": \"noninteractive\"}}: (/solution/solve.sh) > {ORACLE_LOG_PATH} 2>&1"
            ),
            format!("upload /hub/tests -> {TESTS_DIR}"),
            "exec user=root env={}: chmod +x /tests/test.sh".to_string(),
            format!("exec user=- env={{}}: (/tests/test.sh) > {VERIFIER_LOG_PATH} 2>&1"),
            format!("read {REWARD_JSON_PATH}"),
            format!("read {REWARD_TEXT_PATH}"),
            "settle".to_string(),
            "collect".to_string(),
            "cleanup".to_string(),
        ]
    );
    assert!(outcome.reward_valid);
    assert_eq!(outcome.reward, Some(1.0));
    assert!(outcome.passed());
    assert_eq!(outcome.oracle_exit_code, Some(0));
    assert_eq!(outcome.verifier_exit_code, Some(0));
    assert!(outcome.exceptions.is_empty());
    assert_eq!(outcome.model_sessions, 0);
    assert!(outcome.collection.collected);
    assert!(outcome.cleanup.cleaned);
    assert_eq!(
        outcome.states,
        vec![
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
}

#[test]
fn the_solution_environment_wins_over_the_noninteractive_default() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = passing(&clock);
    let mut trial = planned();
    trial
        .solution_env
        .insert("DEBIAN_FRONTEND".into(), "dialog".into());
    trial.solution_env.insert("X".into(), "1".into());
    trial.agent_user = Some("agent".into());
    trial.verifier_user = Some("grader".into());
    trial.verifier_env.insert("V".into(), "2".into());
    drive(&mut fake, &clock, &trial);
    assert!(fake.log.iter().any(|l| l
        == &format!(
            "exec user=agent env={{\"DEBIAN_FRONTEND\": \"dialog\", \"X\": \"1\"}}: (/solution/solve.sh) > {ORACLE_LOG_PATH} 2>&1"
        )), "{:#?}", fake.log);
    assert!(fake.log.iter().any(|l| l
        == &format!(
            "exec user=grader env={{\"V\": \"2\"}}: (/tests/test.sh) > {VERIFIER_LOG_PATH} 2>&1"
        )));
}

#[test]
fn a_nonzero_oracle_exit_is_recorded_and_the_verifier_still_runs() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = passing(&clock).exec_rule("solve.sh) >", secs(5), Ok(3));
    let outcome = drive(&mut fake, &clock, &planned());
    assert_eq!(outcome.oracle_exit_code, Some(3));
    assert!(outcome.exceptions.is_empty());
    assert!(outcome.reward_valid);
    assert!(fake.log.iter().any(|l| l.contains("/tests/test.sh) >")));
}

#[test]
fn a_nonzero_verifier_exit_does_not_decide_the_grade() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = passing(&clock).exec_rule("test.sh) >", secs(5), Ok(1));
    let outcome = drive(&mut fake, &clock, &planned());
    assert_eq!(outcome.verifier_exit_code, Some(1));
    assert!(outcome.reward_valid);
    assert!(outcome.passed());
}

#[test]
fn a_zero_reward_is_a_valid_failed_trial() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = Fake::new(&clock).reward(r#"{"reward": 0}"#);
    let outcome = drive(&mut fake, &clock, &planned());
    assert!(outcome.reward_valid);
    assert_eq!(outcome.reward, Some(0.0));
    assert!(!outcome.passed());
}

#[test]
fn reward_text_is_used_when_there_is_no_json() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = Fake::new(&clock);
    fake.files
        .insert(REWARD_TEXT_PATH.into(), (secs(0), b"1\n".to_vec()));
    let outcome = drive(&mut fake, &clock, &planned());
    assert_eq!(outcome.reward, Some(1.0));
    assert_eq!(outcome.reward_map["reward"], 1.0);
}

// ---- independent clocks --------------------------------------------------------------------

#[test]
fn phase_clocks_are_independent() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    // Each phase uses most of its own budget; the total exceeds every single deadline.
    let mut fake = passing(&clock).exec_rule("solve.sh) >", secs(850), Ok(0));
    fake.start.0 = secs(590);
    fake.files.insert(
        REWARD_JSON_PATH.into(),
        (secs(560), br#"{"reward":1}"#.to_vec()),
    );
    let outcome = drive(&mut fake, &clock, &planned());
    assert!(outcome.reward_valid, "{:?}", outcome.exceptions);
    assert!(clock.get() > secs(900));
    // The verifier saw the whole verifier deadline minus only its own spend, not the agent's.
    let first_tests = fake
        .timeouts
        .iter()
        .position(|(n, _)| n == "upload")
        .unwrap();
    let second_upload = fake
        .timeouts
        .iter()
        .enumerate()
        .filter(|(_, (n, _))| n == "upload")
        .nth(1)
        .unwrap();
    assert!(first_tests < second_upload.0);
    assert_eq!(second_upload.1.1, secs(600));
    let phases: Vec<_> = outcome
        .phases
        .iter()
        .map(|p| (p.state, p.deadline_ms))
        .collect();
    assert!(phases.contains(&(TrialState::EnvironmentReady, Some(600_000))));
    assert!(phases.contains(&(TrialState::AgentRunning, Some(900_000))));
    assert!(phases.contains(&(TrialState::Verifying, Some(600_000))));
}

#[test]
fn an_agent_timeout_settles_then_verifies_with_the_full_verifier_deadline() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake =
        Fake::new(&clock)
            .reward(r#"{"reward": 0.0}"#)
            .exec_rule("solve.sh) >", secs(5000), Ok(0));
    let outcome = drive(&mut fake, &clock, &planned());
    assert_eq!(kinds(&outcome), vec![TrialExceptionKind::AgentTimeout]);
    assert!(outcome.oracle_exit_code.is_none());
    let settle = fake.log.iter().position(|l| l == "settle").unwrap();
    let verify = fake.log.iter().position(|l| l.contains("/tests")).unwrap();
    assert!(settle < verify);
    // A verified, valid reward: the agent timeout stays as the stop reason, not the cause.
    assert!(outcome.reward_valid);
    assert!(outcome.invalid_cause().is_none());
    let tests_upload = fake
        .timeouts
        .iter()
        .enumerate()
        .filter(|(_, (n, _))| n == "upload")
        .nth(1)
        .unwrap();
    assert_eq!(tests_upload.1.1, secs(600));
}

#[test]
fn an_agent_phase_that_overruns_its_deadline_without_the_env_noticing_is_a_timeout() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = passing(&clock).exec_rule("solve.sh) >", secs(901), Ok(0));
    fake.enforce = false;
    let outcome = drive(&mut fake, &clock, &planned());
    assert!(kinds(&outcome).contains(&TrialExceptionKind::AgentTimeout));
}

#[test]
fn failed_settlement_blocks_verification_and_collection_but_still_cleans_up() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = passing(&clock).exec_rule("solve.sh) >", secs(5000), Ok(0));
    fake.settle = Err(EnvError::Failed("exec still running".into()));
    let outcome = drive(&mut fake, &clock, &planned());
    assert_eq!(
        kinds(&outcome),
        vec![
            TrialExceptionKind::AgentTimeout,
            TrialExceptionKind::AgentSettleFailed
        ]
    );
    assert!(!outcome.reward_valid);
    assert!(!fake.log.iter().any(|l| l.contains("/tests")));
    assert!(outcome.collection.attempted && !outcome.collection.collected);
    assert!(
        outcome
            .collection
            .error
            .as_deref()
            .unwrap()
            .contains("exec settlement")
    );
    assert!(fake.log.contains(&"cleanup".to_string()));
}

#[test]
fn a_verifier_timeout_never_reads_a_reward_even_if_one_exists() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = passing(&clock).exec_rule("test.sh) >", secs(601), Ok(0));
    let outcome = drive(&mut fake, &clock, &planned());
    assert_eq!(kinds(&outcome), vec![TrialExceptionKind::VerifierTimeout]);
    assert!(!outcome.reward_valid);
    assert_eq!(outcome.reward, None);
    assert!(!fake.log.iter().any(|l| l.starts_with("read ")));
    assert!(fake.log.contains(&"cleanup".to_string()));
}

#[test]
fn a_reward_read_after_the_verifier_deadline_is_not_accepted() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = Fake::new(&clock);
    fake.enforce = false;
    fake.files.insert(
        REWARD_JSON_PATH.into(),
        (secs(700), br#"{"reward":1}"#.to_vec()),
    );
    let outcome = drive(&mut fake, &clock, &planned());
    assert_eq!(kinds(&outcome), vec![TrialExceptionKind::VerifierTimeout]);
    assert!(!outcome.reward_valid);
    assert!(!outcome.passed());
}

#[test]
fn the_environment_start_has_its_own_deadline() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = passing(&clock);
    fake.start.0 = secs(601);
    let outcome = drive(&mut fake, &clock, &planned());
    assert_eq!(
        kinds(&outcome),
        vec![TrialExceptionKind::EnvironmentStartTimeout]
    );
    assert_eq!(fake.log, vec!["start".to_string(), "cleanup".to_string()]);
    assert!(!outcome.collection.attempted);
    assert!(outcome.cleanup.cleaned);
}

// ---- typed failures ------------------------------------------------------------------------

#[test]
fn invalid_rewards_get_a_typed_cause_and_never_a_valid_zero() {
    let cases: [(Option<&str>, Option<&str>, TrialExceptionKind); 5] = [
        (None, None, TrialExceptionKind::RewardMissing),
        (None, Some("   "), TrialExceptionKind::RewardEmpty),
        (Some("{}"), None, TrialExceptionKind::RewardEmpty),
        (Some("{oops"), Some("1"), TrialExceptionKind::RewardParse),
        (None, Some("nan"), TrialExceptionKind::RewardNonFinite),
    ];
    for (json, text, expected) in cases {
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let mut fake = Fake::new(&clock);
        if let Some(j) = json {
            fake.files
                .insert(REWARD_JSON_PATH.into(), (secs(0), j.as_bytes().to_vec()));
        }
        if let Some(t) = text {
            fake.files
                .insert(REWARD_TEXT_PATH.into(), (secs(0), t.as_bytes().to_vec()));
        }
        let outcome = drive(&mut fake, &clock, &planned());
        assert_eq!(kinds(&outcome), vec![expected], "{json:?} {text:?}");
        assert!(!outcome.reward_valid);
        assert_eq!(outcome.reward, None);
        assert!(!outcome.passed());
        assert!(!expected.retryable());
        assert_eq!(outcome.invalid_cause().unwrap().kind, expected);
    }
}

#[test]
fn the_cause_of_an_invalid_reward_after_an_agent_timeout_is_the_verification_failure() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = Fake::new(&clock).exec_rule("solve.sh) >", secs(5000), Ok(0));
    let outcome = drive(&mut fake, &clock, &planned());
    assert_eq!(
        kinds(&outcome),
        vec![
            TrialExceptionKind::AgentTimeout,
            TrialExceptionKind::RewardMissing
        ]
    );
    assert_eq!(
        outcome.invalid_cause().unwrap().kind,
        TrialExceptionKind::RewardMissing
    );
}

#[test]
fn upload_failures_are_typed_and_stop_before_the_next_phase() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = passing(&clock);
    fake.upload_errors
        .push((SOLUTION_DIR, EnvError::Failed("copy failed".into())));
    let outcome = drive(&mut fake, &clock, &planned());
    assert_eq!(
        kinds(&outcome),
        vec![TrialExceptionKind::SolutionUploadFailed]
    );
    assert!(!fake.log.iter().any(|l| l.contains("/tests")));
    assert!(outcome.cleanup.cleaned && outcome.collection.collected);

    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = passing(&clock);
    fake.upload_errors
        .push((TESTS_DIR, EnvError::Failed("copy failed".into())));
    let outcome = drive(&mut fake, &clock, &planned());
    assert_eq!(
        kinds(&outcome),
        vec![TrialExceptionKind::VerifierUploadFailed]
    );
    assert!(!fake.log.iter().any(|l| l.starts_with("read ")));
}

#[test]
fn a_failed_chmod_is_an_upload_failure() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = passing(&clock).exec_rule("chmod +x /tests", secs(1), Ok(1));
    let outcome = drive(&mut fake, &clock, &planned());
    assert_eq!(
        kinds(&outcome),
        vec![TrialExceptionKind::VerifierUploadFailed]
    );
}

#[test]
fn an_infrastructure_interruption_is_the_only_retryable_cause_and_skips_verification() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = passing(&clock).exec_rule(
        "solve.sh) >",
        secs(3),
        Err(EnvError::Infra("ssh connection lost".into())),
    );
    let outcome = drive(&mut fake, &clock, &planned());
    assert_eq!(kinds(&outcome), vec![TrialExceptionKind::InfraInterrupted]);
    assert!(outcome.invalid_cause().unwrap().kind.retryable());
    assert!(!fake.log.iter().any(|l| l.contains("/tests")));
    assert!(fake.log.contains(&"cleanup".to_string()));

    // An arbitrary exec failure is not proof of infrastructure failure.
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = passing(&clock).exec_rule(
        "solve.sh) >",
        secs(3),
        Err(EnvError::Failed("docker exec refused".into())),
    );
    let outcome = drive(&mut fake, &clock, &planned());
    assert_eq!(kinds(&outcome), vec![TrialExceptionKind::AgentExecFailed]);
    assert!(!outcome.exceptions[0].kind.retryable());
}

#[test]
fn a_verifier_interrupted_by_transport_loss_is_infra_invalid() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = passing(&clock).exec_rule(
        "test.sh) >",
        secs(3),
        Err(EnvError::Infra("connection reset".into())),
    );
    let outcome = drive(&mut fake, &clock, &planned());
    assert_eq!(kinds(&outcome), vec![TrialExceptionKind::InfraInterrupted]);
    assert!(!outcome.reward_valid);
}

#[test]
fn a_start_failure_skips_collection_and_still_cleans_up() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = passing(&clock);
    fake.start.1 = Err(EnvError::Failed("compose up failed".into()));
    let outcome = drive(&mut fake, &clock, &planned());
    assert_eq!(
        kinds(&outcome),
        vec![TrialExceptionKind::EnvironmentStartFailed]
    );
    assert_eq!(fake.log, vec!["start".to_string(), "cleanup".to_string()]);
    assert_eq!(outcome.states.last(), Some(&TrialState::Terminal));
}

#[test]
fn collection_and_cleanup_failures_never_change_a_collected_reward() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = passing(&clock);
    fake.collect = Err(EnvError::Failed("scp failed".into()));
    fake.cleanup = Err(EnvError::Failed("compose down failed".into()));
    let outcome = drive(&mut fake, &clock, &planned());
    assert!(outcome.reward_valid && outcome.passed());
    assert!(outcome.exceptions.is_empty());
    assert!(outcome.collection.attempted && !outcome.collection.collected);
    assert_eq!(outcome.collection.error.as_deref(), Some("scp failed"));
    assert!(outcome.cleanup.attempted && !outcome.cleanup.cleaned);
    assert_eq!(
        outcome.cleanup.error.as_deref(),
        Some("compose down failed")
    );
    // Cleanup ran even though collection failed.
    assert_eq!(fake.log.last().map(String::as_str), Some("cleanup"));
}

#[test]
fn a_cleanup_that_overruns_its_deadline_is_not_cleaned_and_never_changes_the_grade() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = passing(&clock);
    // Succeeds, but only after the 120 s cleanup deadline.
    fake.cleanup_cost = secs(121);
    let outcome = drive(&mut fake, &clock, &planned());
    assert!(outcome.cleanup.attempted && !outcome.cleanup.cleaned);
    let error = outcome.cleanup.error.as_deref().unwrap();
    assert!(
        error.contains("cleanup") && error.contains("deadline"),
        "{error}"
    );
    assert!(!outcome.states.contains(&TrialState::Cleaned));
    assert_eq!(outcome.states.last(), Some(&TrialState::Terminal));
    // The reward already read and its grade are untouched, and no exception is added.
    assert!(outcome.reward_valid && outcome.passed());
    assert!(outcome.exceptions.is_empty());
    let phase = outcome
        .phases
        .iter()
        .find(|p| p.state == TrialState::Cleaned)
        .unwrap();
    assert_eq!(
        (phase.deadline_ms, phase.elapsed_ms),
        (Some(120_000), 121_000)
    );
}

#[test]
fn a_cleanup_within_its_deadline_is_cleaned() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = passing(&clock);
    fake.cleanup_cost = secs(119);
    let outcome = drive(&mut fake, &clock, &planned());
    assert!(outcome.cleanup.attempted && outcome.cleanup.cleaned);
    assert_eq!(outcome.cleanup.error, None);
    assert!(outcome.states.contains(&TrialState::Cleaned));
    assert!(outcome.passed());
}

#[test]
fn a_harness_plan_is_refused_without_touching_the_environment() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let mut fake = passing(&clock);
    let outcome = run_trial(
        &mut fake,
        &FakeClock(clock.clone()),
        &mut PanicModel,
        RunnerMode::Harness,
        &TimeoutPolicy::default(),
        &planned(),
        &TrialInputs {
            solution_dir: Path::new("/s"),
            tests_dir: Path::new("/t"),
            collect_dir: Path::new("/o"),
        },
    );
    assert_eq!(kinds(&outcome), vec![TrialExceptionKind::InvalidPlan]);
    assert!(fake.log.is_empty());
}

#[test]
fn an_outcome_round_trips_through_json() {
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let outcome = drive(&mut passing(&clock), &clock, &planned());
    let back: TrialOutcomeV1 =
        serde_json::from_str(&serde_json::to_string(&outcome).unwrap()).unwrap();
    assert_eq!(back, outcome);
}

// ---- planning from the pinned dataset ------------------------------------------------------

#[test]
fn a_pinned_task_plans_with_its_own_integral_deadlines() {
    let repo = Path::new(rsi_common::manifest_dir!()).join("../..");
    let dataset = crate::tb2::dataset::load(&repo.join(crate::tb2::DATASET)).unwrap();
    let identity = |task: &str| TrialIdentity {
        run_id: "run-1".into(),
        task: task.into(),
        slot: 0,
        retry: 0,
    };
    for task in &dataset.task {
        let plan = planned_trial(task, identity(&task.name)).unwrap();
        assert_eq!(plan.image_digest, task.image_digest);
        assert_eq!(plan.cpus, task.cpus);
        assert!(plan.agent_timeout_sec > 0 && plan.verifier_timeout_sec > 0);
    }
}

#[test]
fn fractional_or_nonpositive_deadlines_are_refused_not_truncated() {
    assert_eq!(whole_seconds("t", 900.0).unwrap(), 900);
    for bad in [900.5, 0.0, -1.0, f64::NAN, f64::INFINITY, 1e30] {
        assert!(whole_seconds("t", bad).is_err(), "{bad}");
    }
}
