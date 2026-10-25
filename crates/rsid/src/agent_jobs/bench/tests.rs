//! The `bench` job kind (#3181): gates at submit and at start, the queued launch,
//! the budget watch, the record at the finish.

use super::config::{fixtures, sha256_prefixed};
use super::transport::{RunnerBinary, RunnerFailure};
use super::*;
use crate::agent_jobs::{JobRuntime, LaunchSpec, SubmitContext};
use crate::deploy_drain::DeployDrain;
use chrono::Duration;
use rsi_common::agent_jobs::{
    BENCH_PIN_CHECK_FAILED, BENCH_PREFLIGHT_FAILED, JOB_SOURCE_MOVED, JobKind,
};
use rsi_common::bench_record::BenchRunRecord;
use rsi_common::bench_runner::{
    CleanupStatus, CollectionStatus, PhaseTiming, RUNNER_VERSION_DISCRIMINATOR, RunnerMode,
    RunnerPlanV1, RunnerSourceManifest, TrialException, TrialExceptionKind, TrialIdentity,
    TrialOutcomeV1, TrialState, trial_key,
};
use rsi_common::openrouter_spend::{
    RUN_BUDGET_EXCEEDS_REMAINDER_CODE, SPEND_CAP_PROBE_STALE_CODE, SPEND_CAP_REACHED_CODE,
    SpendInputs, evaluate,
};
use rsi_common::types::SessionStatus;
use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;

const ORACLE: &str = "eval/bench/tb2/runs/smoke-oracle.toml";
const PAID: &str = "eval/bench/tb2/runs/calibration-flash.toml";

// ---------------------------------------------------------------- fixtures

/// A committed git worktree holding everything a run reads.
pub(crate) struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    pub(crate) fn new() -> Self {
        let repo = Self {
            dir: crate::test_support::disk_backed_tempdir("bench-repo"),
        };
        repo.git(&["init", "-q"]);
        for (path, text) in [
            (ORACLE, fixtures::oracle_config()),
            (PAID, fixtures::rsi_config()),
            (DATASET_PATH, fixtures::DATASET_TOML.to_string()),
            (SPLIT_PATH, fixtures::SPLIT_JSON.to_string()),
        ] {
            repo.write(path, &text);
        }
        repo.commit("fixture");
        repo
    }

    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }

    pub(crate) fn git(&self, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .current_dir(self.path())
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    pub(crate) fn write(&self, relative: &str, text: &str) {
        let path = self.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn commit(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-qm", message]);
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }
}

/// The stamped runner every fake hub has installed.
fn fake_manifest() -> RunnerSourceManifest {
    RunnerSourceManifest::from_entries([
        (
            "crates/rsi-eval/src/tb2_runner/runner.rs".to_string(),
            "a".repeat(64),
        ),
        (
            "crates/rsid/src/agent_jobs/bench.rs".to_string(),
            "b".repeat(64),
        ),
    ])
    .unwrap()
}

fn fake_runner(dir: &Path) -> RunnerBinary {
    let manifest = fake_manifest();
    RunnerBinary {
        path: dir.join("rsi-eval"),
        evaluator_digest: manifest.evaluator_digest(),
        manifest,
    }
}

/// A runner that exited `exit` with a typed `kind: detail` line on stderr.
fn runner_failure(exit: i32, kind: &str, detail: &str) -> RunnerFailure {
    RunnerFailure::exited(
        Some(exit),
        &format!("tb2_runner error [{kind}]: {detail}\n"),
    )
}

fn cap(state: &str) -> OpenRouterSpendCapV1 {
    let now = Utc::now();
    let (cap_usd, probe) = match state {
        "off" => (0.0, None),
        // Armed at usage 100 with a $10 cap; $1 spent.
        "ok" => (10.0, Some((101.0, now))),
        "reached" => (10.0, Some((110.0, now))),
        // Armed with $8.50 spent: $1.50 left.
        "tight" => (10.0, Some((108.5, now))),
        "stale" => (10.0, Some((101.0, now - Duration::seconds(3600)))),
        other => panic!("{other}"),
    };
    evaluate(&SpendInputs {
        cap_usd,
        warn_pct: 80,
        baseline: (cap_usd > 0.0).then_some((100.0, now - Duration::seconds(7200))),
        probe,
        since_probe_usd: 0.0,
        now,
    })
}

fn flash_launch() -> ManagerLaunchChoiceV2 {
    ManagerLaunchChoiceV2 {
        provider: SessionProvider::OpenRouter,
        model: "deepseek/deepseek-v4.1-flash".into(),
        effort: Some("high".into()),
    }
}

/// The ports with every dial set to "a healthy hub and laptop".
pub(crate) struct FakePorts {
    settings: StdMutex<BenchSettings>,
    cap: StdMutex<OpenRouterSpendCapV1>,
    launches: StdMutex<Vec<ManagerLaunchChoiceV2>>,
    /// #3417: what the project's provider grant says about a launch it did not
    /// admit, and the USD it has left while it is active.
    grant_hint: StdMutex<Option<String>>,
    grant_remaining: StdMutex<Option<f64>>,
    runner: StdMutex<std::result::Result<RunnerBinary, String>>,
    /// What the runner's preflight answers, and every plan it was asked about.
    verdict: StdMutex<std::result::Result<(), RunnerFailure>>,
    plans: StdMutex<Vec<RunnerPlanV1>>,
    preflights: StdMutex<u32>,
    opened: StdMutex<Vec<NewBenchRun>>,
    /// R6: a socket path to answer `open_run` with instead of the run's own (a disagreement).
    socket_override: StdMutex<Option<PathBuf>>,
    /// #3191: every overlay leak audit asked for, and the verdict to give.
    audits: StdMutex<Vec<overlay::AuditPlan>>,
    audit_verdict: StdMutex<std::result::Result<(), String>>,
    root: PathBuf,
    _dir: tempfile::TempDir,
}

impl FakePorts {
    pub(crate) fn new() -> Arc<Self> {
        let dir = crate::test_support::disk_backed_tempdir("bench-ports");
        let runner = fake_runner(dir.path());
        Arc::new(Self {
            settings: StdMutex::new(BenchSettings {
                enabled: true,
                max_concurrency: 4,
                laptop_host: "arch-laptop".into(),
                docker_hosts: vec!["ssh://arch-laptop".into()],
            }),
            cap: StdMutex::new(cap("ok")),
            launches: StdMutex::new(vec![flash_launch()]),
            grant_hint: StdMutex::new(None),
            grant_remaining: StdMutex::new(None),
            runner: StdMutex::new(Ok(runner)),
            verdict: StdMutex::new(Ok(())),
            plans: StdMutex::new(Vec::new()),
            preflights: StdMutex::new(0),
            opened: StdMutex::new(Vec::new()),
            socket_override: StdMutex::new(None),
            audits: StdMutex::new(Vec::new()),
            audit_verdict: StdMutex::new(Ok(())),
            root: dir.path().join("bench"),
            _dir: dir,
        })
    }

    pub(crate) fn set(&self, change: impl FnOnce(&mut BenchSettings)) {
        change(&mut self.settings.lock().unwrap());
    }
}

#[async_trait]
impl BenchJobPorts for FakePorts {
    fn settings(&self) -> BenchSettings {
        self.settings.lock().unwrap().clone()
    }

    async fn spend_cap(&self) -> OpenRouterSpendCapV1 {
        self.cap.lock().unwrap().clone()
    }

    fn allowed_launches(&self, _: &Store, _: Uuid) -> Result<Vec<ManagerLaunchChoiceV2>> {
        Ok(self.launches.lock().unwrap().clone())
    }

    fn provider_grant_hint(&self, _: &Store, _: Uuid, _: &ManagerLaunchChoiceV2) -> Option<String> {
        self.grant_hint.lock().unwrap().clone()
    }

    fn provider_grant_remaining(
        &self,
        _: &Store,
        _: Uuid,
        _: &ManagerLaunchChoiceV2,
    ) -> Option<f64> {
        *self.grant_remaining.lock().unwrap()
    }

    async fn runner(&self) -> std::result::Result<RunnerBinary, String> {
        self.runner.lock().unwrap().clone()
    }

    async fn preflight(
        &self,
        _: &RunnerBinary,
        plan: &RunnerPlanV1,
    ) -> std::result::Result<(), RunnerFailure> {
        *self.preflights.lock().unwrap() += 1;
        self.plans.lock().unwrap().push(plan.clone());
        self.verdict.lock().unwrap().clone()
    }

    async fn open_run(&self, run: NewBenchRun) -> Result<PathBuf> {
        let socket = self
            .socket_override
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| self.root.join(run.id.to_string()).join("adapter.sock"));
        self.opened.lock().unwrap().push(run);
        Ok(socket)
    }

    fn bench_root(&self) -> PathBuf {
        self.root.clone()
    }

    async fn leak_audit(
        &self,
        _: &Path,
        plan: overlay::AuditPlan,
    ) -> std::result::Result<(), String> {
        self.audits.lock().unwrap().push(plan);
        self.audit_verdict.lock().unwrap().clone()
    }
}

#[derive(Default)]
struct Recorder {
    launched: StdMutex<Vec<LaunchSpec>>,
}

impl JobRuntime for Recorder {
    fn launch(&self, spec: &LaunchSpec) -> std::result::Result<(), String> {
        self.launched.lock().unwrap().push(spec.clone());
        Ok(())
    }

    fn unit_active(&self, _: &str) -> bool {
        true
    }
}

struct World {
    repo: Repo,
    ports: Arc<FakePorts>,
    store: Arc<rsid_core::store_lock::Mutex<Store>>,
    runtime: Arc<Recorder>,
    jobs: tempfile::TempDir,
    owner: Uuid,
    project: Uuid,
}

fn world() -> World {
    let repo = Repo::new();
    let store = Store::open_in_memory().unwrap();
    let mut owner = rsid_store::test_support::make_test_session();
    owner.status = SessionStatus::Running;
    owner.sandbox_root = Some(repo.path().into());
    owner.working_dir = repo.path().into();
    store.insert_session(&owner).unwrap();
    World {
        project: owner.project_id.unwrap(),
        owner: owner.id,
        repo,
        ports: FakePorts::new(),
        store: Arc::new(rsid_core::store_lock::Mutex::new(store)),
        runtime: Arc::new(Recorder::default()),
        jobs: crate::test_support::disk_backed_tempdir("bench-jobs"),
    }
}

fn params(config: &str, budget_usd: f64) -> BenchJobParams {
    BenchJobParams {
        config: config.into(),
        budget_usd,
    }
}

impl World {
    async fn admit(
        &self,
        config: &str,
        stage: Stage,
    ) -> std::result::Result<Admitted, BenchRefusal> {
        admit(
            &*self.ports,
            &self.store,
            Some(self.project),
            self.repo.path(),
            &params(config, 5.0),
            stage,
        )
        .await
    }

    /// Submit through the gates and the queued insert; the job waits `queued`.
    async fn submit(&self, config: &str) -> AgentJobV1 {
        self.submit_with_budget(config, 5.0).await
    }

    async fn submit_with_budget(&self, config: &str, budget_usd: f64) -> AgentJobV1 {
        let admitted = admit(
            &*self.ports,
            &self.store,
            Some(self.project),
            self.repo.path(),
            &params(config, budget_usd),
            Stage::Submit,
        )
        .await
        .expect("admitted");
        let ctx = SubmitContext {
            owner: self.owner,
            project_id: Some(self.project),
            cwd: self.repo.path().to_path_buf(),
            name: None,
            params: JobParams::Bench(params(config, budget_usd)),
            idempotency_key: None,
            wake: JobWake::Owner,
        };
        let store = self.store.lock().await;
        let (row, replayed) = submit_queued(
            &store,
            &*self.runtime,
            self.jobs.path(),
            ctx,
            &admitted,
            Utc::now(),
        )
        .unwrap();
        assert!(!replayed);
        row.job
    }

    async fn start(&self) -> usize {
        let runtime: Arc<dyn JobRuntime> = self.runtime.clone();
        let ports: Arc<dyn BenchJobPorts> = self.ports.clone();
        start_queued(&self.store, &runtime, &DeployDrain::new(), &ports)
            .await
            .unwrap()
    }

    async fn job(&self, id: Uuid) -> AgentJobV1 {
        self.store
            .lock()
            .await
            .get_agent_job(id)
            .unwrap()
            .unwrap()
            .job
    }
}

// ------------------------------------------------------------------ admission

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_disabled_bench_is_refused_before_anything_is_read_or_probed() {
    let w = world();
    w.ports.set(|s| s.enabled = false);
    for stage in [Stage::Submit, Stage::Start] {
        let refusal = w.admit(ORACLE, stage).await.err().unwrap();
        assert_eq!(refusal.code, BENCH_DISABLED, "{stage:?}");
    }
    assert_eq!(*w.ports.preflights.lock().unwrap(), 0, "no laptop contact");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_dirty_uncommitted_or_missing_config_is_refused_naming_the_files() {
    let w = world();
    // Modified after the commit.
    w.repo
        .write(ORACLE, &format!("{}# edited\n", fixtures::oracle_config()));
    let refusal = w.admit(ORACLE, Stage::Submit).await.err().unwrap();
    assert_eq!(refusal.code, BENCH_CONFIG_UNCOMMITTED);
    assert!(refusal.detail.contains(ORACLE), "{}", refusal.detail);
    w.repo.git(&["checkout", "--", ORACLE]);
    assert!(w.admit(ORACLE, Stage::Submit).await.is_ok(), "clean again");

    // Written but never committed.
    let fresh = "eval/bench/tb2/runs/fresh.toml";
    w.repo.write(fresh, &fixtures::oracle_config());
    let refusal = w.admit(fresh, Stage::Submit).await.err().unwrap();
    assert_eq!(refusal.code, BENCH_CONFIG_UNCOMMITTED);
    assert!(refusal.detail.contains("fresh.toml"));
    // Staged but not committed is still not at HEAD.
    w.repo.git(&["add", fresh]);
    assert_eq!(
        w.admit(fresh, Stage::Submit).await.err().unwrap().code,
        BENCH_CONFIG_UNCOMMITTED
    );
    w.repo.git(&["rm", "-q", "--cached", fresh]);
    std::fs::remove_file(w.repo.path().join(fresh)).unwrap();

    // Any other file the run reads counts too: the pinned dataset and its split.
    for read in [DATASET_PATH, SPLIT_PATH] {
        let committed = std::fs::read_to_string(w.repo.path().join(read)).unwrap();
        w.repo.write(read, &format!("{committed}\n"));
        let refusal = w.admit(ORACLE, Stage::Submit).await.err().unwrap();
        assert_eq!(refusal.code, BENCH_CONFIG_UNCOMMITTED);
        assert!(refusal.detail.contains(read), "{}", refusal.detail);
        w.repo.git(&["checkout", "--", read]);
    }
    // The Rust runner reads nothing else: no adapter package, no pin script (#3681).
    assert_eq!(
        input_paths(ORACLE),
        [ORACLE, DATASET_PATH, SPLIT_PATH],
        "no Python input is under custody"
    );

    // A config that does not exist is invalid, not uncommitted.
    let refusal = w
        .admit("eval/bench/tb2/runs/absent.toml", Stage::Submit)
        .await
        .err()
        .unwrap();
    assert_eq!(refusal.code, BENCH_CONFIG_INVALID);
    // A worktree that is not a git checkout cannot be held to one commit.
    let loose = tempfile::tempdir().unwrap();
    for rel in [ORACLE, DATASET_PATH, SPLIT_PATH] {
        let dest = loose.path().join(rel);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(w.repo.path().join(rel), dest).unwrap();
    }
    let refusal = admit(
        &*w.ports,
        &w.store,
        Some(w.project),
        loose.path(),
        &params(ORACLE, 5.0),
        Stage::Submit,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(refusal.code, BENCH_CONFIG_UNCOMMITTED);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_malformed_config_is_refused_as_invalid_with_the_reason() {
    let w = world();
    w.repo.write(
        ORACLE,
        &fixtures::oracle_config().replace("k = 1", "k = 1\ntimeout_multiplier = 3.0"),
    );
    w.repo.commit("bad multiplier");
    let refusal = w.admit(ORACLE, Stage::Submit).await.err().unwrap();
    assert_eq!(refusal.code, BENCH_CONFIG_INVALID);
    assert!(
        refusal.detail.contains("timeout_multiplier"),
        "{}",
        refusal.detail
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn the_config_host_must_be_the_laptop_and_in_the_docker_host_allowlist() {
    let w = world();
    w.ports.set(|s| s.laptop_host = "other-box".into());
    let refusal = w.admit(ORACLE, Stage::Submit).await.err().unwrap();
    assert_eq!(refusal.code, BENCH_LAPTOP_HOST_MISMATCH);
    w.ports.set(|s| s.laptop_host = "arch-laptop".into());
    w.ports
        .set(|s| s.docker_hosts = vec!["ssh://elsewhere".into()]);
    let refusal = w.admit(ORACLE, Stage::Submit).await.err().unwrap();
    assert_eq!(refusal.code, BENCH_DOCKER_HOST_NOT_ALLOWED);
    w.ports.set(|s| s.docker_hosts.clear());
    assert_eq!(
        w.admit(ORACLE, Stage::Start).await.err().unwrap().code,
        BENCH_DOCKER_HOST_NOT_ALLOWED,
        "an empty allowlist refuses every host"
    );
    w.ports
        .set(|s| s.docker_hosts = vec!["ssh://arch-laptop".into()]);
    assert!(w.admit(ORACLE, Stage::Start).await.is_ok());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_paid_run_needs_a_launch_grant_for_its_model_and_effort() {
    let w = world();
    for stage in [Stage::Submit, Stage::Start] {
        w.ports.launches.lock().unwrap().clear();
        let refusal = w.admit(PAID, stage).await.err().unwrap();
        assert_eq!(
            refusal.code,
            rsi_common::bench::BENCH_LAUNCH_NOT_ALLOWED,
            "{stage:?}"
        );
        // The wrong effort is not the grant.
        *w.ports.launches.lock().unwrap() = vec![ManagerLaunchChoiceV2 {
            effort: Some("low".into()),
            ..flash_launch()
        }];
        assert_eq!(
            w.admit(PAID, stage).await.err().unwrap().code,
            rsi_common::bench::BENCH_LAUNCH_NOT_ALLOWED
        );
        *w.ports.launches.lock().unwrap() = vec![flash_launch()];
        assert!(w.admit(PAID, stage).await.is_ok(), "{stage:?}");
    }
}

/// #3417: a spent or wrong-effort provider grant names itself, its spend and the
/// screen that raises it in the bench refusal, not only the missing tuple.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_spent_provider_grant_names_itself_in_the_bench_refusal() {
    let w = world();
    w.ports.launches.lock().unwrap().clear();
    *w.ports.grant_hint.lock().unwrap() = Some(
        "manager_v2_provider_grant_exhausted: the OpenRouter grant is spent (OpenRouter: $100.00 of $100 used, no expiry); the operator raises it at TUI: :manager board > p (Policy) > Provider grants > OpenRouter grant budget (USD)"
            .into(),
    );
    for stage in [Stage::Submit, Stage::Start] {
        let refusal = w.admit(PAID, stage).await.err().unwrap();
        assert_eq!(refusal.code, rsi_common::bench::BENCH_LAUNCH_NOT_ALLOWED);
        assert!(
            refusal
                .detail
                .contains("manager_v2_provider_grant_exhausted"),
            "{}",
            refusal.detail
        );
        assert!(
            refusal.detail.contains("$100.00 of $100 used"),
            "{}",
            refusal.detail
        );
        assert!(
            refusal.detail.contains("OpenRouter grant budget (USD)"),
            "{}",
            refusal.detail
        );
    }
}

/// #3417: an active provider grant bounds the run by what it has left and stands
/// in for the armed OpenRouter account cap, so the operator's one grant is
/// enough; a run larger than the remainder is refused naming the screen.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn an_active_provider_grant_admits_a_paid_run_with_the_cap_off_up_to_its_remainder() {
    let w = world();
    *w.ports.cap.lock().unwrap() = cap("off");
    *w.ports.grant_remaining.lock().unwrap() = Some(87.6);
    for stage in [Stage::Submit, Stage::Start] {
        assert!(w.admit(PAID, stage).await.is_ok(), "{stage:?}");
    }
    *w.ports.grant_remaining.lock().unwrap() = Some(4.0);
    let refusal = w.admit(PAID, Stage::Submit).await.err().unwrap();
    assert_eq!(refusal.code, rsi_common::bench::BENCH_LAUNCH_NOT_ALLOWED);
    assert!(
        refusal
            .detail
            .contains("$4.00 left under the OpenRouter provider grant"),
        "{}",
        refusal.detail
    );
    assert!(
        refusal.detail.contains("Provider grants"),
        "{}",
        refusal.detail
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_paid_run_is_refused_while_the_spend_cap_is_off_at_submit_and_at_start() {
    let w = world();
    *w.ports.cap.lock().unwrap() = cap("off");
    for stage in [Stage::Submit, Stage::Start] {
        let refusal = w.admit(PAID, stage).await.err().unwrap();
        assert_eq!(refusal.code, BENCH_SPEND_CAP_OFF, "{stage:?}");
        assert!(refusal.detail.contains("OpenRouter"), "{}", refusal.detail);
    }
    assert_eq!(
        *w.ports.preflights.lock().unwrap(),
        0,
        "refused before the laptop"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_run_that_launches_no_model_runs_with_the_cap_off_and_needs_no_grant() {
    let w = world();
    *w.ports.cap.lock().unwrap() = cap("off");
    w.ports.launches.lock().unwrap().clear();
    for stage in [Stage::Submit, Stage::Start] {
        let admitted = w.admit(ORACLE, stage).await.expect("oracle admitted");
        assert!(!admitted.inputs.resolved.is_paid());
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn the_budget_must_fit_under_the_armed_caps_remainder() {
    let w = world();
    for stage in [Stage::Submit, Stage::Start] {
        // $1.50 left under the cap: a $5 run does not fit.
        *w.ports.cap.lock().unwrap() = cap("tight");
        let refusal = w.admit(PAID, stage).await.err().unwrap();
        assert_eq!(refusal.code, RUN_BUDGET_EXCEEDS_REMAINDER_CODE, "{stage:?}");
        *w.ports.cap.lock().unwrap() = cap("reached");
        assert_eq!(
            w.admit(PAID, stage).await.err().unwrap().code,
            SPEND_CAP_REACHED_CODE
        );
        *w.ports.cap.lock().unwrap() = cap("stale");
        assert_eq!(
            w.admit(PAID, stage).await.err().unwrap().code,
            SPEND_CAP_PROBE_STALE_CODE,
            "a cap that cannot be judged fails closed"
        );
        *w.ports.cap.lock().unwrap() = cap("ok");
        assert!(w.admit(PAID, stage).await.is_ok());
    }
    // A per-task cap above the run budget is invalid.
    let refusal = admit(
        &*w.ports,
        &w.store,
        Some(w.project),
        w.repo.path(),
        &params(PAID, 0.2),
        Stage::Submit,
    )
    .await
    .err()
    .unwrap();
    assert_eq!(
        refusal.code,
        rsi_common::openrouter_spend::RUN_BUDGET_INVALID_CODE
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn each_runner_preflight_failure_refuses_with_its_reason_and_pin_drift_has_its_own_code() {
    let w = world();
    let cases: [(RunnerFailure, &str, &str); 7] = [
        (
            runner_failure(20, "docker_unreachable", "the Docker daemon did not answer"),
            BENCH_PREFLIGHT_FAILED,
            "docker_unreachable",
        ),
        (
            runner_failure(23, "ssh_failed", "ssh did not answer"),
            BENCH_PREFLIGHT_FAILED,
            "ssh_failed",
        ),
        (
            runner_failure(20, "disk_insufficient", "1 bytes free, 2 needed"),
            BENCH_PREFLIGHT_FAILED,
            "disk_insufficient",
        ),
        (
            runner_failure(20, "memory_insufficient", "1 bytes available, 2 needed"),
            BENCH_PREFLIGHT_FAILED,
            "memory_insufficient",
        ),
        (
            runner_failure(20, "capacity_unmeasured", "free disk could not be measured"),
            BENCH_PREFLIGHT_FAILED,
            "capacity_unmeasured",
        ),
        (
            runner_failure(
                21,
                "pin_drift",
                "1 drift line(s): task.toml changed: fix-git",
            ),
            BENCH_PIN_CHECK_FAILED,
            "task.toml changed",
        ),
        // A runner that could not run at all is a failed preflight, never a pass.
        (
            RunnerFailure {
                exit: None,
                detail: "timed out after 180s".into(),
            },
            BENCH_PREFLIGHT_FAILED,
            "timed out",
        ),
    ];
    for stage in [Stage::Submit, Stage::Start] {
        for (failure, code, reason) in &cases {
            *w.ports.verdict.lock().unwrap() = Err(failure.clone());
            let refusal = w.admit(PAID, stage).await.err().unwrap();
            assert_eq!(refusal.code, *code, "{reason} at {stage:?}: {refusal:?}");
            assert!(
                refusal.detail.contains(reason),
                "{reason}: {}",
                refusal.detail
            );
        }
    }
    *w.ports.verdict.lock().unwrap() = Ok(());
    assert!(w.admit(PAID, Stage::Submit).await.is_ok());
}

/// A runner that is not installed, or whose stamp is not trusted, refuses the run before any
/// preflight: the daemon never falls back to whatever is on `PATH` (#3681).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_missing_or_unstamped_runner_refuses_the_run_before_the_preflight() {
    let w = world();
    *w.ports.runner.lock().unwrap() = Err("rsi-eval is not installed beside the daemon".into());
    for stage in [Stage::Submit, Stage::Start] {
        let refusal = w.admit(ORACLE, stage).await.err().unwrap();
        assert_eq!(refusal.code, BENCH_PREFLIGHT_FAILED, "{stage:?}");
        assert!(
            refusal.detail.starts_with("runner_unavailable:"),
            "{}",
            refusal.detail
        );
        assert!(
            refusal.detail.contains("not installed"),
            "{}",
            refusal.detail
        );
    }
    assert_eq!(*w.ports.preflights.lock().unwrap(), 0);
}

/// The plan the runner is preflighted with is built from the admitted run: the oracle mode, the
/// task slots with their pinned limits, the allowlist and both concurrency numbers.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn the_preflight_plan_is_the_admitted_run_with_the_operator_settings() {
    let w = world();
    let admitted = w.admit(ORACLE, Stage::Submit).await.expect("admitted");
    let plans = w.ports.plans.lock().unwrap().clone();
    assert_eq!(plans.len(), 1);
    let plan = &plans[0];
    assert_eq!(plan.mode, RunnerMode::Oracle);
    assert_eq!(plan.trials.len(), 1);
    assert_eq!(plan.trials[0].identity.task, "fix-git");
    assert_eq!((plan.trials[0].cpus, plan.trials[0].memory_mb), (1, 2048));
    let remote = plan.remote.as_ref().unwrap();
    assert_eq!(remote.docker_host, "ssh://arch-laptop");
    assert_eq!(remote.laptop_host, "arch-laptop");
    assert_eq!(remote.allowed_docker_hosts, ["ssh://arch-laptop"]);
    assert_eq!(
        (remote.concurrency, remote.max_concurrency),
        (2, 4),
        "the effective concurrency is min(config 2, bench.max_concurrency 4)"
    );
    assert_eq!(admitted.concurrency, 2);
    assert_eq!(
        remote.checkout_dir,
        w.ports
            .root
            .join("tb2")
            .join("2fd12b88aafdd04a52c298e3940bcb189f9766d6")
            .display()
            .to_string()
    );
    assert_eq!(
        remote.dataset_path,
        w.repo.path().join(DATASET_PATH).display().to_string()
    );
    // The admitted run carries the runner whose identity the record will name.
    assert_eq!(
        admitted.runner.evaluator_digest,
        fake_manifest().evaluator_digest()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn the_trial_concurrency_is_the_lower_of_the_config_and_the_operator_setting() {
    let w = world();
    assert_eq!(
        w.admit(PAID, Stage::Submit).await.ok().unwrap().concurrency,
        4
    );
    w.ports.set(|s| s.max_concurrency = 2);
    assert_eq!(
        w.admit(PAID, Stage::Submit).await.ok().unwrap().concurrency,
        2
    );
    w.ports.set(|s| s.max_concurrency = 1);
    assert_eq!(
        w.admit(PAID, Stage::Submit).await.ok().unwrap().concurrency,
        1
    );
    let plans = w.ports.plans.lock().unwrap().clone();
    let last = plans.last().unwrap().remote.as_ref().unwrap();
    assert_eq!((last.concurrency, last.max_concurrency), (1, 1));
}

// ------------------------------------------------------------ submit and start

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_submit_queues_the_job_records_its_digest_and_launches_nothing() {
    let w = world();
    let job = w.submit(PAID).await;
    assert_eq!(job.state, JobState::Queued);
    assert_eq!(job.held.as_deref(), Some(JOB_HELD_FOR_BENCH));
    assert_eq!(job.kind, JobKind::Bench);
    assert!(w.runtime.launched.lock().unwrap().is_empty());
    assert!(
        w.ports.opened.lock().unwrap().is_empty(),
        "no run before the start gates"
    );
    let sidecar = read_sidecar(&job.log_path).expect("recorded at submit");
    assert_eq!(sidecar.config, PAID);
    assert_eq!(
        sidecar.config_digest,
        sha256_prefixed(fixtures::rsi_config().as_bytes())
    );
    assert_eq!(sidecar.head, w.repo.head());
    assert_eq!(sidecar.budget_usd, 5.0);
    // The HEAD guard of the build kind covers it too.
    assert!(
        std::fs::read_to_string(Path::new(&job.log_path).with_extension("source"))
            .unwrap()
            .starts_with(&w.repo.head())
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn the_start_opens_the_run_writes_its_inputs_and_launches_the_runner_unit() {
    let w = world();
    let job = w.submit(PAID).await;
    assert_eq!(w.start().await, 1);
    let started = w.job(job.id).await;
    assert_eq!(started.state, JobState::Running);
    assert_eq!(started.held, None);

    let run_dir = w.ports.root.join(job.id.to_string());
    let launched = w.runtime.launched.lock().unwrap().clone();
    assert_eq!(launched.len(), 1);
    let argv = &launched[0].command.argv;
    // The stamped Rust binary, directly: no interpreter, no script, no repository path.
    assert_eq!(
        argv,
        &[
            w.ports
                .root
                .parent()
                .unwrap()
                .join("rsi-eval")
                .display()
                .to_string(),
            "bench".into(),
            "tb2".into(),
            "run".into(),
            "--plan".into(),
            run_dir.join("plan.json").display().to_string(),
        ]
    );
    // One CPU, never the build slice; a long-lived unit with a stop timeout.
    let command = &launched[0].command;
    assert_eq!(command.cpu_quota_percent, 100);
    assert_eq!(command.runtime_max_secs, BENCH_RUNTIME_MAX_SECS);
    assert!(launched[0].build_environment.is_none());

    // The run: the admitted budget, the per-task cap and the pinned digests.
    let opened = w.ports.opened.lock().unwrap().clone();
    assert_eq!(opened.len(), 1);
    let run = &opened[0];
    assert_eq!(run.id, job.id, "the run is the job");
    assert_eq!(run.job_id.as_deref(), Some(job.id.to_string().as_str()));
    assert_eq!(run.project_id, w.project);
    assert_eq!(run.model, "deepseek/deepseek-v4.1-flash");
    assert_eq!(run.effort, "high");
    assert_eq!(run.budget_usd, 5.0);
    assert_eq!(run.per_task_cap_usd, 0.40);
    assert_eq!(run.concurrency, 4);
    assert_eq!(run.docker_host, "ssh://arch-laptop");
    assert_eq!(
        run.dataset_commit,
        "2fd12b88aafdd04a52c298e3940bcb189f9766d6"
    );
    assert_eq!(
        run.config_digest,
        sha256_prefixed(fixtures::rsi_config().as_bytes())
    );
    let tasks: Vec<_> = run
        .tasks
        .iter()
        .map(|t| (t.task.as_str(), t.pinned_digests.len()))
        .collect();
    assert_eq!(tasks, [("big-one", 1), ("fix-git", 1)]);
    assert_eq!(run.loop_knobs.max_iterations, Some(200));

    // The plan the runner reads: the paid config plans the Harness mode and names the run-local
    // socket the daemon bound (the capability a Rust client drives) and the run's stop marker.
    let plan: RunnerPlanV1 =
        serde_json::from_slice(&std::fs::read(run_dir.join("plan.json")).unwrap()).unwrap();
    plan.validate().unwrap();
    assert_eq!(plan.mode, RunnerMode::Harness);
    // The admission preflight runs before any run (and so any adapter socket) exists: it sees
    // the oracle-shaped plan, never a Harness plan naming a socket that cannot be there yet.
    let preflighted = w.ports.plans.lock().unwrap().clone();
    assert!(!preflighted.is_empty());
    for pre in &preflighted {
        assert_eq!(pre.mode, RunnerMode::Oracle);
        assert_eq!(pre.remote.as_ref().unwrap().socket_path, None);
        pre.validate().unwrap();
    }
    assert_eq!(plan.run_id, job.id.to_string());
    let planned: Vec<_> = plan
        .trials
        .iter()
        .map(|t| (t.identity.task.as_str(), t.identity.slot))
        .collect();
    assert_eq!(planned, [("big-one", 0), ("fix-git", 0)]);
    let remote = plan.remote.as_ref().unwrap();
    assert_eq!(remote.laptop_host, "arch-laptop");
    assert_eq!(remote.effective_concurrency(), 4);
    assert_eq!(
        remote.dataset_path,
        run_dir.join("inputs/dataset.toml").display().to_string(),
        "graded against the dataset copy the run was admitted with"
    );
    assert_eq!(remote.output_root, w.ports.root.display().to_string());
    assert_eq!(
        remote.socket_path.as_deref(),
        Some(run_dir.join("adapter.sock").display().to_string().as_str()),
        "the plan drives the socket the daemon bound for this run"
    );
    assert_eq!(
        remote.stop_file.as_deref(),
        Some(run_dir.join("stop.json").display().to_string().as_str())
    );
    // The exact config and dataset the run used are kept beside it.
    for (name, text) in [
        ("config.toml", fixtures::rsi_config()),
        ("dataset.toml", fixtures::DATASET_TOML.to_string()),
        ("split.json", fixtures::SPLIT_JSON.to_string()),
    ] {
        assert_eq!(
            std::fs::read_to_string(run_dir.join("inputs").join(name)).unwrap(),
            text
        );
    }
    let sidecar = read_sidecar(&job.log_path).unwrap();
    assert_eq!(
        sidecar.run_dir.as_deref(),
        Some(run_dir.display().to_string().as_str())
    );
    // The evaluator identity the record will carry is the runner's, fixed at start.
    assert_eq!(
        sidecar.adapter_hash.as_deref(),
        Some(fake_manifest().evaluator_digest().as_str())
    );
    assert_eq!(
        sidecar.runner_manifest.as_deref(),
        Some(fake_manifest().to_text().as_str())
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn an_oracle_run_starts_with_no_run_row_and_no_adapter_socket() {
    let w = world();
    *w.ports.cap.lock().unwrap() = cap("off");
    w.ports.launches.lock().unwrap().clear();
    let job = w.submit(ORACLE).await;
    assert_eq!(w.start().await, 1);
    assert!(
        w.ports.opened.lock().unwrap().is_empty(),
        "no adapter, no run"
    );
    let plan: RunnerPlanV1 = serde_json::from_slice(
        &std::fs::read(w.ports.root.join(job.id.to_string()).join("plan.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(plan.mode, RunnerMode::Oracle);
    assert_eq!(plan.remote.as_ref().unwrap().effective_concurrency(), 2);
    // The oracle holds no model-session capability, yet still honours the daemon's stop marker.
    assert_eq!(plan.remote.as_ref().unwrap().socket_path, None);
    assert!(plan.remote.as_ref().unwrap().stop_file.is_some());
    assert_eq!(w.job(job.id).await.state, JobState::Running);
}

/// Settled-failed result of the only job, for the start-gate tests.
async fn refused_start(w: &World, job: &AgentJobV1) -> AgentJobResultV1 {
    assert_eq!(w.start().await, 0);
    let after = w.job(job.id).await;
    assert_eq!(after.state, JobState::Failed);
    assert!(w.runtime.launched.lock().unwrap().is_empty(), "no unit");
    assert!(w.ports.opened.lock().unwrap().is_empty(), "no run");
    after.result.expect("a typed result")
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn the_gates_run_again_at_the_start_and_a_failure_settles_the_job_with_the_typed_refusal() {
    // Each dial is turned between submit and start.
    let turns: [(&str, Box<dyn Fn(&World)>); 7] = [
        (
            BENCH_DISABLED,
            Box::new(|w| w.ports.set(|s| s.enabled = false)),
        ),
        (
            BENCH_DOCKER_HOST_NOT_ALLOWED,
            Box::new(|w| w.ports.set(|s| s.docker_hosts.clear())),
        ),
        (
            rsi_common::bench::BENCH_LAUNCH_NOT_ALLOWED,
            Box::new(|w| w.ports.launches.lock().unwrap().clear()),
        ),
        (
            BENCH_SPEND_CAP_OFF,
            Box::new(|w| *w.ports.cap.lock().unwrap() = cap("off")),
        ),
        (
            RUN_BUDGET_EXCEEDS_REMAINDER_CODE,
            Box::new(|w| *w.ports.cap.lock().unwrap() = cap("tight")),
        ),
        (
            BENCH_PIN_CHECK_FAILED,
            Box::new(|w| {
                *w.ports.verdict.lock().unwrap() =
                    Err(runner_failure(21, "pin_drift", "image digest drift: x"));
            }),
        ),
        (
            BENCH_PREFLIGHT_FAILED,
            Box::new(|w| {
                *w.ports.verdict.lock().unwrap() =
                    Err(runner_failure(20, "disk_insufficient", "1 free, 2 needed"));
            }),
        ),
    ];
    for (code, turn) in turns {
        let w = world();
        let job = w.submit(PAID).await;
        turn(&w);
        let result = refused_start(&w, &job).await;
        assert_eq!(result.refusal.as_deref(), Some(code), "{code}: {result:?}");
        assert!(result.detail.is_some());
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_socket_the_daemon_bound_elsewhere_than_the_plan_names_refuses_before_the_unit_starts() {
    let w = world();
    let job = w.submit(PAID).await;
    *w.ports.socket_override.lock().unwrap() =
        Some(PathBuf::from("/tmp/some-other-run/adapter.sock"));
    assert_eq!(w.start().await, 0);
    let after = w.job(job.id).await;
    assert_eq!(after.state, JobState::Failed);
    assert!(w.runtime.launched.lock().unwrap().is_empty(), "no unit");
    let result = after.result.expect("a typed result");
    assert_eq!(
        result.refusal.as_deref(),
        Some(BENCH_RUN_OPEN_FAILED),
        "{result:?}"
    );
    assert!(
        result
            .detail
            .unwrap()
            .contains("not the one the daemon bound")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_moved_head_or_a_changed_config_stops_the_start() {
    let w = world();
    let job = w.submit(ORACLE).await;
    w.repo.write("unrelated.txt", "new\n");
    w.repo.commit("moves HEAD");
    let result = refused_start(&w, &job).await;
    assert_eq!(result.refusal.as_deref(), Some(JOB_SOURCE_MOVED));
    assert!(result.detail.unwrap().contains(&w.repo.head()));

    // Same commit, but the recorded digest is not the config's.
    let w = world();
    let job = w.submit(ORACLE).await;
    let mut sidecar = read_sidecar(&job.log_path).unwrap();
    sidecar.config_digest = "sha256:other".into();
    write_sidecar(&job.log_path, &sidecar).unwrap();
    let result = refused_start(&w, &job).await;
    assert_eq!(result.refusal.as_deref(), Some(BENCH_CONFIG_CHANGED));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_staged_deploy_holds_the_start_and_other_jobs_are_left_alone() {
    let w = world();
    let job = w.submit(ORACLE).await;
    let drain = DeployDrain::new();
    let live = crate::store::agent_deploys::DeployRow {
        id: Uuid::new_v4(),
        owner_session_id: Some(Uuid::new_v4()),
        sha: "0".repeat(40),
        manifest: Vec::new(),
        state: rsi_common::agent_deploy::DeployState::Staged,
        reason: None,
        deadline_at: Utc::now() + Duration::seconds(600),
        operator: false,
        forced: false,
    };
    drain.sync(Some(&live), true, Utc::now());
    let runtime: Arc<dyn JobRuntime> = w.runtime.clone();
    let ports: Arc<dyn BenchJobPorts> = w.ports.clone();
    assert_eq!(
        start_queued(&w.store, &runtime, &drain, &ports)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        w.job(job.id).await.state,
        JobState::Queued,
        "waits, is not failed"
    );
    assert!(w.runtime.launched.lock().unwrap().is_empty());
}

// ----------------------------------------------------------------- budget

struct FakeSpend {
    readings: StdMutex<VecDeque<f64>>,
    last: StdMutex<f64>,
}

impl FakeSpend {
    fn new(readings: &[f64]) -> Self {
        Self {
            readings: StdMutex::new(readings.iter().copied().collect()),
            last: StdMutex::new(0.0),
        }
    }
}

impl RunSpend for FakeSpend {
    fn spent_usd(&self, _: Uuid) -> Result<f64> {
        let mut last = self.last.lock().unwrap();
        if let Some(next) = self.readings.lock().unwrap().pop_front() {
            *last = next;
        }
        Ok(*last)
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn the_trial_gate_stops_launching_once_the_run_budget_is_spent_mid_run() {
    // Each launched trial costs $0.50 against a $3 budget; the spend is re-read
    // before every launch, so the seventh trial never starts.
    struct Meter(StdMutex<f64>);
    impl RunSpend for Meter {
        fn spent_usd(&self, _: Uuid) -> Result<f64> {
            Ok(*self.0.lock().unwrap())
        }
    }
    let meter = Meter(StdMutex::new(0.0));
    let run = Uuid::new_v4();
    let mut launched = 0;
    let mut stopped_at = None;
    for _ in 0..20 {
        match gate_trial(3.0, run, &meter).unwrap() {
            TrialGate::Launch(status) => {
                assert!(!status.exhausted && status.remaining_usd > 0.0);
                launched += 1;
                *meter.0.lock().unwrap() += 0.5;
            }
            TrialGate::Stop(status) => {
                stopped_at = Some(status);
                break;
            }
        }
    }
    assert_eq!(launched, 6);
    let status = stopped_at.expect("the gate stopped the run");
    assert!(status.exhausted);
    assert_eq!(status.spent_usd, 3.0);
    assert_eq!(status.remaining_usd, 0.0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn the_watcher_writes_the_stop_marker_once_when_the_spend_crosses_the_budget() {
    let dir = tempfile::tempdir().unwrap();
    let run = Uuid::new_v4();
    let spend = FakeSpend::new(&[0.0, 1.0, 2.9, 3.0, 4.0]);
    let marker = dir.path().join(STOP_FILE);
    for tick in 0..3 {
        assert!(
            watch_budget(dir.path(), 3.0, run, &spend, Utc::now())
                .unwrap()
                .is_none(),
            "tick {tick} is under the budget"
        );
        assert!(!marker.exists());
    }
    let status = watch_budget(dir.path(), 3.0, run, &spend, Utc::now())
        .unwrap()
        .unwrap();
    assert!(status.exhausted);
    let stop: StopMarker = serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
    assert_eq!(stop.reason, "budget_exhausted");
    assert_eq!(stop.spent_usd, 3.0);
    assert_eq!(stop.budget_usd, 3.0);
    // Later ticks leave the first reading in place.
    assert!(
        watch_budget(dir.path(), 3.0, run, &spend, Utc::now())
            .unwrap()
            .is_none()
    );
    let again: StopMarker = serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
    assert_eq!(again.spent_usd, 3.0);
}

/// A running bench job with a recorded run directory and a run row.
async fn running_job(w: &World, budget: f64) -> (AgentJobV1, PathBuf) {
    let job = w.submit_with_budget(PAID, budget).await;
    assert_eq!(w.start().await, 1);
    let run_dir = w.ports.root.join(job.id.to_string());
    let store = w.store.lock().await;
    let mut run = w.ports.opened.lock().unwrap()[0].clone();
    run.budget_usd = budget;
    store.insert_bench_run(&run, Utc::now()).unwrap();
    drop(store);
    (w.job(job.id).await, run_dir)
}

fn bench_trial(store: &Store, run: Uuid, trial: &str, task: &str, cost: &[f64]) -> Uuid {
    let claim = crate::store::bench::NewBenchTask {
        trial: trial.into(),
        task: task.into(),
        compose_project: trial.into(),
        container: format!("{trial}-main-1"),
        workdir: "/app".into(),
        user: None,
        agent_timeout_sec: 900,
        request_digest: trial.into(),
    };
    store.claim_bench_task(run, &claim, Utc::now()).unwrap();
    let mut session = rsid_store::test_support::make_test_session();
    session.status = SessionStatus::Completed;
    store.insert_session(&session).unwrap();
    store
        .bind_bench_task_session(run, trial, session.id, "/nonexistent/trace.jsonl")
        .unwrap();
    for (n, usd) in cost.iter().enumerate() {
        store
            .insert_bench_invocation_for_test(
                session.id,
                n,
                "session.harness.turn",
                Some(*usd),
                (1000, 900, 50, 5),
            )
            .unwrap();
    }
    session.id
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn the_loop_pass_stops_a_running_run_from_its_real_spend_ledger() {
    let w = world();
    let (job, run_dir) = running_job(&w, 1.0).await;
    {
        let store = w.store.lock().await;
        bench_trial(&store, job.id, "fix-git-s0-r0", "fix-git", &[0.3, 0.3]);
    }
    assert_eq!(
        tick(&w.store, Utc::now()).await.unwrap(),
        0,
        "$0.60 of $1.00"
    );
    assert!(!run_dir.join(STOP_FILE).exists());
    {
        let store = w.store.lock().await;
        bench_trial(&store, job.id, "big-one-s0-r0", "big-one", &[0.45]);
    }
    assert_eq!(
        tick(&w.store, Utc::now()).await.unwrap(),
        1,
        "$1.05 of $1.00"
    );
    let stop: StopMarker =
        serde_json::from_slice(&std::fs::read(run_dir.join(STOP_FILE)).unwrap()).unwrap();
    assert!((stop.spent_usd - 1.05).abs() < 1e-9, "{}", stop.spent_usd);
    assert_eq!(tick(&w.store, Utc::now()).await.unwrap(), 0, "written once");
}

// ----------------------------------------------------------------- the finish

/// One runner outcome as the runner leaves it: a collected, cleaned trial with a valid reward.
fn outcome(run: Uuid, task: &str, slot: u32, mode: RunnerMode, reward: f64) -> TrialOutcomeV1 {
    TrialOutcomeV1 {
        schema_version: 1,
        identity: TrialIdentity {
            run_id: run.to_string(),
            task: task.to_string(),
            slot,
            retry: 0,
        },
        mode,
        reward_map: std::iter::once(("reward".to_string(), reward)).collect(),
        reward: Some(reward),
        reward_valid: true,
        oracle_exit_code: (mode == RunnerMode::Oracle).then_some(0),
        verifier_exit_code: Some(0),
        exceptions: Vec::new(),
        states: vec![
            TrialState::Prepared,
            TrialState::EnvironmentReady,
            TrialState::AgentRunning,
            TrialState::AgentSettled,
            TrialState::Verifying,
            TrialState::Collected,
            TrialState::Cleaned,
            TrialState::Terminal,
        ],
        phases: vec![PhaseTiming {
            state: TrialState::AgentRunning,
            deadline_ms: Some(900_000),
            elapsed_ms: 60_000,
        }],
        collection: CollectionStatus {
            attempted: true,
            collected: true,
            error: None,
        },
        cleanup: CleanupStatus {
            attempted: true,
            cleaned: true,
            error: None,
        },
        model_sessions: 0,
    }
}

/// Write `<run dir>/trials/<key>/outcome.json`, where the runner leaves it.
fn collect_outcome(run_dir: &Path, outcome: &TrialOutcomeV1) {
    let dir = run_dir.join("trials").join(trial_key(&outcome.identity));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("outcome.json"),
        serde_json::to_vec_pretty(outcome).unwrap(),
    )
    .unwrap();
}

async fn finished_world(w: &World) -> (AgentJobV1, PathBuf) {
    let (job, run_dir) = running_job(w, 5.0).await;
    let store = w.store.lock().await;
    let calls = [
        ("fix-git-s0-r0", "fix-git", &[0.01_f64, 0.02][..]),
        ("big-one-s0-r0", "big-one", &[0.03][..]),
    ];
    for (trial, task, cost) in calls {
        let session = bench_trial(&store, job.id, trial, task, cost);
        // The launch row of a real fresh launch settles under its own purpose.
        store
            .insert_invocation_purpose_for_test(session, 99, "session.launch.fresh")
            .unwrap();
        // The post-run route audit the bench task makes, for a real launch path.
        let verdict = crate::session::bench_route::audit_session_route(&store, session).unwrap();
        assert!(verdict.valid, "{verdict:?}");
        store
            .settle_bench_task(
                job.id,
                trial,
                &crate::store::bench::BenchSettlement {
                    state: rsi_common::bench::BenchTaskState::Finished,
                    stop_reason: Some("completed".into()),
                    cost_usd: Some(cost.iter().sum()),
                    prompt_tokens: 1000,
                    cached_tokens: 900,
                    output_tokens: 50,
                    reasoning_tokens: 5,
                    wall_ms: 60_000,
                    harness_build_digest: Some("sha256:build".into()),
                    trace_sha256: Some("sha256:trace".into()),
                    route_verdict: Some("valid".into()),
                    route_violations: Vec::new(),
                    finished_at: Utc::now(),
                },
            )
            .unwrap();
    }
    drop(store);
    (job, run_dir)
}

/// Run `finalize` for `job` with the given settled state and result, as the jobs loop does.
async fn finalize_job(
    w: &World,
    job: &AgentJobV1,
    state: &mut JobState,
    result: &mut AgentJobResultV1,
) {
    let store = w.store.lock().await;
    let job_row = store.get_agent_job(job.id).unwrap().unwrap().job;
    finalize(&store, &job_row, "", state, result);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_finished_run_joins_runner_outcomes_with_the_daemons_rows_into_the_record() {
    let w = world();
    let (job, run_dir) = finished_world(&w).await;
    collect_outcome(
        &run_dir,
        &outcome(job.id, "fix-git", 0, RunnerMode::Harness, 1.0),
    );
    collect_outcome(
        &run_dir,
        &outcome(job.id, "big-one", 0, RunnerMode::Harness, 0.0),
    );
    let mut state = JobState::Succeeded;
    let mut result = AgentJobResultV1 {
        exit_code: Some(0),
        ..AgentJobResultV1::default()
    };
    finalize_job(&w, &job, &mut state, &mut result).await;
    let bench = result.bench.expect("the run summary");
    assert_eq!(state, JobState::Succeeded);
    assert_eq!(bench.run_id, job.id.to_string());
    assert_eq!(bench.config, PAID);
    assert_eq!((bench.expected_trials, bench.trials), (2, 2));
    assert_eq!(
        (
            bench.valid_trials,
            bench.passed_trials,
            bench.invalid_trials
        ),
        (2, 1, 0)
    );
    assert_eq!(bench.not_run_trials, 0);
    assert_eq!(bench.stopped, None);
    assert_eq!(bench.spent_micro_usd, 60_000);
    assert_eq!(bench.budget_micro_usd, 5_000_000);
    assert_eq!(
        bench
            .shards
            .iter()
            .map(|s| (s.shard.as_str(), s.status.as_str(), s.trials))
            .collect::<Vec<_>>(),
        [("search-01", "ok", 1), ("search-02", "ok", 1)]
    );
    let record_path = run_dir.join("record.json");
    assert_eq!(
        bench.record_path.as_deref(),
        Some(record_path.display().to_string().as_str())
    );
    let record =
        BenchRunRecord::from_json(&std::fs::read_to_string(&record_path).unwrap()).unwrap();
    assert_eq!(
        bench.observation_fingerprint.as_deref(),
        Some(record.run.observation_fingerprint.as_str())
    );
    assert_eq!(record.run.harness_build_digest, "sha256:build");
    assert_eq!(
        record.run.config_digest,
        sha256_prefixed(fixtures::rsi_config().as_bytes())
    );
    assert_eq!(record.run.concurrency, 4);
    // The evaluator identity is the stamped runner's, never a Harbor adapter's.
    assert_eq!(
        record.run.adapter_tree_hash,
        fake_manifest().evaluator_digest()
    );
    assert_eq!(record.run.harbor_version, RUNNER_VERSION_DISCRIMINATOR);
    assert_eq!(
        record.run.sampling.get("runner"),
        Some(&fake_manifest().sampling_runner())
    );
    let fix = record.trials.iter().find(|t| t.task == "fix-git").unwrap();
    assert!(fix.passed);
    // Two turns plus the fresh launch's own admitted call: every admitted, completed call counts.
    assert_eq!(fix.turns, 3, "the model calls the daemon counted");
    assert!((fix.cost_usd - 0.03).abs() < 1e-9);
    // The file is private to the operator.
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&record_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

/// The $0 smoke: an oracle run records every trial with zero usage and no session, a valid
/// reward pass each, and the job result counts them.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn an_oracle_run_settles_a_v1_record_with_valid_passes_and_no_model_usage() {
    let w = world();
    *w.ports.cap.lock().unwrap() = cap("off");
    w.ports.launches.lock().unwrap().clear();
    let job = w.submit(ORACLE).await;
    assert_eq!(w.start().await, 1);
    let run_dir = w.ports.root.join(job.id.to_string());
    collect_outcome(
        &run_dir,
        &outcome(job.id, "fix-git", 0, RunnerMode::Oracle, 1.0),
    );
    let mut result = AgentJobResultV1 {
        exit_code: Some(0),
        ..AgentJobResultV1::default()
    };
    let mut state = classify_exit(Some(0), &mut result);
    finalize_job(&w, &job, &mut state, &mut result).await;
    assert_eq!(state, JobState::Succeeded);
    let bench = result.bench.expect("summary");
    assert_eq!(
        (bench.expected_trials, bench.trials, bench.valid_trials),
        (1, 1, 1)
    );
    assert_eq!((bench.passed_trials, bench.invalid_trials), (1, 0));
    assert_eq!((bench.spent_micro_usd, bench.not_run_trials), (0, 0));
    assert_eq!(bench.stopped, None);
    assert!(
        w.ports.opened.lock().unwrap().is_empty(),
        "no model run is ever opened"
    );
    let record =
        BenchRunRecord::from_json(&std::fs::read_to_string(run_dir.join("record.json")).unwrap())
            .unwrap();
    assert_eq!(record.schema_version, 1);
    assert_eq!(record.run.harness_build_digest, "none");
    assert_eq!(
        record.run.adapter_tree_hash,
        fake_manifest().evaluator_digest()
    );
    assert_eq!(record.run.sampling["agent"], "oracle");
    let trial = &record.trials[0];
    assert!(trial.passed && trial.reward_valid);
    assert_eq!((trial.cost_usd, trial.turns), (0.0, 0));
    assert_eq!(trial.session_id, None);
    assert_eq!(trial.trace_sha256, None);
    assert_eq!(record.run.totals.cost_usd, 0.0);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_budget_stop_is_in_the_receipt_and_the_record() {
    let w = world();
    let (job, run_dir) = finished_world(&w).await;
    collect_outcome(
        &run_dir,
        &outcome(job.id, "fix-git", 0, RunnerMode::Harness, 1.0),
    );
    // The second shard never started: the daemon had written the stop.
    std::fs::write(
        run_dir.join(STOP_FILE),
        serde_json::to_vec(&StopMarker {
            reason: "budget_exhausted".into(),
            spent_usd: 5.01,
            budget_usd: 5.0,
            at: Utc::now(),
        })
        .unwrap(),
    )
    .unwrap();
    let mut state = JobState::Succeeded;
    let mut result = AgentJobResultV1::default();
    finalize_job(&w, &job, &mut state, &mut result).await;
    let bench = result.bench.expect("summary");
    assert_eq!(bench.stopped.as_deref(), Some("budget_exhausted"));
    assert_eq!(bench.not_run_trials, 1);
    assert_eq!((bench.expected_trials, bench.trials), (2, 1));
    assert_eq!(
        bench
            .shards
            .iter()
            .map(|s| s.status.as_str())
            .collect::<Vec<_>>(),
        ["ok", "skipped"]
    );
    assert_eq!(
        state,
        JobState::Succeeded,
        "a bounded stop is not a failure"
    );
    assert!(
        result
            .detail
            .as_deref()
            .unwrap()
            .contains("budget_exhausted")
    );
    let record =
        BenchRunRecord::from_json(&std::fs::read_to_string(run_dir.join("record.json")).unwrap())
            .unwrap();
    let unrun = record.trials.iter().find(|t| t.task == "big-one").unwrap();
    assert_eq!(unrun.failure_cause.as_deref(), Some("budget_exhausted"));
    assert_eq!(unrun.stop_reason.as_deref(), Some("budget_exhausted"));
    assert!(!unrun.reward_valid && !unrun.passed);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_failed_runner_still_writes_the_partial_record_and_names_the_cause() {
    let w = world();
    let (job, run_dir) = finished_world(&w).await;
    collect_outcome(
        &run_dir,
        &outcome(job.id, "fix-git", 0, RunnerMode::Harness, 1.0),
    );
    let mut result = AgentJobResultV1 {
        exit_code: Some(22),
        ..AgentJobResultV1::default()
    };
    let mut state = crate::agent_jobs::bench::classify_exit(Some(22), &mut result);
    assert_eq!(state, JobState::Failed);
    assert_eq!(
        result.refusal.as_deref(),
        Some(rsi_common::agent_jobs::BENCH_TRANSPORT_FAILED)
    );
    finalize_job(&w, &job, &mut state, &mut result).await;
    let bench = result.bench.unwrap();
    assert_eq!(bench.stopped.as_deref(), Some("transport_failed"));
    assert_eq!(bench.not_run_trials, 1);
    assert_eq!(
        bench
            .shards
            .iter()
            .map(|s| s.status.as_str())
            .collect::<Vec<_>>(),
        ["ok", "skipped"]
    );
    assert!(run_dir.join("record.json").is_file());
    assert_eq!(
        state,
        JobState::Failed,
        "the exit code still decides the state"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_run_missing_trials_it_was_not_told_about_accounts_for_them_as_incomplete() {
    let w = world();
    let (job, run_dir) = finished_world(&w).await;
    collect_outcome(
        &run_dir,
        &outcome(job.id, "fix-git", 0, RunnerMode::Harness, 1.0),
    );
    let mut state = JobState::Succeeded;
    let mut result = AgentJobResultV1::default();
    finalize_job(&w, &job, &mut state, &mut result).await;
    let bench = result.bench.unwrap();
    assert_eq!(bench.stopped.as_deref(), Some("incomplete"));
    assert_eq!(bench.not_run_trials, 1);
    assert_eq!(bench.expected_trials, bench.trials + bench.not_run_trials);
}

/// Exit 2 is a finished run with a trial that did not pass or was not cleaned up. A trial that
/// was not cleaned is named in the result so the operator can remove what the runner left.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_trial_left_uncleaned_is_named_in_the_result_and_keeps_its_verdict() {
    let w = world();
    *w.ports.cap.lock().unwrap() = cap("off");
    let job = w.submit(ORACLE).await;
    assert_eq!(w.start().await, 1);
    let run_dir = w.ports.root.join(job.id.to_string());
    let mut dirty = outcome(job.id, "fix-git", 0, RunnerMode::Oracle, 1.0);
    dirty.cleanup = CleanupStatus {
        attempted: true,
        cleaned: false,
        error: Some("compose down timed out".into()),
    };
    collect_outcome(&run_dir, &dirty);
    let mut result = AgentJobResultV1::default();
    let mut state = classify_exit(Some(transport::EXIT_NOT_ALL_PASSED), &mut result);
    assert_eq!(state, JobState::Succeeded, "the outcomes are the result");
    assert_eq!(result.refusal, None);
    finalize_job(&w, &job, &mut state, &mut result).await;
    let detail = result.detail.unwrap();
    assert!(detail.contains("fix-git-s0-r0"), "{detail}");
    assert!(detail.contains("not cleaned up"), "{detail}");
    let bench = result.bench.unwrap();
    assert_eq!(bench.passed_trials, 1, "cleanup never changes the grade");
}

/// An infrastructure-interrupted attempt keeps its row but does not fill its slot, so the run
/// reads as incomplete until a replacement fills it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn an_infra_interrupted_attempt_keeps_its_row_without_filling_its_slot() {
    let w = world();
    *w.ports.cap.lock().unwrap() = cap("off");
    let job = w.submit(ORACLE).await;
    assert_eq!(w.start().await, 1);
    let run_dir = w.ports.root.join(job.id.to_string());
    let mut interrupted = outcome(job.id, "fix-git", 0, RunnerMode::Oracle, 0.0);
    interrupted.reward = None;
    interrupted.reward_valid = false;
    interrupted.exceptions = vec![TrialException {
        kind: TrialExceptionKind::InfraInterrupted,
        message: "ssh dropped".into(),
    }];
    collect_outcome(&run_dir, &interrupted);
    let mut state = JobState::Succeeded;
    let mut result = AgentJobResultV1::default();
    finalize_job(&w, &job, &mut state, &mut result).await;
    let bench = result.bench.unwrap();
    assert_eq!(bench.stopped.as_deref(), Some("incomplete"));
    assert_eq!(bench.infra_interrupted_trials, 1);
    assert_eq!(bench.passed_trials, 0);
    assert_eq!(bench.not_run_trials, 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_job_whose_inputs_are_gone_keeps_its_result_and_says_why() {
    let w = world();
    let job = w.submit(ORACLE).await;
    let mut state = JobState::Failed;
    let mut result = AgentJobResultV1 {
        detail: Some("unit output".into()),
        ..AgentJobResultV1::default()
    };
    let store = w.store.lock().await;
    let job_row = store.get_agent_job(job.id).unwrap().unwrap().job;
    finalize(&store, &job_row, "", &mut state, &mut result);
    assert!(result.bench.is_none());
    let detail = result.detail.unwrap();
    assert!(detail.contains("bench record not written"), "{detail}");
    assert!(detail.contains("unit output"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn runner_exits_settle_the_job_with_their_typed_refusal() {
    for (code, refusal) in [
        (0, None),
        (transport::EXIT_NOT_ALL_PASSED, None),
        (transport::EXIT_PREFLIGHT, Some(BENCH_PREFLIGHT_FAILED)),
        (transport::EXIT_PIN_CHECK, Some(BENCH_PIN_CHECK_FAILED)),
        (transport::EXIT_SHARD, Some(BENCH_TRANSPORT_FAILED)),
        (transport::EXIT_SSH, Some(BENCH_TRANSPORT_FAILED)),
        (137, Some(BENCH_TRANSPORT_FAILED)),
    ] {
        let mut result = AgentJobResultV1::default();
        let state = classify_exit(Some(code), &mut result);
        assert_eq!(result.refusal.as_deref(), refusal, "exit {code}");
        assert_eq!(
            state,
            if refusal.is_none() {
                JobState::Succeeded
            } else {
                JobState::Failed
            }
        );
    }
    let mut result = AgentJobResultV1::default();
    assert_eq!(classify_exit(None, &mut result), JobState::Failed);
}

/// The evaluator identity comes from the runner's stamp: a runner built from other sources is a
/// different evaluator, so its record is never compared with the first one's.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_different_runner_build_changes_the_evaluator_identity_of_the_record() {
    let digest_of = |manifest: RunnerSourceManifest| async move {
        let w = world();
        *w.ports.cap.lock().unwrap() = cap("off");
        *w.ports.runner.lock().unwrap() = Ok(RunnerBinary {
            path: w.ports.root.parent().unwrap().join("rsi-eval"),
            evaluator_digest: manifest.evaluator_digest(),
            manifest,
        });
        let job = w.submit(ORACLE).await;
        assert_eq!(w.start().await, 1);
        let run_dir = w.ports.root.join(job.id.to_string());
        collect_outcome(
            &run_dir,
            &outcome(job.id, "fix-git", 0, RunnerMode::Oracle, 1.0),
        );
        let mut state = JobState::Succeeded;
        let mut result = AgentJobResultV1::default();
        finalize_job(&w, &job, &mut state, &mut result).await;
        let record = BenchRunRecord::from_json(
            &std::fs::read_to_string(run_dir.join("record.json")).unwrap(),
        )
        .unwrap();
        (
            record.run.adapter_tree_hash,
            record.run.observation_fingerprint,
        )
    };
    let first = digest_of(fake_manifest()).await;
    let again = digest_of(fake_manifest()).await;
    assert_eq!(
        first, again,
        "the same runner build gives the same identity"
    );
    let changed = RunnerSourceManifest::from_entries([
        (
            "crates/rsi-eval/src/tb2_runner/runner.rs".to_string(),
            "c".repeat(64),
        ),
        (
            "crates/rsid/src/agent_jobs/bench.rs".to_string(),
            "b".repeat(64),
        ),
    ])
    .unwrap();
    let other = digest_of(changed).await;
    assert_ne!(first.0, other.0);
    assert_ne!(first.1, other.1, "the fingerprint follows the evaluator");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_run_whose_job_ended_is_closed_and_a_live_job_keeps_its_run() {
    let w = world();
    let (job, _) = running_job(&w, 5.0).await;
    assert!(ended_runs(&*w.store.lock().await).unwrap().is_empty());
    let store = w.store.lock().await;
    store
        .settle_agent_job_outcome(
            job.id,
            JobState::Failed,
            &AgentJobResultV1::default(),
            false,
            Utc::now(),
        )
        .unwrap();
    assert_eq!(
        ended_runs(&store).unwrap(),
        [(job.id, BenchRunState::Cancelled)]
    );
    // A job that succeeded finishes its run instead of cancelling it.
    let other = Uuid::new_v4();
    store
        .conn
        .execute(
            "UPDATE bench_runs SET job_id = ?1 WHERE id = ?2",
            rusqlite::params![other.to_string(), job.id.to_string()],
        )
        .unwrap();
    assert_eq!(
        ended_runs(&store).unwrap(),
        [(job.id, BenchRunState::Cancelled)],
        "a run whose job row is gone is cancelled"
    );
}

// ------------------------------------------------------- run overlays (#3191)

const CANDIDATE: &str = "eval/bench/tb2/candidates/c01-fixture/";

impl World {
    /// Commit a candidate (its overlay, its other files and a run config naming the
    /// overlay) under `eval/bench/tb2/candidates/<id>/`; returns the run config path.
    fn candidate(&self, id: &str, overlay_toml: &str, files: &[(&str, &str)]) -> String {
        let dir = format!("eval/bench/tb2/candidates/{id}/");
        self.repo.write(&format!("{dir}overlay.toml"), overlay_toml);
        for (name, text) in files {
            self.repo.write(&format!("{dir}{name}"), text);
        }
        let run = format!("{dir}run.toml");
        self.repo.write(
            &run,
            &fixtures::rsi_config().replace(
                "[agent]",
                &format!("overlay = \"{dir}overlay.toml\"\n\n[agent]"),
            ),
        );
        self.repo.commit(&format!("candidate {id}"));
        run
    }
}

const FULL_OVERLAY: &str = "instructions = \"instructions.md\"\n\
    [tool_policy]\ndenied_tools = [\"update_plan\"]\n\
    [context]\ncompact_budget_tokens = 96000\n\
    [loop]\nmax_iterations = 120\n";

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_candidate_run_audits_its_files_and_writes_the_validated_overlay_before_the_run_opens() {
    let w = world();
    let run = w.candidate(
        "c01-fixture",
        FULL_OVERLAY,
        &[
            ("instructions.md", "Plan first, then check.\n"),
            ("notes.md", "Why: fewer steps.\n"),
        ],
    );
    assert!(rsi_common::agent_jobs::bench_config_path_ok(&run));
    let job = w.submit(&run).await;

    // The audit ran once, over the overlay and its instructions (read by a model) and
    // also the notes (held-out terms only).
    let audits = w.ports.audits.lock().unwrap().clone();
    assert_eq!(audits.len(), 1);
    let (overlay_file, instructions, notes) = (
        format!("{CANDIDATE}overlay.toml"),
        format!("{CANDIDATE}instructions.md"),
        format!("{CANDIDATE}notes.md"),
    );
    assert_eq!(
        audits[0].model_files,
        [overlay_file.clone(), instructions.clone()]
    );
    assert_eq!(audits[0].audited_files, [overlay_file, instructions, notes]);

    assert_eq!(w.start().await, 1);
    assert_eq!(
        w.ports.audits.lock().unwrap().len(),
        2,
        "audited again at start"
    );
    let run_dir = w.ports.root.join(job.id.to_string());
    let written: rsi_common::bench_overlay::BenchOverlay =
        serde_json::from_slice(&std::fs::read(run_dir.join(crate::bench::OVERLAY_NAME)).unwrap())
            .unwrap();
    written.verify().expect("the written overlay verifies");
    assert_eq!(written.instructions_text(), Some("Plan first, then check."));
    assert_eq!(written.denied_tools, ["update_plan"]);
    assert_eq!(
        (written.compact_budget_tokens, written.max_iterations),
        (Some(96_000), Some(120))
    );
    // The run row keeps the config's own knobs; the overlay is layered per launch.
    let opened = w.ports.opened.lock().unwrap().clone();
    assert_eq!(opened[0].loop_knobs.max_iterations, Some(200));
    let effective = written.apply_knobs(opened[0].loop_knobs);
    assert_eq!(
        (effective.max_iterations, effective.compaction_budget_tokens),
        (Some(120), Some(96_000))
    );
    // The run config is kept beside the run, and names the overlay.
    let kept = std::fs::read_to_string(run_dir.join("inputs/config.toml")).unwrap();
    assert!(kept.contains("overlay = \"eval/bench/tb2/candidates/c01-fixture/overlay.toml\""));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_dry_run_prints_the_effective_prompt_digest_tools_and_knobs_without_a_model() {
    let w = world();
    let run = w.candidate(
        "c01-fixture",
        FULL_OVERLAY,
        &[("instructions.md", "Plan first, then check.\n")],
    );
    let report = dry_run_config(w.repo.path(), &run).unwrap();
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
    assert_eq!(report["max_iterations"], 120);
    assert_eq!(report["compact_budget_tokens"], 96_000);
    assert_eq!(report["rotation_off"], false);
    assert_eq!(
        report["tools"],
        serde_json::json!([
            "shell",
            "exec_command",
            "write_stdin",
            "read_output",
            "context_status",
            "compact"
        ]),
        "update_plan is denied"
    );
    let digest =
        crate::session::harness::base_prompt::bench_base_digest(Some("Plan first, then check."));
    assert_eq!(report["prompt_digest"], digest);
    assert_ne!(
        report["prompt_digest"],
        crate::session::harness::base_prompt::bench_digest()
    );
    assert!(
        report["overlay_digest"]
            .as_str()
            .is_some_and(|d| d.starts_with("sha256:"))
    );
    assert_eq!(
        report["files"].as_object().map(serde_json::Map::len),
        Some(1)
    );

    // The same config with no overlay: the pinned bench prompt, the full allowlist and the
    // config's own knobs.
    let plain = dry_run_config(w.repo.path(), PAID).unwrap();
    assert_eq!(
        plain["prompt_digest"],
        crate::session::harness::base_prompt::bench_digest()
    );
    assert_eq!(plain["max_iterations"], 200);
    assert_eq!(plain["tools"].as_array().map(Vec::len), Some(7));
    assert_eq!(plain["overlay_digest"], serde_json::Value::Null);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn each_overlay_breach_is_refused_with_its_typed_code_before_any_audit() {
    let w = world();
    let big = "a".repeat(64 * 1024 + 1);
    let cases: [(&str, &str, Vec<(&str, &str)>, &str); 10] = [
        ("unknown", "surprise = 1\n", vec![], "overlay_parse_failed"),
        (
            "range-loop",
            "[loop]\nmax_iterations = 5\n",
            vec![],
            "overlay_out_of_range",
        ),
        (
            "range-budget",
            "[context]\ncompact_budget_tokens = 2000001\n",
            vec![],
            "overlay_out_of_range",
        ),
        (
            "widened",
            "[tool_policy]\nenabled_tools = [\"shell\", \"web_search\"]\n",
            vec![],
            "overlay_tool_widened",
        ),
        (
            "web",
            "[tool_policy]\nweb_access = \"enabled\"\n",
            vec![],
            "overlay_field_not_allowed",
        ),
        (
            "range-compaction",
            "[compaction]\nkeep_recent = 3\n",
            vec![],
            "overlay_out_of_range",
        ),
        (
            "replay",
            "[reasoning_replay]\nmode = \"all\"\n",
            vec![],
            "overlay_field_not_supported",
        ),
        (
            "escape",
            "instructions = \"../c01/instructions.md\"\n",
            vec![],
            "overlay_path_invalid",
        ),
        (
            "missing",
            "instructions = \"instructions.md\"\n",
            vec![],
            "overlay_instructions_invalid",
        ),
        (
            "oversized",
            "instructions = \"instructions.md\"\n",
            vec![("instructions.md", big.as_str())],
            "overlay_instructions_invalid",
        ),
    ];
    for (id, overlay_toml, files, code) in cases {
        let run = w.candidate(id, overlay_toml, &files);
        for stage in [Stage::Submit, Stage::Start] {
            let refusal = w.admit(&run, stage).await.err().expect(id);
            assert_eq!(refusal.code, BENCH_OVERLAY_INVALID, "{id}");
            assert!(refusal.detail.starts_with(code), "{id}: {}", refusal.detail);
        }
    }
    assert!(
        w.ports.audits.lock().unwrap().is_empty(),
        "no audit of a bad overlay"
    );

    // A symlinked instructions file is not read, even when it points inside the tree.
    #[cfg(unix)]
    {
        let run = w.candidate(
            "linked",
            "instructions = \"instructions.md\"\n",
            &[("real.md", "Plan.\n")],
        );
        let link = w
            .repo
            .path()
            .join("eval/bench/tb2/candidates/linked/instructions.md");
        std::os::unix::fs::symlink("real.md", &link).unwrap();
        w.repo.commit("link");
        let refusal = w.admit(&run, Stage::Submit).await.err().unwrap();
        assert_eq!(refusal.code, BENCH_OVERLAY_INVALID);
        assert!(refusal.detail.starts_with("overlay_instructions_invalid"));
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn an_overlay_file_that_is_not_committed_and_clean_is_refused() {
    let w = world();
    let run = w.candidate(
        "c01-fixture",
        FULL_OVERLAY,
        &[("instructions.md", "Plan first, then check.\n")],
    );
    assert!(w.admit(&run, Stage::Submit).await.is_ok());
    let instructions = format!("{CANDIDATE}instructions.md");
    w.repo.write(&instructions, "Edited after the commit.\n");
    let refusal = w.admit(&run, Stage::Submit).await.err().unwrap();
    assert_eq!(refusal.code, BENCH_CONFIG_UNCOMMITTED);
    assert!(refusal.detail.contains(&instructions), "{}", refusal.detail);
    w.repo.git(&["checkout", "--", &instructions]);
    // Audit inputs are held to the same HEAD: modified pins prove nothing.
    w.repo.write(overlay::AUDIT_INPUTS[1], "{}\n");
    let refusal = w.admit(&run, Stage::Submit).await.err().unwrap();
    assert_eq!(refusal.code, BENCH_CONFIG_UNCOMMITTED);
    assert!(
        refusal.detail.contains(overlay::AUDIT_INPUTS[1]),
        "{}",
        refusal.detail
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn a_leak_audit_hit_or_failure_refuses_the_run_at_submit_and_at_start() {
    let w = world();
    let run = w.candidate(
        "c01-fixture",
        FULL_OVERLAY,
        &[("instructions.md", "Plan first, then check.\n")],
    );
    *w.ports.audit_verdict.lock().unwrap() = Err("HIT instructions.md:1 task-name 'x'".into());
    let refusal = w.admit(&run, Stage::Submit).await.err().unwrap();
    assert_eq!(refusal.code, BENCH_OVERLAY_LEAK_AUDIT_FAILED);
    assert!(refusal.detail.contains("HIT"), "{}", refusal.detail);

    // Clean at submit, a hit at start: the queued job settles with the typed refusal.
    *w.ports.audit_verdict.lock().unwrap() = Ok(());
    let job = w.submit(&run).await;
    *w.ports.audit_verdict.lock().unwrap() = Err("the leak audit could not run".into());
    let result = refused_start(&w, &job).await;
    assert_eq!(
        result.refusal.as_deref(),
        Some(BENCH_OVERLAY_LEAK_AUDIT_FAILED)
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn an_empty_overlay_is_the_same_run_as_no_overlay() {
    let w = world();
    let empty = w.candidate("c00-empty", "", &[]);
    let inputs = read_inputs(w.repo.path(), &empty).unwrap();
    assert!(inputs.overlay.is_some(), "the config names an overlay file");
    assert!(inputs.run_overlay().is_none(), "which sets nothing");
    let job = w.submit(&empty).await;
    assert_eq!(w.start().await, 1);
    let run_dir = w.ports.root.join(job.id.to_string());
    assert!(!run_dir.join(crate::bench::OVERLAY_NAME).exists());
    assert!(read_run_overlay(&run_dir).is_none());
    let opened = w.ports.opened.lock().unwrap().clone();
    assert_eq!(opened[0].loop_knobs.max_iterations, Some(200));

    // Everything a launch would use is the no-overlay run's.
    let (with, without) = (
        dry_run_config(w.repo.path(), &empty).unwrap(),
        dry_run_config(w.repo.path(), PAID).unwrap(),
    );
    for key in [
        "prompt_digest",
        "tools",
        "max_iterations",
        "compact_budget_tokens",
        "rotation_off",
        "overlay_digest",
        "files",
    ] {
        assert_eq!(with[key], without[key], "{key}");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[tokio::test]
async fn the_real_leak_audit_flags_held_out_and_search_names_in_any_candidate_file() {
    let root = Path::new(rsi_common::manifest_dir!()).join("../..");
    let tree = crate::test_support::disk_backed_tempdir("bench-audit");
    for name in ["dataset.toml", "split.json"] {
        let dest = tree.path().join("eval/bench/tb2").join(name);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(root.join("eval/bench/tb2").join(name), dest).unwrap();
    }
    let split: serde_json::Value = serde_json::from_slice(
        &std::fs::read(tree.path().join("eval/bench/tb2/split.json")).unwrap(),
    )
    .unwrap();
    let (held_out, search) = (
        split["held_out"][0].as_str().unwrap().to_string(),
        split["search"][0].as_str().unwrap().to_string(),
    );
    let dir = tree.path().join("eval/bench/tb2/candidates/c9");
    std::fs::create_dir_all(&dir).unwrap();
    // A single-quoted path: the script's own directory mode would miss this file.
    std::fs::write(
        dir.join("overlay.toml"),
        "instructions = 'instructions.md'\n",
    )
    .unwrap();
    let write = |name: &str, text: String| std::fs::write(dir.join(name), text).unwrap();
    let rel = |name: &str| format!("eval/bench/tb2/candidates/c9/{name}");
    let plan = |with_notes: bool| overlay::AuditPlan {
        model_files: vec![rel("overlay.toml"), rel("instructions.md")],
        audited_files: [rel("overlay.toml"), rel("instructions.md")]
            .into_iter()
            .chain(with_notes.then(|| rel("notes.md")))
            .collect(),
        checkout: None,
    };
    let audit = |plan: overlay::AuditPlan| {
        let cwd = tree.path().to_path_buf();
        async move {
            tokio::task::spawn_blocking(move || overlay::audit(&cwd, &plan))
                .await
                .unwrap()
        }
    };

    write(
        "instructions.md",
        "Plan first, then check your work.\n".into(),
    );
    audit(plan(false)).await.expect("a clean candidate passes");

    write("instructions.md", format!("Use the {held_out} approach.\n"));
    let error = audit(plan(false)).await.expect_err("a held-out name");
    assert!(
        error.contains("HIT") && error.contains("task-name"),
        "{error}"
    );

    write("instructions.md", format!("Tune for {search} first.\n"));
    let error = audit(plan(false)).await.expect_err("a search name");
    assert!(error.contains("GENERALITY"), "{error}");

    // Notes are audited for held-out content only: a search name there is allowed.
    write("instructions.md", "Plan first.\n".into());
    write(
        "notes.md",
        format!("Worked on {search} in the search split.\n"),
    );
    audit(plan(true))
        .await
        .expect("notes may cite search tasks");
    write("notes.md", format!("Looked at {held_out}.\n"));
    let error = audit(plan(true)).await.expect_err("held-out in notes");
    assert!(error.contains("HIT"), "{error}");

    // A missing audit input cannot pass a candidate.
    std::fs::remove_file(tree.path().join("eval/bench/tb2/split.json")).unwrap();
    assert!(audit(plan(false)).await.is_err(), "fail closed");
}

/// #3191: the overlay's ranges are the operator settings' own, so an overlay can never
/// ask for a value the setting would refuse (and the two cannot drift apart).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-01"))]
#[test]
fn the_overlay_ranges_are_the_operator_settings_ranges() {
    use rsi_common::bench_overlay as o;
    assert_eq!(
        (o::OVERLAY_COMPACT_BUDGET_MIN, o::OVERLAY_COMPACT_BUDGET_MAX),
        (
            crate::config::OPENROUTER_CONTEXT_BUDGET_TOKENS_MIN,
            crate::config::OPENROUTER_CONTEXT_BUDGET_TOKENS_MAX
        )
    );
    assert_eq!(
        (o::OVERLAY_MAX_ITERATIONS_MIN, o::OVERLAY_MAX_ITERATIONS_MAX),
        (
            crate::config::HARNESS_MAX_ITERATIONS_PER_TURN_MIN,
            crate::config::HARNESS_MAX_ITERATIONS_PER_TURN_MAX
        )
    );
}
