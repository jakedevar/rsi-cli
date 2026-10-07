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
struct RecordingLauncher {
    deliveries: StdMutex<Vec<(Uuid, String)>>,
}

impl RecordingLauncher {
    fn deliveries(&self) -> Vec<(Uuid, String)> {
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
        self.deliveries.lock().unwrap().push((target, message));
        Ok(target)
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
        }),
        cwd: "/tmp/sandbox".into(),
        unit_name: format!("rsi-job-{id}"),
        log_path: format!("/tmp/jobs/{id}.log"),
        status_path: format!("/tmp/jobs/{id}.status"),
        idempotency_key: None,
        wake,
    }
}

fn insert_job(store: &Store, owner: Uuid, name: &str) -> Uuid {
    let new = new_job(owner, name, JobWake::None);
    store
        .insert_agent_job(&new, Utc::now())
        .expect("insert job");
    new.id
}

fn finish(store: &Store, id: Uuid, state: JobState, code: i32, refusal: Option<&str>) {
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

fn arm(
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

fn jobs_terminal(ids: &[Uuid]) -> WakePredicate {
    WakePredicate {
        jobs_terminal: Some(ids.to_vec()),
        sha_on_rolling: None,
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
        let store = Store::open(&path).expect("open");
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
    for ids in [vec![theirs], vec![Uuid::new_v4()], vec![mine, theirs]] {
        let error = arm(&store, owner, jobs_terminal(&ids), None).expect_err("refused");
        assert_eq!(
            error.to_string(),
            format!(
                "Invalid parameter: {}",
                rsi_common::wake_predicate::WAKE_WHEN_JOB_NOT_FOUND
            ),
            "{ids:?}"
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
    let message = compose_message("m", &state, &verdict, &missing, None);
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
    let message = compose_message("go", &state, &Verdict::Satisfied, &snaps, None);
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
    make_due(&world, job.id).await;
    world.pass().await;
    let deliveries = world.launcher.deliveries();
    assert_eq!(deliveries.len(), 1, "the probe refreshed origin/rolling");
    assert!(
        deliveries[0]
            .1
            .contains(&format!("sha_on_rolling {fix}: satisfied"))
    );
}
