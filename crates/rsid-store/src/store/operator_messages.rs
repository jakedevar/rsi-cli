//! Operator-authored follow-ups. A queued row is durable before any provider
//! effect; only the monitor or terminal dispatcher may claim it.

use super::Store;
use crate::error::{DaemonError, Result};
use chrono::{SecondsFormat, Utc};
use rsi_common::types::{ConversationEvent, EventType, Role};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use uuid::Uuid;

// Provisional V<next> for #929. Seal after #888's V138 allocation.
pub(crate) const PROVISIONAL_VERSION: i32 = 142;

pub(crate) fn apply_migration(store: &Store, version: i32) -> Result<()> {
    let tx = Transaction::new_unchecked(&store.conn, TransactionBehavior::Immediate)?;
    let prior: i32 = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if prior != version - 1 {
        return Err(DaemonError::Store(format!(
            "operator messages require V{}, found V{prior}",
            version - 1
        )));
    }
    tx.execute_batch(
        "CREATE TABLE operator_messages (
            id TEXT PRIMARY KEY CHECK(rsi_uuid_is_canonical(id)),
            session_id TEXT NOT NULL CHECK(rsi_uuid_is_canonical(session_id)) REFERENCES sessions(id),
            idempotency_key TEXT NOT NULL CHECK(length(idempotency_key) BETWEEN 1 AND 128),
            content TEXT NOT NULL CHECK(length(content) BETWEEN 1 AND 65536),
            original_content TEXT NOT NULL CHECK(length(original_content) BETWEEN 1 AND 65536),
            state TEXT NOT NULL CHECK(state IN ('queued','dispatching','effect_possible','delivered','withdrawn','uncertain')),
            created_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(created_at)),
            updated_at TEXT NOT NULL CHECK(rsi_rfc3339_nanos_is_canonical(updated_at)),
            delivered_at TEXT CHECK(delivered_at IS NULL OR rsi_rfc3339_nanos_is_canonical(delivered_at)),
            UNIQUE(session_id,idempotency_key)
        );
        CREATE INDEX operator_messages_fifo ON operator_messages(session_id,state,created_at,id);
        CREATE UNIQUE INDEX operator_messages_one_dispatching
            ON operator_messages(session_id) WHERE state IN ('dispatching','effect_possible');",
    )?;
    tx.pragma_update(None, "user_version", version)?;
    tx.commit()?;
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
pub struct OperatorMessage {
    pub id: Uuid,
    pub session_id: Uuid,
    pub content: String,
    pub state: String,
    pub created_at: String,
    pub updated_at: String,
    pub delivered_at: Option<String>,
    #[serde(skip_serializing)]
    original_content: String,
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OperatorMessage> {
    let parse = |index| {
        let raw: String = row.get(index)?;
        Uuid::parse_str(&raw).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    };
    Ok(OperatorMessage {
        id: parse(0)?,
        session_id: parse(1)?,
        content: row.get(2)?,
        state: row.get(3)?,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
        delivered_at: row.get(6)?,
        original_content: row.get(7)?,
    })
}

const SELECT: &str =
    "SELECT id,session_id,content,state,created_at,updated_at,delivered_at,original_content
    FROM operator_messages";

fn valid_content(content: &str) -> Result<()> {
    if content.trim().is_empty() || content.len() > 65536 || content.contains('\0') {
        return Err(DaemonError::InvalidParam(
            "operator message must contain 1 to 65536 bytes of text".into(),
        ));
    }
    Ok(())
}

impl Store {
    pub fn queue_operator_message(
        &mut self,
        session_id: Uuid,
        content: &str,
        idempotency_key: &str,
    ) -> Result<OperatorMessage> {
        valid_content(content)?;
        if idempotency_key.is_empty()
            || idempotency_key.len() > 128
            || idempotency_key.contains('\0')
        {
            return Err(DaemonError::InvalidParam(
                "invalid operator message idempotency key".into(),
            ));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let existing = tx
            .query_row(
                &format!("{SELECT} WHERE session_id=?1 AND idempotency_key=?2"),
                params![session_id.to_string(), idempotency_key],
                row,
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing.original_content != content {
                return Err(DaemonError::InvalidParam(
                    "operator message idempotency key reused with different content".into(),
                ));
            }
            tx.commit()?;
            return Ok(existing);
        }
        let kind_status: Option<(String, String)> = tx
            .query_row(
                "SELECT session_kind,status FROM sessions WHERE id=?1",
                [session_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((kind, status)) = kind_status else {
            return Err(DaemonError::SessionNotFound(session_id));
        };
        let kind: rsi_common::types::SessionKind =
            serde_json::from_value(serde_json::Value::String(kind))?;
        if !rsi_common::is_leaf_kind(kind) || matches!(status.as_str(), "Archived" | "Deleted") {
            return Err(DaemonError::InvalidParam(
                "operator messages require a live leaf session".into(),
            ));
        }
        let id = Uuid::new_v4();
        let timestamp = now();
        tx.execute(
            "INSERT INTO operator_messages(id,session_id,idempotency_key,content,original_content,state,created_at,updated_at)
             VALUES(?1,?2,?3,?4,?4,'queued',?5,?5)",
            params![id.to_string(),session_id.to_string(),idempotency_key,content,timestamp],
        )?;
        let message = tx.query_row(&format!("{SELECT} WHERE id=?1"), [id.to_string()], row)?;
        tx.commit()?;
        Ok(message)
    }

    pub fn list_operator_messages(&self, session_id: Uuid) -> Result<Vec<OperatorMessage>> {
        let mut statement = self.conn.prepare(&format!(
            "{SELECT} WHERE session_id=?1 ORDER BY created_at,id"
        ))?;
        let messages = statement
            .query_map([session_id.to_string()], row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(messages)
    }

    pub fn edit_operator_message(&mut self, id: Uuid, content: &str) -> Result<OperatorMessage> {
        valid_content(content)?;
        let changed = self.conn.execute(
            "UPDATE operator_messages SET content=?1,updated_at=?2 WHERE id=?3 AND state='queued'",
            params![content, now(), id.to_string()],
        )?;
        if changed != 1 {
            return Err(DaemonError::InvalidParam(
                "operator message is no longer editable".into(),
            ));
        }
        self.conn
            .query_row(&format!("{SELECT} WHERE id=?1"), [id.to_string()], row)
            .map_err(Into::into)
    }

    pub fn withdraw_operator_message(&mut self, id: Uuid) -> Result<OperatorMessage> {
        let changed = self.conn.execute(
            "UPDATE operator_messages SET state='withdrawn',updated_at=?1 WHERE id=?2 AND state='queued'",
            params![now(),id.to_string()],
        )?;
        if changed != 1 {
            return Err(DaemonError::InvalidParam(
                "operator message is no longer withdrawable".into(),
            ));
        }
        self.conn
            .query_row(&format!("{SELECT} WHERE id=?1"), [id.to_string()], row)
            .map_err(Into::into)
    }

    pub fn claim_operator_message(&mut self, session_id: Uuid) -> Result<Option<OperatorMessage>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let id: Option<String> = tx
            .query_row(
                "SELECT id FROM operator_messages WHERE session_id=?1 AND state='queued'
             ORDER BY created_at,id LIMIT 1",
                [session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(id) = id else {
            tx.commit()?;
            return Ok(None);
        };
        // A prior dispatch remains an explicit recovery owner. Never claim a
        // second row until its outcome is known.
        let changed = tx.execute(
            "UPDATE operator_messages SET state='dispatching',updated_at=?1 WHERE id=?2
             AND state='queued' AND NOT EXISTS (
               SELECT 1 FROM operator_messages WHERE session_id=?3 AND state IN ('dispatching','effect_possible','uncertain'))",
            params![now(),id,session_id.to_string()],
        )?;
        let message = if changed == 1 {
            tx.query_row(&format!("{SELECT} WHERE id=?1"), [id], row)
                .optional()?
        } else {
            None
        };
        tx.commit()?;
        Ok(message)
    }

    pub(crate) fn has_queued_operator_message(&self, session_id: Uuid) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM operator_messages WHERE session_id=?1 AND state='queued'",
            [session_id.to_string()],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Persist the last recoverable boundary before invoking a provider. A
    /// crashed `dispatching` claim has no provider effect and may be retried;
    /// an `effect_possible` claim cannot be retried without provider evidence.
    pub fn mark_operator_message_effect_possible(&mut self, id: Uuid) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE operator_messages SET state='effect_possible',updated_at=?1
             WHERE id=?2 AND state='dispatching'",
            params![now(), id.to_string()],
        )?;
        if changed != 1 {
            return Err(DaemonError::InvalidParam(
                "operator message claim is no longer dispatchable".into(),
            ));
        }
        Ok(())
    }

    /// Call once at daemon startup, before any monitor or dispatcher can send
    /// a turn. Only pre-effect claims are retryable. An interrupted provider
    /// call may already have sent bytes, so its outcome remains uncertain.
    pub fn reconcile_operator_messages_at_startup(&mut self) -> Result<(usize, usize)> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let timestamp = now();
        let retryable = tx.execute(
            "UPDATE operator_messages SET state='queued',updated_at=?1 WHERE state='dispatching'",
            [&timestamp],
        )?;
        let uncertain = tx.execute(
            "UPDATE operator_messages SET state='uncertain',updated_at=?1 WHERE state='effect_possible'",
            [&timestamp],
        )?;
        tx.commit()?;
        Ok((retryable, uncertain))
    }

    pub fn settle_operator_message(&mut self, id: Uuid, delivered: bool) -> Result<()> {
        let timestamp = now();
        let changed = self.conn.execute(
            "UPDATE operator_messages SET state=CASE
                WHEN ?1='delivered' THEN 'delivered'
                WHEN state='dispatching' THEN 'queued'
                ELSE 'uncertain' END,
                updated_at=?2,delivered_at=?3
             WHERE id=?4 AND state IN ('dispatching','effect_possible')
               AND (?1!='delivered' OR state='effect_possible')",
            params![
                if delivered { "delivered" } else { "uncertain" },
                timestamp,
                delivered.then_some(timestamp.as_str()),
                id.to_string()
            ],
        )?;
        if changed != 1 {
            return Err(DaemonError::InvalidParam(
                "operator message effect is no longer settleable".into(),
            ));
        }
        Ok(())
    }

    /// The session an operator message belongs to, or `None` if it is unknown.
    pub fn operator_message_session(&self, id: Uuid) -> Result<Option<Uuid>> {
        let session: Option<String> = self
            .conn
            .query_row(
                "SELECT session_id FROM operator_messages WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        session
            .map(|session| {
                Uuid::parse_str(&session).map_err(|error| DaemonError::Store(error.to_string()))
            })
            .transpose()
    }

    /// Settle a boundary-delivered operator message `delivered` AND write its
    /// operator-labelled transcript event in ONE transaction (#1062). Either
    /// both are durable or neither is: a crash leaves the row `effect_possible`
    /// (reconciled to `uncertain`, #945) with no event, never a delivered row
    /// with no transcript line, and a retry cannot write a second event because
    /// only an `effect_possible` row is settleable.
    ///
    /// `sequence` is the monitor's next per-session sequence, allocated by the
    /// monitor that owns the counter. `None` (no live monitor for the session,
    /// so no racing writer exists) allocates `MAX(sequence)+1` inside this
    /// transaction.
    pub fn deliver_operator_message_with_transcript(
        &mut self,
        id: Uuid,
        sequence: Option<i32>,
    ) -> Result<ConversationEvent> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let (session_id, content): (String, String) = tx
            .query_row(
                "SELECT session_id,content FROM operator_messages WHERE id=?1 AND state='effect_possible'",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| {
                DaemonError::InvalidParam("operator message effect is no longer settleable".into())
            })?;
        let session_uuid =
            Uuid::parse_str(&session_id).map_err(|error| DaemonError::Store(error.to_string()))?;
        let sequence = match sequence {
            Some(sequence) => sequence,
            None => tx.query_row(
                "SELECT COALESCE(MAX(sequence),0)+1 FROM conversation_events WHERE session_id=?1",
                [&session_id],
                |row| row.get(0),
            )?,
        };
        let mut event = ConversationEvent {
            id: 0,
            session_id: session_uuid,
            sequence,
            event_type: EventType::Message,
            role: Some(Role::User),
            content,
            tool_name: None,
            tool_input: None,
            created_at: Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: Some(Box::new(serde_json::json!({
                "source": rsi_common::types::OPERATOR_EVENT_SOURCE,
                "operator_message_id": id,
                "delivery": "tool_boundary",
            }))),
        };
        event.id = Self::insert_event_in_transaction(&tx, &event, None)?;
        let timestamp = now();
        let changed = tx.execute(
            "UPDATE operator_messages SET state='delivered',updated_at=?1,delivered_at=?1
             WHERE id=?2 AND state='effect_possible'",
            params![timestamp, id.to_string()],
        )?;
        if changed != 1 {
            return Err(DaemonError::InvalidParam(
                "operator message effect is no longer settleable".into(),
            ));
        }
        tx.commit()?;
        Ok(event)
    }

    pub fn next_terminal_operator_message_session(&self) -> Result<Option<Uuid>> {
        let id: Option<String> = self.conn.query_row(
            "SELECT m.session_id FROM operator_messages m JOIN sessions s ON s.id=m.session_id
             WHERE m.state='queued' AND s.status IN ('Completed','Failed','Interrupted')
               AND NOT EXISTS (SELECT 1 FROM operator_messages d WHERE d.session_id=m.session_id AND d.state IN ('dispatching','effect_possible','uncertain'))
             ORDER BY m.created_at,m.id LIMIT 1", [], |row| row.get(0),
        ).optional()?;
        id.map(|id| Uuid::parse_str(&id).map_err(|error| DaemonError::Store(error.to_string())))
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests::make_test_session;
    use rsi_common::types::SessionStatus;

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn operator_queue_edit_withdraw_and_restart_preserve_fifo() {
        let directory = tempfile::tempdir().expect("test directory");
        let path = directory.path().join("operator-messages.db");
        let mut store = Store::open(&path).expect("open store");
        let mut session = make_test_session();
        session.status = SessionStatus::Running;
        store.insert_session(&session).expect("insert session");

        let first = store
            .queue_operator_message(session.id, "first", "first-key")
            .expect("queue first");
        let second = store
            .queue_operator_message(session.id, "second", "second-key")
            .expect("queue second");
        let edited = store
            .edit_operator_message(first.id, "first edited")
            .expect("edit queued message");
        assert_eq!(edited.content, "first edited");
        let retried = store
            .queue_operator_message(session.id, "first", "first-key")
            .expect("exact acceptance retry");
        assert_eq!(retried.id, first.id);
        assert_eq!(retried.content, "first edited");
        assert_eq!(
            store
                .list_operator_messages(session.id)
                .expect("list")
                .len(),
            2
        );

        drop(store);
        let mut store = Store::open(&path).expect("reopen store");
        let claimed = store
            .claim_operator_message(session.id)
            .expect("claim")
            .expect("first row");
        assert_eq!(claimed.id, first.id);
        assert_eq!(claimed.content, "first edited");
        assert!(
            store
                .claim_operator_message(session.id)
                .expect("blocked claim")
                .is_none()
        );
        store
            .mark_operator_message_effect_possible(first.id)
            .expect("cross effect boundary");
        store
            .settle_operator_message(first.id, true)
            .expect("record delivery");
        let claimed = store
            .claim_operator_message(session.id)
            .expect("claim second")
            .expect("second row");
        assert_eq!(claimed.id, second.id);
        store
            .mark_operator_message_effect_possible(second.id)
            .expect("cross effect boundary");
        store
            .settle_operator_message(second.id, false)
            .expect("record uncertainty");
        let third = store
            .queue_operator_message(session.id, "third", "third-key")
            .expect("queue third");
        assert!(
            store
                .claim_operator_message(session.id)
                .expect("uncertain predecessor blocks")
                .is_none()
        );
        let withdrawn = store
            .withdraw_operator_message(third.id)
            .expect("withdraw queued third");
        assert_eq!(withdrawn.state, "withdrawn");
        let rows = store
            .list_operator_messages(session.id)
            .expect("list durable outcomes");
        assert_eq!(
            rows.iter()
                .map(|row| row.state.as_str())
                .collect::<Vec<_>>(),
            ["delivered", "uncertain", "withdrawn"]
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn terminal_cli_pre_effect_claim_requeues_after_restart() {
        let directory = tempfile::tempdir().expect("test directory");
        let path = directory.path().join("terminal-operator-message.db");
        let mut store = Store::open(&path).expect("open store");
        let mut session = make_test_session();
        session.status = SessionStatus::Running;
        store.insert_session(&session).expect("insert session");
        let message = store
            .queue_operator_message(session.id, "after tool completes", "cli-key")
            .expect("queue");
        assert_eq!(
            store
                .next_terminal_operator_message_session()
                .expect("scan"),
            None
        );
        store
            .update_session_status(session.id, SessionStatus::Completed)
            .expect("finish current turn");
        drop(store);

        let mut store = Store::open(&path).expect("reopen after daemon restart");
        assert_eq!(
            store
                .next_terminal_operator_message_session()
                .expect("scan"),
            Some(session.id)
        );
        assert_eq!(
            store
                .claim_operator_message(session.id)
                .expect("claim")
                .expect("message")
                .id,
            message.id
        );
        assert_eq!(
            store
                .next_terminal_operator_message_session()
                .expect("second scan"),
            None
        );
        drop(store);
        let mut store = Store::open(&path).expect("reopen during dispatch");
        assert_eq!(
            store
                .next_terminal_operator_message_session()
                .expect("restart scan"),
            None
        );
        assert_eq!(
            store
                .list_operator_messages(session.id)
                .expect("read state")[0]
                .state,
            "dispatching"
        );
        assert_eq!(
            store
                .reconcile_operator_messages_at_startup()
                .expect("startup recovery"),
            (1, 0)
        );
        assert_eq!(
            store
                .next_terminal_operator_message_session()
                .expect("recovered scan"),
            Some(session.id)
        );
        let recovered = store
            .claim_operator_message(session.id)
            .expect("reclaim")
            .expect("one retryable row");
        assert_eq!(recovered.id, message.id);
        store
            .settle_operator_message(message.id, false)
            .expect("pre-effect refusal returns to queue");
        assert_eq!(
            store
                .claim_operator_message(session.id)
                .expect("claim after refusal")
                .expect("same message")
                .id,
            message.id
        );
        store
            .mark_operator_message_effect_possible(message.id)
            .expect("effect boundary");
        store
            .settle_operator_message(message.id, true)
            .expect("provider accepted one turn");
        assert_eq!(
            store
                .next_terminal_operator_message_session()
                .expect("no second turn"),
            None
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn post_effect_crash_preserves_uncertainty_and_blocks_replay() {
        let directory = tempfile::tempdir().expect("test directory");
        let path = directory.path().join("operator-post-effect.db");
        let mut store = Store::open(&path).expect("open store");
        let mut session = make_test_session();
        session.status = SessionStatus::Completed;
        store.insert_session(&session).expect("insert session");
        let first = store
            .queue_operator_message(session.id, "possibly sent", "first")
            .expect("queue first");
        store
            .queue_operator_message(session.id, "next", "second")
            .expect("queue second");
        store
            .claim_operator_message(session.id)
            .expect("claim")
            .expect("first message");
        store
            .mark_operator_message_effect_possible(first.id)
            .expect("provider boundary");
        drop(store);

        let mut store = Store::open(&path).expect("restart");
        assert_eq!(
            store
                .reconcile_operator_messages_at_startup()
                .expect("reconcile"),
            (0, 1)
        );
        assert_eq!(
            store.list_operator_messages(session.id).expect("rows")[0].state,
            "uncertain"
        );
        assert!(
            store
                .claim_operator_message(session.id)
                .expect("blocked retry")
                .is_none()
        );
        assert_eq!(
            store
                .next_terminal_operator_message_session()
                .expect("blocked dispatcher"),
            None
        );
    }
}
