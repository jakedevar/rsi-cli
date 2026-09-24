//! A8 terminal-watch acceleration service — the bus→scheduler bridge.
//!
//! Correctness lives entirely in the scheduler's due-poll over persisted
//! watch rows (`WakeMode::OnTerminal` in `scheduled_jobs`): every fire
//! attempt re-reads persisted session state, so nothing is lost when this
//! service misses an event (zero-subscriber drop, a `Lagged` receiver, or
//! the two `SessionStatusChanged`-bypassing terminal flips, which DO update
//! the DB).
//! This service only converts minutes-latency into seconds-latency by
//! nudging the scheduler (`trigger_now` / `check_now`) when a relevant event
//! flies by. It holds no correctness-bearing state: every nudge re-reads job
//! rows, and firing is idempotent (delivered jobs are disabled; the
//! scheduler's `OnTerminal` arm no-ops on disabled rows).
//!
//! Sibling task beside `stall_detector` (F-012): the inverse concern
//! (event-driven vs scan-cadence), no shared state, deliberately NOT an
//! extension of it.

use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::sync::broadcast::error::RecvError;
use uuid::Uuid;

use rsi_common::types::WakeMode;

use crate::bus::{DaemonEvent, EventBus};
use crate::scheduler::SchedulerHandle;
use crate::store::Store;

/// Spawn the terminal-watch acceleration service.
///
/// Wired in `main.rs` only when the scheduler is enabled — without a
/// scheduler there is nothing to nudge (armed watches persist and activate
/// when it is re-enabled).
pub fn spawn_terminal_watch_service(
    bus: Arc<EventBus>,
    store: Arc<Mutex<Store>>,
    scheduler: SchedulerHandle,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let mut rx = bus.subscribe();
            loop {
                match rx.recv().await {
                    Ok(event) => handle_event(&event, &store, &scheduler).await,
                    Err(RecvError::Lagged(missed)) => {
                        // The receiver fast-forwarded past `missed` events.
                        // The due-tick covers whatever was lost; accelerate
                        // it once rather than reasoning about the gap.
                        tracing::warn!(
                            missed,
                            "terminal watch service lagged; re-checking due jobs"
                        );
                        let _ = scheduler.check_now().await;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
            bus.unsubscribe();
            // Channel closed (daemon shutdown or bus teardown). Re-check
            // once so nothing armed is left waiting on a lost event, then
            // try to resubscribe after a beat — if the process is going
            // down this task dies with it anyway.
            tracing::warn!("terminal watch service bus closed; resubscribing");
            let _ = scheduler.check_now().await;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    })
}

/// Map one bus event onto scheduler nudges.
///
/// Watched-subject signals (→ `trigger_now` per matching watch job):
/// - `SessionStatusChanged` / `SessionReconciled` into a terminal status
///   (the reconciled variant covers the boot-restore and `StoreDesync`
///   flips).
/// - `SessionArchived` (archive paths that bypass a status event).
/// - `SessionQuestionRaised` (D5: `WaitingApproval` is notify-worthy).
///
/// Wake-target signal: a terminal transition of a session that IS the wake
/// target of pending watches means the master just went idle → `check_now`
/// delivers queued wakes now instead of next tick. Manager-notice routes
/// whose recipient just went idle are also triggered by exact job id, so a
/// notice held back only by a busy recipient is delivered immediately.
async fn handle_event(event: &DaemonEvent, store: &Arc<Mutex<Store>>, scheduler: &SchedulerHandle) {
    if let DaemonEvent::ManagerNoticeQueued { job_id } = event {
        let _ = scheduler.trigger_now(*job_id).await;
        return;
    }
    let (session_id, watch_relevant, master_idle) = match event {
        DaemonEvent::SessionStatusChanged {
            session_id,
            new_status,
            ..
        }
        | DaemonEvent::SessionReconciled {
            session_id,
            new_status,
            ..
        } => {
            let terminal = new_status.is_terminal();
            (*session_id, terminal, terminal)
        }
        DaemonEvent::SessionArchived { session_id, .. } => (*session_id, true, true),
        DaemonEvent::SessionQuestionRaised { session_id, .. } => (*session_id, true, false),
        _ => return,
    };
    if !watch_relevant {
        return;
    }

    // Low row count; refreshed per event — the service holds no state.
    let (jobs, ancestors) = {
        let guard = store.lock().await;
        if let Err(error) = guard.reconcile_harness_manager_watches() {
            tracing::error!(%error, "manager watch acceleration deferred to durable reconciliation");
        }
        match guard.list_scheduled_jobs() {
            Ok(jobs) => {
                let mut ancestors = Vec::new();
                let mut current = session_id;
                for _ in 0..crate::session::WATCH_LINEAGE_DEPTH_CAP {
                    let Ok(Some(row)) = guard.get_session(current) else {
                        break;
                    };
                    let Some(parent) = row.continued_from else {
                        break;
                    };
                    if ancestors.contains(&parent) {
                        break;
                    }
                    ancestors.push(parent);
                    current = parent;
                }
                (jobs, ancestors)
            }
            Err(e) => {
                tracing::error!("terminal watch service failed to list jobs: {e}");
                return;
            }
        }
    };

    let mut master_of_pending_watch = false;
    let mut triggered: Vec<Uuid> = Vec::new();
    for job in &jobs {
        let WakeMode::OnTerminal(watched) = job.wake_mode else {
            continue;
        };
        if !job.enabled {
            continue;
        }
        let (manager_watch, manager_route) = {
            let guard = store.lock().await;
            match (
                guard.is_harness_manager_watch(job.id),
                guard.harness_manager_watch_route(job.id),
            ) {
                (Ok(managed), Ok(route)) => (managed, route),
                (Err(error), _) | (_, Err(error)) => {
                    tracing::warn!(%error, "manager watch route unavailable");
                    (true, None)
                }
            }
        };
        if let Some((source, target)) = manager_route {
            if source == session_id {
                triggered.push(job.id);
            }
            if target == session_id {
                master_of_pending_watch = true;
                // A durable manager notice may be waiting only on its idle
                // recipient (issue #627). Re-evaluate that exact job now: a
                // NotReady attempt pushed its due instant past this event.
                if master_idle && !triggered.contains(&job.id) {
                    triggered.push(job.id);
                }
            }
            continue;
        }
        if manager_watch {
            continue;
        }
        if watched == session_id || ancestors.contains(&watched) {
            triggered.push(job.id);
        }
        if job.wake_session_id == Some(session_id) {
            master_of_pending_watch = true;
        }
    }

    for job_id in triggered {
        let _ = scheduler.trigger_now(job_id).await;
    }
    if master_idle && master_of_pending_watch {
        let _ = scheduler.check_now().await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::scheduler::SchedulerCommand;
    use chrono::Utc;
    use rsi_common::types::{Recurrence, ScheduleSpec, ScheduledJob, SessionStatus, WakeMode};
    use tokio::sync::mpsc;

    fn mk_watch_job(watched: Uuid, master: Uuid, enabled: bool) -> ScheduledJob {
        let now = Utc::now();
        ScheduledJob {
            id: Uuid::new_v4(),
            name: "rsi-watch".to_string(),
            message: String::new(),
            schedule: ScheduleSpec {
                recurrence: Recurrence::EverySeconds(60),
                anchor: now,
            },
            last_fired_at: None,
            next_fire_at: now,
            enabled,
            working_dir: None,
            provider: None,
            model: None,
            project_id: None,
            created_at: now,
            updated_at: now,
            wake_mode: WakeMode::OnTerminal(watched),
            wake_session_id: Some(master),
        }
    }

    struct Fixture {
        bus: Arc<EventBus>,
        store: Arc<Mutex<Store>>,
        rx: mpsc::Receiver<SchedulerCommand>,
        _task: tokio::task::JoinHandle<()>,
    }

    async fn fixture() -> Fixture {
        let bus = Arc::new(EventBus::new(64));
        let store = Arc::new(Mutex::new(
            Store::open_in_memory().expect("in-memory store"),
        ));
        let (tx, rx) = mpsc::channel::<SchedulerCommand>(64);
        let task = spawn_terminal_watch_service(
            Arc::clone(&bus),
            Arc::clone(&store),
            SchedulerHandle::new(tx),
        );
        // Let the service subscribe before anything is published (the bus
        // drops events with zero subscribers).
        while bus.subscriber_count() == 0 {
            tokio::task::yield_now().await;
        }
        Fixture {
            bus,
            store,
            rx,
            _task: task,
        }
    }

    async fn recv_command(rx: &mut mpsc::Receiver<SchedulerCommand>) -> Option<SchedulerCommand> {
        tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .ok()
            .flatten()
    }

    async fn assert_no_command(rx: &mut mpsc::Receiver<SchedulerCommand>) {
        let got = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await;
        assert!(
            got.is_err(),
            "expected no scheduler command, got {:?}",
            got.map(|c| c.map(|c| match c {
                SchedulerCommand::TriggerNow(id) => format!("TriggerNow({id})"),
                SchedulerCommand::CheckNow => "CheckNow".to_string(),
                SchedulerCommand::Shutdown => "Shutdown".to_string(),
            }))
        );
    }

    /// Watched child hits a terminal status on the bus → `TriggerNow(job)`.
    /// Covers T-4's service half via `SessionQuestionRaised` below.
    #[tokio::test]
    async fn watched_terminal_event_triggers_job() {
        let mut fx = fixture().await;
        let watched = Uuid::new_v4();
        let master = Uuid::new_v4();
        let job = mk_watch_job(watched, master, true);
        fx.store
            .lock()
            .await
            .insert_scheduled_job(&job)
            .expect("insert");

        fx.bus.publish(DaemonEvent::SessionStatusChanged {
            session_id: watched,
            old_status: SessionStatus::Running,
            new_status: SessionStatus::Completed,
        });

        match recv_command(&mut fx.rx).await {
            Some(SchedulerCommand::TriggerNow(id)) => assert_eq!(id, job.id),
            other => panic!("expected TriggerNow, got {:?}", other.is_some()),
        }
    }

    #[tokio::test]
    async fn watch_service_triggers_ancestor_watch_on_successor_terminal() {
        let mut fx = fixture().await;
        let origin = Uuid::new_v4();
        let successor = Uuid::new_v4();
        let job = mk_watch_job(origin, Uuid::new_v4(), true);
        {
            let guard = fx.store.lock().await;
            let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
            for (id, continued_from) in [(origin, None), (successor, Some(origin))] {
                guard.conn.execute(
                    "INSERT INTO sessions (id, query, working_dir, status, created_at, updated_at, continued_from) VALUES (?1, 'watch fixture', '/tmp', 'Completed', ?2, ?2, ?3)",
                    rusqlite::params![id.to_string(), now, continued_from.map(|id| id.to_string())],
                ).expect("insert lineage row");
            }
            guard.insert_scheduled_job(&job).expect("insert watch");
        }
        fx.bus.publish(DaemonEvent::SessionStatusChanged {
            session_id: successor,
            old_status: SessionStatus::Running,
            new_status: SessionStatus::Completed,
        });
        match recv_command(&mut fx.rx).await {
            Some(SchedulerCommand::TriggerNow(id)) => assert_eq!(id, job.id),
            other => panic!("expected ancestor TriggerNow, got {:?}", other.is_some()),
        }
    }

    #[tokio::test]
    async fn queued_manager_notice_triggers_its_exact_job_without_a_scan() {
        let mut fx = fixture().await;
        let job_id = Uuid::new_v4();
        fx.bus.publish(DaemonEvent::ManagerNoticeQueued { job_id });

        match recv_command(&mut fx.rx).await {
            Some(SchedulerCommand::TriggerNow(id)) => assert_eq!(id, job_id),
            other => panic!("expected exact TriggerNow, got {:?}", other.is_some()),
        }
    }

    /// A bypass-flip shape (`SessionReconciled` only, no status event) still
    /// accelerates — the service maps the reconciled variant too (F-003/4).
    #[tokio::test]
    async fn reconciled_terminal_event_triggers_job() {
        let mut fx = fixture().await;
        let watched = Uuid::new_v4();
        let job = mk_watch_job(watched, Uuid::new_v4(), true);
        fx.store
            .lock()
            .await
            .insert_scheduled_job(&job)
            .expect("insert");

        fx.bus.publish(DaemonEvent::SessionReconciled {
            session_id: watched,
            old_status: SessionStatus::Running,
            new_status: SessionStatus::Failed,
            reason: crate::reconciliation::ReconciliationReason::StoreDesync,
        });

        match recv_command(&mut fx.rx).await {
            Some(SchedulerCommand::TriggerNow(id)) => assert_eq!(id, job.id),
            other => panic!("expected TriggerNow, got {:?}", other.is_some()),
        }
    }

    /// T-4 (service half, D5): a raised question on a watched child triggers
    /// immediate evaluation.
    #[tokio::test]
    async fn question_raised_triggers_job() {
        let mut fx = fixture().await;
        let watched = Uuid::new_v4();
        let job = mk_watch_job(watched, Uuid::new_v4(), true);
        fx.store
            .lock()
            .await
            .insert_scheduled_job(&job)
            .expect("insert");

        fx.bus.publish(DaemonEvent::SessionQuestionRaised {
            session_id: watched,
            question: rsi_common::types::PendingQuestion { questions: vec![] },
        });

        match recv_command(&mut fx.rx).await {
            Some(SchedulerCommand::TriggerNow(id)) => assert_eq!(id, job.id),
            other => panic!("expected TriggerNow, got {:?}", other.is_some()),
        }
    }

    /// T-9: terminal events for sessions nobody watches produce no scheduler
    /// traffic; non-terminal transitions of a watched child are ignored;
    /// disabled watch rows never trigger.
    #[tokio::test]
    async fn non_watched_terminal_session_fires_nothing() {
        let mut fx = fixture().await;
        let watched = Uuid::new_v4();
        let job = mk_watch_job(watched, Uuid::new_v4(), true);
        let disabled = mk_watch_job(Uuid::new_v4(), Uuid::new_v4(), false);
        {
            let guard = fx.store.lock().await;
            guard.insert_scheduled_job(&job).expect("insert");
            guard.insert_scheduled_job(&disabled).expect("insert");
        }

        // Unrelated session terminal.
        fx.bus.publish(DaemonEvent::SessionStatusChanged {
            session_id: Uuid::new_v4(),
            old_status: SessionStatus::Running,
            new_status: SessionStatus::Completed,
        });
        // Watched child merely progressing (non-terminal).
        fx.bus.publish(DaemonEvent::SessionStatusChanged {
            session_id: watched,
            old_status: SessionStatus::Starting,
            new_status: SessionStatus::Running,
        });
        // Disabled watch's subject terminal.
        if let WakeMode::OnTerminal(disabled_watched) = disabled.wake_mode {
            fx.bus.publish(DaemonEvent::SessionStatusChanged {
                session_id: disabled_watched,
                old_status: SessionStatus::Running,
                new_status: SessionStatus::Completed,
            });
        }

        assert_no_command(&mut fx.rx).await;
    }

    /// Issue #627: a manager recipient going idle re-evaluates each exact
    /// manager-notice transport routed to it, not only the due list.
    #[tokio::test]
    async fn manager_recipient_going_idle_triggers_its_notice_transports() {
        let mut fx = fixture().await;
        let (manager, job_id) = {
            let guard = fx.store.lock().await;
            let fixture =
                crate::store::manager_watch_settlement::tests::manager_watch_fixture(&guard);
            let job_id = fixture.notice(&guard, 0, true).id;
            drop(guard);
            (fixture.manager, job_id)
        };

        fx.bus.publish(DaemonEvent::SessionStatusChanged {
            session_id: manager,
            old_status: SessionStatus::Running,
            new_status: SessionStatus::Completed,
        });

        let mut triggered = Vec::new();
        loop {
            match recv_command(&mut fx.rx).await {
                Some(SchedulerCommand::TriggerNow(id)) => triggered.push(id),
                Some(SchedulerCommand::CheckNow) => break,
                other => panic!("expected TriggerNow/CheckNow, got {:?}", other.is_some()),
            }
        }
        assert!(
            triggered.contains(&job_id),
            "idle manager must re-evaluate its to_manager transport {job_id}: {triggered:?}"
        );
    }

    /// Master-idle acceleration: the wake TARGET of a pending watch going
    /// terminal yields `CheckNow` (deliver queued wakes now, not next tick).
    #[tokio::test]
    async fn master_going_idle_checks_now() {
        let mut fx = fixture().await;
        let master = Uuid::new_v4();
        let job = mk_watch_job(Uuid::new_v4(), master, true);
        fx.store
            .lock()
            .await
            .insert_scheduled_job(&job)
            .expect("insert");

        fx.bus.publish(DaemonEvent::SessionStatusChanged {
            session_id: master,
            old_status: SessionStatus::Running,
            new_status: SessionStatus::Completed,
        });

        match recv_command(&mut fx.rx).await {
            Some(SchedulerCommand::CheckNow) => {}
            other => panic!("expected CheckNow, got {:?}", other.is_some()),
        }
    }
}
