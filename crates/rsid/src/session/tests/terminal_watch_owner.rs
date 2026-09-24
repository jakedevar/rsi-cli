//! Owner-side terminal-watch settlement and delivery-epoch coverage (#616).

use super::*;
use crate::issue_tracker::poller::{SessionLauncher, WatchFireOutcome};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

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
