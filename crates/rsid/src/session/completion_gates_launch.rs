//! Launch-time resolution of per-session Harness completion gates (#794).

use crate::store::Store;
use rsi_common::completion_gates::CompletionGates;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

/// The gates `session_id` launches under. A stored row wins over an explicit
/// retry value; otherwise a fresh operator launch carries `explicit`, and a
/// continue or rotation resolves through `continued_from`. Unlike a tool
/// policy, unreadable gates fail the launch rather than silently permitting
/// completion.
pub(super) async fn resolve_launch_completion_gates(
    store: &Arc<Mutex<Store>>,
    session_id: Uuid,
    parent: Option<Uuid>,
    explicit: Option<&CompletionGates>,
) -> crate::error::Result<Option<CompletionGates>> {
    let store = store.lock().await;
    if let Some(stored) = store.get_session_completion_gates(session_id)? {
        return Ok(Some(stored));
    }
    if let Some(gates) = explicit {
        return Ok(Some(gates.clone()));
    }
    let mut found = store.resolve_session_completion_gates(session_id)?;
    if found.is_none()
        && let Some(parent) = parent
    {
        found = store.resolve_session_completion_gates(parent)?;
    }
    Ok(found)
}

pub(super) fn fail_closed() -> CompletionGates {
    CompletionGates {
        gates: vec![rsi_common::completion_gates::CompletionGate {
            name: "completion_gates_unavailable".into(),
            command: "false".into(),
            timeout_secs: rsi_common::completion_gates::COMPLETION_GATES_DEFAULT_TIMEOUT_SECS,
            max_output_bytes: rsi_common::completion_gates::COMPLETION_GATES_DEFAULT_OUTPUT_BYTES,
        }],
        max_attempts: 1,
    }
}

/// Record an explicit operator launch value. The session row must already
/// exist; an existing row is kept.
pub(super) async fn persist_launch_completion_gates(
    store: &Arc<Mutex<Store>>,
    session_id: Uuid,
    gates: &CompletionGates,
) -> crate::error::Result<()> {
    store
        .lock()
        .await
        .insert_session_completion_gates(session_id, gates, chrono::Utc::now())
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gates(name: &str) -> CompletionGates {
        CompletionGates {
            gates: vec![rsi_common::completion_gates::CompletionGate {
                name: name.into(),
                command: "true".into(),
                timeout_secs: 1,
                max_output_bytes: 1024,
            }],
            max_attempts: 2,
        }
    }

    fn store() -> Arc<Mutex<Store>> {
        Arc::new(Mutex::new(Store::open_in_memory().unwrap()))
    }

    async fn session(store: &Arc<Mutex<Store>>, continued_from: Option<Uuid>) -> Uuid {
        let id = Uuid::new_v4();
        let mut row = crate::session::agent_verbs::tests::test_session(
            id,
            std::path::PathBuf::from("/tmp/completion-gates"),
        );
        row.continued_from = continued_from;
        store.lock().await.insert_session(&row).unwrap();
        id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_stored_row_wins_and_rotation_resolves_without_a_copy() {
        let store = store();
        let parent = session(&store, None).await;
        let child = session(&store, Some(parent)).await;
        let first = gates("first");
        persist_launch_completion_gates(&store, parent, &first)
            .await
            .unwrap();
        assert_eq!(
            resolve_launch_completion_gates(&store, child, None, None)
                .await
                .unwrap(),
            Some(first.clone())
        );
        assert_eq!(
            resolve_launch_completion_gates(&store, Uuid::new_v4(), Some(parent), None)
                .await
                .unwrap(),
            Some(first)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_stored_row_wins_over_an_explicit_retry_value() {
        let store = store();
        let id = session(&store, None).await;
        let first = gates("first");
        persist_launch_completion_gates(&store, id, &first)
            .await
            .unwrap();
        let retry = gates("retry");

        assert_eq!(
            resolve_launch_completion_gates(&store, id, None, Some(&retry))
                .await
                .unwrap(),
            Some(first.clone())
        );
        persist_launch_completion_gates(&store, id, &retry)
            .await
            .unwrap();
        assert_eq!(
            store.lock().await.get_session_completion_gates(id).unwrap(),
            Some(first)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn an_invalid_explicit_value_is_refused() {
        let store = store();
        let id = session(&store, None).await;
        let invalid = CompletionGates {
            gates: Vec::new(),
            max_attempts: 11,
        };
        assert!(
            persist_launch_completion_gates(&store, id, &invalid)
                .await
                .is_err()
        );
    }
}
