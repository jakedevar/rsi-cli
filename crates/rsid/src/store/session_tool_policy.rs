//! Durable per-session Harness tool policy (#792).
//!
//! One immutable row per session, written once at launch. A continued or
//! rotated session resolves its policy through the `continued_from` chain, so
//! a benchmark's tool restrictions survive rotation; a spawned child gets its
//! emitter's policy copied. Rows are append-only: the policy a session ran
//! under stays auditable.

use super::Store;
use crate::error::{DaemonError, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rsi_common::harness_tool_policy::HarnessToolPolicy;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior};
use uuid::Uuid;

// The provisional number is assigned at landing.
// RSI-RELEASED-MIGRATION-BEGIN: session-tool-policy-migration
/// Provisional schema version of the tool-policy catalog.
pub(crate) const SESSION_TOOL_POLICY_SCHEMA_VERSION: i32 = 143;

const CATALOG: &str = "CREATE TABLE session_tool_policies (
    session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE RESTRICT CHECK(rsi_uuid_is_canonical(session_id)),
    policy_json TEXT NOT NULL CHECK(json_valid(policy_json)),
    created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at))
);
CREATE TRIGGER session_tool_policies_no_update BEFORE UPDATE ON session_tool_policies BEGIN SELECT RAISE(ABORT,'session tool policies are immutable'); END;
CREATE TRIGGER session_tool_policies_no_delete BEFORE DELETE ON session_tool_policies BEGIN SELECT RAISE(ABORT,'session tool policies are append only'); END;";

/// Catalog objects, for the fixture rewind and presence assertions.
#[cfg(test)]
pub(crate) const CATALOG_OBJECTS: [(&str, &str); 3] = [
    ("table", "session_tool_policies"),
    ("trigger", "session_tool_policies_no_update"),
    ("trigger", "session_tool_policies_no_delete"),
];

pub(crate) fn apply_migration(store: &Store, version: i32) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let prior: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if prior != version - 1 {
        return Err(DaemonError::Store(format!(
            "session tool policy requires V{}, found V{prior}",
            version - 1
        )));
    }
    tx.execute_batch(CATALOG)?;
    tx.pragma_update(None, "user_version", version)?;
    tx.commit()?;
    Ok(())
}
// RSI-RELEASED-MIGRATION-END: session-tool-policy-migration

/// Upper bound on the `continued_from` walk; rotation chains are shallow.
const MAX_LINEAGE_DEPTH: usize = 64;

fn stamp(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

impl Store {
    /// Record `session_id`'s policy once. A second call for the same session
    /// (a retry relaunch) keeps the first row and reports `false`.
    pub(crate) fn insert_session_tool_policy(
        &self,
        session_id: Uuid,
        policy: &HarnessToolPolicy,
        now: DateTime<Utc>,
    ) -> Result<bool> {
        policy
            .validate()
            .map_err(|code| DaemonError::InvalidParam(code.into()))?;
        let json = serde_json::to_string(policy).map_err(|e| DaemonError::Store(e.to_string()))?;
        let inserted = self.conn.execute(
            "INSERT OR IGNORE INTO session_tool_policies(session_id, policy_json, created_at) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![session_id.to_string(), json, stamp(now)],
        )?;
        Ok(inserted == 1)
    }

    pub(crate) fn get_session_tool_policy(
        &self,
        session_id: Uuid,
    ) -> Result<Option<HarnessToolPolicy>> {
        let json: Option<String> = self
            .conn
            .query_row(
                "SELECT policy_json FROM session_tool_policies WHERE session_id=?1",
                [session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|text| {
            serde_json::from_str(&text)
                .map_err(|e| DaemonError::Store(format!("corrupt session tool policy: {e}")))
        })
        .transpose()
    }

    /// The policy a session runs under: its own row, else the nearest
    /// `continued_from` ancestor's. An unreadable row is an error so a launch
    /// fails closed instead of dropping a restriction.
    pub(crate) fn resolve_session_tool_policy(
        &self,
        session_id: Uuid,
    ) -> Result<Option<HarnessToolPolicy>> {
        let mut current = Some(session_id);
        for _ in 0..MAX_LINEAGE_DEPTH {
            let Some(id) = current else {
                return Ok(None);
            };
            if let Some(policy) = self.get_session_tool_policy(id)? {
                return Ok(Some(policy));
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
            "session tool policy lineage exceeds the depth bound".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::harness_tool_policy::WebAccessMode;

    fn policy(mode: WebAccessMode) -> HarnessToolPolicy {
        HarnessToolPolicy {
            web_access: Some(mode),
            denied_tools: vec!["shell".into()],
            ..HarnessToolPolicy::default()
        }
    }

    fn seed_session(store: &Store, continued_from: Option<Uuid>) -> Uuid {
        let id = Uuid::new_v4();
        let mut session = crate::session::agent_verbs::tests::test_session(
            id,
            std::path::PathBuf::from("/tmp/policy"),
        );
        session.continued_from = continued_from;
        store.insert_session(&session).unwrap();
        id
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_policy_for_a_nonexistent_session_fails_the_foreign_key() {
        let store = Store::open_in_memory().unwrap();
        let error = store
            .insert_session_tool_policy(
                Uuid::new_v4(),
                &policy(WebAccessMode::Disabled),
                Utc::now(),
            )
            .expect_err("a policy needs its session row");
        assert!(
            error.to_string().to_lowercase().contains("foreign key"),
            "{error}"
        );
        let rows: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM session_tool_policies", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn policy_is_written_once_and_read_back() {
        let store = Store::open_in_memory().unwrap();
        let id = seed_session(&store, None);
        assert!(store.get_session_tool_policy(id).unwrap().is_none());
        assert!(
            store
                .insert_session_tool_policy(id, &policy(WebAccessMode::Disabled), Utc::now())
                .unwrap()
        );
        assert!(
            !store
                .insert_session_tool_policy(id, &policy(WebAccessMode::Enabled), Utc::now())
                .unwrap()
        );
        assert_eq!(
            store.get_session_tool_policy(id).unwrap(),
            Some(policy(WebAccessMode::Disabled))
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn rows_are_immutable_and_undeletable_and_malformed_policy_is_refused() {
        let store = Store::open_in_memory().unwrap();
        let id = seed_session(&store, None);
        store
            .insert_session_tool_policy(id, &policy(WebAccessMode::Disabled), Utc::now())
            .unwrap();
        assert!(
            store
                .conn
                .execute("UPDATE session_tool_policies SET policy_json='{}'", [])
                .is_err()
        );
        assert!(
            store
                .conn
                .execute("DELETE FROM session_tool_policies", [])
                .is_err()
        );
        let bad = HarnessToolPolicy {
            denied_tools: vec!["Not A Tool".into()],
            ..HarnessToolPolicy::default()
        };
        assert!(
            store
                .insert_session_tool_policy(seed_session(&store, None), &bad, Utc::now())
                .is_err()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-04"))]
    #[test]
    fn a_continued_session_resolves_its_ancestors_policy() {
        let store = Store::open_in_memory().unwrap();
        let root = seed_session(&store, None);
        let child = seed_session(&store, Some(root));
        let grandchild = seed_session(&store, Some(child));
        let unrelated = seed_session(&store, None);
        store
            .insert_session_tool_policy(root, &policy(WebAccessMode::HostedOnly), Utc::now())
            .unwrap();
        for id in [root, child, grandchild] {
            assert_eq!(
                store.resolve_session_tool_policy(id).unwrap(),
                Some(policy(WebAccessMode::HostedOnly))
            );
        }
        assert_eq!(store.resolve_session_tool_policy(unrelated).unwrap(), None);
    }
}
