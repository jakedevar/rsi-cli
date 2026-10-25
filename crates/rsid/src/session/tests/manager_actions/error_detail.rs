//! #3640: a `create_session` whose launch is refused for a reason no specific
//! class names ends `manager_v2_lifecycle_unconfirmed`. The receipt keeps the
//! original error text (bounded, secret-free) as `outcome_detail`, one
//! `manager_launch` friction event is recorded, and a custody `ContextRead`
//! refusal keeps its underlying error on the failed session.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::significant_drop_tightening,
    clippy::large_futures
)]

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
use super::*;

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
const INJECTED: &str = "injected direct launch context authorization failure";

#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
async fn refused_create(p: &Pilot, key: &str) -> (ManagerActionReceiptV2, Uuid) {
    let receipt = p
        .admit(
            key,
            ManagerActionV2::CreateSession {
                parent_id: p.epic,
                kind: SessionKind::Task,
                query: "worker whose launch is refused".into(),
                launch: p.policy.allowed_launches[0].clone(),
                sandbox_source: None,
            },
        )
        .await;
    let id = receipt.target_session_id.unwrap();
    super::super::super::launch::fail_next_direct_launch_context_authorization_for_test(&format!(
        "manager.action:{}",
        receipt.operation_id
    ));
    p.manager.reconcile_manager_actions_once().await.unwrap();
    (p.receipt(receipt.operation_id).await, id)
}

/// The refused create's receipt names the catch-all class AND carries the
/// original error text, so a manager reads the cause without the daemon log.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_create_receipt_carries_the_original_error_text() {
    let p = pilot().await;
    let (settled, _) = refused_create(&p, "error-detail-receipt").await;
    assert_eq!(settled.state, ManagerActionStateV2::Blocked, "{settled:?}");
    assert_eq!(
        settled.outcome.as_deref(),
        Some("manager_v2_lifecycle_unconfirmed")
    );
    assert!(
        settled
            .outcome_detail
            .as_deref()
            .is_some_and(|detail| detail.contains(INJECTED)),
        "the receipt keeps the underlying error: {settled:?}"
    );
    // The stored receipt (what AgentManagerGetAction reads) has it too.
    let stored = p
        .manager
        .store
        .lock()
        .await
        .manager_action_operation(settled.operation_id)
        .unwrap()
        .unwrap()
        .receipt;
    assert_eq!(stored.outcome_detail, settled.outcome_detail);
}

/// One `manager_launch` friction event per catch-all create, carrying codes and
/// the action id only (never the error prose).
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_create_records_one_friction_event() {
    let p = pilot().await;
    let (settled, _) = refused_create(&p, "error-detail-friction").await;
    let store = p.manager.store.lock().await;
    let rows: Vec<(String, Option<String>, Option<String>)> = store
        .conn
        .prepare(
            "SELECT signature, evidence_ref, project_id FROM friction_events
             WHERE signature LIKE 'manager_launch:%'",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0].0,
        "manager_launch:create_session:manager_v2_lifecycle_unconfirmed"
    );
    assert_eq!(
        rows[0].1.as_deref(),
        Some(format!("manager_action:{}", settled.operation_id).as_str())
    );
    assert_eq!(rows[0].2.as_deref(), Some(p.project.to_string().as_str()));
}

/// The `ContextRead` custody refusal keeps its underlying error on the failed
/// session: the class stays `launch_failed:session_context_authorization_failed`
/// and the System event that explains it carries the bounded detail.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_context_read_refusal_keeps_its_error_on_the_failed_session() {
    let p = pilot().await;
    let (_, id) = refused_create(&p, "error-detail-context-read").await;
    let store = p.manager.store.lock().await;
    let session = store.get_session(id).unwrap().unwrap();
    assert_eq!(session.status, SessionStatus::Failed);
    assert_eq!(
        session.terminal_reason.as_deref(),
        Some("launch_failed:session_context_authorization_failed")
    );
    let explanation: String = store
        .conn
        .query_row(
            "SELECT content FROM conversation_events
             WHERE session_id=?1 AND content LIKE 'Launch failed before%'",
            [id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        explanation.contains("session_context_authorization_failed")
            && explanation.contains(INJECTED),
        "{explanation}"
    );
}

/// The kept text is bounded and secret-free: control characters are flattened,
/// a bearer token is redacted and the text is cut at the cap.
#[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-05"))]
#[test]
fn error_detail_is_bounded_and_redacted() {
    use crate::session::manager_actions::bounded_error_detail;
    let long = DaemonError::Process(format!("line one\nline two {}", "x".repeat(2_000)));
    let detail = bounded_error_detail(&long);
    assert_eq!(detail.chars().count(), 300);
    assert!(!detail.contains('\n'));
    let secret = DaemonError::Process(
        "launch failed: Authorization: Bearer sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789"
            .into(),
    );
    let detail = bounded_error_detail(&secret);
    assert!(
        !detail.contains("abcdefghijklmnopqrstuvwxyz0123456789"),
        "{detail}"
    );
}
