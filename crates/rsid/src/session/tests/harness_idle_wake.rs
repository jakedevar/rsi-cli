//! Lifecycle-level coverage of the Harness idle completion wake (#785): a
//! background command outlives the turn that launched it, its completion arms
//! exactly one internal resume job, the next turn claims the notice, and
//! reading a result withdraws a pending job. Everything runs through the real
//! finalizer, process-registry manager, exec tools and scheduled-job store.

use super::*;
use crate::scheduler::{SchedulerCommand, SchedulerHandle};
use crate::session::harness::tools::exec::test_support as exec;
use std::{sync::Arc, time::Duration};

fn wake_name(session_id: Uuid) -> String {
    format!("background-process-{session_id}")
}

async fn enabled_process_wakes(manager: &SessionManager, session_id: Uuid) -> usize {
    manager
        .store
        .lock()
        .await
        .list_owned_scheduled_jobs(session_id, false, 64)
        .expect("owned jobs")
        .into_iter()
        .filter(|(job, _)| job.name == wake_name(session_id) && job.enabled)
        .count()
}

async fn wait_for_enabled_wakes(manager: &SessionManager, session_id: Uuid, want: usize) -> bool {
    for _ in 0..150 {
        if enabled_process_wakes(manager, session_id).await == want {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    false
}

async fn finalize(manager: &SessionManager, session_id: Uuid, decision: TerminalFinalizeDecision) {
    let mut session = bare_session(session_id);
    session.status = SessionStatus::Running;
    manager
        .active
        .write()
        .await
        .insert(session_id, TrackedSession::new_for_test(session));
    SessionManager::finalize_session(
        session_id,
        0,
        decision,
        manager.active.clone(),
        manager.completed.clone(),
        manager.event_bus.clone(),
        manager.store.clone(),
        manager.persistence.clone(),
        None,
        manager.runtime_config.clone(),
        Some(Arc::clone(&manager.harness_process_manager)),
    )
    .await
    .expect("finalizer settled the decision");
}

async fn armed_manager(
    session_id: Uuid,
) -> (
    SessionManager,
    TempDir,
    tokio::sync::mpsc::Receiver<SchedulerCommand>,
) {
    let (manager, dir) = manager();
    let mut session = bare_session(session_id);
    session.status = SessionStatus::Running;
    insert_row(&manager, &session).await;
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    manager
        .harness_process_manager
        .install_scheduler_handle(SchedulerHandle::new(tx));
    (manager, dir, rx)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn normal_completion_keeps_processes_and_arms_one_resume_job() {
    let session_id = Uuid::new_v4();
    let (manager, _dir, mut scheduler_rx) = armed_manager(session_id).await;
    let registry = manager.harness_process_manager.resolve(session_id);

    // Turn: launch two background commands, then return.
    exec::begin_turn(&registry).await;
    exec::start_running(&registry, "sleep 1; echo first").await;
    exec::start_running(&registry, "sleep 1; echo second").await;
    exec::end_turn(&registry);
    assert_eq!(enabled_process_wakes(&manager, session_id).await, 0);

    // The real normal-completion finalizer must not destroy the registry.
    finalize(&manager, session_id, TerminalFinalizeDecision::completed()).await;
    let after = manager.harness_process_manager.resolve(session_id);
    assert!(
        Arc::ptr_eq(&registry, &after),
        "registry survives a normal completion"
    );

    // Delayed completions while idle coalesce into exactly one resume job.
    assert!(wait_for_enabled_wakes(&manager, session_id, 1).await);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(enabled_process_wakes(&manager, session_id).await, 1);
    let job = manager
        .store
        .lock()
        .await
        .list_owned_scheduled_jobs(session_id, false, 64)
        .expect("owned jobs")
        .into_iter()
        .map(|(job, _)| job)
        .find(|job| job.name == wake_name(session_id))
        .expect("internal wake");
    assert_eq!(job.wake_mode, rsi_common::types::WakeMode::Resume);
    assert_eq!(job.wake_session_id, Some(session_id));
    assert!(
        matches!(scheduler_rx.try_recv(), Ok(SchedulerCommand::CheckNow)),
        "arming nudges the scheduler"
    );

    // The resumed turn claims both notices once, then the job is withdrawn.
    exec::begin_turn(&registry).await;
    assert_eq!(exec::take_notices(&registry).await.len(), 2);
    assert!(exec::take_notices(&registry).await.is_empty());
    assert!(wait_for_enabled_wakes(&manager, session_id, 0).await);
    exec::end_turn(&registry);

    manager
        .harness_process_manager
        .shutdown_session(session_id)
        .await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn reading_a_result_withdraws_the_pending_wake_job() {
    let session_id = Uuid::new_v4();
    let (manager, _dir, _scheduler_rx) = armed_manager(session_id).await;
    let registry = manager.harness_process_manager.resolve(session_id);

    exec::begin_turn(&registry).await;
    let process = exec::start_running(&registry, "sleep 1; echo read-me").await;
    exec::end_turn(&registry);
    finalize(&manager, session_id, TerminalFinalizeDecision::completed()).await;

    assert!(wait_for_enabled_wakes(&manager, session_id, 1).await);
    assert!(exec::read_result(&registry, process).await);
    assert!(
        wait_for_enabled_wakes(&manager, session_id, 0).await,
        "reading the result disables the pending resume job"
    );
    assert!(exec::take_notices(&registry).await.is_empty());

    manager
        .harness_process_manager
        .shutdown_session(session_id)
        .await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn real_termination_still_reaps_background_processes() {
    let session_id = Uuid::new_v4();
    let (manager, _dir, _scheduler_rx) = armed_manager(session_id).await;
    let registry = manager.harness_process_manager.resolve(session_id);

    exec::begin_turn(&registry).await;
    let process = exec::start_running(&registry, "sleep 30").await;
    exec::end_turn(&registry);
    finalize(
        &manager,
        session_id,
        TerminalFinalizeDecision::interrupted(),
    )
    .await;

    assert!(!exec::has_process(&registry, process).await);
    let rebuilt = manager.harness_process_manager.resolve(session_id);
    assert!(
        !Arc::ptr_eq(&registry, &rebuilt),
        "terminal status drops the registry"
    );
    assert_eq!(enabled_process_wakes(&manager, session_id).await, 0);
    manager
        .harness_process_manager
        .shutdown_session(session_id)
        .await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn idle_interrupt_and_archive_never_signal_a_disarmed_exited_handle() {
    use crate::process_control::recorded_group_signals::sent_to;
    let session_id = Uuid::new_v4();
    let (manager, _dir, _scheduler_rx) = armed_manager(session_id).await;
    let registry = manager.harness_process_manager.resolve(session_id);

    exec::begin_turn(&registry).await;
    let (process, pgid) = exec::start_running_with_pgid(&registry, "sleep 0.6").await;
    exec::end_turn(&registry);
    finalize(&manager, session_id, TerminalFinalizeDecision::completed()).await;

    // The leader exits while the session idles: its group is killed once, under
    // the pinning zombie, and the retained unread handle is disarmed.
    assert!(wait_for_enabled_wakes(&manager, session_id, 1).await);
    assert_eq!(sent_to(pgid), 1);
    assert!(exec::has_process(&registry, process).await);

    // Operator interrupt of the idle session, then archive: neither signals.
    let _ = manager.interrupt_session(session_id).await;
    assert_eq!(
        sent_to(pgid),
        1,
        "idle interrupt sent a signal to a disarmed handle"
    );
    manager
        .harness_process_manager
        .shutdown_session(session_id)
        .await;
    let _ = manager.archive_session(session_id).await;
    assert_eq!(
        sent_to(pgid),
        1,
        "archive sent a signal to a disarmed handle"
    );
}
