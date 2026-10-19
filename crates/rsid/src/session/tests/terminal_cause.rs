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

/// The cause persisted for a source with no extra evidence: an unattributed
/// interrupt also names the writer (#2514).
fn expected_cause(source: InterruptSource) -> String {
    match source {
        InterruptSource::Unattributed => format!("{}:finalize_session", source.cause()),
        _ => source.cause().to_string(),
    }
}

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
                Some(expected_cause(source).as_str()),
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
            Some(expected_cause(source).as_str()),
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
    // An unattributed row is a defect signal: it names the writer (#2514).
    assert_eq!(
        row.stop_reason.as_deref(),
        Some("interrupted:unattributed:finalize_session")
    );
}

/// #2514: the provider-exit path with a signalled child. Reconciliation
/// records the provider exit as the source, the monitor records the child's
/// terminating signal, and the real finalizer persists both.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn provider_exit_with_a_signalled_child_records_the_signal() {
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize};
    let (manager, _dir) = manager();
    let row = finalize_row(
        &manager,
        SessionProvider::Claude,
        |tracked| {
            // Scripted exit code -15 stands for death by SIGTERM.
            tracked.process = Some(crate::session::types::ProviderProcess::Scripted(
                crate::session::types::ScriptedProcess {
                    alive: Arc::new(AtomicBool::new(false)),
                    exit_code: Arc::new(AtomicI32::new(-15)),
                    interrupt_count: Arc::new(AtomicUsize::new(0)),
                    kill_count: Arc::new(AtomicUsize::new(0)),
                    exit_on_interrupt: false,
                    interrupt_fails: false,
                    kill_fails: false,
                },
            ));
            tracked.record_provider_exit();
            tracked
                .interrupt_source
                .get_or_insert(InterruptSource::ProviderExit);
        },
        TerminalFinalizeDecision::interrupted(),
    )
    .await;
    assert_eq!(row.status, SessionStatus::Interrupted);
    assert_eq!(
        row.stop_reason.as_deref(),
        Some("interrupted:provider_exit:signal=SIGTERM")
    );
    assert_ne!(row.stop_reason.as_deref(), Some("interrupted:unattributed"));
}

/// #2514: a stop with no recorded source still keeps the child's exit
/// evidence and the stopping writer in the cause.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn unattributed_stop_keeps_writer_and_exit_evidence() {
    let (manager, _dir) = manager();
    let row = finalize_row(
        &manager,
        SessionProvider::Claude,
        |tracked| {
            tracked.interrupt_evidence = crate::terminal_cause::InterruptEvidence {
                exit: Some(crate::terminal_cause::ProviderExit::Signal(9)),
                writer: Some("monitor_stop_signal"),
            };
        },
        TerminalFinalizeDecision::interrupted(),
    )
    .await;
    assert_eq!(
        row.stop_reason.as_deref(),
        Some("interrupted:unattributed:monitor_stop_signal:signal=SIGKILL")
    );
}

/// #2514: reconciliation finding a dead provider records the provider exit as
/// the interrupt source and leaves `interrupt_requested` untouched.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn reconciliation_process_death_records_provider_exit_source() {
    let (manager, _dir) = manager();
    let session_id = Uuid::new_v4();
    let mut session = bare_session(session_id);
    session.status = SessionStatus::Running;
    manager
        .active
        .write()
        .await
        .insert(session_id, TrackedSession::new_for_test(session));
    crate::reconciliation::signal_finalization(&manager.active, session_id, &manager.event_bus)
        .await;
    let active = manager.active.read().await;
    let tracked = &active[&session_id];
    assert_eq!(
        tracked.interrupt_source,
        Some(InterruptSource::ProviderExit)
    );
    assert!(!tracked.interrupt_requested);
}

/// Hub #3029: a finished turn whose provider exited 0 is settled Completed even
/// though the reconciler recorded the provider exit as the stop source; the
/// stored cause is the plain completed cause, not `interrupted:provider_exit`.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn clean_provider_exit_after_result_records_completed_not_provider_exit() {
    let (manager, _dir) = manager();
    let row = finalize_row(
        &manager,
        SessionProvider::Claude,
        |tracked| {
            tracked.interrupt_source = Some(InterruptSource::ProviderExit);
            tracked.interrupt_evidence.exit = Some(crate::terminal_cause::ProviderExit::Code(0));
        },
        TerminalFinalizeDecision::completed(),
    )
    .await;
    assert_eq!(row.status, SessionStatus::Completed);
    assert_eq!(row.stop_reason.as_deref(), Some("completed"));
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

/// Evidence of a Codex child that emitted events, then went silent: no result
/// event, a dead provider and a drained stream (#68).
fn silent_after_events_evidence() -> crate::session::types::TerminalEvidence {
    use crate::session::types::{
        MonitorBreakReason, ProcessSettlementOutcome, TerminalEvidence, TerminalResult,
        TerminalRotationAction, TerminalTurnOutcome,
    };
    TerminalEvidence {
        expected_generation: 0,
        active_generation: Some(0),
        break_reason: MonitorBreakReason::StreamClosed,
        received_any_event: true,
        received_meaningful_output: true,
        prior_meaningful_output: false,
        current_result: TerminalResult::None,
        process_handle_present: true,
        process_alive: false,
        exit_code: Some(1),
        stream_drained: true,
        settlement: ProcessSettlementOutcome::AlreadyExited,
        supports_multi_turn: false,
        turn_outcome: TerminalTurnOutcome::NotMultiTurn,
        pending_archive: false,
        stall_interrupted: false,
        interrupt_requested: false,
        pending_question: false,
        provider_exit_stop: false,
        rotation_action: TerminalRotationAction::None,
    }
}

/// Settle `evidence` through the real terminal decision and the real
/// finalizer for a Codex child that had already accounted usage.
async fn settle_silent_codex_child(
    manager: &SessionManager,
    evidence: crate::session::types::TerminalEvidence,
) -> (Session, TerminalFinalizeDecision) {
    let decision = match crate::session::monitor::terminal_decision(evidence) {
        crate::session::types::TerminalDecision::Finalize(decision) => decision,
        other => panic!("a dead, drained Codex child must finalize, got {other:?}"),
    };
    let row = finalize_row(
        manager,
        SessionProvider::Codex,
        |tracked| {
            tracked.session.input_tokens = Some(1200);
            tracked.session.output_tokens = Some(340);
            tracked.session.total_input_tokens = Some(1200);
            tracked.session.total_output_tokens = Some(340);
            tracked.session.num_turns = Some(1);
        },
        decision,
    )
    .await;
    (row, decision)
}

/// #68: a Codex child that was stall-interrupted after its last event settles
/// `Failed` with a typed stall cause and keeps the usage it had accounted.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn silent_codex_child_stall_interrupt_settles_failed_with_typed_cause_and_usage() {
    let (manager, _dir) = manager();
    let mut evidence = silent_after_events_evidence();
    evidence.break_reason = crate::session::types::MonitorBreakReason::StallTimeout;
    evidence.stall_interrupted = true;
    evidence.exit_code = Some(-15);
    let (row, decision) = settle_silent_codex_child(&manager, evidence).await;
    assert_eq!(
        decision,
        TerminalFinalizeDecision::failed(AutofileCause::StallTimeout)
    );
    assert_eq!(row.status, SessionStatus::Failed);
    assert_eq!(
        row.stop_reason.as_deref(),
        Some("terminal_failure:stall-timeout")
    );
    assert_eq!(row.input_tokens, Some(1200));
    assert_eq!(row.output_tokens, Some(340));
    assert_eq!(row.total_input_tokens, Some(1200));
    assert_eq!(row.total_output_tokens, Some(340));
    assert_no_terminal_row_lacks_a_cause(&manager).await;
}

/// #68: a Codex child whose process died non-zero with no result event after
/// meaningful output (a stderr or transport failure) settles `Failed` with the
/// typed exit cause, never `Completed` and never an empty reason.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn silent_codex_child_process_exit_settles_failed_with_typed_cause_and_usage() {
    let (manager, _dir) = manager();
    let (row, decision) = settle_silent_codex_child(&manager, silent_after_events_evidence()).await;
    assert_eq!(
        decision,
        TerminalFinalizeDecision::failed(AutofileCause::NonZeroExit)
    );
    assert_eq!(row.status, SessionStatus::Failed);
    assert_eq!(
        row.stop_reason.as_deref(),
        Some("terminal_failure:non-zero-exit")
    );
    assert_eq!(row.input_tokens, Some(1200));
    assert_eq!(row.output_tokens, Some(340));
    assert_no_terminal_row_lacks_a_cause(&manager).await;
}

/// #68: a provider that is still alive is never settled by the monitor's
/// terminal decision: the stall detector, not process exit, ends that child.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[test]
fn silent_codex_child_with_a_live_provider_is_retained_for_the_stall_detector() {
    use crate::session::types::{TerminalDecision, TerminalRetentionReason};
    let mut evidence = silent_after_events_evidence();
    evidence.process_alive = true;
    evidence.exit_code = None;
    let decision = crate::session::monitor::terminal_decision(evidence);
    assert!(
        matches!(
            decision,
            TerminalDecision::RetainRunning(TerminalRetentionReason::ProviderStillAlive)
        ),
        "{decision:?}"
    );
}
