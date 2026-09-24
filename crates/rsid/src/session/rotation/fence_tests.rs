//! K2 continuation fence against real rotation publication (RPC-1 C1/C2).
//! Race tests order capture -> mutation -> guarded check with the real spawn
//! guards and explicit pause seams, never by holding a guard the mutation
//! needs (K2 finding c).
#![allow(
    clippy::expect_used,
    clippy::significant_drop_tightening,
    clippy::large_futures
)]

use super::super::launch::{
    drop_controller_candidate_test_stream, install_controller_candidate_test_process,
};
use super::super::spawn_single_flight::acquire_spawn_guard;
use super::tests::{rotate_task_successor_for_test, rotation_manager, test_session};
use super::*;
use crate::issue_tracker::poller::SessionLauncher;
use crate::session::harness::tools::schedule_wake::{
    ScheduleWakeRequest, build_agent_scheduled_job,
};
use crate::store::manager_actions::fence::{
    CONTINUATION_PUBLICATION_PENDING, CONTINUATION_RETRY_EXHAUSTED, CONTINUATION_TIP_CHANGED,
    CONTINUATION_TIP_UNESTABLISHED, ContinuationAuthorityV1,
};
use rsi_common::types::SessionKind;
use std::time::Duration;

const WAKE: &str = "scheduled wake for the lineage";

async fn completed_parent(
    manager: &SessionManager,
    dir: &std::path::Path,
    query: &str,
) -> anyhow::Result<Session> {
    let mut parent = test_session(Uuid::new_v4(), SessionStatus::Completed);
    parent.working_dir = dir.to_path_buf();
    parent.query = query.into();
    {
        let mut store = manager.store.lock().await;
        store.insert_session(&parent)?;
        store.publish_startup_ordinary(parent.id)?;
    }
    manager
        .completed
        .write()
        .await
        .insert(parent.id, CompletedSession::for_test(parent.clone()));
    Ok(parent)
}

fn resume_job(
    origin: Uuid,
    dir: &std::path::Path,
) -> anyhow::Result<rsi_common::types::ScheduledJob> {
    build_agent_scheduled_job(ScheduleWakeRequest {
        message: WAKE.into(),
        in_seconds: Some(60),
        at: None,
        name: None,
        every_seconds: None,
        mode: Some("resume".into()),
        working_dir: dir.to_path_buf(),
        provider: Some(SessionProvider::Claude),
        model: Some("fence-scripted-provider".into()),
        project_id: None,
        origin_session_id: Some(origin),
        watch_session_id: None,
    })
    .map_err(anyhow::Error::msg)
}

async fn user_events_containing(manager: &SessionManager, session: Uuid, text: &str) -> usize {
    manager
        .store
        .lock()
        .await
        .load_events(session)
        .unwrap_or_default()
        .iter()
        .filter(|event| event.content.contains(text))
        .count()
}

/// Events reach `SQLite` through the persistence queue; wait for the flush.
async fn delivered_events(manager: &SessionManager, session: Uuid, text: &str) -> usize {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let count = user_events_containing(manager, session, text).await;
        if count > 0 || tokio::time::Instant::now() >= deadline {
            return count;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_inactive(manager: &SessionManager, session: Uuid) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while manager.active.read().await.contains_key(&session) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("session settles");
}

async fn fire(manager: &Arc<SessionManager>, job: &rsi_common::types::ScheduledJob) {
    let launcher: Arc<dyn SessionLauncher> = manager.clone();
    crate::scheduler::fire_job_for_test(&manager.store, manager.event_bus(), &launcher, job).await;
}

/// K2 finding c / design tests 1-2: the dispatcher captures L1, the lineage
/// rotates and publishes L2 before L1's guard is taken, and the guarded check
/// refuses `continuation_tip_changed`. The one-shot wake is retained with
/// durable backoff, L1 is not restarted, and the next pass delivers to L2.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::expect_used)]
async fn continuation_captured_before_rotation_publication_retries_then_delivers_to_successor()
-> anyhow::Result<()> {
    let (manager, dir) = rotation_manager();
    let manager = Arc::new(manager);
    let parent = completed_parent(&manager, dir.path(), "Objective X").await?;
    let job = resume_job(parent.id, dir.path())?;
    manager.store.lock().await.insert_scheduled_job(&job)?;
    let parent_history = manager.store.lock().await.load_events(parent.id)?.len();

    // Capture happens at dispatch; the barrier sits before guard(L1).
    let (reached, resume) =
        crate::session::lifecycle::install_continuation_pause_for_test(parent.id);
    let dispatch = {
        let manager = Arc::clone(&manager);
        let job = job.clone();
        tokio::spawn(async move { fire(&manager, &job).await })
    };
    reached.await?;
    // C1 takes guard(L1) and guard(L2): free, because the dispatcher has not
    // acquired guard(L1) yet. No guard is held by the test.
    let successor = Box::pin(rotate_task_successor_for_test(&manager, parent.id)).await?;
    resume
        .send(())
        .map_err(|()| anyhow::anyhow!("dispatch barrier dropped"))?;
    dispatch.await?;

    let refused_at = chrono::Utc::now();
    {
        let store = manager.store.lock().await;
        let row = store
            .get_scheduled_job(&job.id)?
            .ok_or_else(|| anyhow::anyhow!("wake row missing"))?;
        assert!(row.enabled, "a retryable refusal retains the one-shot wake");
        assert_eq!(row.last_fired_at, None);
        assert!(row.next_fire_at >= refused_at + chrono::Duration::seconds(25));
        let retry = store
            .continuation_retry(job.id)?
            .ok_or_else(|| anyhow::anyhow!("retry state missing"))?;
        assert_eq!(retry.attempts, 1);
        assert_eq!(retry.last_code, CONTINUATION_TIP_CHANGED);
        assert_eq!(retry.last_tip, Some(successor.id));
        assert_eq!(
            store.load_events(parent.id)?.len(),
            parent_history,
            "the superseded predecessor keeps its exact history"
        );
    }

    // Next pass: the fence captures the published successor and delivers.
    wait_inactive(&manager, successor.id).await;
    // The scripted provider reports no session id; a resumable successor has one.
    let provider_session = format!("provider-{}", successor.id);
    manager.store.lock().await.conn.execute(
        "UPDATE sessions SET claude_session_id=?1 WHERE id=?2",
        rusqlite::params![provider_session, successor.id.to_string()],
    )?;
    if let Some(cached) = manager.completed.write().await.get_mut(&successor.id) {
        cached.session.claude_session_id = Some(provider_session);
    }
    let process = install_controller_candidate_test_process(successor.id);
    let retained = manager
        .store
        .lock()
        .await
        .get_scheduled_job(&job.id)?
        .ok_or_else(|| anyhow::anyhow!("wake row missing"))?;
    fire(&manager, &retained).await;
    assert_eq!(delivered_events(&manager, successor.id, WAKE).await, 1);
    assert_eq!(
        process
            .productive_start_count
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    {
        let store = manager.store.lock().await;
        let settled = store
            .get_scheduled_job(&job.id)?
            .ok_or_else(|| anyhow::anyhow!("wake row missing"))?;
        assert!(
            settled.last_fired_at.is_some(),
            "the delivered one-shot is stamped"
        );
        assert_eq!(store.continuation_retry(job.id)?, None);
    }
    drop_controller_candidate_test_stream(successor.id);
    Ok(())
}

/// K2 finding a: C1 publication waits for guard(P) held by an in-flight
/// continuation, so the tip and lead generation cannot move between that
/// continuation's check and its provider installation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rotation_publication_waits_for_in_flight_continuation_guard() -> anyhow::Result<()> {
    let (manager, dir) = rotation_manager();
    let manager = Arc::new(manager);
    let parent = completed_parent(&manager, dir.path(), "Objective X").await?;
    let (reached, resume) = install_rotation_publication_pause_for_test(parent.id);
    let rotation = {
        let manager = Arc::clone(&manager);
        tokio::spawn(
            async move { Box::pin(rotate_task_successor_for_test(&manager, parent.id)).await },
        )
    };
    reached.await?;
    // A continuation of P passed its check and holds guard(P) until its
    // provider is installed.
    let in_flight = acquire_spawn_guard(parent.id).await;
    resume
        .send(())
        .map_err(|()| anyhow::anyhow!("publication barrier dropped"))?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        manager
            .store
            .lock()
            .await
            .published_lineage_tip(parent.id)?,
        Some(parent.id),
        "publication must wait for the continuation's guard"
    );
    drop(in_flight);
    let successor = rotation.await??;
    assert_eq!(
        manager
            .store
            .lock()
            .await
            .published_lineage_tip(parent.id)?,
        Some(successor.id)
    );
    Ok(())
}

/// K2 finding b / design test 3: a reserved-but-unpublished successor is not
/// the tip even when the predecessor's latest earlier rotation is a legacy
/// `completed` without a successor id. Delivery to P is refused
/// `continuation_publication_pending`; after C1 the tip is S.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reserved_unpublished_successor_is_not_the_tip_after_a_legacy_rotation()
-> anyhow::Result<()> {
    let (manager, dir) = rotation_manager();
    let manager = Arc::new(manager);
    let parent = completed_parent(&manager, dir.path(), "Objective X").await?;
    manager.store.lock().await.insert_rotation_event(
        parent.id,
        "legacy",
        "completed",
        "completed",
        None,
    )?;
    let (reached, resume) = install_rotation_publication_pause_for_test(parent.id);
    let rotation = {
        let manager = Arc::clone(&manager);
        tokio::spawn(
            async move { Box::pin(rotate_task_successor_for_test(&manager, parent.id)).await },
        )
    };
    reached.await?;
    let reserved: (String, String) = manager.store.lock().await.conn.query_row(
        "SELECT id, status FROM sessions WHERE continued_from=?1",
        [parent.id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    assert_ne!(reserved.1, "Failed", "the reserved successor is live");
    {
        let store = manager.store.lock().await;
        assert_eq!(store.published_lineage_tip(parent.id)?, Some(parent.id));
        let fence = store
            .capture_continuation_fence(parent.id, ContinuationAuthorityV1::Automated)?
            .ok_or_else(|| anyhow::anyhow!("fence"))?;
        assert_eq!(fence.tip, parent.id);
    }
    let refused = SessionLauncher::resume_scheduled(&*manager, parent.id, WAKE.into())
        .await
        .map_err(|error| error.to_string());
    assert_eq!(
        refused,
        Err(format!(
            "Invalid parameter: {CONTINUATION_PUBLICATION_PENDING}"
        ))
    );
    resume
        .send(())
        .map_err(|()| anyhow::anyhow!("publication barrier dropped"))?;
    let successor = rotation.await??;
    assert_eq!(successor.id.to_string(), reserved.0);
    assert_eq!(
        manager
            .store
            .lock()
            .await
            .published_lineage_tip(parent.id)?,
        Some(successor.id)
    );
    Ok(())
}

/// Design test 14: a stall nudge addressed to a rotated-away session is a
/// typed refusal; the published successor keeps its incarnation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stall_nudge_after_rotation_is_typed_and_successor_keeps_its_incarnation()
-> anyhow::Result<()> {
    let (manager, dir) = rotation_manager();
    let parent = completed_parent(&manager, dir.path(), "Objective X").await?;
    let successor = Box::pin(rotate_task_successor_for_test(&manager, parent.id)).await?;
    wait_inactive(&manager, successor.id).await;
    let successor_history = manager.store.lock().await.load_events(successor.id)?.len();
    let refused = manager
        .continue_stall_nudge(parent.id, "nudge".into())
        .await
        .map_err(|error| error.to_string());
    assert_eq!(
        refused,
        Err(format!("Invalid parameter: {CONTINUATION_TIP_CHANGED}"))
    );
    assert_eq!(
        manager.store.lock().await.load_events(successor.id)?.len(),
        successor_history
    );
    Ok(())
}

/// Design test 5 (RPC-1 C4): a refused successor settled Failed is never the
/// tip, so a scheduled wake still delivers to the predecessor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_unpublished_successor_leaves_wake_delivered_to_predecessor() -> anyhow::Result<()> {
    let (manager, dir) = rotation_manager();
    let parent = completed_parent(&manager, dir.path(), "Objective X").await?;
    let mut refused = test_session(Uuid::new_v4(), SessionStatus::Failed);
    refused.working_dir = dir.path().to_path_buf();
    refused.continued_from = Some(parent.id);
    {
        let store = manager.store.lock().await;
        store.insert_session(&refused)?;
        store.insert_rotation_event(
            parent.id,
            "refused-rotation",
            "reserved",
            "successor_reserved",
            Some(&serde_json::json!({ "successor_id": refused.id }).to_string()),
        )?;
        store.insert_rotation_event(
            parent.id,
            "refused-rotation",
            "completed",
            "refused:lead_transfer",
            None,
        )?;
    }
    let _process = install_controller_candidate_test_process(parent.id);
    let delivered = SessionLauncher::resume_scheduled(&manager, parent.id, WAKE.into()).await?;
    assert_eq!(delivered, parent.id);
    assert_eq!(delivered_events(&manager, parent.id, WAKE).await, 1);
    drop_controller_candidate_test_stream(parent.id);
    Ok(())
}

/// Design test 17: operator-authorized exact-id continuation of a
/// superseded session is unfenced by design and still proceeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn operator_continuation_of_superseded_session_proceeds() -> anyhow::Result<()> {
    let (manager, dir) = rotation_manager();
    let parent = completed_parent(&manager, dir.path(), "Objective X").await?;
    let successor = Box::pin(rotate_task_successor_for_test(&manager, parent.id)).await?;
    wait_inactive(&manager, successor.id).await;
    let restored = manager.store.lock().await.get_session(parent.id)?;
    if let Some(mut row) = restored {
        row.status = SessionStatus::Completed;
        manager
            .completed
            .write()
            .await
            .insert(parent.id, CompletedSession::for_test(row));
    }
    let _process = install_controller_candidate_test_process(parent.id);
    manager
        .continue_session_operator(parent.id, "operator follow-up".into())
        .await?;
    assert_eq!(
        delivered_events(&manager, parent.id, "operator follow-up").await,
        1
    );
    drop_controller_candidate_test_stream(parent.id);
    Ok(())
}

/// Design test 4 (#620): an orphan Starting tip is retried with backoff and,
/// at the bound, the one-shot wake is disabled and stamped (never deleted)
/// with a typed `continuation_retry_exhausted` naming the tip.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn orphan_starting_tip_exhausts_retry_and_settles_typed() -> anyhow::Result<()> {
    let (manager, dir) = rotation_manager();
    let manager = Arc::new(manager);
    let mut orphan = test_session(Uuid::new_v4(), SessionStatus::Starting);
    orphan.working_dir = dir.path().to_path_buf();
    manager.store.lock().await.insert_session(&orphan)?;
    let job = resume_job(orphan.id, dir.path())?;
    manager.store.lock().await.insert_scheduled_job(&job)?;
    let mut messages = manager.event_bus().subscribe();

    fire(&manager, &job).await;
    let first = manager.store.lock().await.continuation_retry(job.id)?;
    assert_eq!(
        first
            .as_ref()
            .map(|retry| (retry.attempts, retry.last_code.as_str())),
        Some((1, CONTINUATION_TIP_UNESTABLISHED))
    );
    for _ in 0..7 {
        fire(&manager, &job).await;
    }
    let store = manager.store.lock().await;
    let settled = store
        .get_scheduled_job(&job.id)?
        .ok_or_else(|| anyhow::anyhow!("wake row kept"))?;
    assert!(!settled.enabled);
    assert!(settled.last_fired_at.is_some());
    drop(store);
    let mut exhausted = Vec::new();
    while let Ok(event) = messages.try_recv() {
        if let DaemonEvent::SystemMessage { message, .. } = event.as_ref()
            && message.starts_with(CONTINUATION_RETRY_EXHAUSTED)
        {
            exhausted.push(message.clone());
        }
    }
    assert_eq!(exhausted.len(), 1);
    assert!(exhausted[0].contains(&orphan.id.to_string()));
    Ok(())
}

/// Review round 2 `retry_exhaustion_crash_reset` (scheduled Resume): the
/// daemon dies after the eighth refusal is found exhausted but before the
/// one-shot settlement. The restarted daemon reopens to the durable attempt
/// history, not a fresh budget: its next refusal settles the job (disabled,
/// stamped, retry state cleared in the same write) with one typed message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exhausted_resume_retry_survives_crash_before_settlement_without_fresh_budget()
-> anyhow::Result<()> {
    let (manager, dir) = rotation_manager();
    let manager = Arc::new(manager);
    let mut orphan = test_session(Uuid::new_v4(), SessionStatus::Starting);
    orphan.working_dir = dir.path().to_path_buf();
    manager.store.lock().await.insert_session(&orphan)?;
    let job = resume_job(orphan.id, dir.path())?;
    manager.store.lock().await.insert_scheduled_job(&job)?;
    for _ in 0..7 {
        fire(&manager, &job).await;
    }
    crate::scheduler::install_crash_before_exhausted_settlement_for_test(job.id);
    fire(&manager, &job).await;
    drop(manager);

    let restarted = Arc::new(super::tests::rotation_manager_on(dir.path(), false));
    {
        let store = restarted.store.lock().await;
        let row = store
            .get_scheduled_job(&job.id)?
            .ok_or_else(|| anyhow::anyhow!("wake row kept"))?;
        assert!(row.enabled);
        assert_eq!(
            store
                .continuation_retry(job.id)?
                .map(|retry| (retry.attempts, retry.last_code)),
            Some((7, CONTINUATION_TIP_UNESTABLISHED.to_string()))
        );
    }
    let mut messages = restarted.event_bus().subscribe();
    fire(&restarted, &job).await;
    let store = restarted.store.lock().await;
    let settled = store
        .get_scheduled_job(&job.id)?
        .ok_or_else(|| anyhow::anyhow!("wake row kept"))?;
    assert!(!settled.enabled);
    assert!(settled.last_fired_at.is_some());
    assert_eq!(store.continuation_retry(job.id)?, None);
    drop(store);
    let mut exhausted = Vec::new();
    while let Ok(event) = messages.try_recv() {
        if let DaemonEvent::SystemMessage { message, .. } = event.as_ref()
            && message.starts_with(CONTINUATION_RETRY_EXHAUSTED)
        {
            exhausted.push(message.clone());
        }
    }
    assert_eq!(exhausted.len(), 1);
    assert!(exhausted[0].contains(&orphan.id.to_string()));
    Ok(())
}

/// Design test 15: an operator "trigger now" refused by the fence is typed
/// and consumes nothing: the row stays enabled, unstamped, with no retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manual_trigger_refused_by_fence_leaves_row_unchanged() -> anyhow::Result<()> {
    let (manager, dir) = rotation_manager();
    let manager = Arc::new(manager);
    let mut orphan = test_session(Uuid::new_v4(), SessionStatus::Starting);
    orphan.working_dir = dir.path().to_path_buf();
    manager.store.lock().await.insert_session(&orphan)?;
    let job = resume_job(orphan.id, dir.path())?;
    manager.store.lock().await.insert_scheduled_job(&job)?;
    let launcher: Arc<dyn SessionLauncher> = manager.clone();
    crate::scheduler::fire_job_manual_for_test(
        &manager.store,
        manager.event_bus(),
        &launcher,
        &job,
    )
    .await;
    let store = manager.store.lock().await;
    let row = store
        .get_scheduled_job(&job.id)?
        .ok_or_else(|| anyhow::anyhow!("wake row kept"))?;
    assert!(row.enabled);
    assert_eq!(row.last_fired_at, None);
    assert_eq!(row.next_fire_at, job.next_fire_at);
    assert_eq!(store.continuation_retry(job.id)?, None);
    Ok(())
}

/// Design test 18: the retry state is daemon-owned. A row without it
/// decodes; an operator schedule edit replaces the spec and resets it; a
/// client-supplied key never reaches the row.
#[tokio::test]
async fn continuation_retry_state_is_daemon_owned_and_reset_by_schedule_edits() -> anyhow::Result<()>
{
    let (manager, dir) = rotation_manager();
    let job = resume_job(Uuid::new_v4(), dir.path())?;
    let store = manager.store.lock().await;
    store.insert_scheduled_job(&job)?;
    assert_eq!(store.continuation_retry(job.id)?, None);
    store.record_continuation_retry(
        job.id,
        CONTINUATION_TIP_CHANGED,
        None,
        chrono::Utc::now(),
        true,
    )?;
    assert_eq!(
        store
            .continuation_retry(job.id)?
            .map(|retry| retry.attempts),
        Some(1)
    );
    assert!(
        store.get_scheduled_job(&job.id)?.is_some(),
        "row still decodes"
    );
    let mut client_spec = serde_json::to_value(&job.schedule)?;
    client_spec["continuation_retry"] = serde_json::json!({
        "attempts": 0, "first_refused_at": "2026-01-01T00:00:00Z",
        "last_code": "client", "last_tip": null
    });
    let spec: rsi_common::types::ScheduleSpec = serde_json::from_value(client_spec)?;
    store.update_scheduled_job(
        &job.id,
        &crate::store::scheduled_jobs::ScheduledJobUpdate {
            name: None,
            message: None,
            schedule: Some(spec),
            enabled: None,
            next_fire_at: None,
        },
    )?;
    assert_eq!(store.continuation_retry(job.id)?, None);
    Ok(())
}

/// An Epic with a completed, resumable leaf child of `kind` per entry.
async fn epic_with_children(
    manager: &SessionManager,
    dir: &std::path::Path,
    kinds: &[SessionKind],
) -> anyhow::Result<(Uuid, Vec<Uuid>)> {
    let group = manager
        .create_container(rsi_common::rpc::CreateContainerParams {
            kind: SessionKind::Group,
            name: "Fence Group".into(),
            parent_id: None,
            project_id: None,
            tags: vec!["fence".into()],
            topology_id: None,
        })
        .await?
        .id;
    let epic = manager
        .create_container(rsi_common::rpc::CreateContainerParams {
            kind: SessionKind::Epic,
            name: "Fence Epic".into(),
            parent_id: Some(group),
            project_id: None,
            tags: vec!["fence".into()],
            topology_id: None,
        })
        .await?
        .id;
    let mut children = Vec::new();
    for kind in kinds {
        let mut row = test_session(Uuid::new_v4(), SessionStatus::Completed);
        row.session_kind = *kind;
        row.parent_id = Some(epic);
        row.working_dir = dir.to_path_buf();
        row.query = "Objective X".into();
        row.claude_session_id = Some(format!("provider-{}", row.id));
        {
            let mut store = manager.store.lock().await;
            store.insert_session(&row)?;
            store.publish_startup_ordinary(row.id)?;
        }
        children.push(row.id);
        manager
            .completed
            .write()
            .await
            .insert(row.id, CompletedSession::for_test(row));
    }
    Ok((epic, children))
}

async fn continue_request(
    manager: &SessionManager,
    target: Uuid,
    query: &str,
) -> anyhow::Result<rsi_common::agent_coordination::AgentContinueChildRequestV1> {
    let cursor = manager
        .store
        .lock()
        .await
        .agent_continuation_cursor(target)?;
    Ok(
        rsi_common::agent_coordination::AgentContinueChildRequestV1 {
            target_session_id: target,
            query: query.into(),
            expected_tip_session_id: cursor.tip_session_id,
            expected_event_sequence: cursor.event_sequence,
            expected_custody_generation: cursor.custody_generation,
            idempotency_key: None,
        },
    )
}

/// The typed `AgentContinueChild` envelope: (`code`, message).
fn continue_refusal(error: &crate::error::DaemonError) -> (String, String) {
    match error {
        crate::error::DaemonError::StructuredRpc { message, data, .. } => (
            data["code"].as_str().unwrap_or_default().to_string(),
            message.clone(),
        ),
        other => (String::new(), other.to_string()),
    }
}

/// Review round 2 `agent_continue_lead_authority_race`: lead L1 is
/// authorized to continue Epic worker W, then `SetEpicLead` replaces L1 with
/// L2 before the effect. The continuation is refused with the typed
/// `continuation_actor_authority_changed` under W's guard, W is not started
/// and keeps its history, and the new lead L2 continues W with the same cursor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_continue_by_a_lead_replaced_after_authorization_is_refused_typed()
-> anyhow::Result<()> {
    use crate::session::lifecycle::{
        ContinuationPauseSeam, install_continuation_seam_pause_for_test,
    };
    use crate::store::manager_actions::fence::CONTINUATION_ACTOR_AUTHORITY_CHANGED;
    let (manager, dir) = rotation_manager();
    let manager = Arc::new(manager);
    let (epic, children) = epic_with_children(
        &manager,
        dir.path(),
        &[SessionKind::Story, SessionKind::Story, SessionKind::Task],
    )
    .await?;
    let (first, replacement, worker) = (children[0], children[1], children[2]);
    manager.set_epic_lead(epic, Some(first)).await?;
    let worker_history = manager
        .store
        .lock()
        .await
        .load_events(worker)
        .unwrap_or_default()
        .len();
    let request = continue_request(&manager, worker, "continue the worker").await?;

    let (reached, resume) = install_continuation_seam_pause_for_test(
        ContinuationPauseSeam::AgentContinueAuthorized,
        worker,
    );
    let process = install_controller_candidate_test_process(worker);
    let continuation = {
        let manager = Arc::clone(&manager);
        let request = request.clone();
        tokio::spawn(async move { manager.agent_continue_child(first, request).await })
    };
    reached.await?;
    // No guard is held by the test or the paused continuation.
    manager.set_epic_lead(epic, Some(replacement)).await?;
    resume
        .send(())
        .map_err(|()| anyhow::anyhow!("continuation barrier dropped"))?;
    let refused = continuation
        .await?
        .err()
        .ok_or_else(|| anyhow::anyhow!("the replaced lead's continuation must be refused"))?;
    let (code, message) = continue_refusal(&refused);
    assert_eq!(code, "target_not_authorized", "{message}");
    assert!(
        message.contains(CONTINUATION_ACTOR_AUTHORITY_CHANGED),
        "{message}"
    );
    assert_eq!(
        process
            .productive_start_count
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(
        manager
            .store
            .lock()
            .await
            .load_events(worker)
            .unwrap_or_default()
            .len(),
        worker_history
    );
    assert_eq!(
        manager
            .get_session(epic)
            .await
            .and_then(|row| row.lead_session_id),
        Some(replacement)
    );

    // The current lead holds the authority: the same cursor continues W.
    let receipt = manager.agent_continue_child(replacement, request).await?;
    assert_eq!(receipt.continued_session_id, worker);
    assert_eq!(
        delivered_events(&manager, worker, "continue the worker").await,
        1
    );
    drop_controller_candidate_test_stream(worker);
    Ok(())
}

/// Review round 3 `agent_continue_lead_authority_race_remains`: the lead
/// change lands AFTER the guarded check has passed under guard(W) and before
/// the effect claim. `SetEpicLead` takes only guard(L1) and guard(L2), so it
/// commits; the effect claim's revalidation then refuses L1 with the typed
/// code. W is not started and keeps its history, and L2 continues W.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_continue_lead_replaced_after_guarded_check_is_refused_at_effect_claim()
-> anyhow::Result<()> {
    use crate::session::lifecycle::{
        ContinuationPauseSeam, install_continuation_seam_pause_for_test,
    };
    use crate::store::manager_actions::fence::CONTINUATION_ACTOR_AUTHORITY_CHANGED;
    let (manager, dir) = rotation_manager();
    let manager = Arc::new(manager);
    let (epic, children) = epic_with_children(
        &manager,
        dir.path(),
        &[SessionKind::Story, SessionKind::Story, SessionKind::Task],
    )
    .await?;
    let (first, replacement, worker) = (children[0], children[1], children[2]);
    manager.set_epic_lead(epic, Some(first)).await?;
    let worker_history = manager.store.lock().await.load_events(worker)?.len();
    let request = continue_request(&manager, worker, "continue the worker").await?;

    let (reached, resume) =
        install_continuation_seam_pause_for_test(ContinuationPauseSeam::AfterFenceCheck, worker);
    let process = install_controller_candidate_test_process(worker);
    let continuation = {
        let manager = Arc::clone(&manager);
        let request = request.clone();
        tokio::spawn(async move { manager.agent_continue_child(first, request).await })
    };
    reached.await?;
    // The continuation holds guard(W) and has passed its guarded check.
    manager.set_epic_lead(epic, Some(replacement)).await?;
    resume
        .send(())
        .map_err(|()| anyhow::anyhow!("continuation barrier dropped"))?;
    let refused = continuation
        .await?
        .err()
        .ok_or_else(|| anyhow::anyhow!("the replaced lead's continuation must be refused"))?;
    let (code, message) = continue_refusal(&refused);
    assert_eq!(code, "target_not_authorized", "{message}");
    assert!(
        message.contains(CONTINUATION_ACTOR_AUTHORITY_CHANGED),
        "{message}"
    );
    assert_eq!(
        process
            .productive_start_count
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(
        manager.store.lock().await.load_events(worker)?.len(),
        worker_history,
        "W keeps its exact history"
    );
    assert_eq!(
        manager
            .get_session(epic)
            .await
            .and_then(|row| row.lead_session_id),
        Some(replacement)
    );

    let receipt = manager.agent_continue_child(replacement, request).await?;
    assert_eq!(receipt.continued_session_id, worker);
    assert_eq!(
        delivered_events(&manager, worker, "continue the worker").await,
        1
    );
    assert_eq!(
        process
            .productive_start_count
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    drop_controller_candidate_test_stream(worker);
    Ok(())
}

/// Design test 9 (`AgentContinueChild`): a tip that moves after authorization
/// is refused with the typed `continuation_tip_changed`; a fresh cursor at the
/// published successor succeeds; and continuing that successor while it is
/// busy still interrupts and delivers (the verb's documented contract).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_continue_child_refuses_moved_tip_typed_then_continues_fresh_and_busy()
-> anyhow::Result<()> {
    use crate::session::lifecycle::{
        ContinuationPauseSeam, install_continuation_seam_pause_for_test,
    };
    let (manager, dir) = rotation_manager();
    let manager = Arc::new(manager);
    let (epic, children) = epic_with_children(
        &manager,
        dir.path(),
        &[SessionKind::Story, SessionKind::Task],
    )
    .await?;
    let (lead, worker) = (children[0], children[1]);
    manager.set_epic_lead(epic, Some(lead)).await?;
    let worker_history = manager.store.lock().await.load_events(worker)?.len();
    let stale = continue_request(&manager, worker, "stale continuation").await?;

    let (reached, resume) = install_continuation_seam_pause_for_test(
        ContinuationPauseSeam::AgentContinueAuthorized,
        worker,
    );
    let continuation = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move { manager.agent_continue_child(lead, stale).await })
    };
    reached.await?;
    let successor = Box::pin(rotate_task_successor_for_test(&manager, worker)).await?;
    resume
        .send(())
        .map_err(|()| anyhow::anyhow!("continuation barrier dropped"))?;
    let refused = continuation
        .await?
        .err()
        .ok_or_else(|| anyhow::anyhow!("a moved tip must be refused"))?;
    let (code, message) = continue_refusal(&refused);
    assert_eq!(code, "continuation_failed", "{message}");
    assert_eq!(
        message,
        format!("agent_continue_failed:{CONTINUATION_TIP_CHANGED}")
    );
    assert_eq!(
        manager.store.lock().await.load_events(worker)?.len(),
        worker_history,
        "the superseded worker keeps its exact history"
    );

    // Fresh cursor: the published successor is continued.
    wait_inactive(&manager, successor.id).await;
    let provider_session = format!("provider-{}", successor.id);
    manager.store.lock().await.conn.execute(
        "UPDATE sessions SET claude_session_id=?1 WHERE id=?2",
        rusqlite::params![provider_session, successor.id.to_string()],
    )?;
    if let Some(cached) = manager.completed.write().await.get_mut(&successor.id) {
        cached.session.claude_session_id = Some(provider_session);
    }
    let process = install_controller_candidate_test_process(successor.id);
    let fresh = continue_request(&manager, worker, "fresh continuation").await?;
    assert_eq!(fresh.expected_tip_session_id, successor.id);
    let receipt = manager.agent_continue_child(lead, fresh).await?;
    assert_eq!(
        (receipt.target_session_id, receipt.continued_session_id),
        (worker, successor.id)
    );
    assert_eq!(
        delivered_events(&manager, successor.id, "fresh continuation").await,
        1
    );

    // Busy: the successor is still running; the verb interrupts and delivers.
    assert!(manager.active.read().await.contains_key(&successor.id));
    let busy = continue_request(&manager, worker, "busy continuation").await?;
    let receipt = manager.agent_continue_child(lead, busy).await?;
    assert_eq!(receipt.continued_session_id, successor.id);
    assert_eq!(
        delivered_events(&manager, successor.id, "busy continuation").await,
        1
    );
    assert!(
        process
            .productive_start_count
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 1
    );
    drop_controller_candidate_test_stream(successor.id);
    Ok(())
}
