//! Durable per-session Harness exec target (#3176).
//!
//! One immutable row per container-bound session, written once at launch. A
//! continued or rotated session resolves its target through the
//! `continued_from` chain, so the binding survives rotation and a daemon
//! restart. Rows are append-only: the target a session ran against stays
//! auditable. A session with no row (and no ancestor with one) runs on the host.

use super::Store;
use crate::error::{DaemonError, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::exec_target::ExecTarget;
use rusqlite::OptionalExtension;
use uuid::Uuid;

/// Upper bound on the `continued_from` walk; rotation chains are shallow.
const MAX_LINEAGE_DEPTH: usize = 64;

impl Store {
    /// Record `session_id`'s container target once. A host target is never
    /// stored. A second call for the same session (a retry relaunch) keeps the
    /// first row and reports `false`.
    pub fn insert_session_exec_target(
        &self,
        session_id: Uuid,
        target: &ExecTarget,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        if !target.is_container() {
            return Err(DaemonError::InvalidParam(format!(
                "{}:host_target_is_not_bound",
                rsi_common::exec_target::EXEC_TARGET_INVALID
            )));
        }
        target.validate_shape().map_err(|reason| {
            DaemonError::InvalidParam(format!(
                "{}:{reason}",
                rsi_common::exec_target::EXEC_TARGET_INVALID
            ))
        })?;
        let json = serde_json::to_string(target).map_err(|e| DaemonError::Store(e.to_string()))?;
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO session_exec_targets(session_id, target_json, created_at) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![
                session_id.to_string(),
                json,
                now.to_rfc3339_opts(SecondsFormat::Nanos, true)
            ],
        )?;
        Ok(inserted == 1)
    }

    /// The target stored for exactly `session_id`, if any.
    pub fn get_session_exec_target(&self, session_id: Uuid) -> Result<Option<ExecTarget>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT target_json FROM session_exec_targets WHERE session_id=?1",
                [session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|text| {
            serde_json::from_str(&text)
                .map_err(|e| DaemonError::Store(format!("corrupt session exec target: {e}")))
        })
        .transpose()
    }

    /// The target a session runs against: its own row, else the nearest
    /// `continued_from` ancestor's. An unreadable row or an over-deep lineage
    /// is an error so a launch fails closed instead of running on the host.
    pub fn resolve_session_exec_target(&self, session_id: Uuid) -> Result<Option<ExecTarget>> {
        let mut current = Some(session_id);
        for _ in 0..MAX_LINEAGE_DEPTH {
            let Some(id) = current else {
                return Ok(None);
            };
            if let Some(target) = self.get_session_exec_target(id)? {
                return Ok(Some(target));
            }
            current = self
                .conn
                .query_row(
                    "SELECT continued_from FROM sessions WHERE id=?1",
                    [id.to_string()],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten()
                .and_then(|text| Uuid::parse_str(&text).ok());
        }
        Err(DaemonError::Store(
            "session exec target lineage exceeds the depth bound".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::exec_target::{ContainerExecTarget, DEFAULT_PER_CALL_CEILING_SECS};

    fn container(name: &str) -> ExecTarget {
        ExecTarget::Container(ContainerExecTarget {
            docker_host: "ssh://arch-laptop".into(),
            container: name.into(),
            workdir: "/app".into(),
            user: None,
            compose_project: "tb2-trial".into(),
            pinned_digests: vec![format!("sha256:{}", "a".repeat(64))],
            per_call_ceiling_secs: DEFAULT_PER_CALL_CEILING_SECS,
        })
    }

    fn seed_session(store: &Store, continued_from: Option<Uuid>) -> Uuid {
        let id = Uuid::new_v4();
        let mut session =
            crate::test_support::test_session(id, std::path::PathBuf::from("/tmp/exec-target"));
        session.continued_from = continued_from;
        store.insert_session(&session).unwrap();
        id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_target_is_written_once_read_back_and_kept_on_a_retry() {
        let store = Store::open_in_memory().unwrap();
        let id = seed_session(&store, None);
        assert_eq!(store.get_session_exec_target(id).unwrap(), None);
        assert!(
            store
                .insert_session_exec_target(id, &container("first"), Utc::now())
                .unwrap()
        );
        assert!(
            !store
                .insert_session_exec_target(id, &container("second"), Utc::now())
                .unwrap()
        );
        assert_eq!(
            store.get_session_exec_target(id).unwrap(),
            Some(container("first"))
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_continued_or_rotated_session_resolves_its_ancestors_target() {
        let store = Store::open_in_memory().unwrap();
        let root = seed_session(&store, None);
        let child = seed_session(&store, Some(root));
        let grandchild = seed_session(&store, Some(child));
        let unrelated = seed_session(&store, None);
        store
            .insert_session_exec_target(root, &container("main"), Utc::now())
            .unwrap();
        for id in [root, child, grandchild] {
            assert_eq!(
                store.resolve_session_exec_target(id).unwrap(),
                Some(container("main"))
            );
        }
        assert_eq!(store.resolve_session_exec_target(unrelated).unwrap(), None);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn rows_are_immutable_undeletable_and_need_a_session_and_a_valid_container() {
        let store = Store::open_in_memory().unwrap();
        let id = seed_session(&store, None);
        store
            .insert_session_exec_target(id, &container("main"), Utc::now())
            .unwrap();
        assert!(
            store
                .conn
                .execute("UPDATE session_exec_targets SET target_json='{}'", [])
                .is_err()
        );
        assert!(
            store
                .conn
                .execute("DELETE FROM session_exec_targets", [])
                .is_err()
        );
        // A host target and a malformed container are not stored.
        let other = seed_session(&store, None);
        assert!(
            store
                .insert_session_exec_target(other, &ExecTarget::Host, Utc::now())
                .is_err()
        );
        let mut bad = container("main");
        if let ExecTarget::Container(inner) = &mut bad {
            inner.docker_host = "-H".into();
        }
        let error = store
            .insert_session_exec_target(other, &bad, Utc::now())
            .expect_err("a malformed target is refused");
        assert!(
            error
                .to_string()
                .contains("exec_target_invalid:docker_host_syntax"),
            "{error}"
        );
        // The foreign key needs the session row.
        let missing = store
            .insert_session_exec_target(Uuid::new_v4(), &container("main"), Utc::now())
            .expect_err("a target needs its session row");
        assert!(
            missing.to_string().to_lowercase().contains("foreign key"),
            "{missing}"
        );
        // A corrupt stored row fails closed rather than reading as the host.
        store
            .conn
            .execute_batch("DROP TRIGGER session_exec_targets_no_update")
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE session_exec_targets SET target_json='{\"kind\":\"nope\"}'",
                [],
            )
            .unwrap();
        assert!(store.resolve_session_exec_target(id).is_err());
    }
}
