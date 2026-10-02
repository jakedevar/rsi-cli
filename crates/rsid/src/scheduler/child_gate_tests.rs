//! #794 S3: hold, keep-alive valve, and the no-second-writer rule.

use super::*;
use crate::session::harness::tools::schedule_wake::{
    ScheduleWakeRequest, build_agent_scheduled_job, deterministic_program_guard_job_id,
};
use crate::store::child_autonomy::{is_keepalive_row, keepalive_row_name};
use rsi_common::agent_contract::ProgramContinuationIntentV1;
use rsi_common::types::{ScheduledJob, Session};
use std::sync::Mutex as StdMutex;

/// Records resumes; any launch is a second writer and fails the test.
struct ResumeOnlyLauncher {
    resumes: StdMutex<Vec<Uuid>>,
    outcome: StdMutex<Option<crate::error::Result<Uuid>>>,
    /// #1073: the deploy drain holds every resume while set.
    deploy_hold: std::sync::atomic::AtomicBool,
}

impl ResumeOnlyLauncher {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            resumes: StdMutex::new(Vec::new()),
            outcome: StdMutex::new(None),
            deploy_hold: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn resumes(&self) -> Vec<Uuid> {
        self.resumes.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl SessionLauncher for ResumeOnlyLauncher {
    async fn launch(&self, _: LaunchConfig) -> crate::error::Result<Uuid> {
        panic!("a keep-alive or hold must never launch a session");
    }

    async fn launch_scheduled_fresh(&self, _: LaunchConfig, _: bool) -> crate::error::Result<Uuid> {
        panic!("a keep-alive or hold must never launch a Fresh session");
    }

    async fn resume_scheduled(&self, target: Uuid, _: String) -> crate::error::Result<Uuid> {
        self.resumes.lock().unwrap().push(target);
        self.outcome.lock().unwrap().take().unwrap_or(Ok(target))
    }

    async fn resume_scheduled_job(
        &self,
        target: Uuid,
        _: String,
        _: Vec<Uuid>,
    ) -> crate::error::Result<Uuid> {
        self.resumes.lock().unwrap().push(target);
        self.outcome.lock().unwrap().take().unwrap_or(Ok(target))
    }

    async fn deploy_holds_resume(&self, _: Uuid) -> bool {
        self.deploy_hold.load(std::sync::atomic::Ordering::SeqCst)
    }

    async fn fire_watch(&self, _: &ScheduledJob) -> crate::error::Result<WatchFireOutcome> {
        Ok(WatchFireOutcome::NotReady)
    }
}

fn session(id: Uuid, status: &str, age_secs: i64) -> Session {
    let at = Utc::now() - chrono::Duration::seconds(age_secs);
    serde_json::from_value(serde_json::json!({
        "id": id, "status": status, "provider": "Claude",
        "created_at": at, "updated_at": at,
        "query": "autonomy", "working_dir": "/tmp/rsi-child-autonomy",
    }))
    .expect("minimal session fixture deserializes")
}

fn agent_job(
    origin: Uuid,
    mode: &str,
    watch: Option<Uuid>,
    name: Option<String>,
    in_seconds: Option<i64>,
) -> ScheduledJob {
    build_agent_scheduled_job(ScheduleWakeRequest {
        message: "autonomy fixture".into(),
        in_seconds,
        at: None,
        name,
        every_seconds: None,
        mode: Some(mode.into()),
        working_dir: "/tmp/rsi-child-autonomy".into(),
        provider: None,
        model: None,
        project_id: None,
        origin_session_id: Some(origin),
        watch_session_id: watch,
    })
    .expect("fixture job builds")
}

struct World {
    store: Arc<Mutex<Store>>,
    bus: Arc<EventBus>,
    parent: Uuid,
    child: Uuid,
}

/// Parent `Completed` (idle for `idle_secs`) waiting on one `Running` child
/// through its automatic `agent-child-*` watch.
async fn world(idle_secs: i64) -> World {
    let store = Store::open_in_memory().expect("store");
    let (parent, child) = (Uuid::new_v4(), Uuid::new_v4());
    store
        .insert_session(&session(parent, "Completed", idle_secs))
        .unwrap();
    store
        .insert_session(&session(child, "Running", idle_secs))
        .unwrap();
    let watch = agent_job(
        parent,
        "on_terminal",
        Some(child),
        Some(format!("agent-child-{child}")),
        None,
    );
    store.insert_scheduled_job(&watch).unwrap();
    World {
        store: Arc::new(Mutex::new(store)),
        bus: Arc::new(EventBus::new(64)),
        parent,
        child,
    }
}

async fn register_program(world: &World) {
    let sentinel = agent_job(world.parent, "program_guard", None, None, None);
    assert_eq!(
        sentinel.id,
        deterministic_program_guard_job_id(world.parent)
    );
    world
        .store
        .lock()
        .await
        .insert_scheduled_job(&sentinel)
        .unwrap();
}

/// A program master's ordinary one-shot Resume wake, due `due_ago` seconds ago.
async fn due_resume(world: &World, due_ago: i64) -> ScheduledJob {
    let mut job = agent_job(
        world.parent,
        "resume",
        None,
        Some("fallback".into()),
        Some(60),
    );
    let due = Utc::now() - chrono::Duration::seconds(due_ago);
    job.schedule.anchor = due;
    job.next_fire_at = due;
    world.store.lock().await.insert_scheduled_job(&job).unwrap();
    job
}

async fn job(world: &World, id: Uuid) -> ScheduledJob {
    world
        .store
        .lock()
        .await
        .get_scheduled_job(&id)
        .unwrap()
        .expect("row exists")
}

async fn set(world: &World, key: &str, value: &str) {
    world
        .store
        .lock()
        .await
        .set_daemon_setting(key, value)
        .unwrap();
}

async fn fire(world: &World, launcher: &Arc<ResumeOnlyLauncher>, job: &ScheduledJob) {
    let dynamic: Arc<dyn SessionLauncher> = launcher.clone();
    fire_job(&world.store, &world.bus, &dynamic, job).await;
}

async fn tick(world: &World, launcher: &Arc<ResumeOnlyLauncher>) {
    let dynamic: Arc<dyn SessionLauncher> = launcher.clone();
    process_due_jobs(&world.store, &world.bus, &dynamic, None).await;
}

async fn valve_rows(world: &World) -> Vec<ScheduledJob> {
    let name = keepalive_row_name(world.parent);
    world
        .store
        .lock()
        .await
        .list_scheduled_jobs()
        .unwrap()
        .into_iter()
        .filter(|job| job.name == name)
        .collect()
}

async fn settle_child(world: &World) {
    world
        .store
        .lock()
        .await
        .update_session_status(world.child, SessionStatus::Completed)
        .unwrap();
}

// ---- Part C: hold ---------------------------------------------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_resume_held_by_a_draining_deploy_stays_due_and_runs_after_it_settles() {
    let world = world(30).await;
    let wake = due_resume(&world, 20).await;
    let launcher = ResumeOnlyLauncher::new();
    launcher
        .deploy_hold
        .store(true, std::sync::atomic::Ordering::SeqCst);

    tick(&world, &launcher).await;

    assert!(launcher.resumes().is_empty(), "deploy_draining holds it");
    let held = job(&world, wake.id).await;
    assert!(held.enabled, "deferred, never dropped");
    assert_eq!(held.next_fire_at, wake.next_fire_at);
    assert_eq!(held.last_fired_at, None);

    launcher
        .deploy_hold
        .store(false, std::sync::atomic::Ordering::SeqCst);
    tick(&world, &launcher).await;
    assert_eq!(launcher.resumes(), vec![world.parent], "runs once released");
}

/// The drain can engage after the wake precheck and before the continuation:
/// the continuation's typed refusal must keep the one-shot wake due.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_drain_engaging_after_the_precheck_leaves_the_wake_due_and_spends_no_retry() {
    let world = world(30).await;
    let wake = due_resume(&world, 20).await;
    let launcher = ResumeOnlyLauncher::new();
    // The precheck passes (no hold), then the continuation refuses.
    *launcher.outcome.lock().unwrap() = Some(Err(crate::deploy_drain::draining_error()));
    assert!(retryable_resume_refusal(
        &crate::deploy_drain::draining_error()
    ));

    fire(&world, &launcher, &wake).await;

    let held = job(&world, wake.id).await;
    assert!(held.enabled, "a one-shot wake is not consumed by the race");
    assert_eq!(held.next_fire_at, wake.next_fire_at);
    assert_eq!(held.last_fired_at, None);
    assert!(
        world
            .store
            .lock()
            .await
            .continuation_retry(wake.id)
            .unwrap()
            .is_none(),
        "no bounded-retry attempt is spent on a drain hold"
    );

    // Released: the next tick delivers it.
    tick(&world, &launcher).await;
    assert_eq!(launcher.resumes().len(), 2, "delivered after the hold");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_program_masters_due_resume_is_held_while_a_child_runs_and_stays_exact() {
    let world = world(30).await;
    register_program(&world).await;
    let wake = due_resume(&world, 20).await;
    let launcher = ResumeOnlyLauncher::new();

    fire(&world, &launcher, &wake).await;

    assert!(launcher.resumes().is_empty(), "the wake is held");
    let held = job(&world, wake.id).await;
    assert!(held.enabled, "a held wake stays enabled");
    assert_eq!(held.next_fire_at, wake.next_fire_at, "and untouched");
    assert_eq!(held.last_fired_at, None);
    // The no-idle invariant: the exact declared wake is still present.
    let sentinel = deterministic_program_guard_job_id(world.parent);
    assert!(
        crate::session::agent_verbs::exact_master_continuation_guard_present(
            &*world.store.lock().await,
            world.parent,
            sentinel,
            &ProgramContinuationIntentV1::RequireResumeWake { job_id: wake.id },
        )
        .unwrap()
    );
    // The read side reports the hold and its release time.
    let holds = world
        .store
        .lock()
        .await
        .list_scheduled_job_holds(Utc::now())
        .unwrap();
    assert_eq!(holds.len(), 1);
    assert_eq!(holds[0].job_id, wake.id);
    assert_eq!(holds[0].running_children, vec![world.child]);
    assert!(holds[0].release_at > Utc::now());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_held_wake_delivers_once_the_last_child_settles() {
    let world = world(30).await;
    register_program(&world).await;
    let wake = due_resume(&world, 20).await;
    let launcher = ResumeOnlyLauncher::new();
    fire(&world, &launcher, &wake).await;
    assert!(launcher.resumes().is_empty());

    settle_child(&world).await;
    fire(&world, &launcher, &wake).await;

    assert_eq!(launcher.resumes(), vec![world.parent]);
    assert!(!job(&world, wake.id).await.enabled, "one-shot consumed");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_held_wake_is_released_once_when_the_window_elapses() {
    let world = world(1000).await;
    register_program(&world).await;
    set(&world, "child_keepalive_window_secs", "300").await;
    let launcher = ResumeOnlyLauncher::new();

    // Due 200 s ago, parent idle 1000 s: inside the 300 s window, held.
    let within = due_resume(&world, 200).await;
    fire(&world, &launcher, &within).await;
    assert!(launcher.resumes().is_empty());

    // Due 400 s ago: the window is over, so the wake is delivered exactly once
    // although the child still runs, and the one-shot is consumed.
    let mut past = within.clone();
    past.next_fire_at = Utc::now() - chrono::Duration::seconds(400);
    world
        .store
        .lock()
        .await
        .update_scheduled_job(
            &past.id,
            &crate::store::scheduled_jobs::ScheduledJobUpdate {
                name: None,
                message: None,
                schedule: None,
                enabled: None,
                next_fire_at: Some(past.next_fire_at),
            },
        )
        .unwrap();
    let past = job(&world, past.id).await;
    fire(&world, &launcher, &past).await;
    assert_eq!(launcher.resumes(), vec![world.parent]);
    assert!(
        !job(&world, past.id).await.enabled,
        "the one-shot is consumed"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn manual_trigger_non_program_wakes_and_the_setting_bypass_the_hold() {
    // Manual trigger-now delivers while held.
    let held = world(30).await;
    register_program(&held).await;
    let wake = due_resume(&held, 20).await;
    let launcher = ResumeOnlyLauncher::new();
    let dynamic: Arc<dyn SessionLauncher> = launcher.clone();
    fire_job_manual_for_test(&held.store, &held.bus, &dynamic, &wake).await;
    assert_eq!(launcher.resumes(), vec![held.parent]);

    // Without a program-guard sentinel the wake is an ordinary Resume.
    let ordinary = world(30).await;
    let wake = due_resume(&ordinary, 20).await;
    let launcher = ResumeOnlyLauncher::new();
    fire(&ordinary, &launcher, &wake).await;
    assert_eq!(launcher.resumes(), vec![ordinary.parent]);

    // The operator switch turns the hold off.
    let switched = world(30).await;
    register_program(&switched).await;
    set(&switched, "program_hold_while_children_run", "false").await;
    let wake = due_resume(&switched, 20).await;
    let launcher = ResumeOnlyLauncher::new();
    fire(&switched, &launcher, &wake).await;
    assert_eq!(launcher.resumes(), vec![switched.parent]);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn only_a_completed_masters_wake_is_held() {
    let world = world(30).await;
    register_program(&world).await;
    let wake = due_resume(&world, 20).await;
    world
        .store
        .lock()
        .await
        .update_session_status(world.parent, SessionStatus::Failed)
        .unwrap();
    let launcher = ResumeOnlyLauncher::new();
    fire(&world, &launcher, &wake).await;
    assert_eq!(
        launcher.resumes(),
        vec![world.parent],
        "a Failed master belongs to the retry owners, not the hold"
    );
}

// ---- Part B: valve --------------------------------------------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn the_valve_is_off_by_default_and_never_inserts_a_row() {
    let world = world(100_000).await;
    let launcher = ResumeOnlyLauncher::new();
    tick(&world, &launcher).await;
    assert!(valve_rows(&world).await.is_empty());
    assert!(launcher.resumes().is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn one_resume_per_window_and_none_inside_it() {
    let world = world(10).await;
    set(&world, "child_keepalive_enabled", "true").await;
    set(&world, "child_keepalive_window_secs", "300").await;
    let launcher = ResumeOnlyLauncher::new();

    // Inside the window: no row, no delivery, however many ticks.
    for _ in 0..3 {
        tick(&world, &launcher).await;
    }
    assert!(valve_rows(&world).await.is_empty());
    assert!(launcher.resumes().is_empty());

    // The parent has been idle past the window (child still running).
    world
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE sessions SET updated_at=?2 WHERE id=?1",
            rusqlite::params![
                world.parent.to_string(),
                (Utc::now() - chrono::Duration::seconds(400))
                    .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            ],
        )
        .unwrap();
    tick(&world, &launcher).await;
    assert_eq!(valve_rows(&world).await.len(), 1);
    assert_eq!(launcher.resumes(), vec![world.parent], "exactly one resume");
    let row = valve_rows(&world).await.remove(0);
    assert!(is_keepalive_row(&row));
    assert!(!row.enabled, "the one-shot is consumed on delivery");

    // Further ticks in the same window add nothing: the new window starts at
    // the valve row, and the parent output has not advanced.
    for _ in 0..3 {
        tick(&world, &launcher).await;
    }
    assert_eq!(valve_rows(&world).await.len(), 1);
    assert_eq!(launcher.resumes().len(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_valve_row_is_retired_undelivered_once_every_child_settled() {
    let world = world(1000).await;
    set(&world, "child_keepalive_enabled", "true").await;
    set(&world, "child_keepalive_window_secs", "300").await;
    let launcher = ResumeOnlyLauncher::new();
    // The reconciler inserts the row while the child still runs ...
    let policy = world.store.lock().await.child_autonomy_policy();
    let inserted = world
        .store
        .lock()
        .await
        .reconcile_child_keepalives(Utc::now(), &policy)
        .unwrap();
    assert_eq!(inserted.len(), 1);
    // ... and the same reconcile pass is a no-op the second time (restart safe).
    assert!(
        world
            .store
            .lock()
            .await
            .reconcile_child_keepalives(Utc::now(), &policy)
            .unwrap()
            .is_empty()
    );
    // The child settles before delivery.
    settle_child(&world).await;
    let row = job(&world, inserted[0]).await;
    fire(&world, &launcher, &row).await;
    assert!(launcher.resumes().is_empty(), "nothing is delivered");
    assert!(!job(&world, row.id).await.enabled, "the row is retired");
    // And no new row appears while no child runs.
    tick(&world, &launcher).await;
    assert_eq!(valve_rows(&world).await.len(), 1);
    assert!(launcher.resumes().is_empty());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn ineligible_parents_get_no_valve_row() {
    for case in [
        "failed",
        "hand_armed_resume",
        "pending_question",
        "operator_pause",
    ] {
        let world = world(1000).await;
        set(&world, "child_keepalive_enabled", "true").await;
        set(&world, "child_keepalive_window_secs", "300").await;
        match case {
            "failed" => world
                .store
                .lock()
                .await
                .update_session_status(world.parent, SessionStatus::Failed)
                .unwrap(),
            "hand_armed_resume" => {
                due_resume(&world, -3600).await;
            }
            "pending_question" => world
                .store
                .lock()
                .await
                .conn
                .execute(
                    "UPDATE sessions SET pending_question_json='{}' WHERE id=?1",
                    [world.parent.to_string()],
                )
                .map(drop)
                .unwrap(),
            _ => world
                .store
                .lock()
                .await
                .set_daemon_setting(&format!("manager_operator_pause:{}", world.parent), "hard")
                .unwrap(),
        }
        let policy = world.store.lock().await.child_autonomy_policy();
        let inserted = world
            .store
            .lock()
            .await
            .reconcile_child_keepalives(Utc::now(), &policy)
            .unwrap();
        assert!(inserted.is_empty(), "{case} must not get a valve row");
    }
}

// ---- AC4: no second writer ------------------------------------------------

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_busy_parent_retries_the_same_row_and_never_launches_a_writer() {
    let world = world(1000).await;
    set(&world, "child_keepalive_enabled", "true").await;
    set(&world, "child_keepalive_window_secs", "300").await;
    let launcher = ResumeOnlyLauncher::new();
    *launcher.outcome.lock().unwrap() = Some(Err(DaemonError::InvalidParam(format!(
        "{}:{}",
        crate::store::manager_actions::fence::CONTINUATION_TARGET_BUSY,
        world.parent
    ))));

    tick(&world, &launcher).await;

    // The refused resume retained the row with backoff; the launcher's
    // `launch*` methods (which panic) were never reached.
    let rows = valve_rows(&world).await;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].enabled, "a busy refusal keeps the wake armed");
    assert_eq!(launcher.resumes(), vec![world.parent]);
    // A retry tick inside the backoff neither delivers nor adds a second row.
    tick(&world, &launcher).await;
    assert_eq!(valve_rows(&world).await.len(), 1);
    assert_eq!(launcher.resumes().len(), 1);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[test]
fn the_autonomy_modules_never_reference_a_fresh_or_provider_launch() {
    // Needles are assembled so this test's own source cannot satisfy them.
    let forbidden = [
        ["WakeMode::", "Fresh"].concat(),
        ["Agent", "Fresh"].concat(),
        ["launch_scheduled", "_fresh"].concat(),
        ["spawn_provider", "_process"].concat(),
        ["launch_", "session"].concat(),
    ];
    for (name, source) in [
        ("scheduler/child_gate.rs", include_str!("child_gate.rs")),
        (
            "store/child_autonomy.rs",
            include_str!("../store/child_autonomy.rs"),
        ),
    ] {
        for needle in &forbidden {
            assert!(
                !source.contains(needle.as_str()),
                "{name} must not reference `{needle}`"
            );
        }
    }
}
