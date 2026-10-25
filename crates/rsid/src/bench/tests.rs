//! Tests of the bench adapter (#3178): the listener, the start gates, idempotency,
//! usage from increments, cancel, finish, the trace pages and the restart.
//!
//! The host is a double: it records every launch and interrupt and stands in for
//! the session manager, the container validator and the M4 budget enforcer (cap
//! off, so the run budget is judged from the store's increment SQL alone).

use super::*;
use crate::store::bench::{BenchRunTask, NewBenchTask};
use rsi_common::bench::{
    BENCH_CONCURRENCY_EXCEEDED, BENCH_LAUNCH_NOT_ALLOWED, BENCH_METHOD_NOT_ALLOWED,
    BENCH_RUN_BUDGET_INSUFFICIENT, BENCH_RUN_NOT_ACTIVE, BENCH_TASK_NOT_FINISHED,
    BENCH_TASK_UNKNOWN, BENCH_TRIAL_CONFLICT, BENCH_TRIAL_UNKNOWN, BenchTaskStartParams,
    BenchTaskState, BenchTaskStatusV1, BenchTraceReadParams,
};
use rsi_common::bench_overlay::{BenchOverlay, RawOverlay};
use rsi_common::bench_profile::LoopKnobs;
use rsi_common::exec_target::ExecTarget;
use rsi_common::openrouter_spend::{SpendInputs, evaluate, run_budget_report};
use rsi_common::types::{SessionProvider, SessionStatus};
use sha2::{Digest, Sha256};
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};

const MODEL: &str = "deepseek/deepseek-v4.1-flash";
const DIGEST_A: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DIGEST_B: &str = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct FakeHost {
    store: Arc<rsid_core::store_lock::Mutex<Store>>,
    launches: StdMutex<Vec<LaunchConfig>>,
    interrupts: StdMutex<Vec<Uuid>>,
    validated: StdMutex<Vec<ContainerExecTarget>>,
    container_error: StdMutex<Option<String>>,
    grants: StdMutex<Vec<ManagerLaunchChoiceV2>>,
    groups: StdMutex<Vec<Uuid>>,
    fail_launch: AtomicBool,
}

impl FakeHost {
    fn new(store: Arc<rsid_core::store_lock::Mutex<Store>>) -> Arc<Self> {
        Arc::new(Self {
            store,
            launches: StdMutex::new(Vec::new()),
            interrupts: StdMutex::new(Vec::new()),
            validated: StdMutex::new(Vec::new()),
            container_error: StdMutex::new(None),
            grants: StdMutex::new(vec![ManagerLaunchChoiceV2 {
                provider: SessionProvider::OpenRouter,
                model: MODEL.into(),
                effort: Some("high".into()),
            }]),
            groups: StdMutex::new(Vec::new()),
            fail_launch: AtomicBool::new(false),
        })
    }

    fn launched(&self) -> Vec<LaunchConfig> {
        self.launches.lock().unwrap().clone()
    }
}

#[async_trait]
impl BenchHost for FakeHost {
    async fn launch(&self, config: LaunchConfig) -> Result<Uuid> {
        if self.fail_launch.load(Ordering::SeqCst) {
            return Err(DaemonError::PolicyDenied(
                "bench_route_required:route_not_harness".into(),
            ));
        }
        let id = Uuid::new_v4();
        let mut session = rsid_store::test_support::test_session(
            id,
            config.working_dir.clone().unwrap_or_default(),
        );
        session.status = SessionStatus::Running;
        session.stop_reason = None;
        self.store.lock().await.insert_session(&session)?;
        self.launches.lock().unwrap().push(config);
        Ok(id)
    }

    async fn interrupt(&self, session_id: Uuid) -> Result<()> {
        self.interrupts.lock().unwrap().push(session_id);
        let store = self.store.lock().await;
        if store
            .get_session(session_id)?
            .is_some_and(|session| !session.status.is_terminal())
        {
            store.set_session_terminal_status(
                session_id,
                SessionStatus::Interrupted,
                "interrupted:operator",
            )?;
        }
        Ok(())
    }

    async fn create_group(&self, project: Uuid, name: String, tags: Vec<String>) -> Result<Uuid> {
        let id = Uuid::new_v4();
        let mut group = rsid_store::test_support::test_session(id, std::path::PathBuf::new());
        group.session_kind = rsi_common::types::SessionKind::Group;
        group.project_id = Some(project);
        group.title = Some(name);
        group.tag = tags.first().cloned().unwrap_or_default();
        group.tags = tags;
        let store = self.store.lock().await;
        store.insert_session(&group)?;
        // The daemon's `create_container` persists tags in `session_tags` too.
        for tag in &group.tags {
            store
                .conn
                .execute(
                    "INSERT INTO session_tags (session_id, tag) VALUES (?1, ?2)",
                    rusqlite::params![id.to_string(), tag],
                )
                .unwrap();
        }
        drop(store);
        self.groups.lock().unwrap().push(id);
        Ok(id)
    }

    async fn validate_container(
        &self,
        target: &ContainerExecTarget,
    ) -> std::result::Result<(), String> {
        self.validated.lock().unwrap().push(target.clone());
        match self.container_error.lock().unwrap().clone() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn allowed_launches(&self, _: &Store, _: Uuid) -> Result<Vec<ManagerLaunchChoiceV2>> {
        Ok(self.grants.lock().unwrap().clone())
    }

    async fn run_budget(&self, budget: f64, sessions: &[Uuid]) -> Result<RunBudgetReportV1> {
        let spent = self
            .store
            .lock()
            .await
            .sessions_invocation_spend(sessions)?;
        let off = evaluate(&SpendInputs {
            cap_usd: 0.0,
            warn_pct: 80,
            baseline: None,
            probe: None,
            since_probe_usd: 0.0,
            now: chrono::Utc::now(),
        });
        Ok(run_budget_report(&off, budget, spent))
    }
}

struct World {
    service: Arc<BenchService>,
    host: Arc<FakeHost>,
    store: Arc<rsid_core::store_lock::Mutex<Store>>,
    root: tempfile::TempDir,
}

fn new_run(id: Uuid) -> NewBenchRun {
    NewBenchRun {
        id,
        project_id: Uuid::new_v4(),
        job_id: Some("job-1".into()),
        config_path: "eval/bench/tb2/runs/smoke.toml".into(),
        config_digest: format!("sha256:{}", "c".repeat(64)),
        dataset_commit: "2fd12b88aafdd04a52c298e3940bcb189f9766d6".into(),
        docker_host: "ssh://arch-laptop".into(),
        model: MODEL.into(),
        effort: "high".into(),
        budget_usd: 1.0,
        per_task_cap_usd: 0.4,
        concurrency: 2,
        tasks: vec![
            BenchRunTask {
                task: "fix-git".into(),
                pinned_digests: vec![DIGEST_A.into()],
            },
            BenchRunTask {
                task: "regex-log".into(),
                pinned_digests: vec![DIGEST_B.into()],
            },
        ],
        loop_knobs: LoopKnobs {
            rotation_off: true,
            ..LoopKnobs::default()
        },
    }
}

async fn world() -> World {
    let store = Arc::new(rsid_core::store_lock::Mutex::new(
        Store::open_in_memory().unwrap(),
    ));
    let host = FakeHost::new(Arc::clone(&store));
    // Unix socket paths are short: keep the root under /tmp.
    let root = tempfile::Builder::new()
        .prefix("rb")
        .tempdir_in("/tmp")
        .unwrap();
    let service = BenchService::new(
        Arc::clone(&store),
        Arc::clone(&host) as Arc<dyn BenchHost>,
        root.path().join("bench"),
    );
    World {
        service,
        host,
        store,
        root,
    }
}

fn start_params(task: &str, trial: &str) -> BenchTaskStartParams {
    BenchTaskStartParams {
        task: task.into(),
        trial: trial.into(),
        compose_project: format!("{trial}-proj"),
        container: format!("{trial}-main-1"),
        workdir: "/app".into(),
        user: Some("agent".into()),
        instruction: "Fix the repository.".into(),
        agent_timeout_sec: 900,
    }
}

async fn start(world: &World, run: Uuid, task: &str, trial: &str) -> BenchTaskStatusV1 {
    world
        .service
        .start_task(run, start_params(task, trial))
        .await
        .unwrap_or_else(|e| panic!("start {trial}: {e}"))
}

fn session_of(status: &BenchTaskStatusV1) -> Uuid {
    Uuid::parse_str(status.session_id.as_deref().unwrap()).unwrap()
}

async fn rpc(path: &Path, method: &str, params: serde_json::Value) -> serde_json::Value {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let stream = tokio::net::UnixStream::connect(path).await.unwrap();
    let (reader, mut writer) = stream.into_split();
    let request = rsi_common::rpc::RpcRequest::new(method, params);
    let mut line = serde_json::to_string(&request).unwrap();
    line.push('\n');
    writer.write_all(line.as_bytes()).await.unwrap();
    let mut response = String::new();
    BufReader::new(reader)
        .read_line(&mut response)
        .await
        .unwrap();
    serde_json::from_str(&response).unwrap()
}

fn error_code(response: &serde_json::Value) -> String {
    response["error"]["data"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("a typed error, got {response}"))
        .to_owned()
}

fn refusal_code(error: &BenchError) -> &str {
    &error.code
}

async fn spend(world: &World, session: Uuid, n: usize, cost: Option<f64>) {
    world
        .store
        .lock()
        .await
        .insert_bench_invocation_for_test(
            session,
            n,
            "session.harness.turn",
            cost,
            (1000, 400, 50, 10),
        )
        .unwrap();
}

async fn end_session(world: &World, session: Uuid, reason: &str) {
    world
        .store
        .lock()
        .await
        .set_session_terminal_status(session, SessionStatus::Failed, reason)
        .unwrap();
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn the_listener_serves_only_the_five_bench_methods_for_its_own_run() {
    let w = world().await;
    let (run_a, run_b) = (Uuid::new_v4(), Uuid::new_v4());
    let socket_a = w.service.open_run(new_run(run_a)).await.unwrap();
    w.service.open_run(new_run(run_b)).await.unwrap();
    assert_eq!(socket_a, w.service.socket_path(run_a));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(socket_a.parent().unwrap()), 0o700, "the run directory");
        assert_eq!(mode(&socket_a), 0o600, "the socket");
    }

    // Every daemon method is refused, whatever it is.
    for method in [
        "GetSession",
        "LaunchSession",
        "AgentSpawnChild",
        "ListSessions",
        "bench",
    ] {
        let response = rpc(&socket_a, method, serde_json::json!({})).await;
        assert_eq!(response["error"]["code"], rsi_common::rpc::METHOD_NOT_FOUND);
        assert_eq!(error_code(&response), BENCH_METHOD_NOT_ALLOWED, "{method}");
    }

    // The run comes from the socket: a trial of run B is not visible on A's.
    start(&w, run_b, "fix-git", "b-trial").await;
    let response = rpc(
        &socket_a,
        "BenchTaskStatus",
        serde_json::json!({"trial": "b-trial"}),
    )
    .await;
    assert_eq!(error_code(&response), BENCH_TRIAL_UNKNOWN);
    // ... and a request cannot name another run.
    let response = rpc(
        &socket_a,
        "BenchTaskStatus",
        serde_json::json!({"trial": "b-trial", "run_id": run_b}),
    )
    .await;
    assert_eq!(error_code(&response), "bench_invalid_request");
    let response = rpc(
        &w.service.socket_path(run_b),
        "BenchTaskStatus",
        serde_json::json!({"trial": "b-trial"}),
    )
    .await;
    assert_eq!(response["result"]["state"], "running");

    // A start over the socket works and is scoped to the run.
    let response = rpc(
        &socket_a,
        "BenchTaskStart",
        serde_json::to_value(start_params("fix-git", "a-trial")).unwrap(),
    )
    .await;
    assert_eq!(response["result"]["run_id"], run_a.to_string());

    // The socket exists only while the run is active.
    assert!(
        w.service
            .end_run(run_a, BenchRunState::Finished)
            .await
            .unwrap()
    );
    assert!(!socket_a.exists());
    assert!(!w.service.is_bound(run_a).await);
    assert!(tokio::net::UnixStream::connect(&socket_a).await.is_err());
    let refused = w
        .service
        .start_task(run_a, start_params("fix-git", "late"))
        .await
        .unwrap_err();
    assert_eq!(refusal_code(&refused), BENCH_RUN_NOT_ACTIVE);
    assert!(w.service.is_bound(run_b).await);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn each_start_refusal_carries_its_typed_code_and_launches_nothing() {
    let w = world().await;
    let run = Uuid::new_v4();
    let mut cfg = new_run(run);
    cfg.concurrency = 4;
    w.service.open_run(cfg).await.unwrap();

    // Malformed request.
    let mut bad = start_params("fix-git", "t");
    bad.workdir = "app".into();
    let error = w.service.start_task(run, bad).await.unwrap_err();
    assert_eq!(refusal_code(&error), "bench_invalid_request");

    // A task outside the run's pinned set.
    let error = w
        .service
        .start_task(run, start_params("not-pinned", "t"))
        .await
        .unwrap_err();
    assert_eq!(refusal_code(&error), BENCH_TASK_UNKNOWN);

    // A run that does not exist reads as not active.
    let error = w
        .service
        .start_task(Uuid::new_v4(), start_params("fix-git", "t"))
        .await
        .unwrap_err();
    assert_eq!(refusal_code(&error), BENCH_RUN_NOT_ACTIVE);

    // `allowed_launches` must cover (OpenRouter, model, effort).
    let granted = std::mem::take(&mut *w.host.grants.lock().unwrap());
    *w.host.grants.lock().unwrap() = vec![ManagerLaunchChoiceV2 {
        provider: SessionProvider::OpenRouter,
        model: MODEL.into(),
        effort: Some("low".into()),
    }];
    let error = w
        .service
        .start_task(run, start_params("fix-git", "t"))
        .await
        .unwrap_err();
    assert_eq!(refusal_code(&error), BENCH_LAUNCH_NOT_ALLOWED);
    *w.host.grants.lock().unwrap() = Vec::new();
    let error = w
        .service
        .start_task(run, start_params("fix-git", "t"))
        .await
        .unwrap_err();
    assert_eq!(
        refusal_code(&error),
        BENCH_LAUNCH_NOT_ALLOWED,
        "an empty grant fails closed"
    );
    *w.host.grants.lock().unwrap() = granted;

    // An invalid container is refused with the validator's own code.
    *w.host.container_error.lock().unwrap() =
        Some("exec_target_invalid:image_digest_not_pinned".into());
    let error = w
        .service
        .start_task(run, start_params("fix-git", "t"))
        .await
        .unwrap_err();
    assert_eq!(refusal_code(&error), "exec_target_invalid");
    assert!(error.message().contains("image_digest_not_pinned"));
    *w.host.container_error.lock().unwrap() = None;
    assert!(
        w.host.launched().is_empty(),
        "no refusal launched a session"
    );
    assert!(
        w.store
            .lock()
            .await
            .list_bench_tasks(run)
            .unwrap()
            .is_empty(),
        "no refusal left a trial row"
    );
    // The validated target is the run's: its host, the task's pinned digests.
    let ok = start(&w, run, "regex-log", "ok").await;
    assert_eq!(ok.state, BenchTaskState::Running);
    let validated = w.host.validated.lock().unwrap().last().cloned().unwrap();
    assert_eq!(validated.docker_host, "ssh://arch-laptop");
    assert_eq!(validated.pinned_digests, [DIGEST_B]);
    assert_eq!(validated.compose_project, "ok-proj");

    // A launch the session layer refuses settles the trial and reports the code.
    w.host.fail_launch.store(true, Ordering::SeqCst);
    let error = w
        .service
        .start_task(run, start_params("fix-git", "failed"))
        .await
        .unwrap_err();
    assert_eq!(refusal_code(&error), "bench_route_required");
    let status = w.service.task_status(run, "failed").await.unwrap();
    assert_eq!(status.state, BenchTaskState::Cancelled);
    assert_eq!(
        status.stop_reason.as_deref(),
        Some("bench_start_failed:bench_route_required")
    );
    w.host.fail_launch.store(false, Ordering::SeqCst);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn the_run_budget_and_the_concurrency_cap_refuse_a_start() {
    let w = world().await;
    let run = Uuid::new_v4();
    let mut cfg = new_run(run);
    cfg.concurrency = 3;
    w.service.open_run(cfg).await.unwrap();

    // Budget $1.00, cap $0.40. The first trial spends $0.50.
    let a = start(&w, run, "fix-git", "a").await;
    spend(&w, session_of(&a), 1, Some(0.50)).await;
    // $0.50 left, the live trial may spend nothing more than its cap's rest
    // (cap exceeded already, so 0): room for one more task's cap.
    let b = start(&w, run, "regex-log", "b").await;
    // Now $0.50 left but b may still spend its full $0.40: $0.10 < the cap.
    let error = w
        .service
        .start_task(run, start_params("fix-git", "c"))
        .await
        .unwrap_err();
    assert_eq!(refusal_code(&error), BENCH_RUN_BUDGET_INSUFFICIENT);
    assert!(
        w.store
            .lock()
            .await
            .get_bench_task(run, "c")
            .unwrap()
            .is_none()
    );

    // A spent budget is the M4 refusal, passed through.
    spend(&w, session_of(&b), 1, Some(0.55)).await;
    let error = w
        .service
        .start_task(run, start_params("fix-git", "d"))
        .await
        .unwrap_err();
    assert_eq!(refusal_code(&error), "openrouter_run_budget_exhausted");

    // Concurrency: a fresh run with a cap of one.
    let narrow = Uuid::new_v4();
    let mut cfg = new_run(narrow);
    cfg.concurrency = 1;
    w.service.open_run(cfg).await.unwrap();
    start(&w, narrow, "fix-git", "n1").await;
    let error = w
        .service
        .start_task(narrow, start_params("regex-log", "n2"))
        .await
        .unwrap_err();
    assert_eq!(refusal_code(&error), BENCH_CONCURRENCY_EXCEEDED);
    assert!(error.retryable);
    // A finished trial frees its slot.
    let n1 = w.service.task_status(narrow, "n1").await.unwrap();
    end_session(&w, session_of(&n1), "completed").await;
    assert_eq!(
        w.service.task_status(narrow, "n1").await.unwrap().state,
        BenchTaskState::Finished
    );
    start(&w, narrow, "regex-log", "n2").await;
}

/// Two starts that each fit the remainder alone, but not together: only one
/// may reserve its per-task cap (the admission is atomic with the claim).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_starts_cannot_both_reserve_the_same_run_budget_remainder() {
    let w = world().await;
    for round in 0..8 {
        let run = Uuid::new_v4();
        let mut cfg = new_run(run);
        // $1.00 left, $0.60 per task: room for one task's cap, not two.
        cfg.budget_usd = 1.0;
        cfg.per_task_cap_usd = 0.6;
        cfg.concurrency = 2;
        w.service.open_run(cfg).await.unwrap();

        let first = {
            let service = Arc::clone(&w.service);
            let trial = format!("a{round}");
            tokio::spawn(async move {
                service
                    .start_task(run, start_params("fix-git", &trial))
                    .await
            })
        };
        let second = {
            let service = Arc::clone(&w.service);
            let trial = format!("b{round}");
            tokio::spawn(async move {
                service
                    .start_task(run, start_params("regex-log", &trial))
                    .await
            })
        };
        let results = [first.await.unwrap(), second.await.unwrap()];
        let admitted = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(admitted, 1, "round {round}: exactly one start is admitted");
        let refused = results.iter().find_map(|r| r.as_ref().err()).unwrap();
        assert_eq!(refusal_code(refused), BENCH_RUN_BUDGET_INSUFFICIENT);
        let live = w
            .store
            .lock()
            .await
            .list_bench_tasks(run)
            .unwrap()
            .iter()
            .filter(|t| t.state.is_live())
            .count();
        assert_eq!(live, 1);
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn start_is_idempotent_and_launches_a_bench_eval_session_in_an_empty_scratch_dir() {
    let w = world().await;
    let run = Uuid::new_v4();
    let cfg = new_run(run);
    let per_task_cap = cfg.per_task_cap_usd;
    w.service.open_run(cfg).await.unwrap();

    let first = start(&w, run, "fix-git", "fix-git__abc").await;
    let again = start(&w, run, "fix-git", "fix-git__abc").await;
    let same = |a: &BenchTaskStatusV1, b: &BenchTaskStatusV1| {
        let (mut a, mut b) = (a.clone(), b.clone());
        (a.usage.wall_ms, b.usage.wall_ms) = (0, 0);
        a == b
    };
    assert!(same(&first, &again), "a repeat answers the existing trial");
    assert_eq!(w.host.launched().len(), 1, "one trial, one session");
    assert_eq!((first.attempt, first.state), (1, BenchTaskState::Running));

    // The same trial id with other parameters is a conflict, not a relaunch.
    let mut other = start_params("fix-git", "fix-git__abc");
    other.instruction = "Something else.".into();
    let error = w.service.start_task(run, other).await.unwrap_err();
    assert_eq!(refusal_code(&error), BENCH_TRIAL_CONFLICT);
    assert_eq!(w.host.launched().len(), 1);

    // A second trial of the same task is attempt 2.
    let second = start(&w, run, "fix-git", "fix-git__def").await;
    assert_eq!(second.attempt, 2);

    let config = w.host.launched().remove(0);
    assert!(config.is_eval && config.skip_context_pipeline);
    assert_eq!(config.provider, Some(SessionProvider::OpenRouter));
    assert_eq!(config.model.as_deref(), Some(MODEL));
    assert_eq!(config.effort.as_deref(), Some("high"));
    assert_eq!(config.query, "Fix the repository.");
    assert_eq!(config.system_prompt, None);
    assert!(config.loop_knobs.rotation_off);
    let ExecTarget::Container(target) = &config.exec_target else {
        panic!("a container target");
    };
    assert_eq!(target.container, "fix-git__abc-main-1");
    assert_eq!(target.compose_project, "fix-git__abc-proj");
    assert_eq!(target.workdir, "/app");
    assert_eq!(target.user.as_deref(), Some("agent"));
    assert_eq!(
        target.pinned_digests,
        [DIGEST_A],
        "the task's own pinned digests"
    );
    let profile = config.bench().expect("the bench profile");
    assert_eq!(profile.task_cap_micro_usd, Some(micro(per_task_cap)));
    let trace_path = PathBuf::from(profile.trace_path.clone().unwrap());
    assert_eq!(
        trace_path,
        w.service.trial_dir(run, "fix-git__abc").join("trace.jsonl")
    );
    let scratch = config.working_dir.clone().unwrap();
    assert_eq!(
        scratch,
        w.service.trial_dir(run, "fix-git__abc").join("scratch")
    );
    assert!(scratch.is_dir());
    assert_eq!(std::fs::read_dir(&scratch).unwrap().count(), 0, "empty");
    // The row is bound to the session and the trace path.
    let row = w
        .store
        .lock()
        .await
        .get_bench_task(run, "fix-git__abc")
        .unwrap()
        .unwrap();
    assert_eq!(row.session_id, Some(session_of(&first)));
    assert_eq!(
        row.trace_path.as_deref(),
        Some(trace_path.to_str().unwrap())
    );
}

/// #3349: a trace that lost a line is never settled as a normal completion,
/// whatever ended the session (the loop cannot see a failed final append).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_trial_whose_trace_lost_a_line_settles_with_the_typed_reason() {
    let w = world().await;
    let run = Uuid::new_v4();
    w.service.open_run(new_run(run)).await.unwrap();
    let started = start(&w, run, "fix-git", "t1").await;
    let session = session_of(&started);
    let trace_path = PathBuf::from(started.trace.as_ref().unwrap().path.clone());
    std::fs::write(&trace_path, "{\"seq\":0,\"kind\":\"model_request\"}\n").unwrap();
    // The sink that writes this file lost its last line.
    crate::bench::trace::TraceSink::open(&trace_path)
        .unwrap()
        .mark_failed_for_test();
    // The session ended as if it had completed.
    w.store
        .lock()
        .await
        .set_session_terminal_status(session, SessionStatus::Completed, "completed")
        .unwrap();
    let done = w.service.finish_task(run, "t1").await.unwrap();
    assert_eq!(
        done.stop_reason.as_deref(),
        Some(rsi_common::bench::BENCH_TRACE_WRITE_FAILED)
    );
}

fn micro(usd: f64) -> u64 {
    (usd * 1_000_000.0).round() as u64
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn usage_comes_from_increments_and_finish_seals_the_trace_hash() {
    let w = world().await;
    let run = Uuid::new_v4();
    w.service.open_run(new_run(run)).await.unwrap();
    let started = start(&w, run, "fix-git", "t1").await;
    let session = session_of(&started);

    // Two session-cumulative rows (running totals 0.10 then 0.25, the stored
    // figure of a launch row) and one turn of its own cost 0.05: the spend is
    // 0.25 + 0.05, never 0.10 + 0.25 + 0.05.
    {
        let store = w.store.lock().await;
        for (n, purpose, cost) in [
            (1, "session.launch.fresh", 0.10),
            (2, "session.launch.fresh", 0.25),
            (3, "session.harness.turn", 0.05),
        ] {
            store
                .insert_bench_invocation_for_test(
                    session,
                    n,
                    purpose,
                    Some(cost),
                    (1000, 400, 50, 10),
                )
                .unwrap();
        }
    }
    let live = w.service.task_status(run, "t1").await.unwrap();
    assert_eq!(live.state, BenchTaskState::Running);
    let cost = live.usage.cost_usd.unwrap();
    assert!((cost - 0.30).abs() < 1e-9, "increments, got {cost}");
    assert_eq!(live.usage.calls, 3);
    assert_eq!(
        (
            live.usage.prompt_tokens,
            live.usage.cached_tokens,
            live.usage.output_tokens
        ),
        (3000, 1200, 150)
    );
    assert_eq!(
        live.route_verdict, None,
        "no verdict while the session runs"
    );

    // Finish before the session ends is retryable.
    let early = w.service.finish_task(run, "t1").await.unwrap_err();
    assert_eq!(refusal_code(&early), BENCH_TASK_NOT_FINISHED);
    assert!(early.retryable);

    // The trace the loop wrote, then the session ends on the per-task cap.
    let trace_path = PathBuf::from(live.trace.as_ref().unwrap().path.clone());
    let body = "{\"seq\":0,\"kind\":\"model_request\"}\n{\"seq\":1,\"kind\":\"model_response\"}\n";
    std::fs::write(&trace_path, body).unwrap();
    end_session(&w, session, rsi_common::bench::BENCH_TASK_BUDGET_EXHAUSTED).await;
    let done = w.service.finish_task(run, "t1").await.unwrap();
    assert_eq!(done.state, BenchTaskState::Finished);
    assert_eq!(
        done.stop_reason.as_deref(),
        Some("bench_task_budget_exhausted"),
        "the capped session is graded normally: finished, with the cap as the reason"
    );
    assert_eq!(done.route_verdict.as_deref(), Some("valid"));
    assert!((done.usage.cost_usd.unwrap() - 0.30).abs() < 1e-9);
    let handle = done.trace.unwrap();
    assert_eq!(handle.bytes, body.len() as u64);
    assert_eq!(
        handle.sha256.as_deref(),
        Some(hex::encode(Sha256::digest(body.as_bytes())).as_str())
    );
    // Finish is repeatable and answers the same final record.
    assert_eq!(
        w.service.finish_task(run, "t1").await.unwrap().usage,
        done.usage
    );

    // A session that ran any other route is an `invalid_route` trial.
    let other = start(&w, run, "regex-log", "t2").await;
    w.store
        .lock()
        .await
        .insert_invocation_purpose_for_test(session_of(&other), 9, "session.continue.resume")
        .unwrap();
    end_session(&w, session_of(&other), "completed").await;
    let bad = w.service.finish_task(run, "t2").await.unwrap();
    assert_eq!(bad.route_verdict.as_deref(), Some("invalid_route"));
    assert_eq!(bad.route_violations, ["purpose:session.continue.resume"]);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn cancel_stops_the_session_once_and_finish_still_seals_it() {
    let w = world().await;
    let run = Uuid::new_v4();
    w.service.open_run(new_run(run)).await.unwrap();
    let started = start(&w, run, "fix-git", "t1").await;
    let session = session_of(&started);
    spend(&w, session, 1, Some(0.07)).await;

    let cancelled = w.service.cancel_task(run, "t1").await.unwrap();
    assert_eq!(cancelled.state, BenchTaskState::Cancelled);
    assert_eq!(cancelled.stop_reason.as_deref(), Some("bench_cancelled"));
    assert_eq!(w.host.interrupts.lock().unwrap().as_slice(), [session]);
    // Cancelling again changes nothing and stops nothing.
    let again = w.service.cancel_task(run, "t1").await.unwrap();
    assert_eq!(again.state, BenchTaskState::Cancelled);
    assert_eq!(w.host.interrupts.lock().unwrap().len(), 1);
    // The interrupted session is terminal, so Finish seals the cancelled trial.
    let done = w.service.finish_task(run, "t1").await.unwrap();
    assert_eq!(done.state, BenchTaskState::Cancelled);
    assert_eq!(done.stop_reason.as_deref(), Some("bench_cancelled"));
    assert!((done.usage.cost_usd.unwrap() - 0.07).abs() < 1e-9);
    // A trial of another run is not reachable.
    let error = w.service.cancel_task(run, "nope").await.unwrap_err();
    assert_eq!(refusal_code(&error), BENCH_TRIAL_UNKNOWN);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_restart_interrupts_live_trials_never_resumes_them_and_rebinds_the_socket() {
    let w = world().await;
    let run = Uuid::new_v4();
    let finished_run = Uuid::new_v4();
    w.service.open_run(new_run(run)).await.unwrap();
    w.service.open_run(new_run(finished_run)).await.unwrap();
    let live = start(&w, run, "fix-git", "live").await;
    let done = start(&w, run, "regex-log", "done").await;
    end_session(&w, session_of(&done), "completed").await;
    assert_eq!(
        w.service.task_status(run, "done").await.unwrap().state,
        BenchTaskState::Finished
    );
    w.service
        .end_run(finished_run, BenchRunState::Finished)
        .await
        .unwrap();

    // The daemon restarts: a fresh service over the same store and root.
    let restarted = BenchService::new(
        Arc::clone(&w.store),
        Arc::clone(&w.host) as Arc<dyn BenchHost>,
        w.root.path().join("bench"),
    );
    assert_eq!(restarted.restore().await.unwrap(), 1, "only the live trial");

    let status = restarted.task_status(run, "live").await.unwrap();
    assert_eq!(status.state, BenchTaskState::InfraInterrupted);
    assert_eq!(status.stop_reason.as_deref(), Some("infra_interrupted"));
    assert!(
        w.host
            .interrupts
            .lock()
            .unwrap()
            .contains(&session_of(&live))
    );
    assert_eq!(
        restarted.task_status(run, "done").await.unwrap().state,
        BenchTaskState::Finished,
        "a settled trial is untouched"
    );
    // The adapter reads that state over the rebound socket.
    assert!(restarted.is_bound(run).await);
    assert!(
        !restarted.is_bound(finished_run).await,
        "an ended run is not bound again"
    );
    let response = rpc(
        &restarted.socket_path(run),
        "BenchTaskStatus",
        serde_json::json!({"trial": "live"}),
    )
    .await;
    assert_eq!(response["result"]["state"], "infra_interrupted");
    // The interrupted trial is not relaunched by a repeated start.
    let launches = w.host.launched().len();
    let repeat = restarted
        .start_task(run, start_params("fix-git", "live"))
        .await
        .unwrap();
    assert_eq!(repeat.state, BenchTaskState::InfraInterrupted);
    assert_eq!(w.host.launched().len(), launches);
    // Finish seals it without waiting for a session that will never end.
    let sealed = restarted.finish_task(run, "live").await.unwrap();
    assert_eq!(sealed.state, BenchTaskState::InfraInterrupted);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn trace_pages_are_whole_lines_and_resume_from_the_next_offset() {
    let w = world().await;
    let run = Uuid::new_v4();
    w.service.open_run(new_run(run)).await.unwrap();
    let started = start(&w, run, "fix-git", "t1").await;
    let path = PathBuf::from(started.trace.as_ref().unwrap().path.clone());
    let lines: Vec<String> = (0..20)
        .map(|n| format!("{{\"seq\":{n},\"pad\":\"{}\"}}\n", "x".repeat(40 + n)))
        .collect();
    let full: String = lines.concat();
    std::fs::write(&path, &full).unwrap();

    let mut offset = 0;
    let mut got = String::new();
    let mut pages = 0;
    loop {
        let page = w
            .service
            .read_trace(
                run,
                BenchTraceReadParams {
                    trial: "t1".into(),
                    offset,
                    limit_bytes: Some(200),
                },
            )
            .await
            .unwrap();
        assert!(page.data.ends_with('\n') || page.data.is_empty());
        got.push_str(&page.data);
        pages += 1;
        offset = page.next_offset;
        if page.eof {
            break;
        }
        assert!(pages < 50, "paging makes progress");
    }
    assert_eq!(
        got, full,
        "the pages concatenate to the whole, untruncated trace"
    );
    assert!(pages > 3);
    // A line longer than the window is returned whole.
    let long = format!("{{\"big\":\"{}\"}}\n", "y".repeat(5000));
    std::fs::write(&path, &long).unwrap();
    let page = w
        .service
        .read_trace(
            run,
            BenchTraceReadParams {
                trial: "t1".into(),
                offset: 0,
                limit_bytes: Some(100),
            },
        )
        .await
        .unwrap();
    assert_eq!(page.data, long);
    assert!(page.eof);
    // An offset past the end, or a trial that is not this run's, is refused.
    let error = w
        .service
        .read_trace(
            run,
            BenchTraceReadParams {
                trial: "t1".into(),
                offset: 1 << 40,
                limit_bytes: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(refusal_code(&error), "bench_invalid_request");
    let error = w
        .service
        .read_trace(
            run,
            BenchTraceReadParams {
                trial: "other".into(),
                offset: 0,
                limit_bytes: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(refusal_code(&error), BENCH_TRIAL_UNKNOWN);
}

// ------------------------------------------------------- run overlays (#3191)

fn overlay_of(value: serde_json::Value, instructions: Option<&str>) -> BenchOverlay {
    serde_json::from_value::<RawOverlay>(value)
        .unwrap()
        .resolve(instructions.map(str::as_bytes))
        .unwrap()
        .expect("an overlay that sets something")
}

fn write_overlay(world: &World, run: Uuid, overlay: &BenchOverlay) {
    std::fs::write(
        world.service.overlay_path(run),
        serde_json::to_vec(overlay).unwrap(),
    )
    .unwrap();
}

fn launch_of(world: &World, run: Uuid, trial: &str) -> LaunchConfig {
    let key = format!("bench:{run}:{trial}");
    world
        .host
        .launched()
        .into_iter()
        .find(|config| config.model_invocation_dedup_key.as_deref() == Some(key.as_str()))
        .unwrap_or_else(|| panic!("no launch for {key}"))
}

/// Two runs started at the same time with different overlays, and a third with none:
/// each trial session gets its own prompt body, tool narrowing, compaction budget and
/// iteration cap, and nothing is shared between them (no daemon setting is written).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn concurrent_runs_launch_their_trials_each_with_its_own_overlay() {
    let w = world().await;
    let (run_a, run_b, run_plain) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    for run in [run_a, run_b, run_plain] {
        w.service.open_run(new_run(run)).await.unwrap();
    }
    let overlay_a = overlay_of(
        serde_json::json!({
            "instructions": "instructions.md",
            "context": {"compact_budget_tokens": 96_000},
            "loop": {"max_iterations": 120},
        }),
        Some("Alpha: plan first."),
    );
    let overlay_b = overlay_of(
        serde_json::json!({
            "instructions": "instructions.md",
            "tool_policy": {"denied_tools": ["compact"]},
            "loop": {"max_iterations": 300},
        }),
        Some("Beta: act, then verify."),
    );
    write_overlay(&w, run_a, &overlay_a);
    write_overlay(&w, run_b, &overlay_b);

    let (a, b, plain) = tokio::join!(
        start(&w, run_a, "fix-git", "a-1"),
        start(&w, run_b, "fix-git", "b-1"),
        start(&w, run_plain, "fix-git", "p-1"),
    );
    assert_eq!(w.host.launched().len(), 3);
    let _ = (a, b, plain);

    let config_a = launch_of(&w, run_a, "a-1");
    let profile_a = config_a.bench().expect("a bench profile");
    assert_eq!(profile_a.overlay.as_ref(), Some(&overlay_a));
    assert_eq!(
        config_a.loop_knobs,
        LoopKnobs {
            max_iterations: Some(120),
            compaction_budget_tokens: Some(96_000),
            rotation_off: true,
            ..LoopKnobs::default()
        }
    );

    let config_b = launch_of(&w, run_b, "b-1");
    let profile_b = config_b.bench().expect("a bench profile");
    assert_eq!(profile_b.overlay.as_ref(), Some(&overlay_b));
    assert_eq!(
        config_b.loop_knobs,
        LoopKnobs {
            max_iterations: Some(300),
            compaction_budget_tokens: None,
            rotation_off: true,
            ..LoopKnobs::default()
        }
    );
    assert_eq!(
        profile_b
            .overlay
            .as_ref()
            .unwrap()
            .tool_policy()
            .denied_tools,
        ["compact"]
    );
    assert_ne!(profile_a.overlay, profile_b.overlay);

    // The run with no overlay keeps the config's knobs and the unmodified profile.
    let config_plain = launch_of(&w, run_plain, "p-1");
    assert_eq!(config_plain.bench().unwrap().overlay, None);
    assert_eq!(config_plain.loop_knobs, new_run(run_plain).loop_knobs);
}

/// An overlay file that cannot be read back whole refuses the trial: it is never run
/// unmodified in its place.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_tampered_or_unreadable_overlay_refuses_the_trial_and_launches_nothing() {
    let w = world().await;
    let run = Uuid::new_v4();
    w.service.open_run(new_run(run)).await.unwrap();
    let overlay = overlay_of(serde_json::json!({"loop": {"max_iterations": 120}}), None);

    // Edited after the job wrote it: the digest no longer matches.
    let mut tampered = overlay.clone();
    tampered.max_iterations = Some(900);
    write_overlay(&w, run, &tampered);
    let error = w
        .service
        .start_task(run, start_params("fix-git", "t-1"))
        .await
        .unwrap_err();
    assert_eq!(refusal_code(&error), BENCH_OVERLAY_UNAVAILABLE);

    // Not an overlay at all, and an oversized file.
    std::fs::write(w.service.overlay_path(run), b"not json").unwrap();
    let error = w
        .service
        .start_task(run, start_params("fix-git", "t-2"))
        .await
        .unwrap_err();
    assert_eq!(refusal_code(&error), BENCH_OVERLAY_UNAVAILABLE);
    std::fs::write(w.service.overlay_path(run), vec![b' '; 300 * 1024]).unwrap();
    let error = w
        .service
        .start_task(run, start_params("fix-git", "t-3"))
        .await
        .unwrap_err();
    assert_eq!(refusal_code(&error), BENCH_OVERLAY_UNAVAILABLE);
    assert!(
        w.host.launched().is_empty(),
        "no session for a refused trial"
    );

    // Each refused trial is over, with the refusal as its stop reason.
    let row = w
        .store
        .lock()
        .await
        .get_bench_task(run, "t-1")
        .unwrap()
        .unwrap();
    assert_eq!(row.state, BenchTaskState::Cancelled);
    assert_eq!(
        row.stop_reason.as_deref(),
        Some("bench_start_failed:bench_overlay_unavailable")
    );

    // Written whole again, the same file launches.
    write_overlay(&w, run, &overlay);
    start(&w, run, "fix-git", "t-4").await;
    assert_eq!(
        launch_of(&w, run, "t-4").loop_knobs.max_iterations,
        Some(120)
    );
}

// ---- R6 (#3684): the per-call cap, concurrent runs and the stop path, with fakes ---------------

/// The inner model-call control under the per-task guard: counts the calls it was asked to
/// admit and the failures the guard recorded, and admits through the test no-op.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[derive(Default)]
struct FakeCalls {
    admitted: StdMutex<u32>,
    pending_failures: StdMutex<Vec<String>>,
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[async_trait]
impl crate::model_control::call_control::ModelCallControl for FakeCalls {
    async fn admit(
        &self,
        kind: crate::model_control::call_control::ModelCallKind,
        dedup_hint: &str,
        fingerprint_hint: &str,
        retry_of_invocation_id: Option<Uuid>,
    ) -> Result<crate::model_control::call_control::AdmittedModelCall> {
        *self.admitted.lock().unwrap() += 1;
        crate::model_control::call_control::NoopModelCallControl
            .admit(kind, dedup_hint, fingerprint_hint, retry_of_invocation_id)
            .await
    }

    async fn complete(
        &self,
        _call: crate::model_control::call_control::ModelCallSettlement,
        _usage: crate::model_control::call_control::ModelCallUsage,
    ) -> Result<()> {
        Ok(())
    }

    async fn fail(
        &self,
        _call: crate::model_control::call_control::ModelCallSettlement,
        _error_class: &str,
    ) -> Result<()> {
        Ok(())
    }

    async fn fail_pending(&self, error_class: &str) -> Result<()> {
        self.pending_failures
            .lock()
            .unwrap()
            .push(error_class.to_owned());
        Ok(())
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
fn guard(
    w: &World,
    inner: &Arc<FakeCalls>,
    session: Uuid,
    call_bound_usd: Option<f64>,
) -> cap::BenchTaskGuardedControl {
    cap::BenchTaskGuardedControl::new(
        Arc::clone(inner) as Arc<dyn crate::model_control::call_control::ModelCallControl>,
        Arc::clone(&w.store),
        Some(session),
        None,
        call_bound_usd,
        None,
    )
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
async fn try_call(guard: &cap::BenchTaskGuardedControl) -> std::result::Result<(), String> {
    use crate::model_control::call_control::{ModelCallControl, ModelCallKind};
    guard
        .admit(ModelCallKind::Primary, "dedup", "fingerprint", None)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// The per-task cap is judged before every model call through the real guard: a call whose
/// upper bound could take the task over its cap never reaches the inner control, and the pending
/// admission is failed with the refusal's own class.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn the_per_call_cap_refuses_a_call_that_could_cross_it_before_the_inner_control_sees_it() {
    use rsi_common::bench::{
        BENCH_TASK_BUDGET_EXHAUSTED, BENCH_TASK_COST_UNKNOWN, BENCH_TASK_NOT_RUNNING,
    };
    let w = world().await;
    let run = Uuid::new_v4();
    w.service.open_run(new_run(run)).await.unwrap(); // cap $0.40 per task
    let inner = Arc::new(FakeCalls::default());

    // Room for the next call: $0.20 spent, the next could cost $0.10 (the largest so far).
    let a = session_of(&start(&w, run, "fix-git", "a").await);
    let under = guard(&w, &inner, a, Some(0.05));
    spend(&w, a, 1, Some(0.10)).await;
    spend(&w, a, 2, Some(0.10)).await;
    try_call(&under)
        .await
        .expect("0.20 + 0.10 stays under 0.40");
    assert_eq!(*inner.admitted.lock().unwrap(), 1);

    // $0.30 spent: the next call's reserve takes the task to its cap, so it is refused before
    // the inner control admits anything.
    spend(&w, a, 3, Some(0.10)).await;
    let refused = try_call(&under).await.unwrap_err();
    assert!(refused.contains(BENCH_TASK_BUDGET_EXHAUSTED), "{refused}");
    assert_eq!(
        *inner.admitted.lock().unwrap(),
        1,
        "never reached the inner control"
    );
    assert_eq!(
        inner.pending_failures.lock().unwrap().as_slice(),
        [BENCH_TASK_BUDGET_EXHAUSTED]
    );

    // A completed call of unknown cost cannot be judged against the cap: fail closed.
    let b = session_of(&start(&w, run, "regex-log", "b").await);
    spend(&w, b, 1, None).await;
    let unknown = try_call(&guard(&w, &inner, b, Some(0.05)))
        .await
        .unwrap_err();
    assert!(unknown.contains(BENCH_TASK_COST_UNKNOWN), "{unknown}");

    // A model with no known price has no per-call bound: refused, never guessed.
    let run2 = Uuid::new_v4();
    w.service.open_run(new_run(run2)).await.unwrap();
    let c = session_of(&start(&w, run2, "fix-git", "c").await);
    let unpriced = try_call(&guard(&w, &inner, c, None)).await.unwrap_err();
    assert!(unpriced.contains(BENCH_TASK_COST_UNKNOWN), "{unpriced}");
    assert_eq!(*inner.admitted.lock().unwrap(), 1);

    // A cancelled trial never spends again, whatever its cap has left.
    let fresh = guard(&w, &inner, c, Some(0.05));
    try_call(&fresh)
        .await
        .expect("a running trial under its cap");
    assert_eq!(*inner.admitted.lock().unwrap(), 2);
    w.service.cancel_task(run2, "c").await.unwrap();
    let stopped = try_call(&fresh).await.unwrap_err();
    assert!(stopped.contains(BENCH_TASK_NOT_RUNNING), "{stopped}");
    assert_eq!(*inner.admitted.lock().unwrap(), 2);
}

/// Two runs reserve at the same time: each run's remainder is its own, so a start in one run
/// never consumes (or is refused by) the other run's budget, while two starts inside one run
/// that do not both fit still admit exactly one.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_runs_reserve_their_own_budgets_independently() {
    let w = world().await;
    for round in 0..6 {
        let runs = [Uuid::new_v4(), Uuid::new_v4()];
        for run in runs {
            let mut cfg = new_run(run);
            // $1.00 and $0.60 per task: one task's cap fits, two do not.
            cfg.budget_usd = 1.0;
            cfg.per_task_cap_usd = 0.6;
            cfg.concurrency = 2;
            w.service.open_run(cfg).await.unwrap();
        }
        let mut starts = Vec::new();
        for (index, run) in runs.into_iter().enumerate() {
            for (side, task) in ["fix-git", "regex-log"].into_iter().enumerate() {
                let service = Arc::clone(&w.service);
                let trial = format!("r{round}-{index}-{side}");
                starts.push((
                    run,
                    tokio::spawn(async move {
                        service.start_task(run, start_params(task, &trial)).await
                    }),
                ));
            }
        }
        let mut admitted = std::collections::HashMap::<Uuid, usize>::new();
        for (run, handle) in starts {
            match handle.await.unwrap() {
                Ok(_) => *admitted.entry(run).or_default() += 1,
                Err(refusal) => assert_eq!(refusal_code(&refusal), BENCH_RUN_BUDGET_INSUFFICIENT),
            }
        }
        for run in runs {
            assert_eq!(
                admitted.get(&run).copied(),
                Some(1),
                "round {round}: each run admits exactly one task of its own"
            );
        }
    }
}

/// #3992: an Epic with a Task under it, and a job that Task submitted.
async fn epic_with_submitted_job(w: &World) -> (Uuid, Uuid) {
    use crate::store::agent_jobs::NewAgentJob;
    use rsi_common::agent_jobs::{BuildCommand, BuildJobParams, JobParams, JobWake};
    use rsi_common::types::SessionKind;
    let (epic, owner, job) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let store = w.store.lock().await;
    let mut parent = rsid_store::test_support::test_session(epic, std::path::PathBuf::new());
    parent.session_kind = SessionKind::Epic;
    store.insert_session(&parent).unwrap();
    let mut submitter = rsid_store::test_support::test_session(owner, std::path::PathBuf::new());
    submitter.session_kind = SessionKind::Task;
    submitter.parent_id = Some(epic);
    store.insert_session(&submitter).unwrap();
    store
        .insert_agent_job(
            &NewAgentJob {
                id: job,
                owner_session_id: owner,
                project_id: None,
                name: None,
                params: JobParams::Build(BuildJobParams {
                    release_members: Vec::new(),
                    command: BuildCommand::Check,
                    package: None,
                    workspace: false,
                    all_targets: false,
                    release: false,
                    no_default_features: false,
                    features: Vec::new(),
                    shards: Vec::new(),
                    bins: Vec::new(),
                    cpu_quota_percent: None,
                    sha: None,
                }),
                cwd: "/tmp".into(),
                unit_name: format!("rsi-test-{job}"),
                log_path: "/tmp/job.log".into(),
                status_path: "/tmp/job.status".into(),
                idempotency_key: None,
                wake: JobWake::None,
            },
            chrono::Utc::now(),
        )
        .unwrap();
    (epic, job)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn trials_of_a_run_launch_in_the_run_group_tagged_with_the_submitter_epic() {
    let w = world().await;
    let (epic, job) = epic_with_submitted_job(&w).await;
    let run = Uuid::new_v4();
    let mut cfg = new_run(run);
    cfg.job_id = Some(job.to_string());
    let project = cfg.project_id;
    w.service.open_run(cfg).await.unwrap();

    let first = session_of(&start(&w, run, "fix-git", "t1").await);
    let second = session_of(&start(&w, run, "regex-log", "t2").await);

    let launches = w.host.launched();
    assert_eq!(launches.len(), 2);
    let group = launches[0].parent_id.expect("the trial has a parent");
    assert_eq!(launches[1].parent_id, Some(group), "one group per run");
    assert_eq!(*w.host.groups.lock().unwrap(), vec![group]);
    let store = w.store.lock().await;
    let group = store.get_session(group).unwrap().unwrap();
    assert_eq!(group.session_kind, rsi_common::types::SessionKind::Group);
    assert_eq!(group.project_id, Some(project));
    assert_eq!(
        group.title.as_deref(),
        Some(format!("bench {MODEL} {}", &run.to_string()[..8]).as_str())
    );
    assert!(group.tags.contains(&format!("bench-epic-{epic}")));
    assert!(group.tags.contains(&format!("bench-run-{run}")));
    // The trials stay launches of the same harness session with their titles.
    assert_eq!(launches[0].title.as_deref(), Some("bench fix-git t1"));
    assert!(launches[0].is_eval);
    assert!(store.get_session(first).unwrap().is_some());
    assert!(store.get_session(second).unwrap().is_some());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn two_runs_of_one_epic_get_two_groups() {
    let w = world().await;
    let (_, job) = epic_with_submitted_job(&w).await;
    let (run_a, run_b) = (Uuid::new_v4(), Uuid::new_v4());
    for run in [run_a, run_b] {
        let mut cfg = new_run(run);
        cfg.job_id = Some(job.to_string());
        w.service.open_run(cfg).await.unwrap();
        start(&w, run, "fix-git", "t1").await;
    }
    let launches = w.host.launched();
    assert_ne!(launches[0].parent_id, launches[1].parent_id);
    assert!(launches.iter().all(|launch| launch.parent_id.is_some()));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn a_run_with_no_submitter_epic_shares_the_project_benchmarks_group() {
    let w = world().await;
    let (run_a, run_b) = (Uuid::new_v4(), Uuid::new_v4());
    let (mut a, mut b) = (new_run(run_a), new_run(run_b));
    b.project_id = a.project_id;
    // "job-1" resolves to no job: an operator-run bench has no submitter Epic.
    a.job_id = Some("job-1".into());
    b.job_id = None;
    let project = a.project_id;
    w.service.open_run(a).await.unwrap();
    w.service.open_run(b).await.unwrap();
    start(&w, run_a, "fix-git", "a1").await;
    start(&w, run_b, "fix-git", "b1").await;

    let launches = w.host.launched();
    let group = launches[0].parent_id.expect("the trial has a parent");
    assert_eq!(launches[1].parent_id, Some(group), "one shared group");
    let group = w.store.lock().await.get_session(group).unwrap().unwrap();
    assert_eq!(group.title.as_deref(), Some("Benchmarks"));
    assert_eq!(group.project_id, Some(project));
    assert_eq!(group.parent_id, None);
}

/// A trial as it sat at the project root before #3992: a bound, ended,
/// `is_eval` Standard session with no parent.
async fn legacy_trial(w: &World, run: &NewBenchRun, trial: &str, status: SessionStatus) -> Uuid {
    use rsi_common::types::SessionKind;
    let id = Uuid::new_v4();
    let store = w.store.lock().await;
    let mut session = rsid_store::test_support::test_session(id, std::path::PathBuf::new());
    session.session_kind = SessionKind::Standard;
    session.project_id = Some(run.project_id);
    session.is_eval = true;
    session.status = status;
    session.title = Some(format!("bench fix-git {trial}"));
    store.insert_session(&session).unwrap();
    let claim = NewBenchTask {
        trial: trial.into(),
        task: "fix-git".into(),
        compose_project: format!("{trial}-proj"),
        container: format!("{trial}-main-1"),
        workdir: "/app".into(),
        user: None,
        agent_timeout_sec: 60,
        request_digest: format!("digest-{trial}"),
    };
    store
        .claim_bench_task(run.id, &claim, chrono::Utc::now())
        .unwrap();
    assert!(
        store
            .bind_bench_task_session(run.id, trial, id, "/tmp/trace.jsonl")
            .unwrap()
    );
    id
}

async fn root_snapshot(w: &World) -> Vec<(String, Option<String>)> {
    let store = w.store.lock().await;
    let mut stmt = store
        .conn
        .prepare("SELECT id, parent_id FROM sessions ORDER BY id")
        .unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-04"))]
#[tokio::test]
async fn group_orphan_trials_moves_finished_root_trials_once_and_spares_active_runs() {
    use rsi_common::bench::BenchRunState;
    use rsi_common::types::SessionKind;
    let w = world().await;
    let (epic, job) = epic_with_submitted_job(&w).await;
    let (run_a, run_b, run_c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let mut a = new_run(run_a);
    a.job_id = Some(job.to_string());
    a.concurrency = 8;
    let project = a.project_id;
    let mut b = new_run(run_b);
    b.project_id = project;
    b.job_id = None;
    let mut c = new_run(run_c);
    c.project_id = project;
    c.job_id = Some(job.to_string());
    for run in [&a, &b, &c] {
        w.service.open_run(run.clone()).await.unwrap();
    }
    let a1 = legacy_trial(&w, &a, "a1", SessionStatus::Completed).await;
    let a2 = legacy_trial(&w, &a, "a2", SessionStatus::Failed).await;
    let b1 = legacy_trial(&w, &b, "b1", SessionStatus::Completed).await;
    // Run C stays active: its finished trial must not move.
    let c1 = legacy_trial(&w, &c, "c1", SessionStatus::Completed).await;
    // A trial of run A that is still running stays put too.
    let a3 = legacy_trial(&w, &a, "a3", SessionStatus::Running).await;
    // An old trial with no run row, and an unrelated eval session.
    let loose = Uuid::new_v4();
    let other = Uuid::new_v4();
    {
        let store = w.store.lock().await;
        for (id, title) in [(loose, "bench old old-s1-r1"), (other, "not a trial")] {
            let mut s = rsid_store::test_support::test_session(id, std::path::PathBuf::new());
            s.session_kind = SessionKind::Standard;
            s.project_id = Some(project);
            s.is_eval = true;
            s.status = SessionStatus::Completed;
            s.title = Some(title.into());
            store.insert_session(&s).unwrap();
        }
        for run in [run_a, run_b] {
            store
                .end_bench_run(run, BenchRunState::Finished, chrono::Utc::now())
                .unwrap();
        }
    }

    assert_eq!(w.service.group_orphan_trials().await.unwrap(), 4);
    let after_first = root_snapshot(&w).await;
    let store = w.store.lock().await;
    let parent = |id: Uuid| store.get_session(id).unwrap().unwrap().parent_id;
    let group_a = parent(a1).expect("a1 is grouped");
    assert_eq!(parent(a2), Some(group_a), "a run's trials share one Group");
    let group = store.get_session(group_a).unwrap().unwrap();
    assert_eq!(group.session_kind, SessionKind::Group);
    assert_eq!(group.parent_id, None);
    assert_eq!(
        group.title.as_deref(),
        Some(format!("bench {MODEL} {}", &run_a.to_string()[..8]).as_str())
    );
    assert!(group.tags.contains(&format!("bench-run-{run_a}")));
    assert!(group.tags.contains(&format!("bench-epic-{epic}")));
    let benchmarks = parent(b1).expect("b1 is grouped");
    assert_ne!(benchmarks, group_a);
    assert_eq!(parent(loose), Some(benchmarks), "no run row: Benchmarks");
    assert_eq!(
        store
            .get_session(benchmarks)
            .unwrap()
            .unwrap()
            .title
            .as_deref(),
        Some("Benchmarks")
    );
    assert_eq!(parent(c1), None, "an active run is never touched");
    assert_eq!(parent(a3), None, "a running trial is never touched");
    assert_eq!(
        parent(other),
        None,
        "a non-bench eval session is left alone"
    );
    assert!(
        store
            .list_ungrouped_finished_bench_sessions(100)
            .unwrap()
            .is_empty(),
        "no finished bench trial of an ended run stays at the root"
    );
    drop(store);

    assert_eq!(w.service.group_orphan_trials().await.unwrap(), 0);
    assert_eq!(
        root_snapshot(&w).await,
        after_first,
        "the second pass changes nothing"
    );
}
