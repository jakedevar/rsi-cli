//! #568/#575 R1: a DB-native review that ends without a receipt records WHY.
#![allow(clippy::unwrap_used, clippy::significant_drop_tightening)]

use super::*;

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
