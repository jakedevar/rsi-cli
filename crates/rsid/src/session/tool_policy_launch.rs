//! Launch-time resolution of the per-session Harness tool policy (#792).
//!
//! Every launch path (fresh, continue, retry, rotation) resolves the policy
//! from the store before any provider effect. A read failure yields the
//! fail-closed policy, so a restriction is never silently dropped.

use crate::store::Store;
use rsi_common::harness_tool_policy::HarnessToolPolicy;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

/// The policy `session_id` launches under. Read-only: a policy row is written
/// separately, by [`persist_launch_tool_policy`], once the session row exists
/// (`session_tool_policies.session_id` references `sessions`).
///
/// A row already stored for `session_id` wins (a retry relaunch cannot widen
/// it). Otherwise `explicit` (the operator's launch parameter, or the emitter
/// policy a spawned child inherits) applies. Otherwise the policy resolves
/// through the `continued_from` chain of `session_id`, then of `parent`; a
/// continued or rotated session therefore needs no copy of its ancestor's row.
/// A read failure yields the fail-closed policy.
pub(super) async fn resolve_launch_tool_policy(
    store: &Arc<Mutex<Store>>,
    session_id: Uuid,
    parent: Option<Uuid>,
    explicit: Option<&HarnessToolPolicy>,
) -> Option<HarnessToolPolicy> {
    let store = store.lock().await;
    let resolved = (|| -> crate::error::Result<Option<HarnessToolPolicy>> {
        if let Some(stored) = store.get_session_tool_policy(session_id)? {
            return Ok(Some(stored));
        }
        if let Some(policy) = explicit {
            return Ok(Some(policy.clone()));
        }
        let mut found = store.resolve_session_tool_policy(session_id)?;
        if found.is_none()
            && let Some(parent) = parent
        {
            found = store.resolve_session_tool_policy(parent)?;
        }
        Ok(found)
    })();
    match resolved {
        Ok(policy) => policy,
        Err(error) => {
            tracing::error!(%session_id, %error, "tool policy unreadable; launching fail-closed");
            Some(HarnessToolPolicy::fail_closed())
        }
    }
}

/// Record an explicit or spawn-inherited policy for `session_id`. The session
/// row must already exist (foreign key); an existing row is kept.
pub(super) async fn persist_launch_tool_policy(
    store: &Arc<Mutex<Store>>,
    session_id: Uuid,
    policy: &HarnessToolPolicy,
) -> crate::error::Result<()> {
    store
        .lock()
        .await
        .insert_session_tool_policy(session_id, policy, chrono::Utc::now())
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::harness_tool_policy::WebAccessMode;

    fn policy(mode: WebAccessMode) -> HarnessToolPolicy {
        HarnessToolPolicy {
            web_access: Some(mode),
            ..HarnessToolPolicy::default()
        }
    }

    fn store() -> Arc<Mutex<Store>> {
        Arc::new(Mutex::new(Store::open_in_memory().unwrap()))
    }

    async fn session(store: &Arc<Mutex<Store>>, continued_from: Option<Uuid>) -> Uuid {
        let id = Uuid::new_v4();
        let mut row = crate::session::agent_verbs::tests::test_session(
            id,
            std::path::PathBuf::from("/tmp/policy-launch"),
        );
        row.continued_from = continued_from;
        store.lock().await.insert_session(&row).unwrap();
        id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn an_explicit_policy_is_read_only_until_persisted_and_a_retry_keeps_the_first() {
        let store = store();
        let id = session(&store, None).await;
        let first = policy(WebAccessMode::Disabled);
        // Resolution writes nothing.
        assert_eq!(
            resolve_launch_tool_policy(&store, id, None, Some(&first)).await,
            Some(first.clone())
        );
        assert!(
            store
                .lock()
                .await
                .get_session_tool_policy(id)
                .unwrap()
                .is_none()
        );
        persist_launch_tool_policy(&store, id, &first)
            .await
            .unwrap();
        // A retry relaunch that carries a wider policy cannot replace the row.
        let wider = policy(WebAccessMode::Enabled);
        assert_eq!(
            resolve_launch_tool_policy(&store, id, None, Some(&wider)).await,
            Some(first.clone())
        );
        persist_launch_tool_policy(&store, id, &wider)
            .await
            .unwrap();
        // A continue resolves the stored row with no explicit policy.
        assert_eq!(
            resolve_launch_tool_policy(&store, id, None, None).await,
            Some(first)
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn a_rotation_child_resolves_the_ancestor_policy_without_a_copy() {
        let store = store();
        let parent = session(&store, None).await;
        let child = session(&store, Some(parent)).await;
        let stored = policy(WebAccessMode::HostedOnly);
        persist_launch_tool_policy(&store, parent, &stored)
            .await
            .unwrap();
        // Through the child's own continued_from link, and through the parent
        // hint for a child row not yet written.
        assert_eq!(
            resolve_launch_tool_policy(&store, child, None, None).await,
            Some(stored.clone())
        );
        assert_eq!(
            resolve_launch_tool_policy(&store, Uuid::new_v4(), Some(parent), None).await,
            Some(stored)
        );
        assert!(
            store
                .lock()
                .await
                .get_session_tool_policy(child)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            resolve_launch_tool_policy(&store, Uuid::new_v4(), None, None).await,
            None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-session-03"))]
    #[tokio::test]
    async fn an_unreadable_policy_launches_fail_closed() {
        let store = store();
        let id = session(&store, None).await;
        store
            .lock()
            .await
            .conn
            .execute_batch(&format!(
                "DROP TRIGGER session_tool_policies_no_update;
                 INSERT INTO session_tool_policies VALUES ('{id}', '{{\"bogus\": 1}}', '2026-09-29T00:00:00.000000000Z');"
            ))
            .unwrap();
        assert_eq!(
            resolve_launch_tool_policy(&store, id, None, None).await,
            Some(HarnessToolPolicy::fail_closed())
        );
    }
}
