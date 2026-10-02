//! #568/#575 R1: a DB-native review that ends without a receipt records WHY.
#![allow(clippy::unwrap_used, clippy::significant_drop_tightening)]

use super::*;
use rsi_common::agent_coordination::AgentContinueChildRequestV1;

/// Bind the fixture invocation to the review's allocation action exactly as a
/// daemon-launched reviewer invocation is bound.
pub(super) fn bind_review_invocation(store: &Store, f: &Fixture, assignment_id: Uuid) {
    store
        .conn
        .execute(
            "UPDATE model_invocations
                SET dedup_key='manager.action:'||(SELECT action_operation_id
                      FROM manager_review_assignments WHERE assignment_id=?2),
                    project_id=(SELECT project_id FROM sessions WHERE id=?3),
                    purpose='session.launch.fresh'
              WHERE id=?1",
            params![
                f.invocation.to_string(),
                assignment_id.to_string(),
                f.reviewer.to_string()
            ],
        )
        .unwrap();
}

/// Bind the fixture invocation to the review's allocation action exactly as a
/// daemon-launched reviewer would be bound, then drive it to a terminal state.
async fn end_bound_review(
    f: &Fixture,
    key: &str,
    session_status: SessionStatus,
    invocation_status: &str,
    error_class: Option<&str>,
    bind_invocation: bool,
    tool_denials: Option<i64>,
) -> (Uuid, bool, String, Option<String>) {
    let assignment_id = request_and_activate_db_review(f, key).await;
    let store = f.handle.store.lock().await;
    if bind_invocation {
        bind_review_invocation(&store, f, assignment_id);
    }
    store
        .update_session_status(f.reviewer, session_status)
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE sessions SET permission_denial_count=?2 WHERE id=?1",
            params![f.reviewer.to_string(), tool_denials],
        )
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE model_invocations SET status=?2,error_class=?3,completed_at=?4 WHERE id=?1",
            params![
                f.invocation.to_string(),
                invocation_status,
                error_class,
                Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            ],
        )
        .unwrap();
    let changed = store
        .refresh_manager_review_assignment(assignment_id)
        .unwrap();
    let (state, code): (String, Option<String>) = store
        .conn
        .query_row(
            "SELECT state,failure_code FROM manager_review_assignments WHERE assignment_id=?1",
            [assignment_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    (assignment_id, changed, state, code)
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn review_normal_final_without_receipt_records_final_without_receipt() {
    let f = fixture().await;
    let (_, changed, state, code) = end_bound_review(
        &f,
        "end-final",
        SessionStatus::Completed,
        "completed",
        None,
        true,
        None,
    )
    .await;
    assert!(changed);
    assert_eq!(state, "failed");
    assert_eq!(
        code.as_deref(),
        Some("manager_review_receipt_missing_final_without_receipt")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn review_restart_interruption_reserves_infra_successor() {
    let f = fixture().await;
    let (_, changed, state, code) = end_bound_review(
        &f,
        "end-restart",
        SessionStatus::Interrupted,
        "failed",
        Some("interrupted"),
        true,
        None,
    )
    .await;
    assert!(changed);
    assert_eq!(state, "superseded");
    assert_eq!(code, None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn review_provider_failure_reserves_infra_successor() {
    let f = fixture().await;
    let (_, changed, state, code) = end_bound_review(
        &f,
        "end-provider",
        SessionStatus::Failed,
        "failed",
        Some("failed"),
        true,
        None,
    )
    .await;
    assert!(changed);
    assert_eq!(state, "superseded");
    assert_eq!(code, None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn review_end_under_unbound_invocation_cannot_settle_assignment() {
    // A terminal invocation not bound to this assignment's allocation action is
    // stale authority: it neither fails nor completes the review.
    let f = fixture().await;
    let (_, changed, state, code) = end_bound_review(
        &f,
        "end-stale",
        SessionStatus::Completed,
        "completed",
        None,
        false,
        None,
    )
    .await;
    assert!(!changed);
    assert_eq!(state, "active");
    assert_eq!(code, None);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn review_with_submitted_receipt_keeps_success_path_at_terminal() {
    let f = fixture().await;
    let assignment_id = request_and_activate_db_review(&f, "end-receipt").await;
    let receipt = f
        .handle
        .agent_submit_review_receipt(
            f.reviewer,
            AgentSubmitReviewReceiptRequestV1 {
                assignment_id,
                verdict: ManagerReviewVerdictV1::Blocked,
                findings: vec![],
                idempotency_key: "end-receipt".into(),
            },
        )
        .await
        .unwrap();
    let store = f.handle.store.lock().await;
    store
        .update_session_status(f.reviewer, SessionStatus::Completed)
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE model_invocations SET status='completed',completed_at=?2 WHERE id=?1",
            params![
                f.invocation.to_string(),
                Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            ],
        )
        .unwrap();
    assert!(
        !store
            .refresh_manager_review_assignment(assignment_id)
            .unwrap()
    );
    let (state, verdict): (String, String) = store
        .conn
        .query_row(
            "SELECT a.state,r.verdict FROM manager_review_assignments a
               JOIN manager_review_receipts r ON r.assignment_id=a.assignment_id
              WHERE a.assignment_id=?1 AND r.receipt_id=?2",
            params![assignment_id.to_string(), receipt.receipt_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((state.as_str(), verdict.as_str()), ("submitted", "blocked"));
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn review_final_with_tool_denials_falls_back_to_missing_receipt() {
    let f = fixture().await;
    let (_, changed, state, code) = end_bound_review(
        &f,
        "end-denials",
        SessionStatus::Completed,
        "completed",
        None,
        true,
        Some(2),
    )
    .await;
    assert!(changed);
    assert_eq!(state, "failed");
    assert_eq!(
        code.as_deref(),
        Some("manager_review_receipt_missing_final_without_receipt")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn review_over_explicit_budget_records_budget_exceeded_end_reason() {
    let f = fixture().await;
    let (_, changed, state, code) = end_bound_review(
        &f,
        "end-budget",
        SessionStatus::Completed,
        "failed",
        Some("over_budget_actual_exceeded"),
        true,
        None,
    )
    .await;
    assert!(changed);
    assert_eq!(state, "failed");
    assert_eq!(
        code.as_deref(),
        Some("manager_review_receipt_missing_budget_exceeded")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn archived_reviewer_without_receipt_fails_once_with_typed_disposition() {
    let f = fixture().await;
    let assignment = request_and_activate_db_review(&f, "archived-reviewer").await;
    let store = f.handle.store.lock().await;
    store
        .update_session_status(f.reviewer, SessionStatus::Archived)
        .unwrap();
    assert!(store.refresh_manager_review_assignment(assignment).unwrap());
    let (state, code): (String, String) = store
        .conn
        .query_row(
            "SELECT state,failure_code FROM manager_review_assignments WHERE assignment_id=?1",
            [assignment.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "failed");
    assert_eq!(code, "manager_review_reviewer_archived");
    assert!(!store.refresh_manager_review_assignment(assignment).unwrap());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn lead_and_operator_continue_preserve_active_reviewer_turn() {
    let f = fixture().await;
    let assignment = request_and_activate_db_review(&f, "lead-continue-refusal").await;
    let cursor = f
        .handle
        .store
        .lock()
        .await
        .agent_continuation_cursor(f.reviewer)
        .unwrap();
    let request = AgentContinueChildRequestV1 {
        target_session_id: f.reviewer,
        query: "replace review verdict".into(),
        expected_tip_session_id: cursor.tip_session_id,
        expected_event_sequence: cursor.event_sequence,
        expected_custody_generation: cursor.custody_generation,
        idempotency_key: None,
    };
    let error = f
        .sessions
        .agent_continue_child(f.source, request)
        .await
        .unwrap_err();
    let crate::error::DaemonError::StructuredRpc { message, .. } = error else {
        panic!("review continuation refusal must be structured");
    };
    assert_eq!(
        message,
        "agent_continue_failed:manager_review_reviewer_continuation_owned"
    );
    let operator_error = f
        .sessions
        .continue_session_operator(f.reviewer, "replace review verdict".into())
        .await
        .unwrap_err();
    assert!(matches!(
        operator_error,
        crate::error::DaemonError::PolicyDenied(ref reason)
            if reason == "manager_review_reviewer_continuation_owned"
    ));
    let store = f.handle.store.lock().await;
    let (state, invocation): (String, String) = store
        .conn
        .query_row(
            "SELECT state,reviewer_invocation_id FROM manager_review_assignments
              WHERE assignment_id=?1",
            [assignment.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "active");
    assert_eq!(invocation, f.invocation.to_string());
    assert_eq!(
        store.get_session(f.reviewer).unwrap().unwrap().status,
        SessionStatus::Running
    );
}

/// Exercise the installed V130 journal and review forward trigger together.
fn stage_restart_review_successor(
    store: &Store,
    f: &Fixture,
    assignment: Uuid,
    journal_invocation: Uuid,
    journal_generation: i64,
) -> Uuid {
    bind_review_invocation(store, f, assignment);
    let custody = store.live_custody_for_session(f.reviewer).unwrap();
    let next = Uuid::new_v4();
    let intent = Uuid::new_v4();
    let stamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    store
        .conn
        .execute(
            "UPDATE model_invocations SET status='failed',error_class='restart_reconciled_interrupted',
                    completed_at=?2 WHERE id=?1",
            params![f.invocation.to_string(), stamp],
        )
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO model_invocations(
                id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                provider,model,trigger_source,session_id,project_id,parent_invocation_id,dedup_key,
                policy_snapshot_json,usage_confidence,created_at)
             VALUES(?1,'session.continue.resume','session_lifecycle','foreground',
                'paid_capable','admitted','running','Claude','test',
                'daemon_restart',?2,?3,?4,?5,'{}','unavailable',?6)",
            params![
                next.to_string(),
                f.reviewer.to_string(),
                store
                    .get_session(f.reviewer)
                    .unwrap()
                    .unwrap()
                    .project_id
                    .unwrap()
                    .to_string(),
                f.invocation.to_string(),
                format!("daemon.restart:{intent}"),
                stamp
            ],
        )
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE sessions SET model_invocation_id=?2 WHERE id=?1",
            params![f.reviewer.to_string(), next.to_string()],
        )
        .unwrap();
    store
        .conn
        .execute(
            "INSERT INTO daemon_restart_intents
                (id,session_id,invocation_id,custody_id,custody_generation,
                 boot_id,continuation_invocation_id,state,outcome,created_at,updated_at,delivered_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,'delivered','restart_reconciled_interrupted',?8,?8,?8)",
            params![
                intent.to_string(),
                f.reviewer.to_string(),
                journal_invocation.to_string(),
                custody.custody_id.to_string(),
                journal_generation,
                Uuid::new_v4().to_string(),
                next.to_string(),
                stamp,
            ],
        )
        .unwrap();
    next
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn active_review_claims_exact_restart_owner_before_generic_recovery() {
    let f = fixture().await;
    let assignment = request_and_activate_db_review(&f, "review-restart-owner").await;
    let store = f.handle.store.lock().await;
    bind_review_invocation(&store, &f, assignment);
    let boot = Uuid::new_v4();
    assert!(store.record_restart_intent(f.reviewer, boot).unwrap());
    store.mark_restart_interrupt_sent(f.reviewer, boot).unwrap();
    assert_eq!(store.prepare_restart_intents_for_restore().unwrap(), 1);
    store
        .conn
        .execute(
            "UPDATE model_invocations SET status='failed',error_class='restart_reconciled_interrupted' WHERE id=?1",
            [f.invocation.to_string()],
        )
        .unwrap();
    assert!(store.next_restart_intent(None).unwrap().is_none());
    let intent = store.next_review_restart_intent(None).unwrap().unwrap();
    assert_eq!(
        (intent.session_id, intent.invocation_id),
        (f.reviewer, f.invocation)
    );
    assert!(
        store
            .claim_review_restart_intent(&intent, Uuid::new_v4())
            .unwrap()
    );
    assert!(store.next_review_restart_intent(None).unwrap().is_some());
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn exact_restart_origin_rebinds_active_review_and_accepts_reviewer_receipt() {
    let f = fixture().await;
    let assignment = request_and_activate_db_review(&f, "restart-rebind").await;
    let store = f.handle.store.lock().await;
    let generation = store
        .live_custody_for_session(f.reviewer)
        .unwrap()
        .generation as i64;
    let next = stage_restart_review_successor(&store, &f, assignment, f.invocation, generation);
    assert!(store.refresh_manager_review_assignment(assignment).unwrap());
    let bound: String = store
        .conn
        .query_row(
            "SELECT reviewer_invocation_id FROM manager_review_assignments WHERE assignment_id=?1",
            [assignment.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(bound, next.to_string());
    drop(store);
    let receipt = f
        .handle
        .agent_submit_review_receipt(
            f.reviewer,
            AgentSubmitReviewReceiptRequestV1 {
                assignment_id: assignment,
                verdict: ManagerReviewVerdictV1::Blocked,
                findings: vec![],
                idempotency_key: "restart-rebound-receipt".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(receipt.assignment_id, assignment);
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn rebound_reviewer_final_without_receipt_settles_typed() {
    let f = fixture().await;
    let assignment = request_and_activate_db_review(&f, "rebound-final").await;
    let store = f.handle.store.lock().await;
    let generation = store
        .live_custody_for_session(f.reviewer)
        .unwrap()
        .generation as i64;
    let next = stage_restart_review_successor(&store, &f, assignment, f.invocation, generation);
    assert!(store.refresh_manager_review_assignment(assignment).unwrap());
    store
        .update_session_status(f.reviewer, SessionStatus::Completed)
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE model_invocations SET status='completed',completed_at=?2 WHERE id=?1",
            params![
                next.to_string(),
                Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
            ],
        )
        .unwrap();
    assert!(store.refresh_manager_review_assignment(assignment).unwrap());
    let (state, code): (String, String) = store
        .conn
        .query_row(
            "SELECT state,failure_code FROM manager_review_assignments WHERE assignment_id=?1",
            [assignment.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "failed");
    assert_eq!(code, "manager_review_receipt_missing_final_without_receipt");
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn forged_restart_intent_cannot_rebind_review() {
    for wrong in ["invocation", "generation", "completed", "parent", "outcome"] {
        let f = fixture().await;
        let assignment = request_and_activate_db_review(&f, wrong).await;
        let store = f.handle.store.lock().await;
        let generation = store
            .live_custody_for_session(f.reviewer)
            .unwrap()
            .generation as i64;
        let old = if wrong == "invocation" {
            Uuid::new_v4()
        } else {
            f.invocation
        };
        if wrong == "invocation" {
            store
                .conn
                .execute(
                    "INSERT INTO model_invocations
                     (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,
                      trigger_source,session_id,created_at)
                     VALUES(?1,'session.launch','session','foreground','paid','admitted','failed',
                            'launch_session',?2,?3)",
                    params![
                        old.to_string(),
                        f.reviewer.to_string(),
                        Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                    ],
                )
                .unwrap();
        }
        let next = stage_restart_review_successor(
            &store,
            &f,
            assignment,
            old,
            generation + i64::from(wrong == "generation"),
        );
        if wrong == "completed" {
            store
                .conn
                .execute(
                    "UPDATE model_invocations SET status='completed',error_class=NULL
                      WHERE id=?1",
                    [f.invocation.to_string()],
                )
                .unwrap();
        }
        if wrong == "parent" {
            store
                .conn
                .execute(
                    "UPDATE model_invocations SET parent_invocation_id=NULL WHERE id=?1",
                    [next.to_string()],
                )
                .unwrap();
        }
        if wrong == "outcome" {
            store
                .conn
                .execute(
                    "UPDATE daemon_restart_intents SET outcome='not_needed' WHERE continuation_invocation_id=?1",
                    [next.to_string()],
                )
                .unwrap();
        }
        if matches!(wrong, "parent" | "outcome") {
            let attempted = store.conn.execute(
                "UPDATE manager_review_assignments
                    SET reviewer_invocation_id=?2,row_version=row_version+1,updated_at=?3
                  WHERE assignment_id=?1",
                params![
                    assignment.to_string(),
                    next.to_string(),
                    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                ],
            );
            assert!(attempted.is_err(), "V130 trigger admitted {wrong} origin");
        }
        assert!(store.refresh_manager_review_assignment(assignment).unwrap());
        let (state, code): (String, String) = store
            .conn
            .query_row(
                "SELECT state,failure_code FROM manager_review_assignments WHERE assignment_id=?1",
                [assignment.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "failed");
        assert_eq!(code, "manager_review_unproven_continuation");
    }
}

/// #967: (assignment state, failure code, reserved reviewer, launch action id,
/// launch action state).
fn review_launch_row(
    store: &Store,
    assignment: Uuid,
) -> (String, Option<String>, Uuid, Uuid, String) {
    let (state, code, reviewer, action, launch): (String, Option<String>, String, String, String) =
        store
            .conn
            .query_row(
                "SELECT a.state,a.failure_code,a.reviewer_session_id,a.action_operation_id,o.state
                   FROM manager_review_assignments a
                   JOIN harness_manager_v2_operations o ON o.id=a.action_operation_id
                  WHERE a.assignment_id=?1",
                [assignment.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
    (
        state,
        code,
        Uuid::parse_str(&reviewer).unwrap(),
        Uuid::parse_str(&action).unwrap(),
        launch,
    )
}

/// #967: request a DB review and allocate it, leaving the reviewer launch
/// action queued with no reviewer session row yet.
async fn allocated_review_before_launch(f: &Fixture, key: &str) -> Uuid {
    record_db_review_source(f, key).await;
    let assignment = request_db_review(f, key).await;
    let store = f.handle.store.lock().await;
    let state: String = store
        .conn
        .query_row(
            "SELECT state FROM manager_review_assignments WHERE assignment_id=?1",
            [assignment.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    if state == "reserved" {
        assert!(
            store
                .allocate_manager_review_assignment(assignment)
                .unwrap()
        );
    }
    assignment
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn review_launch_in_flight_is_pending_and_a_dead_assignment_refuses_its_launch() {
    let f = fixture().await;
    let assignment = allocated_review_before_launch(&f, "launch-in-flight").await;
    let store = f.handle.store.lock().await;
    let (state, _, reviewer, action, launch) = review_launch_row(&store, assignment);
    assert_eq!(state, "allocating");
    assert_eq!(launch, "queued");
    assert!(store.get_session(reviewer).unwrap().is_none());

    // Reconcile before the reviewer session row exists: still pending.
    assert!(!store.refresh_manager_review_assignment(assignment).unwrap());
    store.reconcile_manager_review_assignments_once().unwrap();
    assert_eq!(review_launch_row(&store, assignment).0, "allocating");
    let op = store.manager_action_operation(action).unwrap().unwrap();
    store.require_manager_review_launch_live(&op).unwrap();

    // The launch settled without creating the reviewer: fail, naming its state.
    store
        .conn
        .execute(
            "UPDATE harness_manager_v2_operations SET state='succeeded' WHERE id=?1",
            [action.to_string()],
        )
        .unwrap();
    assert!(store.refresh_manager_review_assignment(assignment).unwrap());
    let (state, code, ..) = review_launch_row(&store, assignment);
    assert_eq!(state, "failed");
    assert_eq!(
        code.as_deref(),
        Some("manager_review_reviewer_unavailable_launch_succeeded")
    );

    // A terminal assignment refuses its launch before any provider turn.
    let error = store
        .require_manager_review_launch_live(&op)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("manager_review_assignment_terminal"),
        "{error}"
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn review_launch_stuck_past_establishment_deadline_fails_typed() {
    let f = fixture().await;
    let assignment = allocated_review_before_launch(&f, "launch-stuck").await;
    let store = f.handle.store.lock().await;
    let (.., action, launch) = review_launch_row(&store, assignment);
    assert_eq!(launch, "queued");
    // The launch action has been queued past the establishment deadline.
    let stale = (Utc::now() - chrono::Duration::minutes(31))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    store
        .conn
        .execute(
            "UPDATE harness_manager_v2_operations SET created_at=?2 WHERE id=?1",
            params![action.to_string(), stale],
        )
        .unwrap();
    assert!(store.refresh_manager_review_assignment(assignment).unwrap());
    let (state, code, ..) = review_launch_row(&store, assignment);
    assert_eq!(state, "failed");
    assert_eq!(
        code.as_deref(),
        Some("manager_review_reviewer_unavailable_launch_queued_timeout")
    );
}

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
#[tokio::test]
async fn failed_review_assignment_marks_its_running_reviewer_for_halt() {
    let f = fixture().await;
    let assignment = request_and_activate_db_review(&f, "orphan-reviewer").await;
    let store = f.handle.store.lock().await;
    // While the review is live its reviewer is not an orphan.
    assert_eq!(
        store.manager_review_orphaned_reviewers().unwrap(),
        Vec::<Uuid>::new()
    );
    let stamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    store
        .conn
        .execute(
            "UPDATE manager_review_assignments
                SET state='failed',failure_code='manager_review_receipt_missing_interrupted',
                    terminal_at=?2,updated_at=?2,row_version=row_version+1
              WHERE assignment_id=?1",
            params![assignment.to_string(), stamp],
        )
        .unwrap();
    assert_eq!(
        store.manager_review_orphaned_reviewers().unwrap(),
        vec![f.reviewer]
    );
}
