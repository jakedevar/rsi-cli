//! Owner-side terminal-watch settlement and delivery-epoch coverage (#616).

use super::*;
use crate::issue_tracker::poller::{SessionLauncher, WatchFireOutcome};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

fn verification_job(owner: Uuid) -> crate::store::agent_jobs::NewAgentJob {
    use rsi_common::agent_jobs::{BuildJobParams, JobParams, JobWake};
    crate::store::agent_jobs::NewAgentJob {
        id: Uuid::new_v4(),
        owner_session_id: owner,
        project_id: None,
        name: None,
        params: JobParams::Build(
            serde_json::from_value::<BuildJobParams>(
                serde_json::json!({"command":"check","package":"rsid"}),
            )
            .unwrap(),
        ),
        cwd: "/tmp".into(),
        unit_name: "rsi-1588-test".into(),
        log_path: "/tmp/log".into(),
        status_path: "/tmp/status".into(),
        idempotency_key: None,
        wake: JobWake::None,
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn terminal_watch_holds_own_wakes_and_excludes_pending_siblings_until_final_end() {
    use rsi_common::types::{Recurrence, WakeMode};
    use rsi_common::wake_predicate::{WakePredicate, WakeWhenState};

    for predicate_wake in [false, true] {
        let (manager, _dir) = manager();
        let manager = Arc::new(manager);
        let owner = Uuid::new_v4();
        let child = Uuid::new_v4();
        let sibling = Uuid::new_v4();
        for id in [owner, child, sibling] {
            insert_row(&manager, &bare_session(id)).await;
        }
        let mut watch = mk_watch_job_for(child, owner, "worker finished");
        let mut other = mk_watch_job_for(sibling, owner, "other finished");
        watch.next_fire_at = chrono::Utc::now() + chrono::Duration::hours(1);
        other.next_fire_at = watch.next_fire_at;
        let mut own = mk_watch_job_for(child, child, "verification complete");
        own.wake_mode = WakeMode::Resume;
        own.schedule.recurrence = Recurrence::Once;
        own.next_fire_at = watch.next_fire_at;
        let job = verification_job(child);
        {
            let store = manager.store.lock().await;
            store.insert_scheduled_job(&watch).unwrap();
            store.insert_scheduled_job(&other).unwrap();
            if predicate_wake {
                store.insert_agent_job(&job, chrono::Utc::now()).unwrap();
                store
                    .insert_wake_when(
                        &own,
                        &WakeWhenState {
                            predicate: WakePredicate {
                                sha_on_rolling: None,
                                jobs_terminal: Some(vec![job.id]),
                            },
                            armed_at: chrono::Utc::now(),
                            deadline: None,
                            repo_dir: None,
                        },
                        false,
                    )
                    .unwrap();
            } else {
                store.insert_scheduled_job(&own).unwrap();
            }
        }
        assert_eq!(
            manager.plan_terminal_watch_fire(&watch).await.unwrap(),
            WatchFirePlan::NotReady
        );
        let WatchFirePlan::Deliver { job_ids, .. } =
            manager.plan_terminal_watch_fire(&other).await.unwrap()
        else {
            panic!("ready sibling should deliver");
        };
        assert_eq!(
            job_ids,
            vec![other.id],
            "pending worker cannot join another completion"
        );

        let launcher = Arc::new(PlannedOwnerLauncher {
            manager: Arc::clone(&manager),
            owner,
            attempts: AtomicUsize::new(0),
            continuations: AtomicUsize::new(0),
            attempted: Notify::new(),
        });
        let scheduler = crate::scheduler::spawn_scheduler(
            Arc::clone(&manager.store),
            Arc::clone(&manager.event_bus),
            Arc::clone(&launcher) as Arc<dyn SessionLauncher>,
            3600,
        );
        scheduler.trigger_now(watch.id).await.unwrap();
        wait_for_count(&launcher.attempts, &launcher.attempted, 1).await;
        let held = wait_for_watch(&manager, watch.id, |row| {
            row.next_fire_at != watch.next_fire_at
        })
        .await;
        assert!(held.enabled);
        assert!(held.last_fired_at.is_none());
        assert_eq!(launcher.continuations.load(Ordering::SeqCst), 0);

        if predicate_wake {
            use rsi_common::agent_jobs::{AgentJobResultV1, JobState};
            manager
                .store
                .lock()
                .await
                .settle_agent_job(
                    job.id,
                    JobState::Succeeded,
                    &AgentJobResultV1::default(),
                    false,
                    chrono::Utc::now(),
                )
                .unwrap();
            assert_eq!(
                manager.plan_terminal_watch_fire(&held).await.unwrap(),
                WatchFirePlan::NotReady,
                "settled job still waits on the worker's enabled completion wake"
            );
        }

        // Wake cancellation (or scheduler consumption) releases the hold;
        // while the resumed worker runs, the ordinary status predicate holds.
        {
            let store = manager.store.lock().await;
            assert!(!store.toggle_scheduled_job(&own.id).unwrap());
            store
                .update_session_status(child, SessionStatus::Running)
                .unwrap();
        }
        assert_eq!(
            manager.plan_terminal_watch_fire(&held).await.unwrap(),
            WatchFirePlan::NotReady
        );
        manager
            .store
            .lock()
            .await
            .update_session_status(child, SessionStatus::Completed)
            .unwrap();
        scheduler.trigger_now(watch.id).await.unwrap();
        wait_for_count(&launcher.continuations, &launcher.attempted, 1).await;
        let delivered = wait_for_watch(&manager, watch.id, |row| row.last_fired_at.is_some()).await;
        push_event(
            &manager,
            owner,
            1,
            EventType::Message,
            Some(Role::Assistant),
            delivered.last_fired_at.unwrap() + chrono::Duration::nanoseconds(1),
        )
        .await;
        scheduler.trigger_now(watch.id).await.unwrap();
        wait_for_watch(&manager, watch.id, |row| !row.enabled).await;
        assert_eq!(launcher.continuations.load(Ordering::SeqCst), 1);
        scheduler.shutdown().await.unwrap();
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test]
async fn terminal_watch_holds_running_job_until_settlement() {
    use rsi_common::agent_jobs::{AgentJobResultV1, JobState};

    let (manager, _dir) = manager();
    let owner = Uuid::new_v4();
    let child = Uuid::new_v4();
    insert_row(&manager, &bare_session(owner)).await;
    insert_row(&manager, &bare_session(child)).await;
    let watch = mk_watch_job_for(child, owner, "worker finished");
    let job = verification_job(child);
    {
        let store = manager.store.lock().await;
        store.insert_scheduled_job(&watch).unwrap();
        store.insert_agent_job(&job, chrono::Utc::now()).unwrap();
    }
    assert_eq!(
        manager.plan_terminal_watch_fire(&watch).await.unwrap(),
        WatchFirePlan::NotReady
    );
    manager
        .store
        .lock()
        .await
        .settle_agent_job(
            job.id,
            JobState::Failed,
            &AgentJobResultV1::default(),
            false,
            chrono::Utc::now(),
        )
        .unwrap();
    assert!(matches!(
        manager.plan_terminal_watch_fire(&watch).await.unwrap(),
        WatchFirePlan::Deliver { .. }
    ));
}

struct PlannedOwnerLauncher {
    manager: Arc<SessionManager>,
    owner: Uuid,
    attempts: AtomicUsize,
    continuations: AtomicUsize,
    attempted: Notify,
}

#[async_trait::async_trait]
impl SessionLauncher for PlannedOwnerLauncher {
    async fn launch(&self, _config: crate::claude::LaunchConfig) -> crate::error::Result<Uuid> {
        Err(crate::error::DaemonError::Rpc(
            "owner-watch fixture launches no external provider".into(),
        ))
    }

    async fn fire_watch(
        &self,
        job: &rsi_common::types::ScheduledJob,
    ) -> crate::error::Result<WatchFireOutcome> {
        let plan = self.manager.plan_terminal_watch_fire(job).await?;
        self.attempts.fetch_add(1, Ordering::SeqCst);
        self.attempted.notify_one();
        match plan {
            WatchFirePlan::NotReady => Ok(WatchFireOutcome::NotReady),
            WatchFirePlan::Abandon(reason) => Ok(WatchFireOutcome::Abandon { reason }),
            WatchFirePlan::AbandonUnconsumed(delivery) => {
                Ok(WatchFireOutcome::AbandonUnconsumed(delivery))
            }
            WatchFirePlan::Confirmed => Ok(WatchFireOutcome::Confirmed),
            WatchFirePlan::Deliver {
                tip,
                job_ids,
                observed_job_versions,
                ..
            } => {
                assert_eq!(tip, self.owner, "the real planner targets the owner");
                let owner_status = self
                    .manager
                    .store
                    .lock()
                    .await
                    .get_session(tip)?
                    .expect("persisted owner")
                    .status;
                if owner_status != SessionStatus::Completed {
                    return Ok(WatchFireOutcome::NotReady);
                }
                self.continuations.fetch_add(1, Ordering::SeqCst);
                self.attempted.notify_one();
                Ok(WatchFireOutcome::Delivered {
                    session_id: tip,
                    delivered_job_ids: job_ids,
                    observed_job_versions,
                })
            }
        }
    }
}

async fn wait_for_count(counter: &AtomicUsize, notify: &Notify, expected: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while counter.load(Ordering::SeqCst) < expected {
            notify.notified().await;
        }
    })
    .await
    .expect("scheduler reached the expected owner-watch boundary");
}

async fn wait_for_watch(
    manager: &SessionManager,
    job_id: Uuid,
    predicate: impl Fn(&rsi_common::types::ScheduledJob) -> bool,
) -> rsi_common::types::ScheduledJob {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let job = manager
                .store
                .lock()
                .await
                .get_scheduled_job(&job_id)
                .expect("read persisted watch")
                .expect("persisted watch");
            if predicate(&job) {
                break job;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("watch state persisted")
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_settling_watch_delivers_once_per_child_epoch_and_preserves_rearm() {
    let (manager, _dir) = manager();
    let manager = Arc::new(manager);
    let owner = Uuid::new_v4();
    let child = Uuid::new_v4();
    let mut owner_row = bare_session(owner);
    owner_row.status = SessionStatus::Running;
    insert_row(&manager, &owner_row).await;
    insert_row(&manager, &bare_session(child)).await;
    manager
        .active
        .write()
        .await
        .insert(owner, TrackedSession::new_for_test(owner_row));

    let mut job = mk_watch_job_for(child, owner, "owner settling");
    job.next_fire_at = chrono::Utc::now() + chrono::Duration::hours(1);
    manager
        .store
        .lock()
        .await
        .insert_scheduled_job(&job)
        .expect("insert child watch");
    let launcher = Arc::new(PlannedOwnerLauncher {
        manager: Arc::clone(&manager),
        owner,
        attempts: AtomicUsize::new(0),
        continuations: AtomicUsize::new(0),
        attempted: Notify::new(),
    });
    let scheduler = crate::scheduler::spawn_scheduler(
        Arc::clone(&manager.store),
        Arc::clone(&manager.event_bus),
        Arc::clone(&launcher) as Arc<dyn SessionLauncher>,
        3600,
    );

    let (removed, release_finalizer) =
        super::super::lifecycle::pause_finalizer_after_active_removal(owner);
    let finalizer = tokio::spawn(SessionManager::finalize_session(
        owner,
        0,
        TerminalFinalizeDecision::completed(),
        Arc::clone(&manager.active),
        Arc::clone(&manager.completed),
        Arc::clone(&manager.event_bus),
        Arc::clone(&manager.store),
        manager.persistence.clone(),
        None,
        Arc::clone(&manager.runtime_config),
        None,
    ));
    tokio::time::timeout(std::time::Duration::from_secs(2), removed)
        .await
        .expect("owner finalizer entered settlement gap")
        .expect("owner finalizer paused");
    assert_eq!(
        manager
            .store
            .lock()
            .await
            .get_session(owner)
            .expect("read owner")
            .expect("owner")
            .status,
        SessionStatus::Running,
    );
    assert!(matches!(
        manager.plan_terminal_watch_fire(&job).await.expect("real watch plan"),
        WatchFirePlan::Deliver { tip, .. } if tip == owner
    ));
    scheduler
        .trigger_now(job.id)
        .await
        .expect("first wake attempt");
    wait_for_count(&launcher.attempts, &launcher.attempted, 1).await;
    assert_eq!(launcher.continuations.load(Ordering::SeqCst), 0);
    assert!(
        manager
            .store
            .lock()
            .await
            .get_scheduled_job(&job.id)
            .expect("read watch")
            .expect("watch")
            .last_fired_at
            .is_none(),
        "settling owner leaves the watch armed without claiming delivery"
    );

    release_finalizer.send(()).expect("release owner finalizer");
    finalizer
        .await
        .expect("finalizer task")
        .expect("owner settled");
    scheduler
        .trigger_now(job.id)
        .await
        .expect("deliver to owner");
    wait_for_count(&launcher.continuations, &launcher.attempted, 1).await;
    let delivered = wait_for_watch(&manager, job.id, |watch| watch.last_fired_at.is_some()).await;
    assert!(delivered.enabled);
    assert_eq!(launcher.continuations.load(Ordering::SeqCst), 1);

    let first_fire = delivered.last_fired_at.expect("first delivery stamp");
    push_event(
        &manager,
        owner,
        1,
        EventType::Message,
        Some(Role::Assistant),
        first_fire + chrono::Duration::nanoseconds(1),
    )
    .await;
    assert_eq!(
        manager
            .plan_terminal_watch_fire(&delivered)
            .await
            .expect("first delivery consumption plan"),
        WatchFirePlan::Confirmed
    );

    // A continued child starts another terminal epoch on the same natural
    // key. The previous owner's output and the previous retirement snapshot
    // cannot consume that new epoch.
    assert!(
        manager
            .agent_control()
            .rearm_child_watch_after_continue(owner, child)
            .await
    );
    let rearmed = manager
        .store
        .lock()
        .await
        .get_scheduled_job(&job.id)
        .expect("read rearmed watch")
        .expect("rearmed watch");
    assert!(rearmed.enabled);
    assert!(rearmed.last_fired_at.is_none());
    assert!(
        !manager
            .store
            .lock()
            .await
            .retire_unchanged_child_watch(&delivered, "consumed")
            .expect("fence old consumption"),
        "old owner turn cannot retire the rearmed child watch"
    );
    assert!(matches!(
        manager
            .plan_terminal_watch_fire(&rearmed)
            .await
            .expect("new epoch plan"),
        WatchFirePlan::Deliver { tip, .. } if tip == owner
    ));

    scheduler
        .trigger_now(job.id)
        .await
        .expect("deliver new epoch");
    wait_for_count(&launcher.continuations, &launcher.attempted, 2).await;
    let second = wait_for_watch(&manager, job.id, |watch| {
        watch.last_fired_at.is_some_and(|at| at > first_fire)
    })
    .await;
    assert!(matches!(
        manager
            .plan_terminal_watch_fire(&second)
            .await
            .expect("second delivery plan"),
        WatchFirePlan::Deliver { .. }
    ));
    push_event(
        &manager,
        owner,
        2,
        EventType::Message,
        Some(Role::Assistant),
        second.last_fired_at.expect("second stamp") + chrono::Duration::nanoseconds(1),
    )
    .await;
    scheduler
        .trigger_now(job.id)
        .await
        .expect("consume second epoch");
    let retired = wait_for_watch(&manager, job.id, |watch| !watch.enabled).await;
    assert!(retired.last_fired_at.is_some());
    assert_eq!(launcher.continuations.load(Ordering::SeqCst), 2);
    scheduler.shutdown().await.expect("stop scheduler");
}

/// Arm the owner's terminal watch on `child` the way each transport does:
/// an explicitly named `schedule_wake` (replacing its name) or the daemon's
/// automatic `agent-child-<id>` watch.
async fn arm_owner_watch(
    manager: &SessionManager,
    owner: Uuid,
    child: Uuid,
    explicit_name: bool,
) -> crate::session::agent_verbs::ArmWatchOutcome {
    use crate::session::harness::tools::schedule_wake::{
        ScheduleWakeRequest, build_agent_scheduled_job,
    };
    let control = manager.agent_control();
    if explicit_name {
        let candidate = build_agent_scheduled_job(ScheduleWakeRequest {
            message: "continued child finished".into(),
            in_seconds: None,
            at: None,
            name: Some("rsi-1391-rearm".into()),
            every_seconds: None,
            mode: Some("on_terminal".into()),
            working_dir: std::path::PathBuf::from("/tmp"),
            provider: None,
            model: None,
            project_id: None,
            origin_session_id: Some(owner),
            watch_session_id: Some(child),
        })
        .expect("explicit on_terminal candidate");
        control
            .arm_terminal_watch_replacing_name(owner, candidate)
            .await
            .expect("explicit re-arm")
    } else {
        control
            .arm_automatic_child_watch(owner, child)
            .await
            .expect("automatic re-arm")
    }
}

/// #1391: a continuation that leaves the owner's delivered watch in place
/// (a manager-controlled AgentContinueChild does not reset it) must not let
/// the owner's re-arm deduplicate onto that consumed epoch. The re-arm opens
/// a new epoch, an identical re-arm while it is live stays idempotent, and
/// the child's next terminal state delivers exactly one more wake.
async fn rearm_after_continue_delivers_next_terminal(explicit_name: bool) {
    use crate::session::agent_verbs::ArmWatchOutcome;
    let (manager, _dir) = manager();
    let manager = Arc::new(manager);
    let owner = Uuid::new_v4();
    let child = Uuid::new_v4();
    insert_row(&manager, &bare_session(owner)).await;
    insert_row(&manager, &bare_session(child)).await;

    let job = match arm_owner_watch(&manager, owner, child, explicit_name).await {
        ArmWatchOutcome::Armed(job) => job,
        other => panic!("first arm inserts a watch, got {other:?}"),
    };
    let launcher = Arc::new(PlannedOwnerLauncher {
        manager: Arc::clone(&manager),
        owner,
        attempts: AtomicUsize::new(0),
        continuations: AtomicUsize::new(0),
        attempted: Notify::new(),
    });
    let scheduler = crate::scheduler::spawn_scheduler(
        Arc::clone(&manager.store),
        Arc::clone(&manager.event_bus),
        Arc::clone(&launcher) as Arc<dyn SessionLauncher>,
        3600,
    );

    scheduler
        .trigger_now(job.id)
        .await
        .expect("deliver first terminal");
    wait_for_count(&launcher.continuations, &launcher.attempted, 1).await;
    let delivered = wait_for_watch(&manager, job.id, |watch| watch.last_fired_at.is_some()).await;
    let first_fire = delivered.last_fired_at.expect("first delivery stamp");

    // Same epoch: the child still sits in the delivered terminal state, so an
    // identical re-arm stays idempotent and keeps the delivery witness.
    match arm_owner_watch(&manager, owner, child, explicit_name).await {
        ArmWatchOutcome::Deduplicated(existing) => assert_eq!(existing.id, job.id),
        other => panic!("same-epoch re-arm deduplicates, got {other:?}"),
    }
    let same_epoch = manager
        .store
        .lock()
        .await
        .get_scheduled_job(&job.id)
        .expect("read watch")
        .expect("watch");
    assert_eq!(same_epoch.last_fired_at, Some(first_fire));
    assert_eq!(same_epoch.updated_at, delivered.updated_at);

    // The owner consumed the wake, then continued the child: it runs again
    // and produces output after the delivery.
    push_event(
        &manager,
        owner,
        1,
        EventType::Message,
        Some(Role::Assistant),
        first_fire + chrono::Duration::nanoseconds(1),
    )
    .await;
    manager
        .store
        .lock()
        .await
        .update_session_status(child, SessionStatus::Running)
        .expect("child continued");
    push_event(
        &manager,
        child,
        1,
        EventType::Message,
        Some(Role::Assistant),
        first_fire + chrono::Duration::nanoseconds(2),
    )
    .await;

    let rearmed = match arm_owner_watch(&manager, owner, child, explicit_name).await {
        ArmWatchOutcome::Rearmed(job) => job,
        other => panic!("re-arm after continuation opens a new epoch, got {other:?}"),
    };
    assert_eq!(rearmed.id, job.id, "the natural-key watch stays single");
    assert!(rearmed.enabled);
    assert_eq!(rearmed.last_fired_at, None);

    // An identical re-arm while the new epoch is live stays idempotent.
    match arm_owner_watch(&manager, owner, child, explicit_name).await {
        ArmWatchOutcome::Deduplicated(existing) => {
            assert_eq!(existing.id, job.id);
            assert_eq!(existing.updated_at, rearmed.updated_at);
        }
        other => panic!("live re-arm deduplicates, got {other:?}"),
    }
    assert!(
        !manager
            .store
            .lock()
            .await
            .retire_unchanged_child_watch(&delivered, "consumed")
            .expect("fence stale consumption"),
        "the consumed epoch's confirmation cannot retire the re-armed watch"
    );

    // While the child runs the watch stays armed and owes nothing.
    scheduler
        .trigger_now(job.id)
        .await
        .expect("running child attempt");
    wait_for_count(&launcher.attempts, &launcher.attempted, 2).await;
    let running = wait_for_watch(&manager, job.id, |watch| watch.enabled).await;
    assert_eq!(running.last_fired_at, None);
    assert_eq!(launcher.continuations.load(Ordering::SeqCst), 1);

    // The child's next terminal state delivers the second wake.
    manager
        .store
        .lock()
        .await
        .update_session_status(child, SessionStatus::Completed)
        .expect("child terminal again");
    scheduler
        .trigger_now(job.id)
        .await
        .expect("deliver second terminal");
    wait_for_count(&launcher.continuations, &launcher.attempted, 2).await;
    let second = wait_for_watch(&manager, job.id, |watch| {
        watch.last_fired_at.is_some_and(|at| at > first_fire)
    })
    .await;
    assert!(second.enabled, "awaiting the owner's confirmation");
    assert_eq!(launcher.continuations.load(Ordering::SeqCst), 2);
    scheduler.shutdown().await.expect("stop scheduler");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_name_rearm_after_continue_delivers_next_terminal() {
    rearm_after_continue_delivers_next_terminal(true).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-02"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_child_watch_rearm_after_continue_delivers_next_terminal() {
    rearm_after_continue_delivers_next_terminal(false).await;
}
