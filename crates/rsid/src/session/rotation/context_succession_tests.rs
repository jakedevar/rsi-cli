//! #959: an idle lead that crossed its context threshold gets exactly one
//! `context-succession` daemon message through a fenced continuation.
#![allow(
    clippy::expect_used,
    clippy::significant_drop_tightening,
    clippy::large_futures
)]

use super::super::context_succession::{CoordinatorRole, record_succession_due};
use super::super::launch::{
    drop_controller_candidate_test_stream, install_controller_candidate_test_process,
};
use super::tests::{rotation_manager, test_session};
use super::*;
use rsi_common::types::SessionKind;
use std::time::Duration;

const ENVELOPE: &str = "source=\"context-succession\"";

async fn envelopes(manager: &SessionManager, session: Uuid) -> usize {
    manager
        .store
        .lock()
        .await
        .load_events(session)
        .unwrap_or_default()
        .iter()
        .filter(|event| event.content.contains(ENVELOPE))
        .count()
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-04"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_lead_gets_one_context_succession_message() -> anyhow::Result<()> {
    let (manager, dir) = rotation_manager();
    manager
        .runtime_config
        .context_rotation_enabled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let mut lead = test_session(Uuid::new_v4(), SessionStatus::Completed);
    lead.working_dir = dir.path().to_path_buf();
    lead.query = "Lead the Epic".into();
    let mut epic = test_session(Uuid::new_v4(), SessionStatus::Completed);
    epic.working_dir = dir.path().to_path_buf();
    epic.session_kind = SessionKind::Epic;
    epic.lead_session_id = Some(lead.id);
    {
        let mut store = manager.store.lock().await;
        store.insert_session(&lead)?;
        store.insert_session(&epic)?;
        store.publish_startup_ordinary(lead.id)?;
        record_succession_due(
            &store,
            lead.id,
            CoordinatorRole::Lead { epic_id: epic.id },
            70.0,
            65.0,
            chrono::Utc::now(),
        )?;
    }
    manager
        .completed
        .write()
        .await
        .insert(lead.id, CompletedSession::for_test(lead.clone()));
    let _process = install_controller_candidate_test_process(lead.id);

    assert_eq!(manager.deliver_due_context_successions().await?, 1);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while envelopes(&manager, lead.id).await == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(envelopes(&manager, lead.id).await, 1);
    // The next pass, busy or idle within the resend delay, sends nothing more.
    assert_eq!(manager.deliver_due_context_successions().await?, 0);
    assert_eq!(envelopes(&manager, lead.id).await, 1);
    drop_controller_candidate_test_stream(lead.id);
    Ok(())
}
