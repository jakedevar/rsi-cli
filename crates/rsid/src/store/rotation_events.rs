use crate::error::Result;
use crate::store::Store;
use rusqlite::OptionalExtension;
use uuid::Uuid;

/// An entered rotation intent that never reached a terminal decision
/// (plan INV-1). Only restart recovery consumes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenRotationIntent {
    pub rotation_id: String,
    /// `writing_handoff` or `pending_interrupt`.
    pub phase: String,
    /// RFC3339 timestamp of the entered event.
    pub entered_at: String,
    /// Handoff-turn start commit, recorded by Slice 2 in the
    /// `writing_handoff/entered` metadata when present.
    pub start_head: Option<String>,
}

impl Store {
    pub fn insert_rotation_event(
        &self,
        session_id: Uuid,
        rotation_id: &str,
        phase: &str,
        event_type: &str,
        metadata: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO rotation_events (session_id, rotation_id, phase, event_type, metadata, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                session_id.to_string(),
                rotation_id,
                phase,
                event_type,
                metadata,
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Durable single-owner claim of an open rotation intent by restart
    /// recovery: one non-terminal `recovery_claimed` row per
    /// (session, rotation). Idempotent. A claimed intent stays recoverable on
    /// every later boot until it reaches a terminal decision, even after its
    /// predecessor is no longer reconciled from a live status (review round 2
    /// `rotation_recovery_second_crash`).
    ///
    /// # Errors
    /// Returns an error when `SQLite` rejects the insert.
    pub(crate) fn claim_open_rotation_intent_for_recovery(
        &self,
        session_id: Uuid,
        intent: &OpenRotationIntent,
    ) -> Result<bool> {
        let inserted = self.conn.execute(
            "INSERT INTO rotation_events (session_id, rotation_id, phase, event_type, metadata, created_at)
             SELECT ?1, ?2, ?3, 'recovery_claimed', NULL, ?4
             WHERE NOT EXISTS(SELECT 1 FROM rotation_events WHERE session_id=?1
                 AND rotation_id=?2 AND event_type='recovery_claimed')",
            rusqlite::params![
                session_id.to_string(),
                intent.rotation_id,
                intent.phase,
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            ],
        )?;
        Ok(inserted == 1)
    }

    /// Predecessors whose recovery-claimed rotation has no terminal decision.
    /// Unclaimed legacy intents are deliberately excluded: only rows restore
    /// reconciled from a live status enter recovery for the first time.
    ///
    /// # Errors
    /// Returns an error when the query fails or a stored id is malformed.
    pub(crate) fn recovery_claimed_open_rotation_sessions(&self) -> Result<Vec<Uuid>> {
        let mut statement = self.conn.prepare(
            "SELECT DISTINCT c.session_id FROM rotation_events c
             WHERE c.event_type='recovery_claimed'
               AND NOT EXISTS(SELECT 1 FROM rotation_events t
                   WHERE t.session_id=c.session_id AND t.rotation_id=c.rotation_id
                     AND (t.event_type IN ('completed','suppressed_final_handoff','aborted_empty_session')
                          OR t.event_type LIKE 'refused:%' OR t.phase='depth_limit_hit'))
             ORDER BY c.session_id",
        )?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.iter()
            .map(|id| {
                Uuid::parse_str(id)
                    .map_err(|error| crate::error::DaemonError::Store(error.to_string()))
            })
            .collect()
    }

    /// The session's latest rotation (by newest event row) when it entered
    /// `writing_handoff` or `pending_interrupt` and has no terminal decision:
    /// `completed`, `suppressed_final_handoff`, `refused:<code>`, an
    /// abandoned handoff write (`aborted_empty_session`), or a depth-limit
    /// stop. Read-only.
    ///
    /// # Errors
    /// Fails on `SQLite` errors.
    pub(crate) fn latest_open_rotation_intent(
        &self,
        session_id: Uuid,
    ) -> Result<Option<OpenRotationIntent>> {
        let session = session_id.to_string();
        let Some(rotation_id) = self
            .conn
            .query_row(
                "SELECT rotation_id FROM rotation_events WHERE session_id=?1
                 ORDER BY id DESC LIMIT 1",
                [&session],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        else {
            return Ok(None);
        };
        let closed: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM rotation_events WHERE session_id=?1 AND rotation_id=?2
               AND (event_type IN ('completed','suppressed_final_handoff','aborted_empty_session')
                    OR event_type LIKE 'refused:%' OR phase='depth_limit_hit'))",
            rusqlite::params![session, rotation_id],
            |row| row.get(0),
        )?;
        if closed {
            return Ok(None);
        }
        let entered = self
            .conn
            .query_row(
                "SELECT phase, created_at, metadata FROM rotation_events
                 WHERE session_id=?1 AND rotation_id=?2 AND event_type='entered'
                   AND phase IN ('writing_handoff','pending_interrupt')
                 ORDER BY id DESC LIMIT 1",
                rusqlite::params![session, rotation_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        Ok(entered.map(|(phase, entered_at, metadata)| {
            let start_head = metadata
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .and_then(|value| value.get("start_head")?.as_str().map(str::to_string));
            OpenRotationIntent {
                rotation_id,
                phase,
                entered_at,
                start_head,
            }
        }))
    }
}
