//! Issue #588: every terminal transition (Completed, Failed, Interrupted)
//! records one normalized, static cause for every provider, written with the
//! status change by the real finalizer.

use super::*;
use crate::store::daemon_settings::AutofileCause;
use crate::terminal_cause::InterruptSource;
use std::sync::Arc;

const PROVIDERS: [SessionProvider; 9] = [
    SessionProvider::Claude,
    SessionProvider::Codex,
    SessionProvider::Pioneer,
    SessionProvider::OpenRouter,
    SessionProvider::Bedrock,
    SessionProvider::Local,
    SessionProvider::Antigravity,
    SessionProvider::CodexAppServer,
    SessionProvider::Harness,
];

/// Run the real finalizer for one tracked session and read the durable row.
async fn finalize_row(
    manager: &SessionManager,
    provider: SessionProvider,
    tweak: impl FnOnce(&mut TrackedSession),
    decision: TerminalFinalizeDecision,
) -> Session {
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.status = SessionStatus::Running;
    session.provider = provider;
    session.stop_reason = None;
    insert_row(manager, &session).await;
    let mut tracked = TrackedSession::new_for_test(session);
    tweak(&mut tracked);
    manager.active.write().await.insert(session_id, tracked);
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
    manager
        .store
        .lock()
        .await
        .get_session(session_id)
        .expect("read row")
        .expect("row exists")
}

async fn assert_no_terminal_row_lacks_a_cause(manager: &SessionManager) {
    let missing: i64 = manager
        .store
        .lock()
        .await
        .conn
        .query_row(
            "SELECT count(*) FROM sessions
             WHERE status IN ('Completed','Failed','Interrupted')
               AND (stop_reason IS NULL OR TRIM(stop_reason) = '')",
            [],
            |row| row.get(0),
        )
        .expect("count terminal rows without a cause");
    assert_eq!(missing, 0, "a terminal transition wrote no cause");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn completed_records_a_cause_for_every_provider() {
    let (manager, _dir) = manager();
    for provider in PROVIDERS {
        let row = finalize_row(
            &manager,
            provider,
            |_| {},
            TerminalFinalizeDecision::completed(),
        )
        .await;
        assert_eq!(row.status, SessionStatus::Completed, "{provider:?}");
        assert_eq!(
            row.stop_reason.as_deref(),
            Some("completed"),
            "{provider:?}"
        );

        // A provider that reports its own stop reason keeps it.
        let row = finalize_row(
            &manager,
            provider,
            |tracked| tracked.session.stop_reason = Some("end_turn".into()),
            TerminalFinalizeDecision::completed(),
        )
        .await;
        assert_eq!(row.stop_reason.as_deref(), Some("end_turn"), "{provider:?}");
    }
    assert_no_terminal_row_lacks_a_cause(&manager).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn completed_by_archive_request_names_the_archive() {
    let (manager, _dir) = manager();
    let row = finalize_row(
        &manager,
        SessionProvider::Codex,
        |tracked| tracked.pending_archive = true,
        TerminalFinalizeDecision::completed(),
    )
    .await;
    assert_eq!(row.stop_reason.as_deref(), Some("completed:archive"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn failed_keeps_its_typed_reason_for_every_provider() {
    let (manager, _dir) = manager();
    for provider in PROVIDERS {
        let row = finalize_row(
            &manager,
            provider,
            |_| {},
            TerminalFinalizeDecision::failed(AutofileCause::NonZeroExit),
        )
        .await;
        assert_eq!(row.status, SessionStatus::Failed, "{provider:?}");
        assert_eq!(
            row.stop_reason.as_deref(),
            Some("terminal_failure:non-zero-exit"),
            "{provider:?}"
        );

        // The #572 usage-limit hold keeps its provider-typed reason.
        let row = finalize_row(
            &manager,
            provider,
            |tracked| {
                tracked.session.stop_reason = Some("provider_error:codex_usage_limit".into());
            },
            TerminalFinalizeDecision::failed(AutofileCause::OtherTerminalFailure),
        )
        .await;
        assert_eq!(
            row.stop_reason.as_deref(),
            Some("provider_error:codex_usage_limit"),
            "{provider:?}"
        );
    }
    assert_no_terminal_row_lacks_a_cause(&manager).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn interrupted_records_its_source_for_every_provider() {
    let (manager, _dir) = manager();
    for provider in PROVIDERS {
        for source in InterruptSource::ALL {
            let row = finalize_row(
                &manager,
                provider,
                |tracked| {
                    tracked.interrupt_requested = true;
                    tracked.interrupt_source = Some(source);
                },
                TerminalFinalizeDecision::interrupted(),
            )
            .await;
            assert_eq!(row.status, SessionStatus::Interrupted, "{provider:?}");
            assert_eq!(
                row.stop_reason.as_deref(),
                Some(source.cause()),
                "{provider:?}/{source:?}"
            );
        }
    }
    assert_no_terminal_row_lacks_a_cause(&manager).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn interrupt_entry_points_record_their_source_through_the_finalizer() {
    let (manager, _dir) = manager();
    for source in InterruptSource::ALL {
        let session_id = Uuid::new_v4();
        let mut session = bare_session(session_id);
        session.status = SessionStatus::Running;
        session.stop_reason = None;
        insert_row(&manager, &session).await;
        manager
            .active
            .write()
            .await
            .insert(session_id, TrackedSession::new_for_test(session));

        manager
            .interrupt_session_from(session_id, source)
            .await
            .expect("interrupt reaches the active turn");
        // A later interrupt from another source does not rewrite the cause.
        manager
            .interrupt_session_from(session_id, InterruptSource::Operator)
            .await
            .expect("second interrupt");

        SessionManager::finalize_session(
            session_id,
            0,
            TerminalFinalizeDecision::completed(),
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
        .expect("finalizer settled the interrupted turn");
        let row = manager
            .store
            .lock()
            .await
            .get_session(session_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.status, SessionStatus::Interrupted, "{source:?}");
        assert_eq!(
            row.stop_reason.as_deref(),
            Some(source.cause()),
            "{source:?}"
        );
    }
    assert_no_terminal_row_lacks_a_cause(&manager).await;
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn interrupted_without_a_recorded_source_is_unattributed() {
    let (manager, _dir) = manager();
    let row = finalize_row(
        &manager,
        SessionProvider::OpenRouter,
        |_| {},
        TerminalFinalizeDecision::interrupted(),
    )
    .await;
    assert_eq!(row.stop_reason.as_deref(), Some("interrupted:unattributed"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn interrupted_usage_limit_hold_keeps_its_provider_reason() {
    let (manager, _dir) = manager();
    let row = finalize_row(
        &manager,
        SessionProvider::Codex,
        |tracked| {
            tracked.interrupt_requested = true;
            tracked.interrupt_source = Some(InterruptSource::Operator);
            tracked.session.stop_reason = Some("provider_error:codex_usage_limit".into());
        },
        TerminalFinalizeDecision::interrupted(),
    )
    .await;
    assert_eq!(
        row.stop_reason.as_deref(),
        Some("provider_error:codex_usage_limit")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn store_terminal_writers_refuse_an_empty_cause_and_default_the_rest() {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.status = SessionStatus::Running;
    session.stop_reason = None;
    insert_row(&manager, &session).await;
    let store = manager.store.lock().await;

    for blank in ["", "   "] {
        assert!(
            store
                .set_session_terminal_status(session_id, SessionStatus::Interrupted, blank)
                .is_err(),
            "empty cause must be refused"
        );
    }
    assert!(
        store
            .set_session_terminal_status(session_id, SessionStatus::Running, "x")
            .is_err(),
        "a non-terminal status is not a terminal write"
    );
    assert_eq!(
        store.get_session(session_id).unwrap().unwrap().status,
        SessionStatus::Running,
        "a refused write changes nothing"
    );

    // A legacy writer that has no richer evidence still records the default.
    store
        .update_session_status(session_id, SessionStatus::Completed)
        .unwrap();
    let row = store.get_session(session_id).unwrap().unwrap();
    assert_eq!(row.status, SessionStatus::Completed);
    assert_eq!(row.stop_reason.as_deref(), Some("completed"));

    // ...and never overwrites a cause the row already carries.
    store
        .update_session_status(session_id, SessionStatus::Interrupted)
        .unwrap();
    let row = store.get_session(session_id).unwrap().unwrap();
    assert_eq!(row.status, SessionStatus::Interrupted);
    assert_eq!(row.stop_reason.as_deref(), Some("completed"));

    store
        .set_session_terminal_status(
            session_id,
            SessionStatus::Interrupted,
            "interrupted:operator",
        )
        .unwrap();
    let row = store.get_session(session_id).unwrap().unwrap();
    assert_eq!(row.stop_reason.as_deref(), Some("interrupted:operator"));
}
