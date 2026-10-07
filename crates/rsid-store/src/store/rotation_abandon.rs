//! #1176: the operator's abandon of a blocked rotation.
//!
//! A rotation of a sandboxed seat P (rotation `X`) is *blocked* (#1156) when
//! its reserved successor S was bound to the seat's transferred sandbox
//! custody and then never started: custody moves only forward, so the seat
//! stays on P while S owns the sandbox. The abandon rotates the blocked
//! *holder* forward: a fresh replacement R becomes a rotation successor of the
//! holder (`continued_from` = holder, custody transferred with cause
//! `rotation`), and R's publication publishes the whole chain P → S (→ …) → R
//! in one commit. The durable record is an `abandon_requested` rotation event
//! on `(P, X)`; no schema change is needed (`rotation_events` has no CHECK on
//! its event type, metadata carries the idempotency key).
use crate::error::{DaemonError, Result};
use crate::store::Store;
use rsi_common::types::{SessionProvider, SessionStatus};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use uuid::Uuid;

/// The `rotation_events.event_type` of an operator abandon request, recorded
/// on the blocked intent `(P, X)`.
pub const ABANDON_REQUESTED: &str = "abandon_requested";
/// The terminal refusal recorded on an abandon rotation `(holder, Y)` that a
/// daemon restart interrupted before it reserved or started its replacement.
pub const ABANDON_INTERRUPTED: &str = "abandon_interrupted";
/// Bound on every `continued_from` walk: a chain is a handful of hops.
const CHAIN_LIMIT: usize = 32;

/// A blocked rotation and the session that holds its sandbox now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedRotation {
    /// The seat P whose rotation is blocked.
    pub predecessor: Uuid,
    /// The blocked rotation `X` of P.
    pub rotation_id: String,
    /// The successor S that `X` reserved and that never started.
    pub reserved_successor: Uuid,
    /// The current custody owner: S, or the newest replacement an earlier
    /// abandon bound before it, too, failed to start.
    pub holder: Uuid,
    pub holder_status: SessionStatus,
}

/// One durable operator abandon request of a blocked rotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationAbandonRequest {
    pub predecessor: Uuid,
    pub rotation_id: String,
    pub seq: i64,
    /// The custody holder the replacement rotates from.
    pub holder_id: Uuid,
    /// The rotation id `Y` of the holder's rotation to the replacement.
    pub abandon_rotation_id: String,
    pub idempotency_key: String,
    pub provider: Option<SessionProvider>,
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RotationAbandonAdmission {
    /// A new request: the caller dispatches the holder's rotation.
    Admitted(RotationAbandonRequest),
    /// The same idempotency key was already recorded: nothing new happens.
    Replayed(RotationAbandonRequest),
}

/// One unpublished rotation hop `from → to` under `rotation_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationAbandonHop {
    pub from: Uuid,
    pub rotation_id: String,
    pub to: Uuid,
}

/// The hops an abandon publication settles before the holder's own hop to
/// the replacement: P → S, then each earlier abandon's holder → replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationAbandonChain {
    pub request: RotationAbandonRequest,
    pub hops: Vec<RotationAbandonHop>,
}

fn parse_uuid(raw: &str) -> Result<Uuid> {
    Uuid::parse_str(raw).map_err(|error| DaemonError::Store(error.to_string()))
}

fn parse_status(raw: String) -> Result<SessionStatus> {
    Ok(serde_json::from_value(serde_json::Value::String(raw))?)
}

fn rotation_settled_on(conn: &Connection, session_id: Uuid, rotation_id: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM rotation_events WHERE session_id=?1 AND rotation_id=?2
           AND (event_type IN ('completed','suppressed_final_handoff','aborted_empty_session')
                OR event_type LIKE 'refused:%' OR phase='depth_limit_hit'))",
        params![session_id.to_string(), rotation_id],
        |row| row.get(0),
    )?)
}

fn session_status_on(conn: &Connection, session_id: Uuid) -> Result<Option<SessionStatus>> {
    conn.query_row(
        "SELECT status FROM sessions WHERE id=?1",
        [session_id.to_string()],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .map(parse_status)
    .transpose()
}

fn continued_from_on(conn: &Connection, session_id: Uuid) -> Result<Option<Uuid>> {
    conn.query_row(
        "SELECT continued_from FROM sessions WHERE id=?1",
        [session_id.to_string()],
        |row| row.get::<_, Option<String>>(0),
    )
    .optional()?
    .flatten()
    .map(|raw| parse_uuid(&raw))
    .transpose()
}

/// The live owner of the custody root `session_id`'s row names.
fn custody_owner_on(conn: &Connection, session_id: Uuid) -> Result<Option<Uuid>> {
    conn.query_row(
        "SELECT r.owner_session_id FROM sessions s
         JOIN sandbox_custody_roots r ON r.custody_id=s.sandbox_custody_id
         WHERE s.id=?1 AND r.state='live' AND r.owner_session_id IS NOT NULL",
        [session_id.to_string()],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .map(|raw| parse_uuid(&raw))
    .transpose()
}

/// `ancestor`, `descendant`, and every row between them, oldest first, when
/// `descendant` reaches `ancestor` by `continued_from` within the bound.
fn lineage_path_on(
    conn: &Connection,
    ancestor: Uuid,
    descendant: Uuid,
) -> Result<Option<Vec<Uuid>>> {
    let mut path = vec![descendant];
    let mut current = descendant;
    for _ in 0..CHAIN_LIMIT {
        if current == ancestor {
            path.reverse();
            return Ok(Some(path));
        }
        let Some(parent) = continued_from_on(conn, current)? else {
            return Ok(None);
        };
        path.push(parent);
        current = parent;
    }
    Ok(None)
}

/// The successors `(session_id, rotation_id)` durably reserved.
fn reserved_successors_on(
    conn: &Connection,
    session_id: Uuid,
    rotation_id: &str,
) -> Result<Vec<Uuid>> {
    let mut statement = conn.prepare(
        "SELECT DISTINCT json_extract(metadata,'$.successor_id') FROM rotation_events
         WHERE session_id=?1 AND rotation_id=?2 AND event_type='successor_reserved'
           AND json_valid(metadata) AND json_extract(metadata,'$.successor_id') IS NOT NULL",
    )?;
    let ids = statement
        .query_map(params![session_id.to_string(), rotation_id], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ids.iter().map(|raw| parse_uuid(raw)).collect()
}

/// The rotation ids under which `from` reserved `to`.
fn reserving_rotations_on(conn: &Connection, from: Uuid, to: Uuid) -> Result<Vec<String>> {
    let mut statement = conn.prepare(
        "SELECT DISTINCT rotation_id FROM rotation_events
         WHERE session_id=?1 AND event_type='successor_reserved'
           AND json_valid(metadata) AND json_extract(metadata,'$.successor_id')=?2",
    )?;
    let ids = statement
        .query_map(params![from.to_string(), to.to_string()], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(ids)
}

fn parse_request(
    predecessor: &str,
    rotation_id: String,
    metadata: &str,
) -> Result<RotationAbandonRequest> {
    let value: serde_json::Value = serde_json::from_str(metadata)?;
    let text = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| DaemonError::Store(format!("abandon request lacks {key}")))
    };
    let provider = match value.get("provider") {
        None | Some(serde_json::Value::Null) => None,
        Some(provider) => Some(serde_json::from_value(provider.clone())?),
    };
    Ok(RotationAbandonRequest {
        predecessor: parse_uuid(predecessor)?,
        rotation_id,
        seq: value
            .get("seq")
            .and_then(serde_json::Value::as_i64)
            .ok_or_else(|| DaemonError::Store("abandon request lacks seq".into()))?,
        holder_id: parse_uuid(&text("holder_id")?)?,
        abandon_rotation_id: text("abandon_rotation_id")?,
        idempotency_key: text("idempotency_key")?,
        provider,
        model: value
            .get("model")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
}

fn abandon_requests_on(
    conn: &Connection,
    predecessor: Uuid,
    rotation_id: &str,
) -> Result<Vec<RotationAbandonRequest>> {
    let mut statement = conn.prepare(
        "SELECT metadata FROM rotation_events
         WHERE session_id=?1 AND rotation_id=?2 AND event_type='abandon_requested'
           AND json_valid(metadata)
         ORDER BY id",
    )?;
    let rows = statement
        .query_map(params![predecessor.to_string(), rotation_id], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    rows.iter()
        .map(|metadata| parse_request(&predecessor.to_string(), rotation_id.to_string(), metadata))
        .collect()
}

/// The abandon request whose holder rotation is `(holder, abandon_rotation_id)`.
fn abandon_request_for_on(
    conn: &Connection,
    holder: Uuid,
    abandon_rotation_id: &str,
) -> Result<Option<RotationAbandonRequest>> {
    let row = conn
        .query_row(
            "SELECT session_id, rotation_id, metadata FROM rotation_events
             WHERE event_type='abandon_requested' AND json_valid(metadata)
               AND json_extract(metadata,'$.holder_id')=?1
               AND json_extract(metadata,'$.abandon_rotation_id')=?2
             ORDER BY id LIMIT 1",
            params![holder.to_string(), abandon_rotation_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    row.map(|(predecessor, rotation_id, metadata)| {
        parse_request(&predecessor, rotation_id, &metadata)
    })
    .transpose()
}

/// The holder of a blocked rotation whose reserved successor is `reserved`:
/// the live custody owner of the root `reserved` was bound to, provided it is
/// `reserved` itself or one of its `continued_from` descendants.
fn blocked_holder_on(conn: &Connection, reserved: Uuid) -> Result<Option<(Uuid, SessionStatus)>> {
    let Some(owner) = custody_owner_on(conn, reserved)? else {
        return Ok(None);
    };
    if lineage_path_on(conn, reserved, owner)?.is_none() {
        return Ok(None);
    }
    Ok(session_status_on(conn, owner)?.map(|status| (owner, status)))
}

/// The successor `X` of `predecessor` that recovery recorded as blocked
/// (the first `recovery_blocked` names the reserved successor).
fn blocked_successor_on(
    conn: &Connection,
    predecessor: Uuid,
    rotation_id: &str,
) -> Result<Option<Uuid>> {
    conn.query_row(
        "SELECT json_extract(metadata,'$.successor_id') FROM rotation_events
         WHERE session_id=?1 AND rotation_id=?2 AND event_type='recovery_blocked'
           AND json_valid(metadata) AND json_extract(metadata,'$.successor_id') IS NOT NULL
         ORDER BY id LIMIT 1",
        params![predecessor.to_string(), rotation_id],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .map(|raw| parse_uuid(&raw))
    .transpose()
}

/// Whether the abandon request's holder rotation is still running: unsettled
/// and its replacement not yet reserved or still live.
fn abandon_in_flight_on(conn: &Connection, request: &RotationAbandonRequest) -> Result<bool> {
    if rotation_settled_on(conn, request.holder_id, &request.abandon_rotation_id)? {
        return Ok(false);
    }
    let reserved = reserved_successors_on(conn, request.holder_id, &request.abandon_rotation_id)?;
    let Some(replacement) = reserved.first() else {
        return Ok(true);
    };
    Ok(matches!(
        session_status_on(conn, *replacement)?,
        Some(SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingApproval)
    ))
}

/// The chain an abandon publication settles before the holder's own hop.
/// `None` when `(holder, rotation_id)` is not an abandon rotation. Fails
/// closed when the blocked intent is already settled or a hop is ambiguous.
pub(super) fn rotation_abandon_chain_on(
    conn: &Connection,
    holder: Uuid,
    rotation_id: &str,
) -> Result<Option<RotationAbandonChain>> {
    let Some(request) = abandon_request_for_on(conn, holder, rotation_id)? else {
        return Ok(None);
    };
    if rotation_settled_on(conn, request.predecessor, &request.rotation_id)? {
        return Err(DaemonError::PolicyDenied(format!(
            "rotation_already_settled:{}:{}",
            request.predecessor, request.rotation_id
        )));
    }
    let Some(path) = lineage_path_on(conn, request.predecessor, holder)? else {
        return Err(DaemonError::PolicyDenied(format!(
            "rotation_abandon_chain_broken:{}:{holder}",
            request.predecessor
        )));
    };
    let mut hops = Vec::with_capacity(path.len().saturating_sub(1));
    for pair in path.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        let rotation_id = if from == request.predecessor {
            if !reserved_successors_on(conn, from, &request.rotation_id)?.contains(&to) {
                return Err(DaemonError::PolicyDenied(format!(
                    "rotation_abandon_chain_unreserved:{from}:{to}"
                )));
            }
            request.rotation_id.clone()
        } else {
            let mut rotations = reserving_rotations_on(conn, from, to)?;
            if rotations.len() != 1 {
                return Err(DaemonError::PolicyDenied(format!(
                    "rotation_abandon_chain_ambiguous:{from}:{to}"
                )));
            }
            let rotation_id = rotations.remove(0);
            if rotation_settled_on(conn, from, &rotation_id)? {
                return Err(DaemonError::PolicyDenied(format!(
                    "rotation_abandon_chain_settled:{from}:{rotation_id}"
                )));
            }
            rotation_id
        };
        hops.push(RotationAbandonHop {
            from,
            rotation_id,
            to,
        });
    }
    Ok(Some(RotationAbandonChain { request, hops }))
}

/// Fail closed on execution evidence. System diagnostics are daemon-owned;
/// at most one user message equal to the original seed is allowed. A launch
/// attempt without its matching spawn-failure receipt is ambiguous even if
/// the provider died before sending its thread id or first output. Continue
/// admits its durable invocation before dispatch but writes its user event
/// afterwards; only a definite spawn failure proves that invocation safe.
pub(super) fn rotation_holder_never_started_on(conn: &Connection, holder: Uuid) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sessions s WHERE s.id=?1
           AND s.claude_session_id IS NULL AND s.provider_cli_version IS NULL
           AND NOT EXISTS(SELECT 1 FROM conversation_events e WHERE e.session_id=s.id
             AND NOT COALESCE((e.event_type='System' OR
               (e.event_type='Message' AND e.role='User' AND e.content=s.query)), 0))
           AND (SELECT COUNT(*) FROM conversation_events e WHERE e.session_id=s.id
             AND e.event_type='Message' AND e.role='User')<=1
           AND NOT EXISTS(SELECT 1 FROM rotation_events attempt WHERE attempt.session_id=s.id
             AND attempt.event_type='provider_spawn_attempt'
             AND NOT EXISTS(SELECT 1 FROM rotation_events failed WHERE failed.session_id=s.id
               AND failed.rotation_id=attempt.rotation_id AND failed.id>attempt.id
               AND failed.event_type='provider_spawn_failed'
               AND failed.metadata=attempt.metadata))
           AND NOT EXISTS(SELECT 1 FROM model_invocations m WHERE m.session_id=s.id
             AND m.purpose='session.continue.resume' AND m.admission_status='admitted'
             AND NOT COALESCE((m.status='failed' AND m.error_class='spawn_failed'), 0))
           AND NOT EXISTS(SELECT 1 FROM conversation_events e
             JOIN conversation_event_provenance p ON p.conversation_event_id=e.id
             WHERE e.session_id=s.id AND p.producer_kind<>'daemon_provider_diagnostic'))",
        [holder.to_string()],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

impl Store {
    /// The blocked rotation `session_id` belongs to: `session_id` is the seat
    /// P with an open, recovery-blocked intent, or a `continued_from`
    /// descendant of such a P (its reserved successor or a replacement).
    ///
    /// # Errors
    /// Fails on `SQLite` errors or malformed rows.
    pub fn blocked_rotation_chain_of(&self, session_id: Uuid) -> Result<Option<BlockedRotation>> {
        let mut candidate = Some(session_id);
        for _ in 0..CHAIN_LIMIT {
            let Some(predecessor) = candidate else {
                return Ok(None);
            };
            if let Some(blocked) = self.blocked_rotation_at(predecessor)? {
                return Ok(Some(blocked));
            }
            candidate = continued_from_on(&self.conn, predecessor)?;
        }
        Ok(None)
    }

    fn blocked_rotation_at(&self, predecessor: Uuid) -> Result<Option<BlockedRotation>> {
        let Some(intent) = self.latest_open_rotation_intent(predecessor)? else {
            return Ok(None);
        };
        let Some(reserved) = blocked_successor_on(&self.conn, predecessor, &intent.rotation_id)?
        else {
            return Ok(None);
        };
        let Some((holder, holder_status)) = blocked_holder_on(&self.conn, reserved)? else {
            return Ok(None);
        };
        Ok(Some(BlockedRotation {
            predecessor,
            rotation_id: intent.rotation_id,
            reserved_successor: reserved,
            holder,
            holder_status,
        }))
    }

    /// The current custody holder of a blocked rotation whose reserved
    /// successor is `reserved` (see [`BlockedRotation::holder`]).
    ///
    /// # Errors
    /// Fails on `SQLite` errors or malformed rows.
    pub fn blocked_rotation_holder(&self, reserved: Uuid) -> Result<Option<(Uuid, SessionStatus)>> {
        blocked_holder_on(&self.conn, reserved)
    }

    /// Record the operator's abandon of `blocked` under `idempotency_key`, in
    /// one IMMEDIATE transaction that re-proves the blocked state: the intent
    /// is open, the holder still owns the sandbox and is `Failed`. A replay of
    /// the key returns its first request; a different key while an earlier
    /// request's replacement is still being started, or while the caller saw
    /// the holder live in this process (`holder_live`), is refused.
    ///
    /// # Errors
    /// `PolicyDenied` when the rotation changed or an abandon is in flight;
    /// `SQLite` errors otherwise.
    pub fn record_rotation_abandon_request(
        &mut self,
        blocked: &BlockedRotation,
        idempotency_key: &str,
        provider: Option<SessionProvider>,
        model: Option<&str>,
        holder_live: bool,
    ) -> Result<RotationAbandonAdmission> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let requests = abandon_requests_on(&tx, blocked.predecessor, &blocked.rotation_id)?;
        if let Some(replay) = requests
            .iter()
            .find(|request| request.idempotency_key == idempotency_key)
        {
            return Ok(RotationAbandonAdmission::Replayed(replay.clone()));
        }
        if holder_live {
            return Err(DaemonError::PolicyDenied(format!(
                "rotation_holder_live:{}",
                blocked.holder
            )));
        }
        if let Some(newest) = requests.last()
            && abandon_in_flight_on(&tx, newest)?
        {
            return Err(DaemonError::PolicyDenied(format!(
                "rotation_abandon_in_flight:{}:{}",
                newest.holder_id, newest.abandon_rotation_id
            )));
        }
        let unchanged = !rotation_settled_on(&tx, blocked.predecessor, &blocked.rotation_id)?
            && blocked_successor_on(&tx, blocked.predecessor, &blocked.rotation_id)?
                == Some(blocked.reserved_successor)
            && blocked_holder_on(&tx, blocked.reserved_successor)?
                == Some((blocked.holder, SessionStatus::Failed));
        if !unchanged {
            return Err(DaemonError::PolicyDenied(format!(
                "rotation_abandon_changed:{}:{}",
                blocked.predecessor, blocked.rotation_id
            )));
        }
        if !rotation_holder_never_started_on(&tx, blocked.holder)? {
            return Err(DaemonError::PolicyDenied(format!(
                "rotation_holder_execution_evidence:{}",
                blocked.holder
            )));
        }
        let seq = i64::try_from(requests.len()).unwrap_or(i64::MAX - 1) + 1;
        let request = RotationAbandonRequest {
            predecessor: blocked.predecessor,
            rotation_id: blocked.rotation_id.clone(),
            seq,
            holder_id: blocked.holder,
            abandon_rotation_id: format!("{}:abandon:{seq}", blocked.rotation_id),
            idempotency_key: idempotency_key.to_string(),
            provider,
            model: model.map(str::to_string),
        };
        insert_abandon_request_on(&tx, &request)?;
        tx.commit()?;
        Ok(RotationAbandonAdmission::Admitted(request))
    }

    /// The abandon request recorded under `idempotency_key` on `session_id`
    /// or one of its `continued_from` ancestors: a replay after the abandon
    /// already published (the rotation is no longer blocked).
    ///
    /// # Errors
    /// Fails on `SQLite` errors or malformed rows.
    pub fn rotation_abandon_request_by_key(
        &self,
        session_id: Uuid,
        idempotency_key: &str,
    ) -> Result<Option<RotationAbandonRequest>> {
        let mut candidate = Some(session_id);
        for _ in 0..CHAIN_LIMIT {
            let Some(predecessor) = candidate else {
                return Ok(None);
            };
            let row = self
                .conn
                .query_row(
                    "SELECT rotation_id, metadata FROM rotation_events
                     WHERE session_id=?1 AND event_type='abandon_requested' AND json_valid(metadata)
                       AND json_extract(metadata,'$.idempotency_key')=?2
                     ORDER BY id LIMIT 1",
                    params![predecessor.to_string(), idempotency_key],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()?;
            if let Some((rotation_id, metadata)) = row {
                return parse_request(&predecessor.to_string(), rotation_id, &metadata).map(Some);
            }
            candidate = continued_from_on(&self.conn, predecessor)?;
        }
        Ok(None)
    }

    /// The abandon request whose holder rotation is `(holder, rotation_id)`.
    ///
    /// # Errors
    /// Fails on `SQLite` errors or malformed rows.
    pub fn rotation_abandon_request_for(
        &self,
        holder: Uuid,
        rotation_id: &str,
    ) -> Result<Option<RotationAbandonRequest>> {
        abandon_request_for_on(&self.conn, holder, rotation_id)
    }

    /// Whether `holder` is the holder of an abandon request whose blocked
    /// intent is still open: the authority a `Failed` holder needs to be
    /// rotated forward.
    ///
    /// # Errors
    /// Fails on `SQLite` errors or malformed rows.
    pub fn holder_has_open_rotation_abandon(&self, holder: Uuid) -> Result<bool> {
        let mut statement = self.conn.prepare(
            "SELECT session_id, rotation_id FROM rotation_events
             WHERE event_type='abandon_requested' AND json_valid(metadata)
               AND json_extract(metadata,'$.holder_id')=?1",
        )?;
        let intents = statement
            .query_map([holder.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for (predecessor, rotation_id) in intents {
            if !rotation_settled_on(&self.conn, parse_uuid(&predecessor)?, &rotation_id)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// A custody-bearing abandon hop must stay open on launch/publication
    /// failure. The transfer receipt is irreversible, even if subsequent
    /// revalidation quarantines the root. Publishing or another abandon must
    /// settle that exact hop; a non-live root cannot erase the custody commit.
    pub fn rotation_abandon_bound_successor(
        &self,
        holder: Uuid,
        rotation_id: &str,
    ) -> Result<Option<Uuid>> {
        let Some(request) = self.rotation_abandon_request_for(holder, rotation_id)? else {
            return Ok(None);
        };
        let Some((replacement, _)) = self.rotation_abandon_replacement(&request)? else {
            return Ok(None);
        };
        let transferred: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sandbox_custody_events
             WHERE event_kind='transferred' AND cause='rotation'
               AND from_owner_session_id=?1 AND to_owner_session_id=?2)",
            params![holder.to_string(), replacement.to_string()],
            |row| row.get(0),
        )?;
        Ok(transferred.then_some(replacement))
    }

    /// The replacement the abandon request reserved, with its status.
    ///
    /// # Errors
    /// Fails on `SQLite` errors or malformed rows.
    pub fn rotation_abandon_replacement(
        &self,
        request: &RotationAbandonRequest,
    ) -> Result<Option<(Uuid, SessionStatus)>> {
        let reserved =
            reserved_successors_on(&self.conn, request.holder_id, &request.abandon_rotation_id)?;
        let Some(replacement) = reserved.first().copied() else {
            return Ok(None);
        };
        Ok(session_status_on(&self.conn, replacement)?.map(|status| (replacement, status)))
    }

    /// The abandon rotation that reserved `replacement`: its `continued_from`
    /// holder and the holder rotation id, when that rotation is an operator
    /// abandon (#1176).
    ///
    /// # Errors
    /// Fails on `SQLite` errors or malformed rows.
    pub fn rotation_abandon_hop_of(&self, replacement: Uuid) -> Result<Option<(Uuid, String)>> {
        let Some(holder) = continued_from_on(&self.conn, replacement)? else {
            return Ok(None);
        };
        for rotation_id in reserving_rotations_on(&self.conn, holder, replacement)? {
            if abandon_request_for_on(&self.conn, holder, &rotation_id)?.is_some() {
                return Ok(Some((holder, rotation_id)));
            }
        }
        Ok(None)
    }

    /// Restart recovery: close the newest abandon request of `(predecessor,
    /// rotation_id)` when the crash left it without a terminal event and
    /// without a replacement that could still settle or that holds the
    /// sandbox (none reserved, or one that failed before its custody bind).
    /// Returns the closed holder rotation id.
    ///
    /// # Errors
    /// Fails on `SQLite` errors or malformed rows.
    pub fn close_interrupted_rotation_abandon(
        &self,
        predecessor: Uuid,
        rotation_id: &str,
    ) -> Result<Option<String>> {
        let Some(newest) = abandon_requests_on(&self.conn, predecessor, rotation_id)?.pop() else {
            return Ok(None);
        };
        if rotation_settled_on(&self.conn, newest.holder_id, &newest.abandon_rotation_id)? {
            return Ok(None);
        }
        if let Some((_replacement, status)) = self.rotation_abandon_replacement(&newest)? {
            // A replacement still settling, or one that was bound to the
            // sandbox: its hop is part of the chain the next publication
            // settles. The transfer receipt survives later root validation.
            if status != SessionStatus::Failed
                || self
                    .rotation_abandon_bound_successor(
                        newest.holder_id,
                        &newest.abandon_rotation_id,
                    )?
                    .is_some()
            {
                return Ok(None);
            }
        }
        self.conn.execute(
            "INSERT INTO rotation_events (session_id, rotation_id, phase, event_type, metadata, created_at)
             VALUES (?1, ?2, 'completed', ?3, NULL, ?4)",
            params![
                newest.holder_id.to_string(),
                newest.abandon_rotation_id,
                format!("refused:{ABANDON_INTERRUPTED}"),
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            ],
        )?;
        Ok(Some(newest.abandon_rotation_id))
    }
}

fn insert_abandon_request_on(tx: &Transaction<'_>, request: &RotationAbandonRequest) -> Result<()> {
    tx.execute(
        "INSERT INTO rotation_events (session_id, rotation_id, phase, event_type, metadata, created_at)
         VALUES (?1, ?2, 'reserved', ?3, ?4, ?5)",
        params![
            request.predecessor.to_string(),
            request.rotation_id,
            ABANDON_REQUESTED,
            serde_json::json!({
                "seq": request.seq,
                "holder_id": request.holder_id,
                "abandon_rotation_id": request.abandon_rotation_id,
                "idempotency_key": request.idempotency_key,
                "provider": request.provider,
                "model": request.model,
            })
            .to_string(),
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        ],
    )?;
    Ok(())
}

/// What one rotation publication committed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RotationPublication {
    /// The Epics whose lead moved to the successor.
    pub epics: Vec<Uuid>,
    /// The chain sessions an abandon publication archived (#1176): P and
    /// every earlier holder before the publishing holder. Empty otherwise.
    pub archived: Vec<Uuid>,
}
