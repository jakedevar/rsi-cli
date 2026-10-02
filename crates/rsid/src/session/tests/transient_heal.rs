//! Issue #1015 slice E: real finalization, scheduler and continuation paths.
use super::*;
use crate::issue_tracker::poller::SessionLauncher;
use crate::session::launch::{
    drop_controller_candidate_test_process, drop_controller_candidate_test_stream,
    install_controller_candidate_test_process,
};
use crate::store::transient_heal::{TRANSIENT_HEAL_MAX_ATTEMPTS, transient_heal_job_id};
use chrono::{Duration, Utc};
use rsi_common::harness_manager_v2::ManagerPolicyV2;
use rsi_common::types::ScheduledJob;
use std::sync::Arc;
use std::sync::atomic::Ordering;

struct Fixture {
    manager: Arc<SessionManager>,
    _dir: TempDir,
    repo: TempDir,
}

fn fixture() -> Fixture {
    let (manager, dir) = manager();
    let repo = disk_backed_tempdir();
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=Heal Test",
            "-c",
            "user.email=heal@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ],
    ] {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Fixture {
        manager: Arc::new(manager),
        _dir: dir,
        repo,
    }
}

fn session(f: &Fixture) -> Session {
    let mut row = bare_session(Uuid::new_v4());
    row.status = SessionStatus::Running;
    row.provider = SessionProvider::Claude;
    row.working_dir = f.repo.path().to_path_buf();
    row.max_retries = None;
    row.retry_attempt = None;
    row
}

async fn fail(f: &Fixture, mut row: Session, reason: &str) {
    row.status = SessionStatus::Running;
    row.terminal_reason = None;
    row.stop_reason = None;
    {
        let store = f.manager.store.lock().await;
        if store.get_session(row.id).unwrap().is_none() {
            store.insert_session(&row).unwrap();
        }
    }
    let (mut tracked, _) = scripted_process_tracked(row.id, 7, false, 1, false, false, false);
    tracked.session = row.clone();
    f.manager.active.write().await.insert(row.id, tracked);
    let (stop_tx, stop_rx) = mpsc::channel(1);
    f.manager
        .active
        .write()
        .await
        .get_mut(&row.id)
        .unwrap()
        .stop_tx = stop_tx;
    let (provider_tx, provider_rx) = mpsc::channel(1);
    provider_tx
        .send(StreamEvent {
            event_type: "result".into(),
            data: serde_json::json!({"subtype": reason, "terminal_reason": reason,
            "is_error": true, "result": "", "num_turns": 1}),
        })
        .await
        .unwrap();
    drop(provider_tx);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        SessionManager::monitor_session(
            row.id,
            7,
            Box::new(crate::provider::CliProviderSession::new(provider_rx)),
            f.manager.active.clone(),
            f.manager.completed.clone(),
            f.manager.event_bus.clone(),
            stop_rx,
            f.manager.store.clone(),
            f.manager.model_call_settlements.handle().unwrap(),
            f.manager.persistence.clone(),
            0,
            false,
            f.manager.socket_path.clone(),
            f.manager.token_counter.clone(),
            None,
            f.manager.retry_tx.clone(),
            f.manager.tool_registry.clone(),
            crate::turn_controller::TurnController::new(
                crate::turn_controller::ContinuationPolicy::Single,
            ),
            f.manager.runtime_config.clone(),
            f.manager.spawn_coordinator.clone(),
            f.manager.agent_tokens.clone(),
            f.manager.spawn_epoch.clone(),
            f.manager.agent_message_arbiter.clone(),
            f.manager.codegraph_handle.clone(),
            f.manager.custody_execution_runtime(),
        ),
    )
    .await
    .expect("failed fake provider finalizes");
    assert_eq!(
        f.manager
            .store
            .lock()
            .await
            .get_session(row.id)
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Failed
    );
}

async fn job(f: &Fixture, id: Uuid) -> ScheduledJob {
    f.manager
        .store
        .lock()
        .await
        .get_scheduled_job(&id)
        .unwrap()
        .unwrap()
}

// Advance only the scheduler clock seam, preserving the persisted backoff proof.
async fn fire_due(f: &Fixture, id: Uuid) {
    let due = Utc::now() - Duration::seconds(1);
    f.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE scheduled_jobs SET next_fire_at=?2 WHERE id=?1",
            rusqlite::params![
                id.to_string(),
                due.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            ],
        )
        .unwrap();
    let snapshot = job(f, id).await;
    assert!(
        f.manager
            .store
            .lock()
            .await
            .list_due_scheduled_jobs(&Utc::now())
            .unwrap()
            .iter()
            .any(|j| j.id == id)
    );
    let launcher: Arc<dyn SessionLauncher> = f.manager.clone();
    crate::scheduler::fire_job_for_test(
        &f.manager.store,
        &f.manager.event_bus,
        &launcher,
        &snapshot,
    )
    .await;
}

async fn assert_running_again(f: &Fixture, id: Uuid) {
    super::super::launch::send_controller_candidate_test_event(id, StreamEvent {
        event_type: "assistant".into(),
        data: serde_json::json!({"role":"assistant", "content":"automatic recovery is running"}),
    }).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let active = f.manager.active.read().await;
            if active.get(&id).is_some_and(|tracked| {
                tracked.session.status == SessionStatus::Running
                    && tracked
                        .events
                        .iter()
                        .any(|e| e.content.contains("automatic recovery is running"))
            }) {
                break;
            }
            drop(active);
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("resumed provider produces output while Running");
}

async fn stop(f: &Fixture, id: Uuid) {
    if f.manager.active.read().await.contains_key(&id) {
        f.manager.interrupt_session(id).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while f.manager.active.read().await.contains_key(&id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fake provider settles");
    }
    drop_controller_candidate_test_stream(id);
    drop_controller_candidate_test_process(id);
}

async fn notice_versions(f: &Fixture, id: Uuid) -> Vec<String> {
    let store = f.manager.store.lock().await;
    let mut stmt = store.conn.prepare("SELECT subject_version FROM harness_manager_notices WHERE subject_id=?1 AND subject_version LIKE 'transient_heal%' ORDER BY subject_version").unwrap();
    stmt.query_map([id.to_string()], |r| r.get(0))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap()
}

async fn lead(f: &Fixture, max_active: u16) -> Session {
    let store = f.manager.store.lock().await;
    let (_, mut lead) = crate::store::manager_resources::tests::fixture(
        &store,
        ManagerPolicyV2 {
            max_active_sessions: max_active,
            ..ManagerPolicyV2::default()
        },
    );
    lead.working_dir = f.repo.path().to_path_buf();
    lead.provider = SessionProvider::Claude;
    lead.max_retries = None;
    lead.retry_attempt = None;
    store.update_session_metadata(&lead).unwrap();
    store.conn.execute("UPDATE sessions SET working_dir=?2, provider='Claude', max_retries=NULL, retry_attempt=NULL WHERE id=?1", rusqlite::params![lead.id.to_string(), lead.working_dir.to_string_lossy()]).unwrap();
    lead
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn aborted_streaming_child_heals_without_manual_resume() {
    let f = fixture();
    let mut child = session(&f);
    let mut owner = session(&f);
    owner.session_kind = SessionKind::Epic;
    child.session_kind = SessionKind::Task;
    f.manager.store.lock().await.insert_session(&owner).unwrap();
    child.parent_id = Some(owner.id);
    let mut events = f.manager.event_bus.subscribe();
    let before = Utc::now();
    fail(&f, child.clone(), "aborted_streaming").await;
    let id = transient_heal_job_id(child.id);
    let heal = job(&f, id).await;
    assert!(heal.enabled);
    assert_eq!(heal.wake_session_id, Some(child.id));
    assert!(heal.next_fire_at >= before + Duration::seconds(30));
    assert!(heal.next_fire_at <= Utc::now() + Duration::seconds(30));
    assert_eq!(
        f.manager
            .store
            .lock()
            .await
            .transient_heal_state(id)
            .unwrap()
            .unwrap()
            .attempts,
        1
    );
    crate::session::transient_heal::heal_failed_session(
        &f.manager.store,
        &f.manager.event_bus,
        child.id,
    )
    .await;
    let mut notices = 0;
    while let Ok(event) = events.try_recv() {
        if let DaemonEvent::SessionHealScheduled {
            session_id,
            owner_session_id,
            attempt,
            ..
        } = event.as_ref()
        {
            assert_eq!(
                (*session_id, *owner_session_id, *attempt),
                (child.id, Some(owner.id), 1)
            );
            notices += 1;
        }
    }
    assert_eq!(notices, 1);
    assert_eq!(
        f.manager
            .store
            .lock()
            .await
            .list_scheduled_jobs()
            .unwrap()
            .iter()
            .filter(|j| j.wake_session_id == Some(child.id))
            .count(),
        1
    );
    let process = install_controller_candidate_test_process(child.id);
    fire_due(&f, id).await;
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    assert!(f.manager.active.read().await.contains_key(&child.id));
    assert!(!job(&f, id).await.enabled);
    let launcher: Arc<dyn SessionLauncher> = f.manager.clone();
    crate::scheduler::fire_job_for_test(&f.manager.store, &f.manager.event_bus, &launcher, &heal)
        .await;
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    assert_running_again(&f, child.id).await;
    stop(&f, child.id).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn capacity_refused_lead_heals_without_manual_resume() {
    let f = fixture();
    let lead = lead(&f, 1).await;
    let mut sibling = session(&f);
    sibling.project_id = lead.project_id;
    sibling.parent_id = lead.parent_id;
    sibling.session_kind = SessionKind::Task;
    f.manager
        .store
        .lock()
        .await
        .insert_session(&sibling)
        .unwrap();
    fail(&f, lead.clone(), "manager_v2_concurrency_capacity").await;
    let id = transient_heal_job_id(lead.id);
    let process = install_controller_candidate_test_process(lead.id);
    fire_due(&f, id).await;
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
    assert!(job(&f, id).await.enabled);
    assert_eq!(
        f.manager
            .store
            .lock()
            .await
            .get_session(lead.id)
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Failed
    );
    assert_eq!(
        f.manager
            .store
            .lock()
            .await
            .transient_heal_state(id)
            .unwrap()
            .unwrap()
            .attempts,
        1,
        "a capacity refusal spends no heal budget"
    );
    assert_eq!(
        notice_versions(&f, lead.id).await,
        ["transient_heal_scheduled:1"]
    );
    f.manager
        .store
        .lock()
        .await
        .update_session_status(sibling.id, SessionStatus::Completed)
        .unwrap();
    fire_due(&f, id).await;
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    assert!(f.manager.active.read().await.contains_key(&lead.id));
    assert!(!job(&f, id).await.enabled);
    assert_eq!(notice_versions(&f, lead.id).await.len(), 1);
    assert_running_again(&f, lead.id).await;
    stop(&f, lead.id).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn transient_heal_blocking_limit_and_operator_interrupt_are_not_retried() {
    let f = fixture();
    for reason in [
        "blocking_limit",
        "aborted_tools",
        "restart_reconciled_failed",
        "codex_resume_tool_history_invalid",
    ] {
        let row = session(&f);
        fail(&f, row.clone(), reason).await;
        assert!(
            f.manager
                .store
                .lock()
                .await
                .get_scheduled_job(&transient_heal_job_id(row.id))
                .unwrap()
                .is_none()
        );
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn transient_heal_capacity_refusals_never_exhaust_the_budget() {
    let f = fixture();
    let lead = lead(&f, 1).await;
    let mut sibling = session(&f);
    sibling.project_id = lead.project_id;
    sibling.parent_id = lead.parent_id;
    sibling.session_kind = SessionKind::Task;
    f.manager
        .store
        .lock()
        .await
        .insert_session(&sibling)
        .unwrap();
    fail(&f, lead.clone(), "manager_v2_concurrency_capacity").await;
    let id = transient_heal_job_id(lead.id);
    let process = install_controller_candidate_test_process(lead.id);
    assert_eq!(TRANSIENT_HEAL_MAX_ATTEMPTS, 8);
    // More refusals than the whole budget: the session never ran, so the heal
    // stays armed with its attempt-1 backoff and sends no further notice.
    for _ in 0..(TRANSIENT_HEAL_MAX_ATTEMPTS + 2) {
        fire_due(&f, id).await;
        assert!(job(&f, id).await.enabled);
    }
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
    let state = f
        .manager
        .store
        .lock()
        .await
        .transient_heal_state(id)
        .unwrap()
        .unwrap();
    assert_eq!(state.attempts, 1);
    assert!(!state.exhausted);
    assert_eq!(
        notice_versions(&f, lead.id).await,
        ["transient_heal_scheduled:1"]
    );
    stop(&f, lead.id).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn transient_heal_sixty_minute_budget_survives_daemon_restart() {
    let f = fixture();
    let lead = lead(&f, 1).await;
    fail(&f, lead.clone(), "aborted_streaming").await;
    let id = transient_heal_job_id(lead.id);
    let first =
        (Utc::now() - Duration::minutes(60)).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    f.manager.store.lock().await.conn.execute(
        "UPDATE scheduled_jobs SET schedule_json=json_set(schedule_json,'$.transient_heal.first_failed_at',?2) WHERE id=?1",
        rusqlite::params![id.to_string(), first],
    ).unwrap();
    // Reopen the actual SQLite database; the scheduler has no volatile retry state.
    let restarted = Arc::new(tokio::sync::Mutex::new(
        Store::open(&f._dir.path().join("rsi.db")).unwrap(),
    ));
    let process = install_controller_candidate_test_process(lead.id);
    let launcher: Arc<dyn SessionLauncher> = f.manager.clone();
    let snapshot = restarted
        .lock()
        .await
        .get_scheduled_job(&id)
        .unwrap()
        .unwrap();
    crate::scheduler::fire_job_for_test(&restarted, &f.manager.event_bus, &launcher, &snapshot)
        .await;
    assert!(!job(&f, id).await.enabled);
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
    assert_eq!(
        notice_versions(&f, lead.id).await,
        ["transient_heal_exhausted", "transient_heal_scheduled:1"]
    );
    stop(&f, lead.id).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn transient_heal_stale_due_snapshot_does_not_resume_running_archived_or_interrupted_target()
{
    let f = fixture();
    for (status, expired) in [
        (SessionStatus::Running, false),
        (SessionStatus::Running, true),
        (SessionStatus::Archived, false),
        (SessionStatus::Archived, true),
        (SessionStatus::Interrupted, false),
        (SessionStatus::Interrupted, true),
    ] {
        let row = session(&f);
        fail(&f, row.clone(), "aborted_streaming").await;
        let id = transient_heal_job_id(row.id);
        f.manager
            .store
            .lock()
            .await
            .update_session_status(row.id, status)
            .unwrap();
        if expired {
            f.manager.store.lock().await.conn.execute(
                "UPDATE scheduled_jobs SET schedule_json=json_set(schedule_json,'$.transient_heal.first_failed_at',?2) WHERE id=?1",
                rusqlite::params![id.to_string(), (Utc::now()-Duration::minutes(61)).to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)],
            ).unwrap();
        }
        let process = install_controller_candidate_test_process(row.id);
        fire_due(&f, id).await;
        assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 0);
        assert_eq!(
            f.manager
                .store
                .lock()
                .await
                .get_session(row.id)
                .unwrap()
                .unwrap()
                .status,
            status
        );
        assert!(!job(&f, id).await.enabled);
        stop(&f, row.id).await;
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn transient_heal_rotation_successor_inherits_budget_and_survives_retention() {
    let f = fixture();
    let row = session(&f);
    fail(&f, row.clone(), "aborted_streaming").await;
    let id = transient_heal_job_id(row.id);
    let process = install_controller_candidate_test_process(row.id);
    fire_due(&f, id).await;
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    stop(&f, row.id).await;
    let mut successor = session(&f);
    successor.continued_from = Some(row.id);
    fail(&f, successor.clone(), "aborted_streaming").await;
    let heal = job(&f, id).await;
    assert_eq!(heal.wake_session_id, Some(successor.id));
    assert_eq!(
        f.manager
            .store
            .lock()
            .await
            .transient_heal_state(id)
            .unwrap()
            .unwrap()
            .attempts,
        2
    );
    f.manager
        .store
        .lock()
        .await
        .conn
        .execute(
            "UPDATE scheduled_jobs SET enabled=0 WHERE id=?1",
            [id.to_string()],
        )
        .unwrap();
    f.manager
        .store
        .lock()
        .await
        .retention_sweep_batch(Utc::now() + Duration::minutes(20), None, 1000)
        .unwrap();
    assert_eq!(
        f.manager
            .store
            .lock()
            .await
            .transient_heal_state(id)
            .unwrap()
            .unwrap()
            .attempts,
        2
    );
    let next_process = install_controller_candidate_test_process(successor.id);
    crate::session::transient_heal::heal_failed_session(
        &f.manager.store,
        &f.manager.event_bus,
        successor.id,
    )
    .await;
    fire_due(&f, id).await;
    assert_eq!(
        next_process.productive_start_count.load(Ordering::SeqCst),
        1
    );
    stop(&f, successor.id).await;
}

/// Hold the scheduler's successful-resume receipt until the real provider
/// monitor has already finalized a second failure. This makes the early-exit
/// race deterministic without replacing either lifecycle path.
struct FastFailureLauncher {
    manager: Arc<SessionManager>,
    process: super::super::launch::ControllerCandidateTestProcess,
}

#[async_trait::async_trait]
impl SessionLauncher for FastFailureLauncher {
    async fn launch(&self, _: crate::claude::LaunchConfig) -> crate::error::Result<Uuid> {
        panic!("a heal must resume its existing session");
    }

    async fn reconcile_transient_heal_after_resume(&self, target: Uuid) {
        self.manager
            .reconcile_transient_heal_after_resume(target)
            .await;
    }

    async fn resume_scheduled_job(
        &self,
        target: Uuid,
        query: String,
        jobs: Vec<Uuid>,
    ) -> crate::error::Result<Uuid> {
        let id = SessionLauncher::resume_scheduled_job(self.manager.as_ref(), target, query, jobs)
            .await?;
        super::super::launch::send_controller_candidate_test_event(id, StreamEvent {
            event_type: "result".into(),
            data: serde_json::json!({"subtype":"aborted_streaming", "terminal_reason":"aborted_streaming", "is_error":true, "result":""}),
        }).await;
        self.process.alive.store(false, Ordering::SeqCst);
        drop_controller_candidate_test_stream(id);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let durable_failed = self
                    .manager
                    .store
                    .lock()
                    .await
                    .get_session(id)
                    .unwrap()
                    .unwrap()
                    .status
                    == SessionStatus::Failed;
                if durable_failed && !self.manager.active.read().await.contains_key(&id) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fast fake provider fails before scheduler receipt");
        // Let the finalizer's heal hook observe the still-enabled delivery.
        crate::session::transient_heal::heal_failed_session(
            &self.manager.store,
            &self.manager.event_bus,
            id,
        )
        .await;
        Ok(id)
    }
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn transient_heal_provider_failing_before_scheduler_settlement_rearms_once() {
    let f = fixture();
    let row = session(&f);
    fail(&f, row.clone(), "aborted_streaming").await;
    let id = transient_heal_job_id(row.id);
    let process = install_controller_candidate_test_process(row.id);
    let launcher: Arc<dyn SessionLauncher> = Arc::new(FastFailureLauncher {
        manager: f.manager.clone(),
        process: process.clone(),
    });
    let snapshot = job(&f, id).await;
    crate::scheduler::fire_job_for_test(
        &f.manager.store,
        &f.manager.event_bus,
        &launcher,
        &snapshot,
    )
    .await;
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    assert!(job(&f, id).await.enabled);
    assert_eq!(
        f.manager
            .store
            .lock()
            .await
            .transient_heal_state(id)
            .unwrap()
            .unwrap()
            .attempts,
        2
    );
    assert_eq!(
        f.manager
            .store
            .lock()
            .await
            .list_scheduled_jobs()
            .unwrap()
            .iter()
            .filter(|j| j.wake_session_id == Some(row.id))
            .count(),
        1
    );
    // Reusing the old due snapshot must respect attempt 2's new backoff.
    let next_process = install_controller_candidate_test_process(row.id);
    let ordinary_launcher: Arc<dyn SessionLauncher> = f.manager.clone();
    crate::scheduler::fire_job_for_test(
        &f.manager.store,
        &f.manager.event_bus,
        &ordinary_launcher,
        &snapshot,
    )
    .await;
    assert_eq!(
        next_process.productive_start_count.load(Ordering::SeqCst),
        0
    );
    assert!(job(&f, id).await.enabled);
    stop(&f, row.id).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn transient_heal_pending_job_resumes_after_daemon_restart_without_completed_cache() {
    let mut f = fixture();
    let row = session(&f);
    fail(&f, row.clone(), "aborted_streaming").await;
    let id = transient_heal_job_id(row.id);
    let store = Store::open(&f._dir.path().join("rsi.db")).unwrap();
    f.manager = Arc::new(
        SessionManager::new(
            Arc::new(EventBus::new(16)),
            store,
            false,
            f._dir.path().join("restart.sock"),
            None,
            Vec::new(),
            f.manager.runtime_config.clone(),
            f._dir.path().join("sandboxes"),
        )
        .unwrap(),
    );
    assert!(f.manager.completed.read().await.is_empty());
    let process = install_controller_candidate_test_process(row.id);
    fire_due(&f, id).await;
    assert_eq!(process.productive_start_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        f.manager
            .store
            .lock()
            .await
            .transient_heal_state(id)
            .unwrap()
            .unwrap()
            .attempts,
        1
    );
    assert!(!job(&f, id).await.enabled);
    stop(&f, row.id).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[test]
fn heal_bootstrap_treats_any_non_system_non_user_event_as_an_effect() {
    let event = |event_type, role| {
        let mut e = rsi_common::types::ConversationEvent {
            id: 0,
            session_id: Uuid::new_v4(),
            sequence: 0,
            event_type,
            role,
            content: String::new(),
            tool_name: None,
            tool_input: None,
            created_at: Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };
        e.content = "x".into();
        e
    };
    let quiet = [
        event(EventType::Message, Some(Role::User)),
        event(EventType::System, None),
    ];
    assert!(super::super::lifecycle::heal_bootstrap_events_have_no_effect(&quiet));
    for effect in [
        event(EventType::Message, Some(Role::Assistant)),
        event(EventType::ToolUse, None),
        event(EventType::ToolResult, None),
        event(EventType::Thinking, None),
        event(EventType::Plan, None),
        event(EventType::Compressed, None),
        event(EventType::CompletionGate, None),
    ] {
        let mut events = quiet.to_vec();
        events.push(effect);
        assert!(!super::super::lifecycle::heal_bootstrap_events_have_no_effect(&events));
    }
}
