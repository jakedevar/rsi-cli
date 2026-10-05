//! Durable per-session Harness completion gates (#794).
//!
//! One immutable row per session, written at fresh launch. Continue and
//! rotation resolve through the `continued_from` chain; a child never gets
//! gates from an agent emitter.

use super::Store;
use crate::error::{DaemonError, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::completion_gates::CompletionGates;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior};
use uuid::Uuid;

// The provisional number is assigned at landing.
// RSI-RELEASED-MIGRATION-BEGIN: session-completion-gates-migration
/// Provisional schema version of the completion-gate table.
pub(crate) const SESSION_COMPLETION_GATES_SCHEMA_VERSION: i32 = 148;
const SESSION_COMPLETION_GATES_PRIOR_SCHEMA_VERSION: i32 = 147;

const CATALOG: &str = "CREATE TABLE session_completion_gates (
    session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE RESTRICT CHECK(rsi_uuid_is_canonical(session_id)),
    gates_json TEXT NOT NULL CHECK(json_valid(gates_json)),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at))
);
CREATE TRIGGER session_completion_gates_no_update BEFORE UPDATE ON session_completion_gates BEGIN SELECT RAISE(ABORT,'session completion gates are immutable'); END;
CREATE TRIGGER session_completion_gates_no_delete BEFORE DELETE ON session_completion_gates BEGIN SELECT RAISE(ABORT,'session completion gates are append only'); END;";

/// Catalog objects, for the fixture rewind and presence assertions.
#[cfg(test)]
pub(crate) const CATALOG_OBJECTS: [(&str, &str); 3] = [
    ("table", "session_completion_gates"),
    ("trigger", "session_completion_gates_no_update"),
    ("trigger", "session_completion_gates_no_delete"),
];

pub(crate) fn apply_migration(store: &Store, version: i32) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let prior: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if prior != SESSION_COMPLETION_GATES_PRIOR_SCHEMA_VERSION {
        return Err(DaemonError::Store(format!(
            "session completion gates requires V{SESSION_COMPLETION_GATES_PRIOR_SCHEMA_VERSION}, found V{prior}"
        )));
    }
    tx.execute_batch(CATALOG)?;
    tx.pragma_update(None, "user_version", version)?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: session-completion-gates-migration

/// Upper bound on the `continued_from` walk; rotation chains are shallow.
const MAX_LINEAGE_DEPTH: usize = 64;

fn stamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

impl Store {
    /// Record `session_id`'s gates once. A retry relaunch keeps the first row.
    pub fn insert_session_completion_gates(
        &self,
        session_id: Uuid,
        gates: &CompletionGates,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        gates
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let json = serde_json::to_string(gates).map_err(|e| DaemonError::Store(e.to_string()))?;
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO session_completion_gates(session_id, gates_json, created_at) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![session_id.to_string(), json, stamp(now)],
        )?;
        Ok(inserted == 1)
    }

    pub fn get_session_completion_gates(
        &self,
        session_id: Uuid,
    ) -> Result<Option<CompletionGates>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT gates_json FROM session_completion_gates WHERE session_id=?1",
                [session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|text| {
            serde_json::from_str(&text)
                .map_err(|e| DaemonError::Store(format!("corrupt session completion gates: {e}")))
        })
        .transpose()
    }

    /// The gates a session runs under: its own row, else the nearest
    /// `continued_from` ancestor's. An unreadable row is an error so a launch
    /// fails closed instead of dropping a restriction.
    pub fn resolve_session_completion_gates(
        &self,
        session_id: Uuid,
    ) -> Result<Option<CompletionGates>> {
        let mut current = Some(session_id);
        for _ in 0..MAX_LINEAGE_DEPTH {
            let Some(id) = current else {
                return Ok(None);
            };
            if let Some(gates) = self.get_session_completion_gates(id)? {
                return Ok(Some(gates));
            }
            current = self.parent_session(id)?;
        }
        Err(DaemonError::Store(
            "session completion gates lineage exceeds the depth bound".into(),
        ))
    }

    fn parent_session(&self, session_id: Uuid) -> Result<Option<Uuid>> {
        self.conn
            .query_row(
                "SELECT continued_from FROM sessions WHERE id=?1",
                [session_id.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
            .and_then(|text| Uuid::parse_str(&text).ok())
            .map_or(Ok(None), |id| Ok(Some(id)))
    }
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
            max_attempts: 1,
        }
    }

    fn seed_session(store: &Store, continued_from: Option<Uuid>) -> Uuid {
        let id = Uuid::new_v4();
        let mut session =
            crate::test_support::test_session(id, std::path::PathBuf::from("/tmp/gates"));
        session.continued_from = continued_from;
        store.insert_session(&session).unwrap();
        id
    }

    fn catalog_object_exists(
        connection: &rusqlite::Connection,
        object_type: &str,
        name: &str,
    ) -> bool {
        connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type=?1 AND name=?2",
                (object_type, name),
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or_default()
            == 1
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn gates_are_written_once_and_read_back() {
        let store = Store::open_in_memory().unwrap();
        let id = seed_session(&store, None);
        assert!(store.get_session_completion_gates(id).unwrap().is_none());
        assert!(
            store
                .insert_session_completion_gates(id, &gates("check"), Utc::now())
                .unwrap()
        );
        assert!(
            !store
                .insert_session_completion_gates(id, &gates("other"), Utc::now())
                .unwrap()
        );
        assert_eq!(
            store.get_session_completion_gates(id).unwrap(),
            Some(gates("check"))
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_continued_session_resolves_its_ancestors_gates() {
        let store = Store::open_in_memory().unwrap();
        let root = seed_session(&store, None);
        let child = seed_session(&store, Some(root));
        let grandchild = seed_session(&store, Some(child));
        let unrelated = seed_session(&store, None);
        store
            .insert_session_completion_gates(root, &gates("check"), Utc::now())
            .unwrap();
        for id in [root, child, grandchild] {
            assert_eq!(
                store.resolve_session_completion_gates(id).unwrap(),
                Some(gates("check"))
            );
        }
        assert_eq!(
            store.resolve_session_completion_gates(unrelated).unwrap(),
            None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn rows_are_immutable_and_invalid_gates_are_refused() {
        let store = Store::open_in_memory().unwrap();
        let id = seed_session(&store, None);
        store
            .insert_session_completion_gates(id, &gates("check"), Utc::now())
            .unwrap();
        assert!(
            store
                .conn
                .execute("UPDATE session_completion_gates SET gates_json='{}'", [])
                .is_err()
        );
        assert!(
            store
                .conn
                .execute("DELETE FROM session_completion_gates", [])
                .is_err()
        );
        assert!(
            store
                .insert_session_completion_gates(
                    seed_session(&store, None),
                    &CompletionGates::default(),
                    Utc::now()
                )
                .is_ok()
        );
        assert!(
            store
                .insert_session_completion_gates(
                    seed_session(&store, None),
                    &CompletionGates {
                        gates: Vec::new(),
                        max_attempts: 11,
                    },
                    Utc::now()
                )
                .is_err()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn completion_gates_migration_installs_rewinds_and_replays() {
        let directory = tempfile::tempdir().expect("create migration database directory");
        let database = directory
            .path()
            .join("session-completion-gates-v148.sqlite");
        let store = Store::open(&database).expect("open schema head containing V148");
        let session_id = seed_session(&store, None);
        let expected = gates("check");
        store
            .insert_session_completion_gates(session_id, &expected, Utc::now())
            .expect("persist completion gates");
        assert_eq!(
            store.get_session_completion_gates(session_id).unwrap(),
            Some(expected.clone())
        );
        for (object_type, name) in CATALOG_OBJECTS {
            assert!(
                catalog_object_exists(&store.conn, object_type, name),
                "V148 DDL installs {object_type} {name}"
            );
        }
        drop(store);

        let reopened = Store::open(&database).expect("idempotently reopen schema head");
        assert_eq!(
            reopened
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                .unwrap(),
            crate::store::LATEST_SCHEMA_VERSION
        );
        assert_eq!(
            reopened.get_session_completion_gates(session_id).unwrap(),
            Some(expected)
        );
        crate::store::tests::rewind_store_to_schema_version(
            &reopened.conn,
            SESSION_COMPLETION_GATES_PRIOR_SCHEMA_VERSION,
        );
        assert!(!catalog_object_exists(
            &reopened.conn,
            "table",
            "session_completion_gates"
        ));
        drop(reopened);

        let replayed = Store::open(&database).expect("replay V148 migration");
        assert_eq!(
            replayed
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i32>(0))
                .unwrap(),
            crate::store::LATEST_SCHEMA_VERSION
        );
        assert!(catalog_object_exists(
            &replayed.conn,
            "table",
            "session_completion_gates"
        ));
    }
}
