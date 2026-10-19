//! #1006: daemon-evaluated wait predicates. One resume wake for a batch of jobs,
//! evaluated by the scheduler with no model turn.

use super::wake_when::{Verdict, compose_message, decide_jobs};
use super::*;
use crate::session::harness::tools::schedule_wake::{WakeWhenRequest, build_wake_when_job};
use crate::store::agent_jobs::NewAgentJob;
use rsi_common::agent_jobs::{
    AgentJobResultV1, BuildCommand, BuildJobParams, JobParams, JobState, JobWake,
};
use rsi_common::types::ScheduledJob;
use rsi_common::wake_predicate::{WakePredicate, WakeWhenState};
use std::sync::Mutex as StdMutex;

/// Records every resume delivery with its message.
#[derive(Default)]
pub(crate) struct RecordingLauncher {
    deliveries: StdMutex<Vec<(Uuid, String)>>,
    /// #2204: the daemon has decided to restart (scheduler paused).
    restart_draining: std::sync::atomic::AtomicBool,
    /// #2204: a waiting deploy holds this target's resume.
    deploy_hold: std::sync::atomic::AtomicBool,
    /// #2204: the next N resumes are refused by a drain that engaged after the
    /// scheduler's check, as the continuation path does.
    drain_refusals: std::sync::atomic::AtomicUsize,
    /// Resume attempts made, refused or not.
    resume_attempts: std::sync::atomic::AtomicUsize,
    /// #2747: what the live daemon reports to a `health_anomaly` wake.
    health_live: StdMutex<crate::health_anomaly::LiveHealthInputs>,
}

impl RecordingLauncher {
    pub(crate) fn deliveries(&self) -> Vec<(Uuid, String)> {
        self.deliveries.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl SessionLauncher for RecordingLauncher {
    async fn launch(&self, _: LaunchConfig) -> crate::error::Result<Uuid> {
        panic!("a predicate wake must never launch a session");
    }

    async fn launch_scheduled_fresh(&self, _: LaunchConfig, _: bool) -> crate::error::Result<Uuid> {
        panic!("a predicate wake must never launch a Fresh session");
    }

    async fn resume_scheduled(&self, target: Uuid, message: String) -> crate::error::Result<Uuid> {
        self.deliveries.lock().unwrap().push((target, message));
        Ok(target)
    }

    async fn resume_scheduled_job(
        &self,
        target: Uuid,
        message: String,
        _: Vec<Uuid>,
    ) -> crate::error::Result<Uuid> {
        use std::sync::atomic::Ordering;
        self.resume_attempts.fetch_add(1, Ordering::SeqCst);
        let refused = self
            .drain_refusals
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        if refused {
            return Err(crate::deploy_drain::restart_drain_error());
        }
        self.deliveries.lock().unwrap().push((target, message));
        Ok(target)
    }

    fn restart_draining(&self) -> bool {
        self.restart_draining
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn deploy_holds_resume(&self, _: Uuid) -> bool {
        self.deploy_hold.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn health_live_inputs(
        &self,
        _: &crate::health_anomaly::Scope,
    ) -> crate::health_anomaly::LiveHealthInputs {
        self.health_live.lock().unwrap().clone()
    }

    async fn fire_watch(&self, _: &ScheduledJob) -> crate::error::Result<WatchFireOutcome> {
        Ok(WatchFireOutcome::NotReady)
    }
}

struct World {
    store: Arc<Mutex<Store>>,
    bus: Arc<EventBus>,
    launcher: Arc<RecordingLauncher>,
    owner: Uuid,
}

impl World {
    fn new() -> Self {
        Self::with_store(Store::open_in_memory().expect("store"))
    }

    fn with_store(store: Store) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            bus: Arc::new(EventBus::new(64)),
            launcher: Arc::new(RecordingLauncher::default()),
            owner: Uuid::new_v4(),
        }
    }

    fn dyn_launcher(&self) -> Arc<dyn SessionLauncher> {
        self.launcher.clone()
    }

    /// One fast-lane pass: what the scheduler runs every few seconds.
    async fn pass(&self) {
        process_due_wake_when(&self.store, &self.bus, &self.dyn_launcher()).await;
    }
}

fn new_job(owner: Uuid, name: &str, wake: JobWake) -> NewAgentJob {
    let id = Uuid::new_v4();
    NewAgentJob {
        id,
        owner_session_id: owner,
        project_id: None,
        name: Some(name.into()),
        params: JobParams::Build(BuildJobParams {
            command: BuildCommand::Check,
            package: None,
            workspace: true,
            all_targets: false,
            release: false,
            no_default_features: false,
            features: Vec::new(),
            shards: Vec::new(),
            cpu_quota_percent: None,
            sha: None,
        }),
        cwd: "/tmp/sandbox".into(),
        unit_name: format!("rsi-job-{id}"),
        log_path: format!("/tmp/jobs/{id}.log"),
        status_path: format!("/tmp/jobs/{id}.status"),
        idempotency_key: None,
        wake,
    }
}

pub(crate) fn insert_job(store: &Store, owner: Uuid, name: &str) -> Uuid {
    let new = new_job(owner, name, JobWake::None);
    store
        .insert_agent_job(&new, Utc::now())
        .expect("insert job");
    new.id
}

pub(crate) fn finish(store: &Store, id: Uuid, state: JobState, code: i32, refusal: Option<&str>) {
    let result = AgentJobResultV1 {
        exit_code: Some(code),
        refusal: refusal.map(str::to_string),
        ..AgentJobResultV1::default()
    };
    // A `wake: none` job settles without its per-job owner wake.
    assert_eq!(
        store
            .settle_agent_job(id, state, &result, false, Utc::now())
            .expect("settle"),
        None
    );
}

pub(crate) fn arm(
    store: &Store,
    owner: Uuid,
    predicate: WakePredicate,
    timeout_seconds: Option<i64>,
) -> crate::error::Result<ScheduledJob> {
    let (job, state) = build_wake_when_job(WakeWhenRequest {
        message: "batch done?".into(),
        name: None,
        predicate,
        timeout_seconds,
        working_dir: "/tmp/sandbox".into(),
        provider: None,
        model: None,
        project_id: None,
        origin_session_id: owner,
    })
    .expect("builds");
    store.insert_wake_when(&job, &state, false)?;
    Ok(job)
}

pub(crate) fn jobs_terminal(ids: &[Uuid]) -> WakePredicate {
    WakePredicate {
        jobs_terminal: Some(ids.to_vec()),
        sha_on_rolling: None,
        issue_closed: None,
        todo_closed: None,
        topology_execution_settled: None,
        health_anomaly: None,
        settlement_run_settled: None,
    }
}

/// Make the row due again, as the fast lane's five-second spacing would.
async fn make_due(world: &World, id: Uuid) {
    world
        .store
        .lock()
        .await
        .defer_wake_when(id, Utc::now() - chrono::Duration::seconds(1))
        .expect("defer");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn three_jobs_wake_the_owner_once_whatever_the_finish_order() {
    for order in [[0, 1, 2], [2, 0, 1], [1, 2, 0]] {
        let world = World::new();
        let (ids, wake) = {
            let store = world.store.lock().await;
            let ids: Vec<Uuid> = ["check", "shard-a", "shard-b"]
                .iter()
                .map(|name| insert_job(&store, world.owner, name))
                .collect();
            let wake = arm(&store, world.owner, jobs_terminal(&ids), None).unwrap();
            (ids, wake)
        };
        // Armed and due: pending, nothing delivered.
        world.pass().await;
        assert!(world.launcher.deliveries().is_empty());

        for (n, index) in order.into_iter().enumerate() {
            {
                let store = world.store.lock().await;
                let (state, code, refusal) = if index == 1 {
                    (JobState::Failed, 101, Some("compile_error"))
                } else {
                    (JobState::Succeeded, 0, None)
                };
                finish(&store, ids[index], state, code, refusal);
            }
            make_due(&world, wake.id).await;
            world.pass().await;
            let delivered = world.launcher.deliveries().len();
            assert_eq!(delivered, usize::from(n == 2), "order {order:?} step {n}");
        }

        let deliveries = world.launcher.deliveries();
        assert_eq!(deliveries.len(), 1, "one wake for the batch");
        let (target, message) = &deliveries[0];
        assert_eq!(*target, world.owner);
        assert!(message.contains("\nbatch done?\n"), "{message}");
        assert!(message.contains("timed_out: false"), "{message}");
        for (id, name) in ids.iter().zip(["check", "shard-a", "shard-b"]) {
            assert!(message.contains(&id.to_string()), "{message}");
            assert!(message.contains(&format!("name={name}")), "{message}");
        }
        assert!(message.contains("state=failed exit_code=101 refusal=compile_error"));
        assert!(message.contains("state=succeeded exit_code=0"));

        // Settled for good: further passes never wake again.
        make_due(&world, wake.id).await;
        world.pass().await;
        assert_eq!(world.launcher.deliveries().len(), 1);
        let row = world
            .store
            .lock()
            .await
            .get_scheduled_job(&wake.id)
            .unwrap()
            .unwrap();
        assert!(!row.enabled, "a delivered predicate wake is spent");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_restart_between_completions_still_fires_exactly_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("rsi.db");
    let owner = Uuid::new_v4();
    let (ids, wake_id) = {
        let store = Store::open_test_fixture(&path).expect("open");
        let ids: Vec<Uuid> = ["a", "b"]
            .iter()
            .map(|name| insert_job(&store, owner, name))
            .collect();
        let wake = arm(&store, owner, jobs_terminal(&ids), None).unwrap();
        finish(&store, ids[0], JobState::Succeeded, 0, None);
        (ids, wake.id)
    };
    // The daemon restarts: a new store handle, a new scheduler, no memory.
    let mut world = World::with_store(Store::open(&path).expect("reopen"));
    world.owner = owner;
    world.pass().await;
    assert!(world.launcher.deliveries().is_empty(), "b is still running");
    finish(&*world.store.lock().await, ids[1], JobState::Lost, 1, None);
    make_due(&world, wake_id).await;
    world.pass().await;
    assert_eq!(world.launcher.deliveries().len(), 1);

    // A second restart after delivery does not fire again.
    let mut again = World::with_store(Store::open(&path).expect("reopen again"));
    again.owner = owner;
    make_due(&again, wake_id).await;
    again.pass().await;
    assert!(again.launcher.deliveries().is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn a_foreign_or_unknown_job_id_is_refused_at_scheduling_time() {
    let store = Store::open_in_memory().expect("store");
    let (owner, stranger) = (Uuid::new_v4(), Uuid::new_v4());
    let mine = insert_job(&store, owner, "mine");
    let theirs = insert_job(&store, stranger, "theirs");
    let unknown = Uuid::new_v4();
    for (ids, refused_id) in [
        (vec![theirs], theirs),
        (vec![unknown], unknown),
        (vec![mine, theirs], theirs),
    ] {
        let error = arm(&store, owner, jobs_terminal(&ids), None).expect_err("refused");
        // The stable code stays the prefix (the andon signature keys on it) and
        // the refusal names the offending id and the fix (#2867).
        assert_eq!(
            error.to_string(),
            format!(
                "Invalid parameter: {}",
                rsi_common::wake_predicate::wake_when_job_not_found_message(refused_id)
            ),
            "{ids:?}"
        );
        assert!(
            error
                .to_string()
                .contains("your own AgentSubmitJob call returned"),
            "{ids:?}"
        );
        assert_eq!(
            crate::friction::agent_refusal_code(&error),
            rsi_common::wake_predicate::WAKE_WHEN_JOB_NOT_FOUND
        );
    }
    assert!(
        store.list_scheduled_jobs().unwrap().is_empty(),
        "a refused predicate leaves no row"
    );
    assert!(arm(&store, owner, jobs_terminal(&[mine]), None).is_ok());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_timeout_fires_once_with_timed_out_true() {
    let world = World::new();
    let id = insert_job(&*world.store.lock().await, world.owner, "slow");
    let request = |timeout_seconds| WakeWhenRequest {
        message: "waiting".into(),
        name: None,
        predicate: jobs_terminal(&[id]),
        timeout_seconds,
        working_dir: "/tmp/sandbox".into(),
        provider: None,
        model: None,
        project_id: None,
        origin_session_id: world.owner,
    };
    // Before the deadline the job is still running: pending, and the row is
    // deferred no later than the deadline so the timeout fires on time.
    let (job, state) = build_wake_when_job(request(Some(60))).unwrap();
    let deadline = state.deadline.expect("deadline");
    world
        .store
        .lock()
        .await
        .insert_wake_when(&job, &state, false)
        .unwrap();
    world.pass().await;
    assert!(world.launcher.deliveries().is_empty());
    let deferred = world
        .store
        .lock()
        .await
        .get_scheduled_job(&job.id)
        .unwrap()
        .unwrap();
    assert!(deferred.next_fire_at <= deadline);

    // The deadline passes with the job still running.
    let expired = WakeWhenState {
        deadline: Some(Utc::now() - chrono::Duration::seconds(1)),
        ..state
    };
    world
        .store
        .lock()
        .await
        .set_wake_when_state_for_test(job.id, &expired)
        .unwrap();
    make_due(&world, job.id).await;
    world.pass().await;
    let deliveries = world.launcher.deliveries();
    assert_eq!(deliveries.len(), 1, "exactly one timed-out wake");
    assert!(
        deliveries[0].1.contains("timed_out: true"),
        "{}",
        deliveries[0].1
    );
    assert!(
        deliveries[0].1.contains("state=running"),
        "{}",
        deliveries[0].1
    );
    make_due(&world, job.id).await;
    world.pass().await;
    assert_eq!(world.launcher.deliveries().len(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn a_vanished_job_settles_with_a_typed_reason_and_satisfied_beats_timeout() {
    let now = Utc::now();
    let id = Uuid::new_v4();
    let state = WakeWhenState {
        predicate: jobs_terminal(&[id]),
        armed_at: now,
        deadline: Some(now - chrono::Duration::seconds(5)),
        repo_dir: None,
    };
    let missing = vec![(id, None)];
    let verdict = decide_jobs(&missing, &state, now, false);
    assert_eq!(verdict, Verdict::Unsatisfiable("job_missing"));
    let message = compose_message("m", &state, &verdict, &missing, &[], None);
    assert!(message.contains("reason: job_missing"), "{message}");
    assert!(message.contains("state=missing"), "{message}");
    assert!(message.contains("timed_out: false"), "{message}");

    // A pending predicate past its deadline times out; a terminal one is satisfied.
    let store = Store::open_in_memory().unwrap();
    let job_id = insert_job(&store, Uuid::new_v4(), "j");
    let running = store.get_agent_job(job_id).unwrap().unwrap().job;
    let snaps = vec![(job_id, Some(running))];
    assert_eq!(decide_jobs(&snaps, &state, now, false), Verdict::TimedOut);
    finish(&store, job_id, JobState::Succeeded, 0, None);
    let done = store.get_agent_job(job_id).unwrap().unwrap().job;
    let snaps = vec![(job_id, Some(done))];
    assert_eq!(decide_jobs(&snaps, &state, now, false), Verdict::Satisfied);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn the_wake_message_is_bounded_however_many_jobs_and_refusals() {
    let store = Store::open_in_memory().unwrap();
    let owner = Uuid::new_v4();
    let long = "x".repeat(5_000);
    let mut snaps = Vec::new();
    for n in 0..rsi_common::wake_predicate::WAKE_WHEN_MAX_JOBS {
        let id = insert_job(&store, owner, &format!("job-{n}"));
        finish(&store, id, JobState::Failed, 1, Some(&long));
        snaps.push((id, Some(store.get_agent_job(id).unwrap().unwrap().job)));
    }
    let ids: Vec<Uuid> = snaps.iter().map(|(id, _)| *id).collect();
    let state = WakeWhenState {
        predicate: jobs_terminal(&ids),
        armed_at: Utc::now(),
        deadline: None,
        repo_dir: None,
    };
    let message = compose_message("go", &state, &Verdict::Satisfied, &snaps, &[], None);
    assert!(
        message.len() <= "go\n\n".len() + rsi_common::wake_predicate::WAKE_WHEN_MESSAGE_BYTES + 128,
        "{}",
        message.len()
    );
    assert!(message.contains("more jobs omitted"), "{message}");
    assert!(!message.contains(&long));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn ordinary_wakes_are_untouched_by_the_predicate_lane() {
    let world = World::new();
    let now = Utc::now();
    let plain = ScheduledJob {
        id: Uuid::new_v4(),
        name: "plain".into(),
        message: "plain resume".into(),
        schedule: rsi_common::types::ScheduleSpec {
            recurrence: Recurrence::Once,
            anchor: now,
        },
        last_fired_at: None,
        next_fire_at: now - chrono::Duration::seconds(1),
        enabled: true,
        working_dir: None,
        provider: None,
        model: None,
        project_id: None,
        created_at: now,
        updated_at: now,
        wake_mode: WakeMode::Resume,
        wake_session_id: Some(world.owner),
    };
    world
        .store
        .lock()
        .await
        .insert_scheduled_job(&plain)
        .unwrap();
    // The fast lane leaves it for the general poll.
    world.pass().await;
    assert!(world.launcher.deliveries().is_empty());
    // The general due poll delivers it verbatim, exactly as before #1006.
    process_due_jobs(&world.store, &world.bus, &world.dyn_launcher(), None).await;
    assert_eq!(
        world.launcher.deliveries(),
        vec![(
            world.owner,
            rsi_common::daemon_message::wrap("scheduled-wake", "plain resume")
        )]
    );
}

fn git(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn sha_on_rolling_fires_when_the_commit_reaches_origin_rolling() {
    let repo = tempfile::tempdir().expect("repo");
    git(repo.path(), &["init", "-q", "-b", "work"]);
    git(
        repo.path(),
        &["commit", "-q", "--allow-empty", "-m", "base"],
    );
    let base = git(repo.path(), &["rev-parse", "HEAD"]);
    git(
        repo.path(),
        &["update-ref", "refs/remotes/origin/rolling", &base],
    );
    git(repo.path(), &["commit", "-q", "--allow-empty", "-m", "fix"]);
    let fix = git(repo.path(), &["rev-parse", "HEAD"]);

    let world = World::new();
    let (job, state) = build_wake_when_job(WakeWhenRequest {
        message: "landed?".into(),
        name: None,
        predicate: WakePredicate {
            jobs_terminal: None,
            sha_on_rolling: Some(fix.clone()),
            issue_closed: None,
            todo_closed: None,
            topology_execution_settled: None,
            health_anomaly: None,
            settlement_run_settled: None,
        },
        timeout_seconds: None,
        working_dir: repo.path().to_path_buf(),
        provider: None,
        model: None,
        project_id: None,
        origin_session_id: world.owner,
    })
    .unwrap();
    world
        .store
        .lock()
        .await
        .insert_wake_when(&job, &state, false)
        .unwrap();
    world.pass().await;
    assert!(world.launcher.deliveries().is_empty(), "not on rolling yet");
    let deferred = world
        .store
        .lock()
        .await
        .get_scheduled_job(&job.id)
        .unwrap()
        .unwrap();
    assert!(
        deferred.next_fire_at > Utc::now(),
        "polled at a bounded interval"
    );

    git(
        repo.path(),
        &["update-ref", "refs/remotes/origin/rolling", &fix],
    );
    make_due(&world, job.id).await;
    world.pass().await;
    let deliveries = world.launcher.deliveries();
    assert_eq!(deliveries.len(), 1);
    assert!(
        deliveries[0]
            .1
            .contains(&format!("sha_on_rolling {fix}: satisfied"))
    );
    make_due(&world, job.id).await;
    world.pass().await;
    assert_eq!(world.launcher.deliveries().len(), 1, "fires at most once");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn sha_on_rolling_fires_when_rolling_advances_on_the_remote_without_a_local_fetch() {
    let remote = tempfile::tempdir().expect("remote");
    git(remote.path(), &["init", "-q", "--bare", "-b", "rolling"]);
    let repo = tempfile::tempdir().expect("repo");
    git(repo.path(), &["init", "-q", "-b", "rolling"]);
    git(
        repo.path(),
        &["commit", "-q", "--allow-empty", "-m", "base"],
    );
    git(
        repo.path(),
        &["remote", "add", "origin", remote.path().to_str().unwrap()],
    );
    git(repo.path(), &["push", "-q", "origin", "rolling"]);
    git(repo.path(), &["fetch", "-q", "origin"]);
    git(repo.path(), &["commit", "-q", "--allow-empty", "-m", "fix"]);
    let fix = git(repo.path(), &["rev-parse", "HEAD"]);

    let world = World::new();
    let (job, state) = build_wake_when_job(WakeWhenRequest {
        message: "landed?".into(),
        name: None,
        predicate: WakePredicate {
            jobs_terminal: None,
            sha_on_rolling: Some(fix.clone()),
            issue_closed: None,
            todo_closed: None,
            topology_execution_settled: None,
            health_anomaly: None,
            settlement_run_settled: None,
        },
        timeout_seconds: None,
        working_dir: repo.path().to_path_buf(),
        provider: None,
        model: None,
        project_id: None,
        origin_session_id: world.owner,
    })
    .unwrap();
    world
        .store
        .lock()
        .await
        .insert_wake_when(&job, &state, false)
        .unwrap();
    world.pass().await;
    assert!(world.launcher.deliveries().is_empty(), "not on rolling yet");

    // The lander publishes to the remote; nothing fetches into `repo`.
    git(repo.path(), &["push", "-q", "origin", "rolling"]);
    git(
        repo.path(),
        &["update-ref", "refs/remotes/origin/rolling", "HEAD~1"],
    );
    let fetch_head = repo.path().join(".git/FETCH_HEAD");
    std::fs::write(&fetch_head, "sentinel\n").unwrap();
    make_due(&world, job.id).await;
    world.pass().await;
    let deliveries = world.launcher.deliveries();
    assert_eq!(deliveries.len(), 1, "the probe refreshed origin/rolling");
    assert_eq!(
        std::fs::read_to_string(&fetch_head).unwrap(),
        "sentinel\n",
        "the background fetch leaves the caller's FETCH_HEAD alone"
    );
    assert!(
        deliveries[0]
            .1
            .contains(&format!("sha_on_rolling {fix}: satisfied"))
    );
}

fn issue_closed(number: u32) -> WakePredicate {
    WakePredicate {
        issue_closed: Some(number),
        ..WakePredicate::default()
    }
}

fn arm_in_project(
    store: &Store,
    owner: Uuid,
    project: Option<Uuid>,
    predicate: WakePredicate,
) -> crate::error::Result<ScheduledJob> {
    let (job, state) = build_wake_when_job(WakeWhenRequest {
        message: "dependency landed?".into(),
        name: None,
        predicate,
        timeout_seconds: None,
        working_dir: "/tmp/sandbox".into(),
        provider: None,
        model: None,
        project_id: project,
        origin_session_id: owner,
    })
    .expect("builds");
    store.insert_wake_when(&job, &state, false)?;
    Ok(job)
}

fn project_with_issue(store: &Store) -> (Uuid, Uuid, u32) {
    let project = Uuid::new_v4();
    let now = Utc::now();
    store
        .insert_project(&rsi_common::types::Project {
            id: project,
            name: format!("p-{project}"),
            path: None,
            description: None,
            color: rsi_common::types::Project::DEFAULT_COLOR.into(),
            context_files: None,
            created_at: now,
            updated_at: now,
        })
        .expect("project");
    let issue = store
        .create_issue(&rsi_common::types::NewIssue {
            project_id: project,
            title: "dependency".into(),
            body: String::new(),
            priority: None,
            labels: Vec::new(),
            created_by_session_id: None,
            assignee: None,
            idea_id: None,
            source_event_id: None,
            source_finding_ref: None,
        })
        .expect("issue");
    let number = u32::try_from(issue.display_number).expect("number");
    (project, issue.id, number)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn issue_closed_fires_once_when_the_dependency_issue_closes_without_a_sha() {
    use rsi_common::types::IssueStatus;
    let world = World::new();
    let (project, issue_id, number) = project_with_issue(&*world.store.lock().await);
    let job = arm_in_project(
        &*world.store.lock().await,
        world.owner,
        Some(project),
        issue_closed(number),
    )
    .expect("armed");

    world.pass().await;
    assert!(
        world.launcher.deliveries().is_empty(),
        "Open: still pending"
    );
    let deferred = world
        .store
        .lock()
        .await
        .get_scheduled_job(&job.id)
        .unwrap()
        .unwrap();
    assert!(
        deferred.next_fire_at > Utc::now(),
        "polled at a bounded interval"
    );

    world
        .store
        .lock()
        .await
        .update_issue_status(issue_id, IssueStatus::InProgress)
        .unwrap();
    make_due(&world, job.id).await;
    world.pass().await;
    assert!(
        world.launcher.deliveries().is_empty(),
        "InProgress: still pending"
    );

    world
        .store
        .lock()
        .await
        .update_issue_status(issue_id, IssueStatus::Closed)
        .unwrap();
    make_due(&world, job.id).await;
    world.pass().await;
    let deliveries = world.launcher.deliveries();
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].0, world.owner);
    assert!(
        deliveries[0].1.contains(&format!(
            "issue_closed #{number}: satisfied\ntimed_out: false"
        )),
        "{}",
        deliveries[0].1
    );
    make_due(&world, job.id).await;
    world.pass().await;
    assert_eq!(world.launcher.deliveries().len(), 1, "fires at most once");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn issue_closed_settles_unsatisfiable_when_the_issue_is_cancelled() {
    use rsi_common::types::IssueStatus;
    let world = World::new();
    let (project, issue_id, number) = project_with_issue(&*world.store.lock().await);
    let job = arm_in_project(
        &*world.store.lock().await,
        world.owner,
        Some(project),
        issue_closed(number),
    )
    .expect("armed");
    world
        .store
        .lock()
        .await
        .update_issue_status(issue_id, IssueStatus::Cancelled)
        .unwrap();
    make_due(&world, job.id).await;
    world.pass().await;
    let deliveries = world.launcher.deliveries();
    assert_eq!(deliveries.len(), 1);
    assert!(
        deliveries[0].1.contains("issue_closed #"),
        "{}",
        deliveries[0].1
    );
    assert!(
        deliveries[0].1.contains("unsatisfiable")
            && deliveries[0].1.contains("reason: issue_cancelled"),
        "{}",
        deliveries[0].1
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn an_issue_outside_the_callers_project_is_refused_at_scheduling_time() {
    let store = Store::open_in_memory().expect("store");
    let (project, _, number) = project_with_issue(&store);
    let (other_project, _, other_number) = project_with_issue(&store);
    let owner = Uuid::new_v4();
    let refused = format!(
        "Invalid parameter: {}",
        rsi_common::wake_predicate::WAKE_WHEN_ISSUE_NOT_FOUND
    );
    for (scope, predicate) in [
        (Some(project), issue_closed(number + 1000)),
        (None, issue_closed(number)),
    ] {
        let error = arm_in_project(&store, owner, scope, predicate).expect_err("refused");
        assert_eq!(error.to_string(), refused);
    }
    // Each project resolves only its own Issue numbers.
    assert_eq!(
        arm_in_project(&store, owner, Some(other_project), issue_closed(number))
            .expect_err("refused")
            .to_string(),
        refused
    );
    assert!(
        arm_in_project(
            &store,
            owner,
            Some(other_project),
            issue_closed(other_number)
        )
        .is_ok()
    );
    assert!(arm_in_project(&store, owner, Some(project), issue_closed(number)).is_ok());
    assert_eq!(store.list_scheduled_jobs().unwrap().len(), 2);
}

fn todo_closed(id: Uuid) -> WakePredicate {
    WakePredicate {
        todo_closed: Some(id),
        ..WakePredicate::default()
    }
}

/// A `global` portfolio seat over a fresh project, and one open operator
/// to-do item in its scope.
fn seat_with_todo(store: &Store) -> (Uuid, Uuid) {
    use crate::store::portfolio_nodes::PortfolioGrantor;
    use rsi_common::harness_manager_v2::{ManagerLaunchChoiceV2, ManagerPolicyV2};
    use rsi_common::operator_todos::AgentOperatorTodoUpsertRequestV1;
    use rsi_common::portfolio_nodes::ConfigurePortfolioNodeRequestV1;
    use rsi_common::types::{SessionProvider, SessionStatus};
    let (project, _, _) = project_with_issue(store);
    let seat = Uuid::new_v4();
    let mut row = rsid_store::test_support::test_session(seat, "/tmp/sandbox".into());
    row.status = SessionStatus::Completed;
    store.insert_session(&row).expect("seat session");
    store
        .configure_portfolio_node_confirmed(
            &ConfigurePortfolioNodeRequestV1 {
                node_id: None,
                parent_node_id: None,
                adopt_node_ids: vec![],
                expected_parent_grant_version: None,
                tier_label: "global".into(),
                seat_session_id: seat,
                project_ids: vec![project],
                allowed_launches: vec![ManagerLaunchChoiceV2 {
                    provider: SessionProvider::Claude,
                    model: "claude-opus-5-5".into(),
                    effort: None,
                }],
                policy: ManagerPolicyV2::default(),
                child_policy: None,
                max_direct_reports: 5,
                expected_node_grant_version: 0,
                expected_authority_epoch: 0,
                idempotency_key: format!("todo-wake-{seat}"),
            },
            PortfolioGrantor::Operator,
            "operator:test",
            true,
        )
        .expect("portfolio seat");
    let actor = store.operator_todo_agent_actor(seat).expect("actor");
    let todo = store
        .operator_todo_upsert(
            &actor,
            &AgentOperatorTodoUpsertRequestV1 {
                todo_key: "pm-hold-release".into(),
                title: "Release the PM hold".into(),
                how: "rsi hold release".into(),
                link: None,
                priority: None,
                idempotency_key: "todo-wake-k1".into(),
            },
        )
        .expect("todo")
        .todo;
    (seat, todo.id)
}

async fn close_todo(world: &World, seat: Uuid, status: OperatorTodoStatusV1) {
    use rsi_common::operator_todos::AgentOperatorTodoCloseRequestV1;
    let store = world.store.lock().await;
    let actor = store.operator_todo_agent_actor(seat).expect("actor");
    store
        .operator_todo_agent_close(
            &actor,
            &AgentOperatorTodoCloseRequestV1 {
                todo_id: None,
                todo_key: Some("pm-hold-release".into()),
                status,
                evidence: "[observed] hold flag reads released".into(),
                observed: true,
                idempotency_key: format!("close-{}", status.as_str()),
            },
        )
        .expect("close");
}

use rsi_common::operator_todos::OperatorTodoStatusV1;

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn todo_closed_fires_once_when_the_item_is_done_or_dismissed_with_its_evidence() {
    for status in [OperatorTodoStatusV1::Done, OperatorTodoStatusV1::Dismissed] {
        let world = World::new();
        let (seat, todo) = seat_with_todo(&*world.store.lock().await);
        let job = arm_in_project(&*world.store.lock().await, seat, None, todo_closed(todo))
            .expect("armed");

        world.pass().await;
        assert!(world.launcher.deliveries().is_empty(), "Open: pending");

        close_todo(&world, seat, status).await;
        make_due(&world, job.id).await;
        world.pass().await;
        let deliveries = world.launcher.deliveries();
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].0, seat);
        let body = &deliveries[0].1;
        assert!(
            body.contains(&format!("todo_closed {todo}: satisfied\ntimed_out: false")),
            "{body}"
        );
        assert!(
            body.contains(&format!("status={}", status.as_str()))
                && body.contains("evidence=[observed] hold flag reads released"),
            "{body}"
        );
        make_due(&world, job.id).await;
        world.pass().await;
        assert_eq!(world.launcher.deliveries().len(), 1, "fires at most once");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn todo_closed_times_out_while_the_item_stays_open() {
    let world = World::new();
    let (seat, todo) = seat_with_todo(&*world.store.lock().await);
    let job = arm(
        &*world.store.lock().await,
        seat,
        todo_closed(todo),
        Some(60),
    )
    .expect("armed");
    let mut state = world
        .store
        .lock()
        .await
        .wake_when_state(job.id)
        .unwrap()
        .expect("state");
    state.deadline = Some(Utc::now() - chrono::Duration::seconds(1));
    world
        .store
        .lock()
        .await
        .set_wake_when_state_for_test(job.id, &state)
        .unwrap();
    make_due(&world, job.id).await;
    world.pass().await;
    let deliveries = world.launcher.deliveries();
    assert_eq!(deliveries.len(), 1);
    assert!(
        deliveries[0].1.contains("todo_closed")
            && deliveries[0].1.contains("timed_out: true")
            && deliveries[0].1.contains("status=open"),
        "{}",
        deliveries[0].1
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn a_todo_outside_the_callers_scope_or_unknown_is_refused_at_scheduling_time() {
    let store = Store::open_in_memory().expect("store");
    let (seat, todo) = seat_with_todo(&store);
    let refused = format!(
        "Invalid parameter: {}",
        rsi_common::wake_predicate::WAKE_WHEN_TODO_NOT_FOUND
    );
    // An unknown id, and a caller with no manager seat, are the same refusal.
    for (owner, id) in [(seat, Uuid::new_v4()), (Uuid::new_v4(), todo)] {
        let error = arm(&store, owner, todo_closed(id), None).expect_err("refused");
        assert_eq!(error.to_string(), refused);
    }
    assert!(arm(&store, seat, todo_closed(todo), None).is_ok());
    assert_eq!(store.list_scheduled_jobs().unwrap().len(), 1);
}

/// #1935: a settled `when` wake whose covered worker already wrote its
/// `PIPELINE HANDOFF` after arming it is retired without a delivery; a wake
/// armed after the handoff, and an uncovered owner's wake, still deliver.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_when_wake_armed_before_the_owners_handoff_is_retired_not_delivered() {
    use rsi_common::types::{ConversationEvent, EventType, Role};
    let store = Store::open_in_memory().expect("store");
    let manager = rsid_store::test_support::make_test_session();
    store.insert_session(&manager).expect("manager");
    let mut worker = rsid_store::test_support::make_test_session();
    worker.parent_id = Some(manager.id);
    store.insert_session(&worker).expect("worker");
    let mut world = World::with_store(store);
    world.owner = worker.id;

    let (stale, job_id) = {
        let store = world.store.lock().await;
        let job_id = insert_job(&store, world.owner, "check");
        let stale = arm(&store, world.owner, jobs_terminal(&[job_id]), None).unwrap();
        finish(&store, job_id, JobState::Succeeded, 0, None);
        store
            .insert_event(&ConversationEvent {
                id: 0,
                session_id: worker.id,
                sequence: 1,
                event_type: EventType::Message,
                role: Some(Role::Assistant),
                content: "PIPELINE HANDOFF — IMPLEMENTATION:\nFriction: none".into(),
                tool_name: None,
                tool_input: None,
                created_at: Utc::now() + chrono::Duration::seconds(5),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            })
            .expect("handoff");
        (stale, job_id)
    };
    world.pass().await;
    assert!(
        world.launcher.deliveries().is_empty(),
        "a wake armed before the handoff opens no new turn"
    );
    let row = world
        .store
        .lock()
        .await
        .get_scheduled_job(&stale.id)
        .unwrap()
        .expect("row kept");
    assert!(!row.enabled, "the superseded wake is retired");

    // A wake armed after the handoff is a real continuation and delivers.
    let fresh = {
        let store = world.store.lock().await;
        let mut fresh = arm(&store, world.owner, jobs_terminal(&[job_id]), None).unwrap();
        fresh.created_at = Utc::now() + chrono::Duration::seconds(60);
        store
            .conn
            .execute(
                "UPDATE scheduled_jobs SET created_at=?2 WHERE id=?1",
                rusqlite::params![
                    fresh.id.to_string(),
                    fresh
                        .created_at
                        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                ],
            )
            .unwrap();
        fresh
    };
    make_due(&world, fresh.id).await;
    world.pass().await;
    assert_eq!(world.launcher.deliveries().len(), 1);
}

/// #2054: a worker that wrote a provisional `PIPELINE HANDOFF` while its
/// `jobs_terminal` predicate was still pending is resumed once when the job
/// ends: the handoff predates the result, so it must not retire the wake.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_provisional_handoff_while_the_job_runs_does_not_retire_the_wake() {
    use rsi_common::types::{ConversationEvent, EventType, Role};
    let store = Store::open_in_memory().expect("store");
    let manager = rsid_store::test_support::make_test_session();
    store.insert_session(&manager).expect("manager");
    let mut worker = rsid_store::test_support::make_test_session();
    worker.parent_id = Some(manager.id);
    store.insert_session(&worker).expect("worker");
    let mut world = World::with_store(store);
    world.owner = worker.id;

    let wake = {
        let store = world.store.lock().await;
        let job_id = insert_job(&store, world.owner, "check");
        let wake = arm(&store, world.owner, jobs_terminal(&[job_id]), None).unwrap();
        store
            .insert_event(&ConversationEvent {
                id: 0,
                session_id: worker.id,
                sequence: 1,
                event_type: EventType::Message,
                role: Some(Role::Assistant),
                content: "PIPELINE HANDOFF — IMPLEMENTATION:\nprovisional\nFriction: none".into(),
                tool_name: None,
                tool_input: None,
                created_at: Utc::now() + chrono::Duration::seconds(1),
                offload_id: None,
                tool_use_id: None,
                metadata: None,
            })
            .expect("handoff");
        // The job ends after the provisional handoff.
        store
            .settle_agent_job(
                job_id,
                JobState::Succeeded,
                &AgentJobResultV1 {
                    exit_code: Some(0),
                    ..AgentJobResultV1::default()
                },
                false,
                Utc::now() + chrono::Duration::seconds(5),
            )
            .expect("settle");
        wake
    };
    make_due(&world, wake.id).await;
    world.pass().await;
    let deliveries = world.launcher.deliveries();
    assert_eq!(deliveries.len(), 1, "the worker is resumed with the result");
    assert!(deliveries[0].1.contains("succeeded") || deliveries[0].1.contains("Succeeded"));
}

/// #2054 (#2051 parked semantics): while a worker's `when` wake is enabled the
/// child watch stays quiet; a cancelled or disabled wake releases it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_disabled_wake_lets_the_terminal_watcher_fire() {
    let store = Store::open_in_memory().expect("store");
    let mut worker = rsid_store::test_support::make_test_session();
    worker.status = rsi_common::types::SessionStatus::Completed;
    store.insert_session(&worker).expect("worker");
    let job_id = insert_job(&store, worker.id, "check");
    let wake = arm(&store, worker.id, jobs_terminal(&[job_id]), None).unwrap();
    // Only the wake may hold the watcher, not a still-running job (#1588).
    finish(&store, job_id, JobState::Succeeded, 0, None);
    assert!(
        store.worker_terminal_watch_pending(&worker).unwrap(),
        "an enabled wake keeps the watcher quiet"
    );
    store
        .update_scheduled_job(
            &wake.id,
            &rsid_store::store::scheduled_jobs::ScheduledJobUpdate {
                name: None,
                message: None,
                schedule: None,
                enabled: Some(false),
                next_fire_at: None,
            },
        )
        .unwrap();
    assert!(
        !store.worker_terminal_watch_pending(&worker).unwrap(),
        "a disabled wake lets the watcher fire"
    );
}

/// The wake row as the scheduler left it.
async fn wake_row(world: &World, id: Uuid) -> ScheduledJob {
    world
        .store
        .lock()
        .await
        .get_scheduled_job(&id)
        .unwrap()
        .unwrap()
}

/// #2204: a `when` wake whose predicate is satisfied while the daemon is
/// stopping, or while a waiting deploy holds its owner, is neither delivered
/// nor consumed. It is delivered once on the next incarnation, and only then
/// recorded fired.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_when_wake_satisfied_during_a_drain_is_delivered_once_after_the_restart_2204() {
    use std::sync::atomic::Ordering::SeqCst;
    let world = World::new();
    let (job_id, wake) = {
        let store = world.store.lock().await;
        let job_id = insert_job(&store, world.owner, "verify");
        let wake = arm(&store, world.owner, jobs_terminal(&[job_id]), None).unwrap();
        finish(&store, job_id, JobState::Succeeded, 0, None);
        (job_id, wake)
    };
    make_due(&world, wake.id).await;
    let due = wake_row(&world, wake.id).await;

    // The restart is decided: the fast lane fires nothing.
    world.launcher.restart_draining.store(true, SeqCst);
    world.pass().await;
    assert_eq!(world.launcher.resume_attempts.load(SeqCst), 0);

    // A waiting deploy holds the owner's resume: no continuation is attempted.
    world.launcher.restart_draining.store(false, SeqCst);
    world.launcher.deploy_hold.store(true, SeqCst);
    world.pass().await;
    assert_eq!(world.launcher.resume_attempts.load(SeqCst), 0);

    // The drain engages after the scheduler's check: the refusal consumes
    // nothing.
    world.launcher.deploy_hold.store(false, SeqCst);
    world.launcher.drain_refusals.store(1, SeqCst);
    world.pass().await;
    assert_eq!(world.launcher.resume_attempts.load(SeqCst), 1);

    assert!(world.launcher.deliveries().is_empty());
    let held = wake_row(&world, wake.id).await;
    assert!(held.enabled, "an undelivered wake stays armed");
    assert_eq!(held.last_fired_at, None, "never fired while undelivered");
    assert_eq!(held.next_fire_at, due.next_fire_at);

    // The next incarnation delivers it once and only then records it fired.
    world.pass().await;
    let deliveries = world.launcher.deliveries();
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].0, world.owner);
    assert!(deliveries[0].1.contains(&job_id.to_string()));
    let spent = wake_row(&world, wake.id).await;
    assert!(!spent.enabled);
    assert!(spent.last_fired_at.is_some(), "fired once it was delivered");
    make_due(&world, wake.id).await;
    world.pass().await;
    assert_eq!(world.launcher.deliveries().len(), 1, "exactly once");
}

/// #2204: a test job a staged deploy stopped is requeued under its own id, so
/// the owner's `jobs_terminal` wake is not satisfied by the interruption. It
/// fires once, on the rerun's end, and its message names the requeued job and
/// the deploy that did it.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_wake_follows_a_job_the_deploy_requeued_and_names_it_2204() {
    let world = World::new();
    let deploy = Uuid::new_v4();
    let (job_id, plain_id, wake) = {
        let store = world.store.lock().await;
        let job_id = insert_job(&store, world.owner, "verify");
        let plain_id = insert_job(&store, world.owner, "other");
        let wake = arm(
            &store,
            world.owner,
            jobs_terminal(&[job_id, plain_id]),
            None,
        )
        .unwrap();
        // The deploy stops the running job and puts it back in the queue.
        assert!(store.requeue_agent_job_for_deploy(job_id, deploy).unwrap());
        finish(&store, plain_id, JobState::Succeeded, 0, None);
        (job_id, plain_id, wake)
    };
    make_due(&world, wake.id).await;
    world.pass().await;
    assert!(
        world.launcher.deliveries().is_empty(),
        "the interrupted run does not satisfy the wait"
    );
    assert!(wake_row(&world, wake.id).await.enabled);

    // The new daemon reruns the same job id; its end settles the wait.
    {
        let store = world.store.lock().await;
        assert!(store.start_queued_agent_job(job_id, Utc::now()).unwrap());
        finish(&store, job_id, JobState::Succeeded, 0, None);
    }
    make_due(&world, wake.id).await;
    world.pass().await;
    let deliveries = world.launcher.deliveries();
    assert_eq!(deliveries.len(), 1, "one wake, on the rerun");
    let message = &deliveries[0].1;
    assert!(
        message.contains(&format!("job {job_id} name=verify state=succeeded")),
        "{message}"
    );
    assert!(
        message.contains(&format!(
            "job {job_id} was stopped by deploy {deploy} and requeued"
        )),
        "{message}"
    );
    assert!(message.contains("Do not resubmit"), "{message}");
    assert!(
        !message.contains(&format!("job {plain_id} was stopped")),
        "a job no deploy touched carries no note: {message}"
    );
}

// ---- #2747: `when.health_anomaly` --------------------------------------------

use crate::health_anomaly::{
    HostReading, LiveHealthInputs, PmContext, SatelliteReading, TEST_HOST, sustain,
};
use rsi_common::health_anomaly::{
    CONTEXT_FULL_GRACE_SECS, HealthAnomalySpec, KIND_ATTEMPT_PAST_WALL_LIMIT, KIND_CPU_HOT,
    KIND_EXECUTION_BLOCKED, KIND_HUB_DISK_HIGH, KIND_PM_CONTEXT_FULL, KIND_PROVIDER_AUTH_FAILURE,
    KIND_SATELLITE_DISK_LOW, THERMAL_SUSTAIN_SECS,
};

fn health(spec: HealthAnomalySpec) -> WakePredicate {
    WakePredicate {
        health_anomaly: Some(spec),
        ..WakePredicate::default()
    }
}

/// The host this test thread reads; restored to the real host on drop.
struct HostGuard;

impl HostGuard {
    fn set(disk_used_pct: f64, cpu_c: f64) -> Self {
        TEST_HOST.with(|cell| {
            cell.set(Some(HostReading {
                disk_used_pct: Some(disk_used_pct),
                cpu_c: Some(cpu_c),
                build_cache_reclaim_unsupported: false,
            }));
        });
        Self
    }
}

impl Drop for HostGuard {
    fn drop(&mut self) {
        TEST_HOST.with(|cell| cell.set(None));
    }
}

/// Arm, evaluate until the wake settles or `passes` run out, and return the
/// deliveries. Every pass re-arms the 60 s defer so it is due again.
async fn health_run(world: &World, spec: HealthAnomalySpec, passes: usize) -> Vec<String> {
    health_run_seeded(world, spec, passes, |_| {}).await
}

/// `seed` runs once the wake exists, to start a "held for N seconds" clock in
/// the past without waiting for it.
async fn health_run_seeded(
    world: &World,
    spec: HealthAnomalySpec,
    passes: usize,
    seed: impl FnOnce(Uuid),
) -> Vec<String> {
    let wake = arm(&*world.store.lock().await, world.owner, health(spec), None).unwrap();
    seed(wake.id);
    for _ in 0..passes {
        make_due(world, wake.id).await;
        world.pass().await;
    }
    world
        .launcher
        .deliveries()
        .into_iter()
        .map(|(target, message)| {
            assert_eq!(target, world.owner);
            message
        })
        .collect()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn health_anomaly_with_nothing_abnormal_takes_no_model_turn() {
    let _host = HostGuard::set(41.0, 48.0);
    let world = World::new();
    let wake = arm(
        &*world.store.lock().await,
        world.owner,
        health(HealthAnomalySpec::default()),
        None,
    )
    .unwrap();
    // A day of one-minute evaluations: not one resume, the wake stays armed.
    for _ in 0..24 * 60 {
        make_due(&world, wake.id).await;
        world.pass().await;
    }
    assert!(world.launcher.deliveries().is_empty());
    assert_eq!(
        world
            .launcher
            .resume_attempts
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    let row = world
        .store
        .lock()
        .await
        .get_scheduled_job(&wake.id)
        .unwrap()
        .unwrap();
    assert!(row.enabled, "a quiet daemon leaves the wake armed");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn an_injected_host_condition_wakes_the_owner_exactly_once_naming_it() {
    let _host = HostGuard::set(98.0, 50.0);
    let world = World::new();
    let messages = health_run(&world, HealthAnomalySpec::default(), 5).await;
    assert_eq!(
        messages.len(),
        1,
        "one wake, however many evaluations: {messages:?}"
    );
    let message = &messages[0];
    assert!(
        message.contains("[wake_when] health_anomaly: satisfied"),
        "{message}"
    );
    assert!(
        message.contains(&format!("ignore_key={KIND_HUB_DISK_HIGH}:/ ")),
        "{message}"
    );
    assert!(message.contains("used=98.0%"), "{message}");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn an_accepted_condition_in_ignore_does_not_wake() {
    let _host = HostGuard::set(98.0, 50.0);
    let world = World::new();
    let spec = HealthAnomalySpec {
        ignore: vec![format!("{KIND_HUB_DISK_HIGH}:/")],
        ..HealthAnomalySpec::default()
    };
    assert!(health_run(&world, spec, 3).await.is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn injected_live_conditions_each_wake_once_with_their_subject() {
    let _host = HostGuard::set(41.0, 50.0);
    let (session, peer) = (Uuid::new_v4(), Uuid::new_v4());
    let world = World::new();
    *world.launcher.health_live.lock().unwrap() = LiveHealthInputs {
        pm_contexts: vec![PmContext {
            project_id: Uuid::new_v4(),
            session_id: session,
            fill_pct: 71.0,
            threshold_pct: 60.0,
            succession_queued: false,
        }],
        satellites: vec![SatelliteReading {
            peer_id: peer,
            label: "laptop".into(),
            disk_free_bytes: Some(1 << 30),
            disk_floor_bytes: Some(30 << 30),
            provider_faults: vec![rsi_common::satellite::SatelliteProviderFaultV1 {
                provider: "Codex".into(),
                kind: rsi_common::satellite::SATELLITE_FAULT_AUTH.into(),
                model: None,
            }],
        }],
    };
    // The PM has been over its threshold for longer than the grace.
    let messages = health_run_seeded(&world, HealthAnomalySpec::default(), 4, |wake| {
        sustain().lock().unwrap().entry(wake).or_default().held(
            &format!("{KIND_PM_CONTEXT_FULL}:{session}"),
            true,
            Utc::now() - chrono::Duration::seconds(CONTEXT_FULL_GRACE_SECS + 5),
            CONTEXT_FULL_GRACE_SECS,
        );
    })
    .await;
    assert_eq!(messages.len(), 1, "{messages:?}");
    let message = &messages[0];
    for expected in [
        format!("ignore_key={KIND_PM_CONTEXT_FULL}:{session} "),
        format!("ignore_key={KIND_SATELLITE_DISK_LOW}:{peer} "),
        format!("ignore_key={KIND_PROVIDER_AUTH_FAILURE}:{peer}:Codex "),
    ] {
        assert!(message.contains(&expected), "{expected}\n{message}");
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_sustained_hot_cpu_wakes_once_and_a_brief_one_does_not() {
    let _host = HostGuard::set(41.0, 91.0);
    let world = World::new();
    let messages = health_run_seeded(&world, HealthAnomalySpec::default(), 3, |wake| {
        sustain().lock().unwrap().entry(wake).or_default().held(
            KIND_CPU_HOT,
            true,
            Utc::now() - chrono::Duration::seconds(THERMAL_SUSTAIN_SECS + 5),
            THERMAL_SUSTAIN_SECS,
        );
    })
    .await;
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(messages[0].contains(&format!("ignore_key={KIND_CPU_HOT}:hub ")));
    // A spike that has only just begun is not sustained.
    let spike = World::new();
    assert!(
        health_run(&spike, HealthAnomalySpec::default(), 3)
            .await
            .is_empty()
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn stuck_topology_work_in_the_database_wakes_once_with_the_ids() {
    let _host = HostGuard::set(41.0, 50.0);
    let world = World::new();
    let now = Utc::now();
    let (execution, attempt) = (Uuid::new_v4(), Uuid::new_v4());
    {
        let store = world.store.lock().await;
        let project = crate::health_anomaly::tests::project_row(&store);
        crate::health_anomaly::tests::portfolio_seat(&store, "global", world.owner, &[project]);
        let ago = |minutes: i64| (now - chrono::Duration::minutes(minutes)).to_rfc3339();
        for (id, status, deadline) in [
            (execution, "blocked", None),
            (Uuid::new_v4(), "running", Some(ago(120))),
        ] {
            store
                .conn
                .execute(
                    "INSERT INTO topology_executions (id, topology_name_snapshot, workflow_id,
                         definition_json, definition_digest, project_id, requested_by_kind,
                         repo_root, base_commit, custody_plan_json, status, deadline_at,
                         created_at, updated_at)
                     VALUES (?1,'t','w','{}','d',?6,'operator','/tmp',?2,'{}',?3,?4,?5,?5)",
                    rusqlite::params![
                        id.to_string(),
                        "b".repeat(40),
                        status,
                        deadline,
                        ago(90),
                        project.to_string()
                    ],
                )
                .unwrap();
        }
        let overrun: String = store
            .conn
            .query_row(
                "SELECT id FROM topology_executions WHERE status='running'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO topology_node_attempts (id, execution_id, node_id, iteration,
                     attempt_no, node_kind, status, dedup_key, input_json, created_at, updated_at)
                 VALUES (?1,?2,'implement',0,1,'session','running',?1,'{}',?3,?3)",
                rusqlite::params![attempt.to_string(), overrun, now.to_rfc3339()],
            )
            .unwrap();
    }
    let messages = health_run(&world, HealthAnomalySpec::default(), 3).await;
    assert_eq!(messages.len(), 1, "{messages:?}");
    let message = &messages[0];
    assert!(
        message.contains(&format!("ignore_key={KIND_EXECUTION_BLOCKED}:{execution} ")),
        "{message}"
    );
    assert!(
        message.contains(&format!(
            "ignore_key={KIND_ATTEMPT_PAST_WALL_LIMIT}:{attempt} "
        )),
        "{message}"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn only_a_manager_seat_may_arm_a_health_wake() {
    let store = Store::open_in_memory().unwrap();
    let owner = Uuid::new_v4();
    let (job, state) = build_wake_when_job(WakeWhenRequest {
        message: "health".into(),
        name: None,
        predicate: health(HealthAnomalySpec::default()),
        timeout_seconds: None,
        working_dir: "/tmp/sandbox".into(),
        provider: None,
        model: None,
        project_id: None,
        origin_session_id: owner,
    })
    .unwrap();
    let refused = arm_authorized(&store, &job, &state, false).unwrap_err();
    assert!(
        refused
            .to_string()
            .contains(rsi_common::wake_predicate::WAKE_WHEN_HEALTH_MANAGER_ONLY),
        "{refused}"
    );
    assert!(
        store.wake_when_state(job.id).unwrap().is_none(),
        "nothing armed"
    );
}

// ---- #2788: the wake reports only what the owner's grant covers ---------------

/// Arm through the production path (`arm_authorized`), so the stored scope is
/// the one the daemon pins, not the test's.
fn arm_pinned(store: &Store, owner: Uuid) -> ScheduledJob {
    let (job, state) = build_wake_when_job(WakeWhenRequest {
        message: "health".into(),
        name: None,
        predicate: health(HealthAnomalySpec::default()),
        timeout_seconds: None,
        working_dir: "/tmp/sandbox".into(),
        provider: None,
        model: None,
        project_id: None,
        origin_session_id: owner,
    })
    .unwrap();
    arm_authorized(store, &job, &state, false).unwrap();
    job
}

fn blocked_execution(store: &Store, project: Uuid, now: chrono::DateTime<Utc>) -> Uuid {
    let id = Uuid::new_v4();
    store
        .conn
        .execute(
            "INSERT INTO topology_executions (id, topology_name_snapshot, workflow_id,
                 definition_json, definition_digest, project_id, requested_by_kind, repo_root,
                 base_commit, custody_plan_json, status, blocked_reason_json, created_at,
                 updated_at)
             VALUES (?1,'t','w','{}','d',?2,'operator','/tmp',?3,'{}','blocked',
                     '{\"kind\":\"review_unsettled\"}',?4,?4)",
            rusqlite::params![
                id.to_string(),
                project.to_string(),
                "b".repeat(40),
                (now - chrono::Duration::minutes(90)).to_rfc3339()
            ],
        )
        .unwrap();
    id
}

async fn deliveries_after(world: &World, wake: Uuid, passes: usize) -> Vec<(Uuid, String)> {
    for _ in 0..passes {
        make_due(world, wake).await;
        world.pass().await;
    }
    world.launcher.deliveries()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_blocked_execution_wakes_only_the_seat_covering_its_project_2788() {
    let _host = HostGuard::set(41.0, 50.0);
    let world = World::new();
    let (seat_a, seat_b) = (world.owner, Uuid::new_v4());
    let now = Utc::now();
    let (wake_a, wake_b, blocked_b) = {
        let store = world.store.lock().await;
        let a = crate::health_anomaly::tests::project_row(&store);
        let b = crate::health_anomaly::tests::project_row(&store);
        crate::health_anomaly::tests::portfolio_seat(&store, "swarm-a", seat_a, &[a]);
        crate::health_anomaly::tests::portfolio_seat(&store, "swarm-b", seat_b, &[b]);
        let blocked_b = blocked_execution(&store, b, now);
        (
            arm_pinned(&store, seat_a),
            arm_pinned(&store, seat_b),
            blocked_b,
        )
    };
    // Both wakes are due on every pass. Project B's blocked execution is
    // nothing to seat A, however long it waits; seat B hears of it once.
    deliveries_after(&world, wake_a.id, 4).await;
    deliveries_after(&world, wake_b.id, 3).await;
    let delivered = world.launcher.deliveries();
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    assert_eq!(delivered[0].0, seat_b, "only seat B is woken");
    assert!(
        delivered[0]
            .1
            .contains(&format!("ignore_key={KIND_EXECUTION_BLOCKED}:{blocked_b} ")),
        "{}",
        delivered[0].1
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_grant_narrowed_after_arming_stops_reporting_the_removed_project_2788() {
    let _host = HostGuard::set(41.0, 50.0);
    let world = World::new();
    let now = Utc::now();
    let wake = {
        let store = world.store.lock().await;
        let a = crate::health_anomaly::tests::project_row(&store);
        let b = crate::health_anomaly::tests::project_row(&store);
        let node =
            crate::health_anomaly::tests::portfolio_seat(&store, "global", world.owner, &[a, b]);
        let wake = arm_pinned(&store, world.owner);
        let stored = store.wake_when_state(wake.id).unwrap().unwrap();
        let spec = stored.predicate.health_anomaly.unwrap();
        assert_eq!(spec.projects.len(), 2, "armed over the whole grant");
        assert_eq!(spec.project, None);
        blocked_execution(&store, b, now);
        // The operator takes B away after the wake was armed.
        crate::health_anomaly::tests::narrow_seat(&store, &node, &[a]);
        wake
    };
    assert!(deliveries_after(&world, wake.id, 4).await.is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_seat_covering_every_project_still_hears_of_all_of_them_2788() {
    let _host = HostGuard::set(41.0, 50.0);
    let world = World::new();
    let now = Utc::now();
    let (wake, blocked) = {
        let store = world.store.lock().await;
        let a = crate::health_anomaly::tests::project_row(&store);
        let b = crate::health_anomaly::tests::project_row(&store);
        crate::health_anomaly::tests::portfolio_seat(&store, "global", world.owner, &[a, b]);
        let blocked = [
            blocked_execution(&store, a, now),
            blocked_execution(&store, b, now),
        ];
        (arm_pinned(&store, world.owner), blocked)
    };
    let delivered = deliveries_after(&world, wake.id, 3).await;
    assert_eq!(delivered.len(), 1, "{delivered:?}");
    for id in blocked {
        assert!(
            delivered[0]
                .1
                .contains(&format!("ignore_key={KIND_EXECUTION_BLOCKED}:{id} ")),
            "{}",
            delivered[0].1
        );
    }
}

/// #2789: a minimal settlement run for the `settlement_run_settled` verdicts.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
fn settlement_run(finished: bool) -> rsi_common::cohort_settlement::SourceWorktreeSettlementRunV1 {
    use rsi_common::cohort_settlement::{
        SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION, SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
        SourceWorktreeGitOidV1, SourceWorktreeSettlementCountsV1, SourceWorktreeSettlementItemV1,
        SourceWorktreeSettlementPhaseV1, SourceWorktreeSettlementRunStateV1,
        SourceWorktreeSettlementRunV1,
    };
    use rsi_common::types::Sha256Digest;
    let timestamp = "2026-10-16T00:00:00.000000000Z".to_string();
    SourceWorktreeSettlementRunV1 {
        attribution: None,
        schema_version: SOURCE_WORKTREE_SETTLEMENT_SCHEMA_VERSION,
        policy_version: SOURCE_WORKTREE_SETTLEMENT_POLICY_VERSION,
        run_id: Uuid::new_v4(),
        repository_identity: "repo:test".into(),
        canonical_repo_dir: "/tmp/repository".into(),
        target_ref: "refs/heads/rolling".into(),
        target_oid: SourceWorktreeGitOidV1::parse("a".repeat(40)).unwrap(),
        plan_digest: Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).unwrap(),
        idempotency_key: "settlement-test".into(),
        state: if finished {
            SourceWorktreeSettlementRunStateV1::Settled
        } else {
            SourceWorktreeSettlementRunStateV1::Applying
        },
        counts: SourceWorktreeSettlementCountsV1 {
            observed: 1,
            eligible: 1,
            retained: 0,
            settled: u32::from(finished),
            refused: 0,
            recovery_required: 0,
            unattempted: 0,
        },
        items: vec![SourceWorktreeSettlementItemV1 {
            sequence: 0,
            session_id: Uuid::new_v4(),
            custody_id: Uuid::new_v4(),
            source_ref: "refs/heads/rsi/source".into(),
            expected_source_oid: SourceWorktreeGitOidV1::parse("b".repeat(40)).unwrap(),
            phase: if finished {
                SourceWorktreeSettlementPhaseV1::Settled
            } else {
                SourceWorktreeSettlementPhaseV1::IntentCommitted
            },
            refusal_code: None,
            before_observation: None,
            after_observation: None,
        }],
        retained_roots: Vec::new(),
        created_at: timestamp.clone(),
        updated_at: timestamp.clone(),
        finished_at: finished.then_some(timestamp),
        terminal_error: None,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn settlement_run_settled_waits_for_the_run_to_finish_2789() {
    use super::wake_when::decide_settlement_run;
    let run_id = Uuid::new_v4();
    let state = |deadline| WakeWhenState {
        predicate: WakePredicate {
            settlement_run_settled: Some(run_id),
            ..WakePredicate::default()
        },
        armed_at: Utc::now(),
        deadline,
        repo_dir: None,
    };
    let now = Utc::now();
    assert_eq!(
        decide_settlement_run(Some(&settlement_run(true)), &state(None), now, false),
        Verdict::Satisfied
    );
    // A run that is still applying keeps the wake pending, whatever its counts.
    assert_eq!(
        decide_settlement_run(Some(&settlement_run(false)), &state(None), now, false),
        Verdict::Pending
    );
    assert_eq!(
        decide_settlement_run(
            Some(&settlement_run(false)),
            &state(Some(now - chrono::Duration::seconds(1))),
            now,
            false
        ),
        Verdict::TimedOut
    );
    assert_eq!(
        decide_settlement_run(None, &state(None), now, false),
        Verdict::Unsatisfiable("settlement_run_missing")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn an_unknown_settlement_run_is_refused_at_arming_and_a_vanished_one_wakes_unsatisfiable_2789()
 {
    let world = World::new();
    let run_id = Uuid::new_v4();
    let predicate = || WakePredicate {
        settlement_run_settled: Some(run_id),
        ..WakePredicate::default()
    };
    {
        let store = world.store.lock().await;
        let (job, state) = build_wake_when_job(WakeWhenRequest {
            message: "run done?".into(),
            name: None,
            predicate: predicate(),
            timeout_seconds: None,
            working_dir: "/tmp/sandbox".into(),
            provider: None,
            model: None,
            project_id: None,
            origin_session_id: world.owner,
        })
        .expect("builds");
        let refusal = super::wake_when::arm_authorized(&store, &job, &state, false)
            .expect_err("an unknown run cannot be armed");
        assert!(
            refusal
                .to_string()
                .contains(rsi_common::wake_predicate::WAKE_WHEN_SETTLEMENT_RUN_NOT_FOUND),
            "{refusal}"
        );
    }
    // A run that vanishes after arming settles the wait instead of stranding it.
    let wake = {
        let store = world.store.lock().await;
        arm(&store, world.owner, predicate(), None).unwrap()
    };
    make_due(&world, wake.id).await;
    world.pass().await;
    let deliveries = world.launcher.deliveries();
    assert_eq!(deliveries.len(), 1, "{deliveries:?}");
    let message = &deliveries[0].1;
    assert!(
        message.contains(&format!(
            "[wake_when] settlement_run_settled {run_id}: unsatisfiable"
        )),
        "{message}"
    );
    assert!(
        message.contains("reason: settlement_run_missing"),
        "{message}"
    );
}
