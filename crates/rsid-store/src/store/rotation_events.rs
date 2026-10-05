use crate::error::{DaemonError, Result};
use crate::store::Store;
use chrono::{DateTime, Utc};
use rsi_common::types::SessionStatus;
use rusqlite::{Connection, OptionalExtension};
use uuid::Uuid;

/// The successor one rotation request started, resolved by reservation
/// identity and never by recency (#1153).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RotationRequestSuccessor {
    /// The request started no successor: nothing it reserved exists.
    None,
    /// The one successor the request's own `successor_reserved` marker names
    /// (whatever its status), or, for a request that has no marker, the one
    /// unclaimed legacy row created since the request.
    Found { id: Uuid, status: SessionStatus },
    /// Several candidates and no way to tell which one is the request's:
    /// the caller fails closed.
    Ambiguous { candidates: usize },
}

/// A rotation recovery blocked on its exact reserved successor (#1158).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedRotation {
    pub predecessor: Uuid,
    pub rotation_id: String,
    pub successor: Uuid,
}

/// The `entered` phase of a rotation that starts from an idle (`Completed`)
/// predecessor: there is no handoff turn to write, so the intent is open from
/// the trigger until the decider's terminal event (#1149).
pub const COMPLETED_TRIGGER_PHASE: &str = "completed_trigger";

fn parse_uuid(raw: &str) -> Result<Uuid> {
    Uuid::parse_str(raw).map_err(|error| DaemonError::Store(error.to_string()))
}

fn parse_status(raw: String) -> Result<SessionStatus> {
    Ok(serde_json::from_value(serde_json::Value::String(raw))?)
}

/// The distinct successors `rotation_id` of `predecessor` durably reserved
/// (`successor_reserved{successor_id}` markers), oldest first.
fn reserved_successors_on(
    conn: &Connection,
    predecessor: Uuid,
    rotation_id: &str,
) -> Result<Vec<String>> {
    let mut statement = conn.prepare(
        "SELECT json_extract(metadata,'$.successor_id') FROM rotation_events
         WHERE session_id=?1 AND rotation_id=?2 AND event_type='successor_reserved'
           AND json_valid(metadata) AND json_extract(metadata,'$.successor_id') IS NOT NULL
         ORDER BY id",
    )?;
    let mut ids: Vec<String> = statement
        .query_map(
            rusqlite::params![predecessor.to_string(), rotation_id],
            |row| row.get::<_, String>(0),
        )?
        .collect::<std::result::Result<_, _>>()?;
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// Why `successor` may not be published as the successor of `rotation_id`,
/// read inside the publication transaction (#1153): when the rotation
/// reserved a successor, only that row is publishable under it; when it
/// reserved none, a row another rotation or an agent reservation owns is not.
/// `None`: the reservation identity holds.
pub(crate) fn rotation_successor_identity_denied_on(
    conn: &Connection,
    predecessor: Uuid,
    successor: Uuid,
    rotation_id: &str,
) -> Result<Option<&'static str>> {
    let own = reserved_successors_on(conn, predecessor, rotation_id)?;
    if !own.is_empty() {
        return Ok(if own.iter().any(|id| *id == successor.to_string()) {
            None
        } else {
            Some("reserved_another_successor")
        });
    }
    let owned_elsewhere: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM rotation_events e
                       WHERE e.session_id=?1 AND e.event_type='successor_reserved'
                         AND json_valid(e.metadata)
                         AND json_extract(e.metadata,'$.successor_id')=?2)
            OR EXISTS(SELECT 1 FROM agent_successor_reservations r
                      WHERE r.candidate_session_id=?2)",
        rusqlite::params![predecessor.to_string(), successor.to_string()],
        |row| row.get(0),
    )?;
    Ok(owned_elsewhere.then_some("reserved_by_another_owner"))
}

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

    /// #1149: make the rotation `rotation_id` of the idle `session_id` a
    /// durable open intent before its decider can reserve a successor, so a
    /// crash anywhere between the trigger and the publication leaves an
    /// intent restart recovery owns. Idempotent: a replay of the same
    /// rotation (a re-dispatched cap request) keeps its first intent.
    /// Returns whether this call wrote it.
    ///
    /// # Errors
    /// Returns an error when `SQLite` rejects the insert.
    pub fn record_completed_trigger_intent(
        &self,
        session_id: Uuid,
        rotation_id: &str,
        trigger: &str,
    ) -> Result<bool> {
        let inserted = self.conn.execute(
            "INSERT INTO rotation_events (session_id, rotation_id, phase, event_type, metadata, created_at)
             SELECT ?1, ?2, ?3, 'entered', ?4, ?5
             WHERE NOT EXISTS(SELECT 1 FROM rotation_events WHERE session_id=?1
                 AND rotation_id=?2 AND event_type='entered')",
            rusqlite::params![
                session_id.to_string(),
                rotation_id,
                COMPLETED_TRIGGER_PHASE,
                serde_json::json!({ "trigger": trigger }).to_string(),
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            ],
        )?;
        Ok(inserted == 1)
    }

    /// Whether `successor`, a `continued_from` child of `predecessor`, holds
    /// the predecessor's transferred sandbox custody: its row names a custody
    /// root that it owns. Such a successor is bound, so its custody moves only
    /// forward; recovery never closes the rotation as if nothing had moved
    /// (#1156). An ordinary (unsandboxed) successor holds no custody.
    ///
    /// # Errors
    /// Returns an error when the query fails.
    pub fn rotation_successor_holds_transferred_custody(
        &self,
        predecessor: Uuid,
        successor: Uuid,
    ) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sessions s
                 JOIN sandbox_custody_roots r ON r.custody_id=s.sandbox_custody_id
                 WHERE s.id=?1 AND s.continued_from=?2 AND r.owner_session_id=s.id)",
            rusqlite::params![successor.to_string(), predecessor.to_string()],
            |row| row.get(0),
        )?)
    }

    /// Record, once, that recovery of `rotation_id` is blocked on
    /// `successor`'s bound custody (a non-terminal `recovery_blocked` event).
    /// Returns whether this call recorded it, so the escalation fires once.
    ///
    /// # Errors
    /// Returns an error when `SQLite` rejects the insert.
    pub fn record_rotation_recovery_blocked(
        &self,
        predecessor: Uuid,
        rotation_id: &str,
        successor: Uuid,
    ) -> Result<bool> {
        let inserted = self.conn.execute(
            "INSERT INTO rotation_events (session_id, rotation_id, phase, event_type, metadata, created_at)
             SELECT ?1, ?2, 'reserved', 'recovery_blocked', ?3, ?4
             WHERE NOT EXISTS(SELECT 1 FROM rotation_events WHERE session_id=?1
                 AND rotation_id=?2 AND event_type='recovery_blocked')",
            rusqlite::params![
                predecessor.to_string(),
                rotation_id,
                serde_json::json!({ "successor_id": successor }).to_string(),
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            ],
        )?;
        Ok(inserted == 1)
    }

    /// The rotation of `predecessor` that recovery blocked on its exact
    /// reserved successor (#1158): the latest open intent, whose one
    /// reservation names a `Failed` successor that holds the predecessor's
    /// transferred sandbox custody and whose `recovery_blocked` event names
    /// that same successor. `None` for any other state, so operator
    /// resolution acts only on a rotation recovery itself blocked.
    ///
    /// # Errors
    /// Returns an error when a query fails or a stored id is malformed.
    pub fn blocked_rotation_of(&self, predecessor: Uuid) -> Result<Option<BlockedRotation>> {
        let Some(intent) = self.latest_open_rotation_intent(predecessor)? else {
            return Ok(None);
        };
        let reserved = reserved_successors_on(&self.conn, predecessor, &intent.rotation_id)?;
        let [successor] = reserved.as_slice() else {
            return Ok(None);
        };
        let successor = parse_uuid(successor)?;
        let blocked: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM rotation_events WHERE session_id=?1 AND rotation_id=?2
                 AND event_type='recovery_blocked' AND json_valid(metadata)
                 AND json_extract(metadata,'$.successor_id')=?3)
               AND EXISTS(SELECT 1 FROM sessions WHERE id=?3 AND continued_from=?1
                 AND status='Failed')",
            rusqlite::params![
                predecessor.to_string(),
                intent.rotation_id,
                successor.to_string()
            ],
            |row| row.get(0),
        )?;
        if !blocked || !self.rotation_successor_holds_transferred_custody(predecessor, successor)? {
            return Ok(None);
        }
        Ok(Some(BlockedRotation {
            predecessor,
            rotation_id: intent.rotation_id,
            successor,
        }))
    }

    /// [`Self::blocked_rotation_of`] keyed by the blocked successor: the
    /// rotation `successor` is the exact blocked reservation of, or `None`.
    ///
    /// # Errors
    /// Returns an error when a query fails or a stored id is malformed.
    pub fn blocked_rotation_of_successor(
        &self,
        successor: Uuid,
    ) -> Result<Option<BlockedRotation>> {
        let predecessor: Option<String> = self
            .conn
            .query_row(
                "SELECT continued_from FROM sessions WHERE id=?1",
                [successor.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(predecessor) = predecessor else {
            return Ok(None);
        };
        Ok(self
            .blocked_rotation_of(parse_uuid(&predecessor)?)?
            .filter(|blocked| blocked.successor == successor))
    }

    /// The rotation whose blocked reservation the operator's Continue already
    /// published (#1180): the terminal `completed` event of `successor`'s
    /// predecessor names `successor`, that rotation's `recovery_blocked` event
    /// names the same successor, and `successor` is `Failed` and still holds
    /// the seat's transferred sandbox custody. Publication commits as soon as
    /// the continuation installs its provider, which can precede the first
    /// provider event: a crash then leaves exactly this state (no provider
    /// thread, nothing to replay), and the operator's next Continue must be
    /// able to start the seat again (custody never moves back to the
    /// predecessor). `None` for every other state, so the authorization lapses
    /// once the successor is not `Failed` and ends once a provider thread or
    /// transcript effect exists (the launch gate checks both).
    ///
    /// # Errors
    /// Returns an error when a query fails or a stored id is malformed.
    pub fn published_blocked_rotation_of_successor(
        &self,
        successor: Uuid,
    ) -> Result<Option<BlockedRotation>> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT c.session_id, c.rotation_id FROM rotation_events c
                 JOIN sessions s ON s.id=?1 AND s.continued_from=c.session_id
                      AND s.status='Failed'
                 WHERE c.event_type='completed' AND json_valid(c.metadata)
                   AND json_extract(c.metadata,'$.successor_id')=?1
                   AND EXISTS(SELECT 1 FROM rotation_events b
                       WHERE b.session_id=c.session_id AND b.rotation_id=c.rotation_id
                         AND b.event_type='recovery_blocked' AND json_valid(b.metadata)
                         AND json_extract(b.metadata,'$.successor_id')=?1)
                 ORDER BY c.id DESC LIMIT 1",
                [successor.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((predecessor, rotation_id)) = row else {
            return Ok(None);
        };
        let predecessor = parse_uuid(&predecessor)?;
        if !self.rotation_successor_holds_transferred_custody(predecessor, successor)? {
            return Ok(None);
        }
        Ok(Some(BlockedRotation {
            predecessor,
            rotation_id,
            successor,
        }))
    }

    /// Whether `(predecessor, rotation_id)` is already settled by a published
    /// `completed` event naming exactly `successor` (#1180): a second
    /// publication of the same reservation is then a success, never a
    /// settlement conflict. Any other terminal state, or another successor, is
    /// `false`.
    ///
    /// # Errors
    /// Returns an error when the query fails.
    pub fn rotation_published_successor_is(
        &self,
        predecessor: Uuid,
        rotation_id: &str,
        successor: Uuid,
    ) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM rotation_events WHERE session_id=?1 AND rotation_id=?2
                 AND event_type='completed' AND json_valid(metadata)
                 AND json_extract(metadata,'$.successor_id')=?3)",
            rusqlite::params![predecessor.to_string(), rotation_id, successor.to_string()],
            |row| row.get(0),
        )?)
    }

    /// The trigger recorded with `rotation_id`'s completed-trigger intent
    /// (`manual_triggered` or `cap_triggered`): immutable provenance of who
    /// started the rotation. `None` for a rotation with no such intent.
    ///
    /// # Errors
    /// Returns an error when the query fails.
    pub fn rotation_intent_trigger(
        &self,
        session_id: Uuid,
        rotation_id: &str,
    ) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT json_extract(metadata,'$.trigger') FROM rotation_events
                 WHERE session_id=?1 AND rotation_id=?2 AND event_type='entered'
                   AND phase='completed_trigger' AND json_valid(metadata)
                 ORDER BY id LIMIT 1",
                rusqlite::params![session_id.to_string(), rotation_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Close the still-open intent of `rotation_id` with the terminal
    /// `refused:<code>` event, in the caller's transaction when it has one:
    /// the rotation was superseded or cancelled and must never be recovered or
    /// replayed (#1149). A no-op (`false`) when the rotation has no intent or
    /// already has a terminal event.
    ///
    /// # Errors
    /// Returns an error when `SQLite` rejects the insert.
    pub fn close_open_rotation_intent(
        &self,
        session_id: Uuid,
        rotation_id: &str,
        code: &str,
    ) -> Result<bool> {
        let inserted = self.conn.execute(
            "INSERT INTO rotation_events (session_id, rotation_id, phase, event_type, metadata, created_at)
             SELECT ?1, ?2, 'completed', ?3, NULL, ?4
             WHERE EXISTS(SELECT 1 FROM rotation_events WHERE session_id=?1
                 AND rotation_id=?2 AND event_type='entered')
               AND NOT EXISTS(SELECT 1 FROM rotation_events t
                   WHERE t.session_id=?1 AND t.rotation_id=?2
                     AND (t.event_type IN ('completed','suppressed_final_handoff','aborted_empty_session')
                          OR t.event_type LIKE 'refused:%' OR t.phase='depth_limit_hit'))",
            rusqlite::params![
                session_id.to_string(),
                rotation_id,
                format!("refused:{code}"),
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            ],
        )?;
        Ok(inserted == 1)
    }

    /// Sessions with an open completed-trigger intent (#1149): restart
    /// recovery takes these on first sight, since their predecessor is
    /// `Completed` and is never reconciled from a live status.
    ///
    /// # Errors
    /// Returns an error when the query fails or a stored id is malformed.
    pub fn open_completed_trigger_rotation_sessions(&self) -> Result<Vec<Uuid>> {
        let mut statement = self.conn.prepare(
            "SELECT DISTINCT c.session_id FROM rotation_events c
             WHERE c.event_type='entered' AND c.phase='completed_trigger'
               AND NOT EXISTS(SELECT 1 FROM rotation_events t
                   WHERE t.session_id=c.session_id AND t.rotation_id=c.rotation_id
                     AND (t.event_type IN ('completed','suppressed_final_handoff','aborted_empty_session')
                          OR t.event_type LIKE 'refused:%' OR t.phase='depth_limit_hit'))
             ORDER BY c.session_id",
        )?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ids.iter().map(|id| parse_uuid(id)).collect()
    }

    /// Whether restart recovery has claimed the still-open intent of
    /// `rotation_id`: it, not the cap pass, then owns the request's effect.
    ///
    /// # Errors
    /// Returns an error when the query fails.
    pub fn rotation_intent_claimed_open(
        &self,
        session_id: Uuid,
        rotation_id: &str,
    ) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM rotation_events c
                 WHERE c.session_id=?1 AND c.rotation_id=?2 AND c.event_type='recovery_claimed')
               AND NOT EXISTS(SELECT 1 FROM rotation_events t
                 WHERE t.session_id=?1 AND t.rotation_id=?2
                   AND (t.event_type IN ('completed','suppressed_final_handoff','aborted_empty_session')
                        OR t.event_type LIKE 'refused:%' OR t.phase='depth_limit_hit'))",
            rusqlite::params![session_id.to_string(), rotation_id],
            |row| row.get(0),
        )?)
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
    pub fn claim_open_rotation_intent_for_recovery(
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
    pub fn recovery_claimed_open_rotation_sessions(&self) -> Result<Vec<Uuid>> {
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

    /// The successor the rotation request `rotation_id` of `predecessor`
    /// started, by reservation identity (#1153). The request's own
    /// `successor_reserved` marker names it exactly, whatever the row's status
    /// (a reserved child that a restart moved to `Failed` is still the
    /// request's effect), and no timestamp candidate overrides it. Only a
    /// request with no marker falls back to the unclaimed legacy rows created
    /// at or after `since`; rows reserved by another rotation or an agent
    /// reservation never qualify, and several candidates are `Ambiguous`.
    ///
    /// # Errors
    /// Fails on `SQLite` errors, a malformed row, or a marker that names a row
    /// that is not a `continued_from` child of `predecessor`.
    pub fn rotation_request_successor(
        &self,
        predecessor: Uuid,
        rotation_id: &str,
        since: Option<DateTime<Utc>>,
    ) -> Result<RotationRequestSuccessor> {
        let reserved = reserved_successors_on(&self.conn, predecessor, rotation_id)?;
        match reserved.as_slice() {
            [] => {}
            [id] => {
                let status: Option<String> = self
                    .conn
                    .query_row(
                        "SELECT status FROM sessions WHERE id=?1 AND continued_from=?2",
                        rusqlite::params![id, predecessor.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(status) = status else {
                    return Err(DaemonError::Store(format!(
                        "rotation_reserved_successor_missing:{predecessor}:{id}"
                    )));
                };
                return Ok(RotationRequestSuccessor::Found {
                    id: parse_uuid(id)?,
                    status: parse_status(status)?,
                });
            }
            many => {
                return Ok(RotationRequestSuccessor::Ambiguous {
                    candidates: many.len(),
                });
            }
        }
        let mut statement = self.conn.prepare(
            "SELECT s.id, s.status, s.created_at FROM sessions s WHERE s.continued_from=?1
               AND NOT EXISTS(SELECT 1 FROM rotation_events e
                              WHERE e.session_id=?1 AND e.event_type='successor_reserved'
                                AND json_valid(e.metadata)
                                AND json_extract(e.metadata,'$.successor_id')=s.id)
               AND NOT EXISTS(SELECT 1 FROM agent_successor_reservations r
                              WHERE r.candidate_session_id=s.id)
             ORDER BY s.created_at DESC",
        )?;
        let rows = statement
            .query_map([predecessor.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut candidates = Vec::new();
        for (id, status, created_at) in rows {
            let created = crate::store::parse_timestamp(&created_at).map_err(DaemonError::Store)?;
            if since.is_none_or(|since| created >= since) {
                candidates.push((id, status));
            }
        }
        match candidates.len() {
            0 => Ok(RotationRequestSuccessor::None),
            1 => {
                let (id, status) = candidates.remove(0);
                Ok(RotationRequestSuccessor::Found {
                    id: parse_uuid(&id)?,
                    status: parse_status(status)?,
                })
            }
            candidates => Ok(RotationRequestSuccessor::Ambiguous { candidates }),
        }
    }

    /// The successor a rotation of `predecessor` durably published: the exact
    /// `completed{successor_id}` witness of `rotation_id`, else (`None`, a
    /// record from before cap rotations carried an id) the lineage's published
    /// tip with its explicit legacy provenance rules. A reserved, failed or
    /// merely archived-parent child is never a witness (#1142).
    ///
    /// # Errors
    /// Fails on `SQLite` errors or a malformed UUID.
    pub fn published_rotation_successor_of(
        &self,
        predecessor: Uuid,
        rotation_id: Option<&str>,
    ) -> Result<Option<Uuid>> {
        let Some(rotation_id) = rotation_id else {
            return self.find_published_rotation_successor(predecessor);
        };
        let witnessed: Option<String> = self
            .conn
            .query_row(
                "SELECT json_extract(e.metadata,'$.successor_id') FROM rotation_events e
                 WHERE e.session_id=?1 AND e.rotation_id=?2 AND e.event_type='completed'
                   AND json_valid(e.metadata)
                   AND json_extract(e.metadata,'$.successor_id') IS NOT NULL
                   AND EXISTS(SELECT 1 FROM sessions s
                              WHERE s.id=json_extract(e.metadata,'$.successor_id')
                                AND s.continued_from=?1)
                 ORDER BY e.id DESC LIMIT 1",
                rusqlite::params![predecessor.to_string(), rotation_id],
                |row| row.get(0),
            )
            .optional()?;
        witnessed
            .map(|id| {
                Uuid::parse_str(&id)
                    .map_err(|error| crate::error::DaemonError::Store(error.to_string()))
            })
            .transpose()
    }

    /// The session's latest rotation (by newest event row) when it entered
    /// `writing_handoff` or `pending_interrupt` and has no terminal decision:
    /// `completed`, `suppressed_final_handoff`, `refused:<code>`, an
    /// abandoned handoff write (`aborted_empty_session`), or a depth-limit
    /// stop. Read-only.
    ///
    /// # Errors
    /// Fails on `SQLite` errors.
    pub fn latest_open_rotation_intent(
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
                   AND phase IN ('writing_handoff','pending_interrupt','completed_trigger')
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
