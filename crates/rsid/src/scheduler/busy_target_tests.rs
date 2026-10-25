//! #189: a self-wake refused `continuation_turn_changed` by a live target is
//! held for the target's next idle point with no retry spent; a gone or
//! archived target keeps the bounded retry and its recovery escalation.

use super::*;

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
fn turn_changed() -> DaemonError {
    DaemonError::InvalidParam(
        crate::store::manager_actions::fence::CONTINUATION_TURN_CHANGED.into(),
    )
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
fn armed_wake(store: &Store, status: SessionStatus) -> (rsi_common::types::ScheduledJob, Uuid) {
    armed_wake_at(store, status, Utc::now() - chrono::Duration::seconds(1))
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
fn armed_wake_at(
    store: &Store,
    status: SessionStatus,
    due: chrono::DateTime<Utc>,
) -> (rsi_common::types::ScheduledJob, Uuid) {
    let mut session = rsid_store::test_support::make_test_session();
    session.id = Uuid::new_v4();
    session.status = status;
    store.insert_session(&session).unwrap();
    let mut job = crate::session::harness::tools::schedule_wake::build_agent_scheduled_job(
        crate::session::harness::tools::schedule_wake::ScheduleWakeRequest {
            message: "sha landed".into(),
            in_seconds: Some(60),
            at: None,
            name: None,
            every_seconds: None,
            mode: Some("resume".into()),
            working_dir: session.working_dir.clone(),
            provider: Some(session.provider),
            model: None,
            project_id: None,
            origin_session_id: Some(session.id),
            watch_session_id: None,
        },
    )
    .unwrap();
    assert_eq!(job.wake_mode, WakeMode::Resume);
    job.next_fire_at = due;
    job.schedule.anchor = due;
    store.insert_scheduled_job(&job).unwrap();
    let job = store.get_scheduled_job(&job.id).unwrap().unwrap();
    (job, session.id)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_live_busy_target_holds_its_wake_unspent_for_the_next_idle_point() {
    let store = Store::open_in_memory().unwrap();
    let (job, target) = armed_wake(&store, SessionStatus::Running);
    let store = Arc::new(rsid_core::store_lock::Mutex::new(store));
    let before = Utc::now();
    let bus = Arc::new(EventBus::new(16));
    assert!(hold_wake_for_busy_target(&store, &bus, &job, target, &turn_changed(), before).await);
    let guard = store.lock().await;
    let row = guard.get_scheduled_job(&job.id).unwrap().unwrap();
    assert!(row.enabled);
    assert_eq!(row.last_fired_at, None);
    assert_eq!(row.updated_at, job.updated_at, "row version untouched");
    assert!(row.next_fire_at >= before + chrono::Duration::seconds(BUSY_TARGET_WAKE_DELAY_SECS));
    assert_eq!(
        guard.continuation_retry(job.id).unwrap(),
        None,
        "no retry spent"
    );
    let seen = guard
        .self_wake_input_seen(job.id)
        .unwrap()
        .expect("input acknowledged");
    assert_eq!(
        seen.through_sequence,
        guard.conversation_max_sequence(target).unwrap()
    );
    assert_eq!(seen.armed_at(), Some(job.created_at));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_gone_or_archived_target_keeps_the_bounded_retry_path() {
    for gone in [false, true] {
        let store = Store::open_in_memory().unwrap();
        let (job, mut target) = armed_wake(&store, SessionStatus::Archived);
        if gone {
            target = Uuid::new_v4();
        }
        let store = Arc::new(rsid_core::store_lock::Mutex::new(store));
        let bus = Arc::new(EventBus::new(16));
        assert!(
            !hold_wake_for_busy_target(&store, &bus, &job, target, &turn_changed(), Utc::now())
                .await
        );
        let guard = store.lock().await;
        assert_eq!(guard.self_wake_input_seen(job.id).unwrap(), None);
        let row = guard.get_scheduled_job(&job.id).unwrap().unwrap();
        assert_eq!(row.next_fire_at, job.next_fire_at, "nothing written");
    }
}

/// The launcher's idle point is injected; no provider process or sleep.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[derive(Default)]
struct BusyLauncher {
    idle: std::sync::atomic::AtomicBool,
    deliveries: std::sync::Mutex<Vec<(Uuid, String)>>,
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[async_trait::async_trait]
impl SessionLauncher for BusyLauncher {
    async fn launch(&self, _: LaunchConfig) -> crate::error::Result<Uuid> {
        panic!("a self-wake only resumes");
    }

    async fn resume_scheduled(&self, target: Uuid, message: String) -> crate::error::Result<Uuid> {
        if !self.idle.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(DaemonError::InvalidParam(
                crate::store::manager_actions::fence::CONTINUATION_TARGET_BUSY.into(),
            ));
        }
        self.deliveries.lock().unwrap().push((target, message));
        Ok(target)
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_never_idle_target_reaches_its_original_deadline_across_restart_and_escalates_once() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("busy-wake.sqlite");
    let first_due = Utc::now() - chrono::Duration::minutes(61);
    let store = Store::open(&path).unwrap();
    let (job, target) = armed_wake_at(&store, SessionStatus::Running, first_due);
    let deadline = first_due + crate::store::scheduled_jobs::BUSY_TARGET_WAKE_MAX_HOLD;
    let store = Arc::new(rsid_core::store_lock::Mutex::new(store));
    let bus = Arc::new(EventBus::new(16));
    // The first refusal is observed later than the original due time. It
    // cannot extend the deadline by that scheduler delay.
    let first_attempt = first_due + chrono::Duration::minutes(5);
    assert!(
        hold_wake_for_busy_target(&store, &bus, &job, target, &turn_changed(), first_attempt).await
    );
    let held = store
        .lock()
        .await
        .get_scheduled_job(&job.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        held.next_fire_at,
        first_attempt + chrono::Duration::seconds(30)
    );
    assert_eq!(
        store.lock().await.busy_wake_deadline(job.id).unwrap(),
        Some(deadline)
    );
    // Further refusals spend no budget and cannot move the original bound.
    // The last deferral is capped at the exact deadline.
    assert!(
        hold_wake_for_busy_target(
            &store,
            &bus,
            &held,
            target,
            &turn_changed(),
            deadline - chrono::Duration::seconds(1)
        )
        .await
    );
    let held = store
        .lock()
        .await
        .get_scheduled_job(&job.id)
        .unwrap()
        .unwrap();
    assert_eq!(held.next_fire_at, deadline);
    drop(store);

    // A new Store and scheduler lifetime sees the persisted deadline. The
    // target is still busy, so the real fire path must settle immediately.
    let store = Arc::new(rsid_core::store_lock::Mutex::new(
        Store::open(&path).unwrap(),
    ));
    let launcher = Arc::new(BusyLauncher::default());
    let dynamic: Arc<dyn SessionLauncher> = launcher;
    fire_job(&store, &bus, &dynamic, &held).await;
    fire_job(&store, &bus, &dynamic, &held).await;
    let guard = store.lock().await;
    let row = guard.get_scheduled_job(&job.id).unwrap().unwrap();
    assert!(!row.enabled);
    assert!(row.last_fired_at.is_some());
    assert_eq!(guard.continuation_retry(job.id).unwrap(), None);
    let view = guard
        .list_owned_wake_views(target, true, 10)
        .unwrap()
        .into_iter()
        .find(|view| view.job_id == job.id)
        .unwrap();
    assert_eq!(view.disabled_reason.as_deref(), Some("wake_recoverable"));
    assert_eq!(view.deadline, Some(deadline));
    let notices = guard
        .operator_todo_list(None)
        .unwrap()
        .open
        .into_iter()
        .filter(|todo| todo.todo_key == format!("gone-wake:{}", job.id))
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0]
            .how
            .contains("wake_recoverable: busy_target_deadline")
    );
    assert!(notices[0].how.contains(&job.message));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-other-03"))]
#[tokio::test]
async fn a_busy_target_that_idles_before_deadline_receives_the_original_wake_once() {
    let store = Store::open_in_memory().unwrap();
    let (job, target) = armed_wake(&store, SessionStatus::Running);
    let deadline = job.next_fire_at + crate::store::scheduled_jobs::BUSY_TARGET_WAKE_MAX_HOLD;
    let store = Arc::new(rsid_core::store_lock::Mutex::new(store));
    let bus = Arc::new(EventBus::new(16));
    let launcher = Arc::new(BusyLauncher::default());
    let dynamic: Arc<dyn SessionLauncher> = launcher.clone();
    // Direct continuation_target_busy (without a prior turn_changed) gets
    // the same unspent durable hold.
    fire_job(&store, &bus, &dynamic, &job).await;
    let held = store
        .lock()
        .await
        .get_scheduled_job(&job.id)
        .unwrap()
        .unwrap();
    assert!(held.enabled);
    assert_eq!(held.last_fired_at, None);
    assert_eq!(
        store.lock().await.busy_wake_deadline(job.id).unwrap(),
        Some(deadline)
    );
    assert_eq!(store.lock().await.continuation_retry(job.id).unwrap(), None);
    launcher
        .idle
        .store(true, std::sync::atomic::Ordering::SeqCst);
    fire_job(&store, &bus, &dynamic, &held).await;
    fire_job(&store, &bus, &dynamic, &held).await;
    let guard = store.lock().await;
    let row = guard.get_scheduled_job(&job.id).unwrap().unwrap();
    assert!(!row.enabled);
    assert!(row.last_fired_at.is_some());
    assert_eq!(guard.busy_wake_deadline(job.id).unwrap(), None);
    assert_eq!(
        launcher.deliveries.lock().unwrap().as_slice(),
        &[(
            target,
            rsi_common::daemon_message::wrap("scheduled-wake", &job.message)
        )]
    );
}
