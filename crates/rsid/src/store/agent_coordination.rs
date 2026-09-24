//! V80 durable agent spawn identity, aggregate progress, and Phase 2 mailbox seam.

#[path = "watch_repair_v123.rs"]
pub(crate) mod watch_repair_v123;

#[path = "agent_archive_child.rs"]
mod agent_archive_child;

use super::row_mappers::{
    event_type_to_str, role_to_str, session_kind_to_str, session_status_to_str,
    str_to_session_kind, str_to_session_provider, str_to_session_status,
};
use super::sandbox_custody::{
    SessionCustodyBinding, bind_on, binding_custody_id, lock_custody_root,
};
use super::{Store, parse_timestamp, sessions::insert_session_on};
use crate::error::{DaemonError, Result};
use chrono::{DateTime, Utc};
use rsi_common::agent_coordination::{
    AGENT_MESSAGE_MAX_CANONICAL_JSONRPC_ID_BYTES, AGENT_MESSAGE_MAX_CAPABILITY_ID_BYTES,
    AGENT_MESSAGE_MAX_ENUM_BYTES, AGENT_MESSAGE_MAX_ERROR_CLASS_BYTES,
    AGENT_MESSAGE_MAX_IDEMPOTENCY_KEY_BYTES, AGENT_MESSAGE_MAX_METHOD_BYTES,
    AGENT_MESSAGE_MAX_PAYLOAD_BYTES, AGENT_MESSAGE_MAX_PENDING_PER_OWNER,
    AGENT_MESSAGE_MAX_PENDING_PER_TARGET, AGENT_MESSAGE_MAX_PROVIDER_TURN_BYTES,
    AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS, AGENT_PROGRESS_MAX_COHORT, AgentContinuationCursorV1,
    AgentGetProgressResultV1, AgentMessageAckCursorV1, AgentMessageCountsV1,
    AgentMessageErrorCodeV1, AgentMessageQueueSummaryV1, AgentMessageStateV1,
    AgentProgressCursorV1, AgentProgressFreshnessV1, AgentProgressObligationV1, AgentProgressRowV1,
    AgentProgressStatusCountsV1, AgentProgressStatusV1, AgentSendMessageRequestV1,
    AgentSendMessageResultV1, AgentSpawnChildRequestV1, AgentSpawnStateV1, AgentWatchStateV1,
    AttemptStateV1, AttemptTerminalDispositionV1, BoundaryAdmissionV1, BoundaryCapabilityKindV1,
    BoundaryClassificationV1, BoundaryProviderKindV1, CorrelationStateV1, EffectClassificationV1,
    MessageAttemptFenceV1, agent_message_digest, agent_message_idempotency_digest,
    agent_message_payload_digest, agent_message_request_fingerprint,
};
use rsi_common::types::{
    ConversationEvent, Recurrence, Role, SandboxCustodyErrorCodeV1, ScheduleSpec, Session,
    SessionKind, SessionStatus,
};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeSet, HashSet};
use uuid::Uuid;

/// Omitted message deadlines are bounded at acceptance; exact replays keep the
/// original persisted deadline and never recompute it.
const AGENT_MESSAGE_DEFAULT_EXPIRY: chrono::Duration = chrono::Duration::minutes(30);

/// P2-06a: **the narrowed crash-window rule, in its single canonical form.**
///
/// Held as a named constant rather than inlined so the rule has exactly one
/// greppable home and a test can assert against the same text the production
/// reader executes.
///
/// The `CASE` is the whole rule. `claimed` with no recorded admission is the
/// ONLY shape that proves no effect, because
/// [`Store::mark_agent_message_attempt_dispatching`] commits `dispatching`
/// strictly before the provider send and refuses to send when it cannot. Every
/// other live shape — `dispatching` above all — answers `uncertain`.
///
/// **Widening the `WHEN` arm redelivers already-sent paid model turns.** In
/// particular there is deliberately NO `claim_expires_at` predicate anywhere in
/// this statement: lease expiry is never proof of no effect.
/// P2-06b: `?3`/`?4` are the EXCLUSIVE keyset position, both NULL for the first
/// page. Keyset, never `OFFSET`: recovery MUTATES the rows it walks, so an
/// offset would shift underneath the scan and skip rows.
const CRASHED_AGENT_MESSAGE_ATTEMPT_SQL: &str = "\
     SELECT a.message_id, a.attempt_number, m.state, m.state_version, a.attempt_state,
            CASE
              WHEN a.attempt_state='claimed' AND a.admission_classification IS NULL
                THEN 'proved_no_effect_requeue'
              ELSE 'uncertain'
            END
       FROM agent_message_delivery_attempts a
       JOIN agent_messages m
         ON m.id=a.message_id AND m.current_attempt_number=a.attempt_number
      WHERE a.attempt_state!='terminal'
        AND a.delivery_boot_id!=?1
        AND m.state IN ('claimed','injected')
        AND (?3 IS NULL
             OR a.message_id > ?3
             OR (a.message_id = ?3 AND a.attempt_number > ?4))
      ORDER BY a.message_id, a.attempt_number
      LIMIT ?2";

/// P2-06a: what startup reconciliation concluded about one crashed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CrashRecoveryVerdictV1 {
    /// `claimed` + foreign boot + no admission. The dead incarnation never
    /// reached the pre-dispatch marker, so it never reached the send.
    ProvedNoEffectRequeue,
    /// Everything else that is still live. The send may have completed and
    /// never been recorded, so the message STRANDS as `uncertain` rather than
    /// risking a double-delivered paid model turn.
    Uncertain,
}

impl CrashRecoveryVerdictV1 {
    #[must_use]
    pub(crate) fn from_str_exact(value: &str) -> Option<Self> {
        Some(match value {
            "proved_no_effect_requeue" => Self::ProvedNoEffectRequeue,
            "uncertain" => Self::Uncertain,
            _ => return None,
        })
    }
}

/// P2-06a: one live attempt whose owning daemon incarnation is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CrashedAgentMessageAttemptV1 {
    pub(crate) message_id: Uuid,
    pub(crate) attempt_number: u32,
    pub(crate) aggregate_state: AgentMessageStateV1,
    pub(crate) state_version: i64,
    pub(crate) attempt_state: AttemptStateV1,
    pub(crate) verdict: CrashRecoveryVerdictV1,
}

/// P2-06b: an EXCLUSIVE position in the `(message_id, attempt_number)` order of
/// [`CRASHED_AGENT_MESSAGE_ATTEMPT_SQL`] — the next page starts strictly after
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CrashRecoveryCursorV1 {
    pub(crate) message_id: Uuid,
    pub(crate) attempt_number: u32,
}

/// P2-06b: one bounded page of crashed attempts, plus its continuation.
///
/// `next_cursor` is the whole point of this type. Before P2-06b the classifier
/// returned a bare `Vec` capped at [`AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS`]
/// with NO way for a caller to learn the cap had bitten, so a restart holding
/// more than one page of crashed attempts would have silently recovered the
/// first page and abandoned the rest — neither requeued nor marked uncertain,
/// with no error and no log line. A silent cap is worse than a loud one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CrashedAgentMessageAttemptPageV1 {
    pub(crate) attempts: Vec<CrashedAgentMessageAttemptV1>,
    /// `None` means EXHAUSTED — the scan reached the end of the population.
    /// `Some(cursor)` means the page bound was reached and the caller MUST come
    /// back with this cursor before it may conclude recovery is complete.
    pub(crate) next_cursor: Option<CrashRecoveryCursorV1>,
}

/// Read the aggregate's exact version while asserting it is still the state
/// recovery classified, so a concurrent writer cannot be silently overwritten.
fn read_recoverable_aggregate_version(
    tx: &Transaction<'_>,
    message_id: Uuid,
    attempt_number: u32,
    expected: AgentMessageStateV1,
) -> Result<i64> {
    let found: Option<(String, i64)> = tx
        .query_row(
            "SELECT state, state_version FROM agent_messages
              WHERE id=?1 AND current_attempt_number=?2",
            params![message_id.to_string(), i64::from(attempt_number)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((state, state_version)) = found else {
        return Err(DaemonError::Store(
            "restart recovery target message is not pointing at this attempt".into(),
        ));
    };
    if state != expected.as_str() {
        return Err(DaemonError::Store(format!(
            "restart recovery expected aggregate state {} but found {state}",
            expected.as_str()
        )));
    }
    Ok(state_version)
}

/// Pinned semantic catalog digest for V80's tables, indexes, and triggers.
pub(crate) const AGENT_COORDINATION_V80_SCHEMA_FINGERPRINT: &str =
    "sha256:8313fef0df44f98f1d85d82df3c642b156e932becf184a2b74d92fa5b6e4739b";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentSpawnRequestRecord {
    pub spawn_request_id: Uuid,
    pub owner_session_id: Uuid,
    pub idempotency_digest: String,
    pub request_fingerprint: String,
    pub request: AgentSpawnChildRequestV1,
    pub child_session_id: Uuid,
    pub epic_id: Uuid,
    pub epic_spawn_ordinal: u32,
    pub kind: SessionKind,
    pub state: AgentSpawnStateV1,
    pub safe_error_class: Option<String>,
    pub reserved_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReserveAgentSpawnOutcome {
    Reserved(AgentSpawnRequestRecord),
    Replayed(AgentSpawnRequestRecord),
}

fn uuid_guard(column: &str, optional: bool) -> String {
    let guard = format!(
        "length(NEW.{column})=36 AND NEW.{column}!='00000000-0000-0000-0000-000000000000' \
         AND NEW.{column}=lower(NEW.{column}) \
         AND substr(NEW.{column},9,1)='-' AND substr(NEW.{column},14,1)='-' \
         AND substr(NEW.{column},19,1)='-' AND substr(NEW.{column},24,1)='-' \
         AND replace(NEW.{column},'-','') NOT GLOB '*[^0-9a-f]*'"
    );
    if optional {
        format!("(NEW.{column} IS NULL OR ({guard}))")
    } else {
        format!("({guard})")
    }
}

fn timestamp_guard(column: &str, optional: bool) -> String {
    let guard = format!(
        "length(NEW.{column})=30 \
         AND strftime('%Y-%m-%dT%H:%M:%S',NEW.{column})=substr(NEW.{column},1,19) \
         AND substr(NEW.{column},20,1)='.' \
         AND substr(NEW.{column},21,9) NOT GLOB '*[^0-9]*' \
         AND substr(NEW.{column},30,1)='Z'"
    );
    if optional {
        format!("(NEW.{column} IS NULL OR ({guard}))")
    } else {
        format!("({guard})")
    }
}

pub(crate) fn install_v80_schema(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE TABLE agent_spawn_requests (
            spawn_request_id TEXT PRIMARY KEY,
            owner_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT,
            idempotency_digest TEXT NOT NULL CHECK(length(idempotency_digest)=71 AND substr(idempotency_digest,1,7)='sha256:' AND substr(idempotency_digest,8) NOT GLOB '*[^0-9a-f]*'),
            request_fingerprint TEXT NOT NULL CHECK(length(request_fingerprint)=71 AND substr(request_fingerprint,1,7)='sha256:' AND substr(request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
            request_json TEXT NOT NULL CHECK(json_valid(request_json)),
            child_session_id TEXT NOT NULL UNIQUE,
            epic_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT,
            kind TEXT NOT NULL CHECK(kind IN ('Standard','TaskRabbit','Story','Task','Bug','Feature','Refactor','Research')),
            state TEXT NOT NULL CHECK(state IN ('reserved','queued','launching','launched','failed')),
            safe_error_class TEXT CHECK(safe_error_class IS NULL OR (length(CAST(safe_error_class AS BLOB)) BETWEEN 1 AND 128 AND safe_error_class NOT GLOB '*[^A-Za-z0-9._:-]*')),
            reserved_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            queued_at TEXT,
            launch_started_at TEXT,
            launched_at TEXT,
            failed_at TEXT,
            UNIQUE(owner_session_id,idempotency_digest),
            CHECK((state!='queued' OR queued_at IS NOT NULL)
              AND (state!='launching' OR launch_started_at IS NOT NULL)
              AND (state!='launched' OR launched_at IS NOT NULL)
              AND (state!='failed' OR (failed_at IS NOT NULL AND safe_error_class IS NOT NULL)))
        );
        CREATE TABLE agent_messages (
            id TEXT PRIMARY KEY,
            owner_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT,
            target_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT,
            idempotency_digest TEXT NOT NULL CHECK(length(idempotency_digest)=71 AND substr(idempotency_digest,1,7)='sha256:' AND substr(idempotency_digest,8) NOT GLOB '*[^0-9a-f]*'),
            request_fingerprint TEXT NOT NULL CHECK(length(request_fingerprint)=71 AND substr(request_fingerprint,1,7)='sha256:' AND substr(request_fingerprint,8) NOT GLOB '*[^0-9a-f]*'),
            payload_digest TEXT NOT NULL CHECK(length(payload_digest)=71 AND substr(payload_digest,1,7)='sha256:' AND substr(payload_digest,8) NOT GLOB '*[^0-9a-f]*'),
            payload TEXT NOT NULL CHECK(length(CAST(payload AS BLOB)) BETWEEN 1 AND 16384),
            state TEXT NOT NULL CHECK(state IN ('queued','claimed','injected','acknowledged','uncertain','failed','expired')),
            safe_error_class TEXT CHECK(safe_error_class IS NULL OR (length(CAST(safe_error_class AS BLOB)) BETWEEN 1 AND 128 AND safe_error_class NOT GLOB '*[^A-Za-z0-9._:-]*')),
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            claimed_at TEXT,
            injected_at TEXT,
            acknowledged_at TEXT,
            expires_at TEXT,
            UNIQUE(owner_session_id,idempotency_digest)
        );
        CREATE INDEX idx_agent_spawn_requests_owner_cohort ON agent_spawn_requests(owner_session_id,state,child_session_id);
        CREATE INDEX idx_agent_spawn_requests_child_state ON agent_spawn_requests(child_session_id,state);
        CREATE INDEX idx_agent_messages_owner_state ON agent_messages(owner_session_id,state,created_at,id);
        CREATE INDEX idx_agent_messages_target_fifo ON agent_messages(target_session_id,state,created_at,id) WHERE state='queued';
        CREATE INDEX idx_scheduled_jobs_agent_watch_natural ON scheduled_jobs(wake_session_id,wake_mode) WHERE enabled=1 AND wake_mode LIKE 'on_terminal:%';
        CREATE TRIGGER agent_spawn_requests_no_delete BEFORE DELETE ON agent_spawn_requests BEGIN SELECT RAISE(ABORT,'agent spawn request history cannot be deleted'); END;
        CREATE TRIGGER agent_messages_no_delete BEFORE DELETE ON agent_messages BEGIN SELECT RAISE(ABORT,'agent message history cannot be deleted'); END;
        CREATE TRIGGER agent_spawn_requests_identity_immutable BEFORE UPDATE ON agent_spawn_requests
        WHEN OLD.spawn_request_id!=NEW.spawn_request_id OR OLD.owner_session_id!=NEW.owner_session_id
          OR OLD.idempotency_digest!=NEW.idempotency_digest OR OLD.request_fingerprint!=NEW.request_fingerprint
          OR OLD.request_json!=NEW.request_json OR OLD.child_session_id!=NEW.child_session_id
          OR OLD.epic_id!=NEW.epic_id OR OLD.kind!=NEW.kind OR OLD.reserved_at!=NEW.reserved_at
        BEGIN SELECT RAISE(ABORT,'agent spawn request identity is immutable'); END;
        CREATE TRIGGER agent_messages_identity_immutable BEFORE UPDATE ON agent_messages
        WHEN OLD.id!=NEW.id OR OLD.owner_session_id!=NEW.owner_session_id
          OR OLD.target_session_id!=NEW.target_session_id OR OLD.idempotency_digest!=NEW.idempotency_digest
          OR OLD.request_fingerprint!=NEW.request_fingerprint OR OLD.payload_digest!=NEW.payload_digest
          OR OLD.payload!=NEW.payload OR OLD.created_at!=NEW.created_at
        BEGIN SELECT RAISE(ABORT,'agent message identity is immutable'); END;",
    )?;

    let spawn_predicate = [
        uuid_guard("spawn_request_id", false),
        uuid_guard("owner_session_id", false),
        uuid_guard("child_session_id", false),
        uuid_guard("epic_id", false),
        timestamp_guard("reserved_at", false),
        timestamp_guard("updated_at", false),
        timestamp_guard("queued_at", true),
        timestamp_guard("launch_started_at", true),
        timestamp_guard("launched_at", true),
        timestamp_guard("failed_at", true),
    ]
    .join(" AND ");
    let message_predicate = [
        uuid_guard("id", false),
        uuid_guard("owner_session_id", false),
        uuid_guard("target_session_id", false),
        timestamp_guard("created_at", false),
        timestamp_guard("updated_at", false),
        timestamp_guard("claimed_at", true),
        timestamp_guard("injected_at", true),
        timestamp_guard("acknowledged_at", true),
        timestamp_guard("expires_at", true),
    ]
    .join(" AND ");
    for operation in ["INSERT", "UPDATE"] {
        tx.execute_batch(&format!(
            "CREATE TRIGGER agent_spawn_requests_v80_validate_{suffix} BEFORE {operation} ON agent_spawn_requests WHEN NOT ({spawn_predicate}) BEGIN SELECT RAISE(ABORT,'V80 invalid agent spawn identity or timestamp'); END;
             CREATE TRIGGER agent_messages_v80_validate_{suffix} BEFORE {operation} ON agent_messages WHEN NOT ({message_predicate}) BEGIN SELECT RAISE(ABORT,'V80 invalid agent message identity or timestamp'); END;",
            suffix = operation.to_ascii_lowercase(),
        ))?;
    }
    Ok(())
}

fn normalize_catalog_sql(sql: &str) -> String {
    sql.lines()
        .map(|line| line.split_once("--").map_or(line, |(before, _)| before))
        .collect::<Vec<_>>()
        .join(" ")
        .replace('"', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

pub(crate) fn v80_schema_fingerprint(connection: &rusqlite::Connection) -> Result<String> {
    let mut stmt = connection.prepare(
        "SELECT type,name,tbl_name,sql FROM sqlite_master
         WHERE name IN (
           'agent_spawn_requests','agent_messages',
           'idx_agent_spawn_requests_owner_cohort','idx_agent_spawn_requests_child_state',
           'idx_agent_messages_owner_state','idx_agent_messages_target_fifo',
           'idx_scheduled_jobs_agent_watch_natural',
           'agent_spawn_requests_no_delete','agent_messages_no_delete',
           'agent_spawn_requests_identity_immutable','agent_messages_identity_immutable',
           'agent_spawn_requests_v80_validate_insert','agent_spawn_requests_v80_validate_update',
           'agent_messages_v80_validate_insert','agent_messages_v80_validate_update'
         ) ORDER BY type,name",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.len() != 15 {
        return Err(DaemonError::Store(format!(
            "V80 coordination catalog inventory mismatch: expected 15 objects, found {}",
            rows.len()
        )));
    }
    let mut hasher = Sha256::new();
    for (kind, name, table, sql) in rows {
        for value in [kind, name, table, normalize_catalog_sql(&sql)] {
            hasher.update((value.len() as u64).to_be_bytes());
            hasher.update(value.as_bytes());
        }
    }
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}

fn parse_spawn_state(value: &str) -> Result<AgentSpawnStateV1> {
    match value {
        "reserved" => Ok(AgentSpawnStateV1::Reserved),
        "queued" => Ok(AgentSpawnStateV1::Queued),
        "launching" => Ok(AgentSpawnStateV1::Launching),
        "launched" => Ok(AgentSpawnStateV1::Launched),
        "failed" => Ok(AgentSpawnStateV1::Failed),
        _ => Err(DaemonError::Store(format!(
            "invalid agent spawn state: {value}"
        ))),
    }
}

fn map_spawn_request_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentSpawnRequestRecord> {
    let parse_uuid = |raw: String| {
        Uuid::parse_str(&raw).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
    };
    let request_json: String = row.get(4)?;
    let state_raw: String = row.get(8)?;
    let reserved_raw: String = row.get(10)?;
    let updated_raw: String = row.get(11)?;
    let ordinal_raw: i64 = row.get(12)?;
    Ok(AgentSpawnRequestRecord {
        spawn_request_id: parse_uuid(row.get(0)?)?,
        owner_session_id: parse_uuid(row.get(1)?)?,
        idempotency_digest: row.get(2)?,
        request_fingerprint: row.get(3)?,
        request: serde_json::from_str(&request_json).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        child_session_id: parse_uuid(row.get(5)?)?,
        epic_id: parse_uuid(row.get(6)?)?,
        epic_spawn_ordinal: u32::try_from(ordinal_raw)
            .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(12, ordinal_raw))?,
        kind: str_to_session_kind(&row.get::<_, String>(7)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        state: parse_spawn_state(&state_raw).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                8,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        safe_error_class: row.get(9)?,
        reserved_at: parse_timestamp(&reserved_raw).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                10,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
            )
        })?,
        updated_at: parse_timestamp(&updated_raw).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                11,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
            )
        })?,
    })
}

const SPAWN_REQUEST_SELECT: &str = "SELECT spawn_request_id,owner_session_id,idempotency_digest,request_fingerprint,request_json,child_session_id,epic_id,kind,state,safe_error_class,reserved_at,updated_at,epic_spawn_ordinal FROM agent_spawn_requests";

fn get_spawn_request_by_owner_digest(
    tx: &Transaction<'_>,
    owner_session_id: Uuid,
    idempotency_digest: &str,
) -> Result<Option<AgentSpawnRequestRecord>> {
    Ok(tx
        .query_row(
            &format!("{SPAWN_REQUEST_SELECT} WHERE owner_session_id=?1 AND idempotency_digest=?2"),
            params![owner_session_id.to_string(), idempotency_digest],
            map_spawn_request_row,
        )
        .optional()?)
}

impl Store {
    pub(crate) fn find_agent_spawn_request(
        &self,
        owner_session_id: Uuid,
        idempotency_digest: &str,
    ) -> Result<Option<AgentSpawnRequestRecord>> {
        let tx = self.conn.unchecked_transaction()?;
        let record = get_spawn_request_by_owner_digest(&tx, owner_session_id, idempotency_digest)?;
        tx.commit()?;
        Ok(record)
    }

    /// Look up the spawn reservation that owns `child_session_id`, if any.
    ///
    /// P2-03 authority uses this for the pre-launch window in which a target
    /// has been reserved but has no `sessions` row yet. `child_session_id` is
    /// `UNIQUE` on `agent_spawn_requests`, so at most one row can match and the
    /// returned `owner_session_id` is the sole authority the caller is checked
    /// against — a reservation owned by another Session must not grant a send.
    pub(crate) fn find_agent_spawn_request_by_child(
        &self,
        child_session_id: Uuid,
    ) -> Result<Option<AgentSpawnRequestRecord>> {
        let tx = self.conn.unchecked_transaction()?;
        let record = tx
            .query_row(
                &format!("{SPAWN_REQUEST_SELECT} WHERE child_session_id=?1"),
                [child_session_id.to_string()],
                map_spawn_request_row,
            )
            .optional()?;
        tx.commit()?;
        Ok(record)
    }

    pub(crate) fn reserve_agent_spawn_request(
        &self,
        owner_session_id: Uuid,
        idempotency_digest: &str,
        request_fingerprint: &str,
        request: &AgentSpawnChildRequestV1,
        epic_id: Uuid,
        spawn_request_id: Uuid,
        child_session_id: Uuid,
    ) -> Result<ReserveAgentSpawnOutcome> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        if let Some(existing) =
            get_spawn_request_by_owner_digest(&tx, owner_session_id, idempotency_digest)?
        {
            if existing.request_fingerprint != request_fingerprint || existing.request != *request {
                return Err(DaemonError::InvalidParam(format!(
                    "agent_spawn_idempotency_conflict:{}",
                    existing.spawn_request_id
                )));
            }
            tx.commit()?;
            return Ok(ReserveAgentSpawnOutcome::Replayed(existing));
        }
        let owner_is_current_lead = tx.query_row(
            "SELECT EXISTS(
                 SELECT 1
                   FROM sessions
                  WHERE id=?1
                    AND session_kind='Epic'
                    AND lead_session_id=?2
             )",
            params![epic_id.to_string(), owner_session_id.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        if !owner_is_current_lead {
            return Err(DaemonError::PolicyDenied(
                "agent_spawn_owner_is_not_current_epic_lead".into(),
            ));
        }
        let now = Utc::now();
        let now_string = now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let request_json = serde_json::to_string(request)?;
        let next_ordinal: i64 = tx.query_row(
            "INSERT INTO epic_spawn_counters(epic_id,next_ordinal) VALUES(?1,2)
             ON CONFLICT(epic_id) DO UPDATE SET next_ordinal=epic_spawn_counters.next_ordinal+1
             RETURNING next_ordinal-1",
            [epic_id.to_string()],
            |row| row.get(0),
        )?;
        let epic_spawn_ordinal = u32::try_from(next_ordinal)
            .map_err(|_| DaemonError::Store("agent spawn ordinal exhausted".into()))?;
        tx.execute(
            "INSERT INTO agent_spawn_requests
             (spawn_request_id,owner_session_id,idempotency_digest,request_fingerprint,request_json,child_session_id,epic_id,kind,state,reserved_at,updated_at,epic_spawn_ordinal)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'reserved',?9,?9,?10)",
            params![
                spawn_request_id.to_string(),
                owner_session_id.to_string(),
                idempotency_digest,
                request_fingerprint,
                request_json,
                child_session_id.to_string(),
                epic_id.to_string(),
                session_kind_to_str(request.kind),
                now_string,
                i64::from(epic_spawn_ordinal),
            ],
        )?;
        let record = get_spawn_request_by_owner_digest(&tx, owner_session_id, idempotency_digest)?
            .ok_or_else(|| DaemonError::Store("reserved agent spawn row disappeared".into()))?;
        tx.commit()?;
        Ok(ReserveAgentSpawnOutcome::Reserved(record))
    }

    pub(crate) fn get_agent_spawn_request(
        &self,
        spawn_request_id: Uuid,
    ) -> Result<Option<AgentSpawnRequestRecord>> {
        Ok(self
            .conn
            .query_row(
                &format!("{SPAWN_REQUEST_SELECT} WHERE spawn_request_id=?1"),
                [spawn_request_id.to_string()],
                map_spawn_request_row,
            )
            .optional()?)
    }

    pub(crate) fn mark_agent_spawn_queued(&self, spawn_request_id: Uuid) -> Result<()> {
        self.transition_agent_spawn(spawn_request_id, "queued", None)
    }

    pub(crate) fn mark_agent_spawn_launching(&self, spawn_request_id: Uuid) -> Result<()> {
        self.transition_agent_spawn(spawn_request_id, "launching", None)
    }

    pub(crate) fn mark_agent_spawn_failed(
        &self,
        spawn_request_id: Uuid,
        safe_error_class: &str,
    ) -> Result<()> {
        self.transition_agent_spawn(spawn_request_id, "failed", Some(safe_error_class))
    }

    fn transition_agent_spawn(
        &self,
        spawn_request_id: Uuid,
        state: &str,
        safe_error_class: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let (time_column, allowed) = match state {
            "queued" => ("queued_at", "state IN ('reserved','queued','launching')"),
            "launching" => (
                "launch_started_at",
                "state IN ('reserved','queued','launching')",
            ),
            "failed" => ("failed_at", "state!='launched'"),
            _ => return Err(DaemonError::Store("invalid spawn transition target".into())),
        };
        let changed = self.conn.execute(
            &format!(
                "UPDATE agent_spawn_requests SET state=?1,updated_at=?2,{time_column}=COALESCE({time_column},?2),safe_error_class=?3 WHERE spawn_request_id=?4 AND {allowed}"
            ),
            params![state, now, safe_error_class, spawn_request_id.to_string()],
        )?;
        if changed == 0 {
            let existing = self.get_agent_spawn_request(spawn_request_id)?;
            if existing.is_some_and(|row| row.state == AgentSpawnStateV1::Launched) {
                return Ok(());
            }
            return Err(DaemonError::Store(format!(
                "agent spawn transition rejected: {spawn_request_id} -> {state}"
            )));
        }
        Ok(())
    }

    pub(crate) fn admit_agent_child_session(
        &self,
        session: &Session,
        invocation_id: Uuid,
        spawn_request_id: Uuid,
        owner_session_id: Uuid,
        binding: SessionCustodyBinding,
    ) -> Result<()> {
        // H1-04 (F-011): a forked child binds its independent generation-1
        // root in the same transaction that admits the child row and settles
        // the spawn reservation to `launched`. Hold the root stripe across
        // the whole admission so a concurrent custody transition on the same
        // custody id cannot interleave.
        let _root_guard = binding_custody_id(&binding).map(lock_custody_root);
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let request = tx
            .query_row(
                &format!("{SPAWN_REQUEST_SELECT} WHERE spawn_request_id=?1"),
                [spawn_request_id.to_string()],
                map_spawn_request_row,
            )
            .optional()?
            .ok_or_else(|| DaemonError::Store("agent spawn request missing at launch".into()))?;
        if request.owner_session_id != owner_session_id
            || request.child_session_id != session.id
            || request.epic_id != session.parent_id.unwrap_or(Uuid::nil())
            || request.kind != session.session_kind
            || request.request.agent_role != session.agent_role
            || Some(request.epic_spawn_ordinal) != session.epic_spawn_ordinal
        {
            return Err(DaemonError::Store(
                "agent child launch identity mismatch".into(),
            ));
        }
        if request.state == AgentSpawnStateV1::Launched {
            tx.commit()?;
            return Ok(());
        }
        if request.state == AgentSpawnStateV1::Failed {
            return Err(DaemonError::Store(
                "failed agent spawn request cannot launch".into(),
            ));
        }
        insert_session_on(&tx, session)?;
        tx.execute(
            "UPDATE sessions SET model_invocation_id=?1 WHERE id=?2",
            params![invocation_id.to_string(), session.id.to_string()],
        )?;
        bind_on(&tx, session.id, binding)?;
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let changed = tx.execute(
            "UPDATE agent_spawn_requests SET state='launched',updated_at=?1,launched_at=?1,safe_error_class=NULL WHERE spawn_request_id=?2 AND state IN ('reserved','queued','launching')",
            params![now, spawn_request_id.to_string()],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "agent spawn launch settlement lost compare-and-set".into(),
            ));
        }
        // The launch commits an arm intent even if the later scheduled-job
        // insertion fails. Restart repair may trust this V123 epoch witness.
        tx.execute(
            "INSERT INTO agent_child_watch_witness
                 (owner_session_id,child_session_id,job_id,state,updated_at)
             VALUES (?1,?2,NULL,'pending',?3)
             ON CONFLICT(owner_session_id,child_session_id) DO NOTHING",
            params![owner_session_id.to_string(), session.id.to_string(), now],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// H1-04 (F-011): settle a refused/failed AgentSpawnChild custody
    /// transition in one immediate transaction. The reserved spawn row is
    /// compare-and-set to `failed` with `safe_error_class =
    /// sandbox_custody.<code>`, and the reserved child session id becomes a
    /// durably Failed, non-executable row (`stop_reason =
    /// sandbox_custody:<code>`, invalid projection, no effective cwd, no
    /// custody link). The emitter's custody, roots, and events are untouched.
    pub(crate) fn settle_failed_agent_spawn_custody(
        &mut self,
        spawn_request_id: Uuid,
        child: &Session,
        code: SandboxCustodyErrorCodeV1,
    ) -> Result<()> {
        if child.status != SessionStatus::Failed
            || child.stop_reason.as_deref() != Some(&format!("sandbox_custody:{}", code.as_str()))
            || child.sandbox_kind.is_some()
            || child.sandbox_root.is_some()
            || child.sandbox_branch.is_some()
            || child.sandbox_cleanup_state.is_some()
        {
            return Err(DaemonError::Store(
                "agent spawn custody settlement requires a Failed ordinary child template".into(),
            ));
        }
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let request = tx
            .query_row(
                &format!("{SPAWN_REQUEST_SELECT} WHERE spawn_request_id=?1"),
                [spawn_request_id.to_string()],
                map_spawn_request_row,
            )
            .optional()?
            .ok_or_else(|| {
                DaemonError::Store("agent spawn request missing at custody settlement".into())
            })?;
        if request.child_session_id != child.id
            || request.kind != child.session_kind
            || request.epic_id != child.parent_id.unwrap_or(Uuid::nil())
            || request.request.agent_role != child.agent_role
            || Some(request.epic_spawn_ordinal) != child.epic_spawn_ordinal
        {
            return Err(DaemonError::Store(
                "agent spawn custody settlement identity mismatch".into(),
            ));
        }
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let changed = tx.execute(
            "UPDATE agent_spawn_requests SET state='failed',updated_at=?1,failed_at=COALESCE(failed_at,?1),safe_error_class=?2 WHERE spawn_request_id=?3 AND state!='launched'",
            params![
                now,
                format!("sandbox_custody.{}", code.as_str()),
                spawn_request_id.to_string()
            ],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(
                "agent spawn custody settlement lost compare-and-set".into(),
            ));
        }
        insert_session_on(&tx, child)?;
        let projection = tx.execute(
            "UPDATE session_execution_projections SET execution_state='invalid', freshness='invalid', effective_cwd=NULL, custody_id=NULL, custody_generation=NULL, validated_at=?2, error_code=?3, updated_at=?2 WHERE session_id=?1",
            params![child.id.to_string(), now, code.as_str()],
        )?;
        if projection != 1 {
            return Err(DaemonError::Store(
                "agent spawn custody settlement lost exact projection fence".into(),
            ));
        }
        tx.commit()?;
        Ok(())
    }

    /// Settle an AgentSpawnChild failure after its launch request, admitted
    /// invocation, Starting Session, and generation-one custody root have
    /// already committed together.  This is intentionally distinct from
    /// [`Self::settle_failed_agent_spawn_custody`]: the prelaunch helper must
    /// reject a `launched` request and creates no owned root, while this
    /// transaction retains the exact bound root for recovery and only makes
    /// the child projection non-executable.
    pub(crate) fn settle_post_bind_agent_child_failure(
        &mut self,
        spawn_request_id: Uuid,
        child_session_id: Uuid,
        invocation_id: Uuid,
        safe_error_class: &str,
    ) -> Result<()> {
        if safe_error_class.is_empty()
            || safe_error_class.len() > 128
            || !safe_error_class
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
        {
            return Err(DaemonError::Store(
                "post-bind agent child failure class is invalid".into(),
            ));
        }

        let custody_id = self
            .conn
            .query_row(
                "SELECT sandbox_custody_id FROM sessions
                 WHERE id=?1 AND status='Starting' AND model_invocation_id=?2
                   AND sandbox_custody_id IS NOT NULL",
                params![child_session_id.to_string(), invocation_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| {
                DaemonError::Store(
                    "post-bind agent child failure missing its Starting custody binding".into(),
                )
            })?;
        let custody_id = Uuid::parse_str(&custody_id).map_err(|_| {
            DaemonError::Store("post-bind agent child custody id is invalid".into())
        })?;
        let _root_guard = lock_custody_root(custody_id);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let request = tx
            .query_row(
                &format!("{SPAWN_REQUEST_SELECT} WHERE spawn_request_id=?1"),
                [spawn_request_id.to_string()],
                map_spawn_request_row,
            )
            .optional()?
            .ok_or_else(|| {
                DaemonError::Store("post-bind agent child spawn request is missing".into())
            })?;
        if request.state != AgentSpawnStateV1::Launched
            || request.child_session_id != child_session_id
        {
            return Err(DaemonError::Store(
                "post-bind agent child failure lost exact Launched request fence".into(),
            ));
        }

        let exact_bound: bool = tx.query_row(
            "SELECT EXISTS(
                 SELECT 1
                   FROM sessions AS child
                   JOIN model_invocations AS invocation
                     ON invocation.id=child.model_invocation_id
                   JOIN sandbox_custody_roots AS custody
                     ON custody.custody_id=child.sandbox_custody_id
                   JOIN session_execution_projections AS projection
                     ON projection.session_id=child.id
                  WHERE child.id=?1
                    AND child.status='Starting'
                    AND child.parent_id=?2
                    AND child.session_kind=?3
                    AND child.agent_role IS ?4
                    AND child.epic_spawn_ordinal IS ?5
                    AND child.model_invocation_id=?6
                    AND child.sandbox_kind='GitWorktree'
                    AND child.sandbox_cleanup_state='Live'
                    AND child.sandbox_root=custody.sandbox_root
                    AND child.sandbox_branch=custody.sandbox_branch
                    AND invocation.session_id=child.id
                    AND invocation.purpose='agent.spawn_child'
                    AND invocation.admission_status='admitted'
                    AND invocation.status='running'
                    AND custody.custody_id=?7
                    AND custody.state='live'
                    AND custody.owner_session_id=child.id
                    AND custody.allocation_id=child.id
                    AND custody.generation=1
                    AND custody.validation_state='verified'
                    AND custody.validated_generation=1
                    AND projection.execution_state='live_sandboxed'
                    AND projection.freshness='verified'
                    AND projection.effective_cwd=custody.sandbox_root
                    AND projection.custody_id=custody.custody_id
                    AND projection.custody_generation=custody.generation
                    AND projection.error_code IS NULL
             )",
            params![
                child_session_id.to_string(),
                request.epic_id.to_string(),
                session_kind_to_str(request.kind),
                request.request.agent_role.as_deref(),
                i64::from(request.epic_spawn_ordinal),
                invocation_id.to_string(),
                custody_id.to_string(),
            ],
            |row| row.get(0),
        )?;
        if !exact_bound {
            return Err(DaemonError::Store(
                "post-bind agent child failure lost Session/invocation/custody fence".into(),
            ));
        }

        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let request_changed = tx.execute(
            "UPDATE agent_spawn_requests
             SET state='failed',updated_at=?1,failed_at=COALESCE(failed_at,?1),safe_error_class=?2
             WHERE spawn_request_id=?3 AND child_session_id=?4 AND state='launched'",
            params![
                now,
                safe_error_class,
                spawn_request_id.to_string(),
                child_session_id.to_string(),
            ],
        )?;
        if request_changed != 1 {
            return Err(DaemonError::Store(
                "post-bind agent child failure lost request compare-and-set".into(),
            ));
        }
        let session_changed = tx.execute(
            "UPDATE sessions
             SET status='Failed',stop_reason=?1,updated_at=?2
             WHERE id=?3 AND status='Starting' AND model_invocation_id=?4
               AND sandbox_custody_id=?5",
            params![
                format!("sandbox_custody:{safe_error_class}"),
                now,
                child_session_id.to_string(),
                invocation_id.to_string(),
                custody_id.to_string(),
            ],
        )?;
        if session_changed != 1 {
            return Err(DaemonError::Store(
                "post-bind agent child failure lost Starting Session fence".into(),
            ));
        }
        let projection_changed = tx.execute(
            "UPDATE session_execution_projections
             SET execution_state='invalid',freshness='invalid',effective_cwd=NULL,
                 validated_at=?2,error_code=?3,updated_at=?2
             WHERE session_id=?1 AND custody_id=?4 AND custody_generation=1
               AND execution_state='live_sandboxed' AND freshness='verified'",
            params![
                child_session_id.to_string(),
                now,
                safe_error_class,
                custody_id.to_string(),
            ],
        )?;
        if projection_changed != 1 {
            return Err(DaemonError::Store(
                "post-bind agent child failure lost exact projection fence".into(),
            ));
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn list_agent_spawn_requests_for_watch_repair(
        &self,
    ) -> Result<Vec<AgentSpawnRequestRecord>> {
        let mut statement = self.conn.prepare(&format!(
            "{SPAWN_REQUEST_SELECT} AS request
             WHERE request.state='launched'
               AND EXISTS (
                 SELECT 1 FROM sessions AS owner
                 WHERE owner.id=request.owner_session_id
                   AND owner.status NOT IN ('Archived','Deleted')
               )
               AND EXISTS (
                 SELECT 1 FROM agent_child_watch_witness AS witness
                 WHERE witness.owner_session_id=request.owner_session_id
                   AND witness.child_session_id=request.child_session_id
                   AND witness.state IN ('pending','armed','consumed')
               )
               AND EXISTS (
                 SELECT 1 FROM sessions AS child
                 WHERE child.id=request.child_session_id
                   AND (
                     child.status IN ('Starting','Running','WaitingApproval')
                     OR (
                       child.status IN ('Completed','Failed','Interrupted')
                       AND EXISTS (
                         SELECT 1 FROM agent_child_watch_witness AS terminal_witness
                         WHERE terminal_witness.owner_session_id=request.owner_session_id
                           AND terminal_witness.child_session_id=request.child_session_id
                           AND terminal_witness.state IN ('pending','armed')
                       )
                     )
                   )
               )
               AND NOT EXISTS (
                 SELECT 1 FROM scheduled_jobs AS watch
                 WHERE watch.wake_session_id=request.owner_session_id
                   AND watch.wake_mode='on_terminal:' || request.child_session_id
                   AND watch.enabled=1
                   AND NOT EXISTS (
                     SELECT 1 FROM harness_manager_watches AS manager_watch
                     WHERE manager_watch.job_id=watch.id
                   )
               )
             ORDER BY request.owner_session_id,request.child_session_id"
        ))?;
        Ok(statement
            .query_map([], map_spawn_request_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub(crate) fn list_incomplete_agent_spawn_requests(
        &self,
    ) -> Result<Vec<AgentSpawnRequestRecord>> {
        let mut statement = self.conn.prepare(&format!(
            "{SPAWN_REQUEST_SELECT} WHERE state IN ('reserved','queued','launching') ORDER BY reserved_at,spawn_request_id"
        ))?;
        Ok(statement
            .query_map([], map_spawn_request_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub(crate) fn agent_get_progress_snapshot(
        &self,
        caller_session_id: Uuid,
        requested_ids: &[Uuid],
    ) -> Result<AgentGetProgressResultV1> {
        let observed_at = Utc::now();
        let tx = self.conn.unchecked_transaction()?;
        let mut authorized = authorized_child_ids(&tx, caller_session_id)?;
        // The empty request retains the historical child cohort. Manager
        // reach applies only to rows explicitly named by the caller.
        for id in requested_ids {
            if !authorized.contains(id)
                && self
                    .manager_session_control_scope(caller_session_id, *id, false)?
                    .is_some()
            {
                authorized.insert(*id);
            }
        }
        let selected: BTreeSet<Uuid> = if requested_ids.is_empty() {
            authorized
        } else {
            let mut selected = BTreeSet::new();
            for id in requested_ids {
                if !authorized.contains(id) {
                    return Err(DaemonError::InvalidParam(format!(
                        "agent_verb_scope_denied: session {caller_session_id} may not observe {id} in AgentGetProgress"
                    )));
                }
                selected.insert(*id);
            }
            selected
        };
        if selected.len() > AGENT_PROGRESS_MAX_COHORT {
            return Err(crate::error::agent_progress_cohort_too_large(
                selected.len(),
            ));
        }

        let mut rows = Vec::with_capacity(selected.len());
        let mut status_counts = AgentProgressStatusCountsV1::default();
        let mut unhandled_terminal_children = 0_u32;
        for session_id in selected {
            let row = progress_row(&tx, caller_session_id, session_id, observed_at)?;
            status_counts.record(row.cursor.status);
            if row.obligation.is_some() {
                unhandled_terminal_children = unhandled_terminal_children.saturating_add(1);
            }
            rows.push(row);
        }
        tx.commit()?;
        let cohort_size = u32::try_from(rows.len()).map_err(|error| {
            DaemonError::Store(format!(
                "agent progress cohort size conversion failed: {error}"
            ))
        })?;
        Ok(AgentGetProgressResultV1 {
            observed_at,
            cohort_size,
            status_counts,
            rows,
            unhandled_terminal_children,
        })
    }

    /// Accept one `AgentSendMessage`, or exactly replay a prior acceptance
    /// under the same `(owner, idempotency key)` (P2-03).
    ///
    /// This is the FIRST production writer of the V81 mailbox. One
    /// `BEGIN IMMEDIATE` transaction derives the stable message UUID, enforces
    /// the frozen pending caps against the immutable logical root, inserts the
    /// aggregate, and authors its immutable version-0 `none→queued` acceptance
    /// transition. Provider dispatch is an external effect and never occurs
    /// here.
    ///
    /// `target_session_id` is the immutable logical/reserved root. No rotation
    /// tip is resolved or written during acceptance; when the target has no
    /// Session row yet the caller binds the matching spawn request instead.
    ///
    /// # Errors
    ///
    /// Returns a typed `agent_message_*` class for an idempotency conflict
    /// (same key, different target/payload/expiry) or for a full target/owner
    /// pending queue.
    pub(crate) fn accept_agent_message(
        &self,
        owner_session_id: Uuid,
        target_spawn_request_id: Option<Uuid>,
        request: &AgentSendMessageRequestV1,
    ) -> Result<AcceptAgentMessageOutcome> {
        self.accept_agent_message_inner(
            owner_session_id,
            target_spawn_request_id,
            None,
            request,
            false,
            Utc::now,
        )
    }

    /// Guarded production acceptance used by `AgentControlHandle`. The
    /// low-level `accept_agent_message` remains available to Store-level
    /// delivery tests, which intentionally do not model send authority.
    pub(crate) fn accept_authorized_agent_message(
        &self,
        owner_session_id: Uuid,
        target_spawn_request_id: Option<Uuid>,
        manager_epic_id: Option<Uuid>,
        request: &AgentSendMessageRequestV1,
    ) -> Result<AcceptAgentMessageOutcome> {
        self.accept_agent_message_inner(
            owner_session_id,
            target_spawn_request_id,
            manager_epic_id,
            request,
            true,
            Utc::now,
        )
    }

    fn accept_agent_message_inner(
        &self,
        owner_session_id: Uuid,
        target_spawn_request_id: Option<Uuid>,
        manager_epic_id: Option<Uuid>,
        request: &AgentSendMessageRequestV1,
        validate_current_target: bool,
        acceptance_clock: impl FnOnce() -> DateTime<Utc>,
    ) -> Result<AcceptAgentMessageOutcome> {
        request
            .validate()
            .map_err(|class| DaemonError::InvalidParam(class.to_string()))?;

        // Scoped to (caller,key) per P2-03 `:2299-2302`. The target is bound
        // into `request_fingerprint`, not here, so a changed target lands on
        // this same row and is caught as drift below.
        let idempotency_digest =
            agent_message_idempotency_digest(owner_session_id, &request.idempotency_key);
        let request_fingerprint = agent_message_request_fingerprint(owner_session_id, request);
        let payload_digest = agent_message_payload_digest(&request.message);
        let message_id = agent_message_id_from_idempotency_digest(&idempotency_digest);

        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let manager_scope = if let Some(epic_id) = manager_epic_id {
            let scope = self
                .manager_session_control_scope(owner_session_id, request.target_session_id, true)?
                .ok_or_else(|| DaemonError::InvalidParam("agent_verb_scope_denied".into()))?;
            if scope.epic_id != epic_id {
                return Err(DaemonError::InvalidParam("manager_v2_scope_changed".into()));
            }
            Some(scope)
        } else {
            None
        };

        // Exact replay returns the ORIGINAL receipt. A same-key send whose
        // target, payload, or expiry drifted is a conflict, never a silent
        // overwrite: the request fingerprint covers all three.
        if let Some(existing) =
            get_agent_message_by_owner_digest(&tx, owner_session_id, &idempotency_digest)?
        {
            if existing.request_fingerprint != request_fingerprint {
                return Err(crate::error::agent_message_error(
                    AgentMessageErrorCodeV1::IdempotencyConflict,
                    Some(existing.message_id.to_string()),
                    Some(existing.message_id),
                    None,
                    None,
                ));
            }
            tx.commit()?;
            return Ok(AcceptAgentMessageOutcome::Replayed(existing.into_receipt()));
        }

        // The runtime authorization above is an early, payload-safe gate. Its
        // snapshot can race a terminal transition or rotation, so bind a new
        // acceptance to the current durable lineage and authority while this
        // BEGIN IMMEDIATE transaction holds the SQLite write lock.
        if validate_current_target {
            validate_agent_message_live_target(
                &tx,
                owner_session_id,
                request.target_session_id,
                manager_scope.is_some(),
            )?;
        }

        // Caps count PENDING rows against the immutable logical root, never a
        // delivery tip.
        let target_pending =
            count_pending_agent_messages(&tx, "target_session_id", request.target_session_id)?;
        if target_pending >= AGENT_MESSAGE_MAX_PENDING_PER_TARGET {
            return Err(crate::error::agent_message_error(
                AgentMessageErrorCodeV1::TargetQueueFull,
                Some(format!(
                    "{target_pending}/{AGENT_MESSAGE_MAX_PENDING_PER_TARGET}"
                )),
                None,
                Some(target_pending),
                Some(AGENT_MESSAGE_MAX_PENDING_PER_TARGET),
            ));
        }
        let owner_pending =
            count_pending_agent_messages(&tx, "owner_session_id", owner_session_id)?;
        if owner_pending >= AGENT_MESSAGE_MAX_PENDING_PER_OWNER {
            return Err(crate::error::agent_message_error(
                AgentMessageErrorCodeV1::OwnerQueueFull,
                Some(format!(
                    "{owner_pending}/{AGENT_MESSAGE_MAX_PENDING_PER_OWNER}"
                )),
                None,
                Some(owner_pending),
                Some(AGENT_MESSAGE_MAX_PENDING_PER_OWNER),
            ));
        }

        // Sample only for a new insert; an exact replay retains its first deadline.
        let now = acceptance_clock();
        let now_string = now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let expires_string = Some(
            request
                .expires_at
                .unwrap_or(now + AGENT_MESSAGE_DEFAULT_EXPIRY)
                .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        );

        tx.execute(
            "INSERT INTO agent_messages (
                 id, owner_session_id, target_session_id, target_spawn_request_id,
                 idempotency_digest, request_fingerprint, payload_digest, payload,
                 created_at, expires_at, state, state_version, attempt_count,
                 current_attempt_number, safe_error_class, updated_at,
                 acknowledged_at, uncertain_at, failed_at, expired_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'queued',0,0,NULL,NULL,?9,NULL,NULL,NULL,NULL)",
            params![
                message_id.to_string(),
                owner_session_id.to_string(),
                request.target_session_id.to_string(),
                target_spawn_request_id.map(|id| id.to_string()),
                idempotency_digest,
                request_fingerprint,
                payload_digest,
                request.message,
                now_string,
                expires_string,
            ],
        )?;

        // Exactly one immutable version-0 acceptance edge. No attempt can
        // precede acceptance, so `attempt_number` is NULL by construction.
        tx.execute(
            "INSERT INTO agent_message_state_transitions (
                 message_id, state_version, from_state, to_state, attempt_number,
                 authority_kind, authority_id, evidence_digest, created_at)
             VALUES (?1,0,'none','queued',NULL,'acceptance',?2,?3,?4)",
            params![
                message_id.to_string(),
                owner_session_id.to_string(),
                request_fingerprint,
                now_string,
            ],
        )?;

        let mut accepted =
            get_agent_message_by_owner_digest(&tx, owner_session_id, &idempotency_digest)?
                .ok_or_else(|| {
                    DaemonError::Store("accepted agent message row disappeared".into())
                })?;
        // A fresh insert is an original acceptance, not a deduplication.
        accepted.deduplicated = false;
        if let Some(scope) = manager_scope.as_ref() {
            self.audit_manager_session_control(scope, "AgentSendMessage", "accepted")?;
        }
        tx.commit()?;
        Ok(AcceptAgentMessageOutcome::Accepted(accepted.into_receipt()))
    }

    /// P2-04 / C-P2-19 / C-P2-23: the dispatcher's effect-free selection pass.
    ///
    /// Returns one bounded, keyset-paginated page of `queued` mail in global
    /// `(created_at,id)` FIFO order, each row already joined to the delivery
    /// facts its claim will be fenced on. Ordering by `(created_at,id)` across
    /// all roots also orders each individual logical root's mail, which is what
    /// P2-04's "FIFO by logical root" requires; the dispatcher groups by root
    /// afterwards without re-sorting.
    ///
    /// This function performs NO mutation and takes NO write lock. It exists
    /// because the claim transaction needs the caller to state its exact
    /// expectation of the delivery Session — status, generation, and prior model
    /// invocation — and something must read those facts first. Everything it
    /// returns is therefore advisory: [`Self::claim_agent_message_exact`]
    /// re-checks every one of them under `BEGIN IMMEDIATE` and returns a typed
    /// CAS loss if any moved in between. Selecting a row here is not permission
    /// to deliver it.
    ///
    /// **Why status and generation are read in one snapshot (C-P2-21).**
    /// `sessions.rotation_depth` — the generation — is CONSTANT for the life of
    /// a row, because rotation REPLACES a session row rather than mutating it.
    /// So the generation does not detect rotation and was never meant to: it
    /// detects a *torn read*, where the dispatcher resolved the tip at one
    /// moment and its facts at another. The rotated-away case is carried
    /// entirely by [`session_status_admits_delivery`]. Both are read here inside
    /// one transaction and both are re-checked by the claim, and the claim's
    /// separate `DeliverySessionNotLive` and `SessionGenerationMismatch` losses
    /// keep the two guards individually observable rather than collapsing into
    /// one pass/fail. Removing or weakening the status check would silently
    /// delete the only rotation guard while every generation assertion kept
    /// passing.
    ///
    /// Bounded per C-P2-23: at most `limit` rows (hard-capped at
    /// [`AGENT_MESSAGE_DISPATCH_SCAN_MAX_ROWS`]) or
    /// [`AGENT_MESSAGE_DISPATCH_SCAN_MAX_MILLIS`] of tip resolution, whichever
    /// comes first, with a continuation cursor so a large backlog is drained
    /// across ticks instead of in one unbounded scan.
    pub(crate) fn list_dispatchable_agent_messages(
        &self,
        after: Option<AgentMessageDispatchCursor>,
        limit: usize,
    ) -> Result<DispatchableAgentMessagePage> {
        let limit = limit.clamp(1, AGENT_MESSAGE_DISPATCH_SCAN_MAX_ROWS);
        let started = std::time::Instant::now();
        let now = Utc::now();

        // A read-only snapshot. Deliberately NOT `Immediate`: selection must
        // never contend with the claim transactions it feeds.
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;

        let (after_created_at, after_id) = match after {
            Some(cursor) => (
                Some(
                    cursor
                        .created_at
                        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
                ),
                Some(cursor.message_id.to_string()),
            ),
            None => (None, None),
        };

        let mut statement = tx.prepare(
            "SELECT id, owner_session_id, target_session_id, created_at, expires_at,
                    state_version, current_attempt_number, payload
               FROM agent_messages
              WHERE state=?1
                AND (?2 IS NULL
                     OR created_at > ?2
                     OR (created_at = ?2 AND id > ?3))
              ORDER BY created_at, id
              LIMIT ?4",
        )?;
        let rows = statement
            .query_map(
                params![
                    AgentMessageStateV1::Queued.as_str(),
                    after_created_at,
                    after_id,
                    i64::try_from(limit).unwrap_or(i64::MAX),
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);

        let scanned = rows.len();
        let mut messages = Vec::with_capacity(scanned);
        let mut next_cursor = None;
        let mut truncated_by_time = false;

        for (
            id,
            owner,
            target,
            created_at,
            expires_at,
            state_version,
            current_attempt_number,
            payload,
        ) in rows
        {
            let message_id = parse_store_uuid("agent message id", &id)?;
            let owner_session_id = parse_store_uuid("agent message owner", &owner)?;
            let logical_root_session_id = parse_store_uuid("agent message target", &target)?;
            let created_at = parse_store_timestamp("agent message created_at", &created_at)?;
            let expires_at = expires_at
                .as_deref()
                .map(|value| parse_store_timestamp("agent message expires_at", value))
                .transpose()?;

            // The cursor advances for every row REACHED, including rows that
            // resolve ineligible. Advancing only on eligible rows would re-scan
            // a permanently-ineligible head forever and starve everything behind
            // it.
            next_cursor = Some(AgentMessageDispatchCursor {
                created_at,
                message_id,
            });

            let delivery_session_id = resolve_lineage_tip(&tx, logical_root_session_id)?;
            let delivery = read_delivery_session_facts(&tx, delivery_session_id)?;

            let eligibility = if expires_at.is_some_and(|expiry| expiry <= now) {
                // C-P2-09: a durably expired queued row belongs to
                // `expiry_reconciler`, which authors `queued→expired` with a NULL
                // attempt. The dispatcher must not claim it — a claim would
                // create an attempt the terminal-coherence trigger then refuses
                // to reconcile.
                AgentMessageDispatchEligibility::Expired
            } else {
                match &delivery {
                    None => AgentMessageDispatchEligibility::DeliverySessionMissing,
                    Some(facts) if session_status_admits_delivery(facts.status) => {
                        AgentMessageDispatchEligibility::Ready
                    }
                    Some(facts) if terminal_recovery_pending_tx(&tx, delivery_session_id)? => {
                        AgentMessageDispatchEligibility::RecoveryPending(facts.status)
                    }
                    Some(facts) => {
                        AgentMessageDispatchEligibility::DeliverySessionNotLive(facts.status)
                    }
                }
            };

            messages.push(DispatchableAgentMessage {
                message_id,
                owner_session_id,
                logical_root_session_id,
                created_at,
                expires_at,
                state_version,
                current_attempt_number: current_attempt_number
                    .map(u32::try_from)
                    .transpose()
                    .map_err(|_| {
                        DaemonError::Store("agent message attempt pointer out of range".into())
                    })?,
                payload: DeliverablePayload(payload),
                delivery_session_id,
                delivery: delivery.clone(),
                eligibility,
            });

            if started.elapsed().as_millis() >= u128::from(AGENT_MESSAGE_DISPATCH_SCAN_MAX_MILLIS) {
                truncated_by_time = true;
                break;
            }
        }

        // Read-only: rolling back and committing are equivalent, but committing
        // matches every other transaction in this module and releases the
        // snapshot deterministically.
        tx.commit()?;

        // Only offer a continuation when the page may not have reached the end
        // of the queue. A short page that was not time-truncated IS the end.
        let exhausted = !truncated_by_time && scanned < limit;
        Ok(DispatchableAgentMessagePage {
            messages,
            next_cursor: if exhausted { None } else { next_cursor },
        })
    }

    /// P2-04 / C-P2-21: the ONE claim transaction, `queued` → `claimed`.
    ///
    /// In a single `BEGIN IMMEDIATE` it inserts attempt `k+1`, inserts the
    /// exact `queued→claimed` transition at version `N+1`, moves the aggregate
    /// pointer, CAS-binds the delivery Session's current model invocation, and
    /// arms the exact rotated-tip watch (C-P2-19). Any failure — including an
    /// affected-row count other than one at ANY statement — rolls the whole
    /// aggregate back, so a lost CAS leaves zero attempt, zero transition, zero
    /// Session binding, zero watch, and zero external effect.
    ///
    /// Dispatch is deliberately NOT part of this function: provider dispatch is
    /// an external effect and never happens inside a SQLite transaction. The
    /// caller dispatches only after this commits.
    ///
    /// A CAS loss is a typed [`ClaimAgentMessageOutcome::CasLost`], not an
    /// error, because the dispatcher must respond to it by settling the unused
    /// model invocation with a pre-effect error class and releasing its grant —
    /// which is ordinary control flow, not a fault.
    pub(crate) fn claim_agent_message_exact(
        &self,
        request: &ClaimAgentMessageRequest,
    ) -> Result<ClaimAgentMessageOutcome> {
        let outcome = self.claim_agent_message_exact_inner(request)?;
        // CAS losses are a required metric (C-P2-23), and the exact class is
        // what makes them diagnosable rather than just countable.
        if let ClaimAgentMessageOutcome::CasLost(loss) = outcome {
            tracing::warn!(
                target: "agent_coordination",
                message_id = %request.message_id,
                delivery_session_id = %request.delivery_session_id,
                cas_loss_class = loss.as_str(),
                "agent message claim lost its CAS; no attempt, effect, or watch was written"
            );
        }
        Ok(outcome)
    }

    fn claim_agent_message_exact_inner(
        &self,
        request: &ClaimAgentMessageRequest,
    ) -> Result<ClaimAgentMessageOutcome> {
        let capability_kind = request.provider_kind.capability_kind();
        let boundary_kind = capability_kind.boundary_kind();
        let next_attempt_number = request
            .expected_current_attempt_number
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| DaemonError::Store("agent message attempt counter overflow".into()))?;

        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;

        // ---- 1. exact aggregate fence -----------------------------------
        let aggregate: Option<(String, i64, i64, Option<i64>)> = tx
            .query_row(
                "SELECT state, state_version, attempt_count, current_attempt_number
                   FROM agent_messages WHERE id=?1",
                params![request.message_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((state, state_version, attempt_count, current_attempt_number)) = aggregate else {
            return Ok(ClaimAgentMessageOutcome::CasLost(
                AgentMessageClaimCasLoss::MessageMissing,
            ));
        };
        if state != AgentMessageStateV1::Queued.as_str() {
            return Ok(ClaimAgentMessageOutcome::CasLost(
                AgentMessageClaimCasLoss::MessageNotQueued,
            ));
        }
        if state_version != request.expected_state_version {
            return Ok(ClaimAgentMessageOutcome::CasLost(
                AgentMessageClaimCasLoss::StateVersionMismatch,
            ));
        }
        // The caller's view of the attempt history must be exact: `attempt_count`
        // is what the next attempt number is derived from, and the pointer must
        // agree with it (the aggregate CHECK already ties them together, so a
        // disagreement here means the caller read a different generation).
        let expected_attempt_count =
            i64::from(request.expected_current_attempt_number.unwrap_or(0));
        if attempt_count != expected_attempt_count
            || current_attempt_number != request.expected_current_attempt_number.map(i64::from)
        {
            return Ok(ClaimAgentMessageOutcome::CasLost(
                AgentMessageClaimCasLoss::CurrentAttemptMismatch,
            ));
        }

        // ---- 2. exact delivery-Session fence ----------------------------
        let session: Option<(String, i64, Option<String>)> = tx
            .query_row(
                "SELECT status, rotation_depth, model_invocation_id
                   FROM sessions WHERE id=?1",
                params![request.delivery_session_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((status_text, rotation_depth, prior_invocation)) = session else {
            return Ok(ClaimAgentMessageOutcome::CasLost(
                AgentMessageClaimCasLoss::DeliverySessionMissing,
            ));
        };
        let status = str_to_session_status(&status_text)?;
        if !session_status_admits_delivery(status) {
            return Ok(ClaimAgentMessageOutcome::CasLost(
                AgentMessageClaimCasLoss::DeliverySessionNotLive,
            ));
        }
        if rotation_depth != request.expected_session_generation {
            return Ok(ClaimAgentMessageOutcome::CasLost(
                AgentMessageClaimCasLoss::SessionGenerationMismatch,
            ));
        }
        let prior_invocation_uuid = prior_invocation
            .as_deref()
            .and_then(|value| Uuid::parse_str(value).ok());
        if prior_invocation_uuid != request.expected_prior_model_invocation_id {
            return Ok(ClaimAgentMessageOutcome::CasLost(
                AgentMessageClaimCasLoss::SessionInvocationMismatch,
            ));
        }

        // The attempt row's FK would catch a missing invocation, but a typed
        // loss is what the dispatcher can actually act on.
        let invocation_exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM model_invocations WHERE id=?1)",
            params![request.delivery_model_invocation_id.to_string()],
            |row| row.get(0),
        )?;
        if !invocation_exists {
            return Ok(ClaimAgentMessageOutcome::CasLost(
                AgentMessageClaimCasLoss::ModelInvocationMissing,
            ));
        }

        let now = Utc::now();
        let now_string = now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let expires_string = request
            .claim_expires_at
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);

        // ---- 3. insert the append-only attempt --------------------------
        //
        // Correlation custody is AppServer-only: a native-multi-turn attempt
        // starts `correlation_pending` because the forward-only trigger admits
        // ONLY `correlation_pending → correlated|sealed_live_uncertain`, so an
        // attempt that started `not_applicable` could never later correlate.
        // Every terminal-one-turn provider is permanently `not_applicable`.
        let correlation_state = match capability_kind {
            BoundaryCapabilityKindV1::NativeMultiTurn => CorrelationStateV1::CorrelationPending,
            BoundaryCapabilityKindV1::TerminalOneTurn => CorrelationStateV1::NotApplicable,
        };
        let attempt_rows = tx.execute(
            "INSERT INTO agent_message_delivery_attempts (
                 message_id, attempt_number, claim_token, delivery_boot_id,
                 logical_root_session_id, delivery_session_id,
                 delivery_session_generation, delivery_model_invocation_id,
                 provider_kind, capability_kind, boundary_kind,
                 claimed_at, claim_expires_at, correlation_state,
                 attempt_state, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?12)",
            params![
                request.message_id.to_string(),
                next_attempt_number,
                request.claim_token.to_string(),
                request.delivery_boot_id.to_string(),
                request.logical_root_session_id.to_string(),
                request.delivery_session_id.to_string(),
                request.expected_session_generation,
                request.delivery_model_invocation_id.to_string(),
                request.provider_kind.as_str(),
                capability_kind.as_str(),
                boundary_kind.as_str(),
                now_string,
                expires_string,
                correlation_state.as_str(),
                AttemptStateV1::Claimed.as_str(),
            ],
        )?;
        exactly_one_row("agent_message_delivery_attempts insert", attempt_rows)?;

        // ---- 4. insert the exact queued→claimed transition --------------
        let next_state_version = state_version
            .checked_add(1)
            .ok_or_else(|| DaemonError::Store("agent message state version overflow".into()))?;
        let evidence_digest = agent_message_digest(
            "claim",
            &[
                request.message_id.as_bytes(),
                &next_attempt_number.to_be_bytes(),
                request.claim_token.as_bytes(),
                request.delivery_boot_id.as_bytes(),
                request.delivery_session_id.as_bytes(),
                &request.expected_session_generation.to_be_bytes(),
                request.delivery_model_invocation_id.as_bytes(),
            ],
        );
        let transition_rows = tx.execute(
            "INSERT INTO agent_message_state_transitions (
                 message_id, state_version, from_state, to_state, attempt_number,
                 authority_kind, authority_id, evidence_digest, created_at)
             VALUES (?1,?2,'queued','claimed',?3,'dispatcher',?4,?5,?6)",
            params![
                request.message_id.to_string(),
                next_state_version,
                next_attempt_number,
                request.authority_id.to_string(),
                evidence_digest,
                now_string,
            ],
        )?;
        exactly_one_row("agent_message_state_transitions insert", transition_rows)?;

        // ---- 5. move the aggregate under its exact CAS ------------------
        let aggregate_rows = tx.execute(
            "UPDATE agent_messages
                SET state='claimed', state_version=?2, attempt_count=?3,
                    current_attempt_number=?3, updated_at=?4
              WHERE id=?1 AND state='queued' AND state_version=?5",
            params![
                request.message_id.to_string(),
                next_state_version,
                next_attempt_number,
                now_string,
                state_version,
            ],
        )?;
        exactly_one_row("agent_messages claim CAS", aggregate_rows)?;

        // ---- 6. CAS-bind the delivery Session's model invocation --------
        //
        // Never an unconditional overwrite (C-P2-21): the exact row, a still
        // live/admissible status, an unchanged generation, and the caller's
        // exact prior invocation must ALL still hold at write time.
        let session_rows = tx.execute(
            "UPDATE sessions
                SET model_invocation_id=?2
              WHERE id=?1
                AND rotation_depth=?3
                AND status=?4
                AND model_invocation_id IS ?5",
            params![
                request.delivery_session_id.to_string(),
                request.delivery_model_invocation_id.to_string(),
                request.expected_session_generation,
                status_text,
                request
                    .expected_prior_model_invocation_id
                    .map(|id| id.to_string()),
            ],
        )?;
        exactly_one_row("sessions model invocation CAS", session_rows)?;

        // ---- 7. arm the exact rotated-tip watch -------------------------
        //
        // C-P2-19: the EXACT live tip, not the logical root, and inside this
        // same transaction — a claim that cannot arm its watch does not happen.
        arm_agent_message_watch_in_tx(
            &tx,
            request.owner_session_id,
            request.delivery_session_id,
            now,
        )?;

        tx.commit()?;
        Ok(ClaimAgentMessageOutcome::Claimed(MessageAttemptFenceV1 {
            message_id: request.message_id,
            attempt_number: next_attempt_number,
            claim_token: request.claim_token,
            delivery_boot_id: request.delivery_boot_id,
            delivery_session_id: request.delivery_session_id,
            delivery_session_generation: request.expected_session_generation,
            delivery_model_invocation_id: request.delivery_model_invocation_id,
        }))
    }

    /// H21-P2-R5-001: commit the durable PRE-DISPATCH marker.
    ///
    /// This is a deliberately tiny transaction whose ONLY job is to be durable
    /// before the provider send. It writes no evidence and reaches no
    /// conclusion; it records that the delivery path is about to cross the
    /// external-effect boundary.
    ///
    /// **Its value is entirely in the ordering.** The caller must commit this
    /// BEFORE the send and must REFUSE to send if it fails. A marker written
    /// after the send, or skipped on error, proves nothing and rebuilds exactly
    /// the ambiguity it exists to remove: without it, a crash before the send
    /// and a crash after a completed-but-unrecorded send leave the identical
    /// durable triple (`claimed`, foreign `delivery_boot_id`, no admission),
    /// which is why the crash-window requeue licence was withdrawn as unsound.
    ///
    /// With the marker the two windows separate, and P2-06 may read them as:
    /// `claimed` + foreign boot ⇒ proved no effect; `dispatching` + foreign
    /// boot ⇒ uncertain, never requeue. **This slice does not implement that
    /// recovery** — it only makes the evidence exist.
    ///
    /// The `attempt_state='claimed'` guard makes this exactly-once: a second
    /// call matches zero rows and errors rather than restamping a window that
    /// has already opened.
    pub(crate) fn mark_agent_message_attempt_dispatching(
        &self,
        fence: &MessageAttemptFenceV1,
    ) -> Result<()> {
        let now_string = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let rows = self.conn.execute(
            "UPDATE agent_message_delivery_attempts
                SET attempt_state='dispatching',
                    dispatching_at=?3,
                    updated_at=?3
              WHERE message_id=?1 AND attempt_number=?2
                AND attempt_state='claimed'
                AND dispatching_at IS NULL",
            params![
                fence.message_id.to_string(),
                fence.attempt_number,
                now_string,
            ],
        )?;
        exactly_one_row("agent message attempt pre-dispatch marker", rows)?;
        Ok(())
    }

    /// P2-04: persist the returned [`BoundaryAdmissionV1`] in the SECOND
    /// immediate transaction, after dispatch has already happened.
    ///
    /// All three classifications settle in ONE transaction, so there is never a
    /// later aggregate CAS that could be lost after the attempt already moved:
    ///
    /// * `rejected_before_effect` — seals the attempt proved-no-effect, inserts
    ///   its transition, and requeues/expires/fails the aggregate together.
    /// * `unsupported` — seals `unsupported`, inserts the matching transition
    ///   evidence, and CASes the aggregate to `failed`.
    /// * `admitted_effect_possible` — fills the boundary/effect fields, moves
    ///   the attempt to `effect_possible`, inserts the transition evidence, and
    ///   CASes the aggregate `claimed → injected`.
    ///
    /// A crash or error after an adapter's effect threshold but before this
    /// commits has no durable rejection proof. Reconciliation must then record
    /// conservative effect-possible evidence and advance to `uncertain` — never
    /// back to `queued`. That asymmetry is why only a PROVED rejection may
    /// requeue here.
    pub(crate) fn record_agent_message_admission(
        &self,
        fence: &MessageAttemptFenceV1,
        admission: &BoundaryAdmissionV1,
        no_effect_disposition: NoEffectDisposition,
        authority_id: Uuid,
    ) -> Result<RecordAdmissionOutcome> {
        admission
            .validate()
            .map_err(|class| DaemonError::InvalidParam(class.to_string()))?;
        if admission.delivery_session_id != fence.delivery_session_id
            || admission.session_generation != fence.delivery_session_generation
            || admission.model_invocation_id != fence.delivery_model_invocation_id
        {
            return Err(DaemonError::InvalidParam(
                "agent_message_admission_does_not_match_its_attempt_fence".into(),
            ));
        }

        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;

        let Some(current) = read_claimed_attempt_fence(&tx, fence)? else {
            return Ok(RecordAdmissionOutcome::FenceLost);
        };

        let now = Utc::now();
        let now_string = now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let next_state_version = current
            .state_version
            .checked_add(1)
            .ok_or_else(|| DaemonError::Store("agent message state version overflow".into()))?;
        let evidence_digest = agent_message_digest(
            "admission",
            &[
                fence.message_id.as_bytes(),
                &fence.attempt_number.to_be_bytes(),
                fence.claim_token.as_bytes(),
                admission.classification.as_str().as_bytes(),
                admission
                    .provider_error_class
                    .as_deref()
                    .unwrap_or("")
                    .as_bytes(),
            ],
        );

        let (to_state, terminal_disposition, effect_classification, attempt_state) =
            match admission.classification {
                BoundaryClassificationV1::AdmittedEffectPossible => (
                    AgentMessageStateV1::Injected,
                    None,
                    EffectClassificationV1::EffectPossible,
                    AttemptStateV1::EffectPossible,
                ),
                BoundaryClassificationV1::Unsupported => (
                    AgentMessageStateV1::Failed,
                    // `unsupported` is its own disposition: the provider was
                    // never called, so it is proved no-effect, but it must not
                    // masquerade as a retryable rejection.
                    Some(AttemptTerminalDispositionV1::Unsupported),
                    EffectClassificationV1::ProvedNoEffect,
                    AttemptStateV1::Terminal,
                ),
                BoundaryClassificationV1::RejectedBeforeEffect => {
                    let (state, disposition) = match no_effect_disposition {
                        NoEffectDisposition::Requeue => (
                            AgentMessageStateV1::Queued,
                            AttemptTerminalDispositionV1::ProvedNoEffectRequeue,
                        ),
                        NoEffectDisposition::Expired => (
                            AgentMessageStateV1::Expired,
                            AttemptTerminalDispositionV1::ProvedNoEffectExpired,
                        ),
                        NoEffectDisposition::Failed => (
                            AgentMessageStateV1::Failed,
                            AttemptTerminalDispositionV1::ProvedNoEffectFailed,
                        ),
                    };
                    // A caller may not expire a message whose durable expiry has
                    // not actually passed: expiry is a property of the row, not
                    // of the dispatcher's opinion.
                    if no_effect_disposition == NoEffectDisposition::Expired
                        && !current.expires_at.is_some_and(|expiry| expiry <= now)
                    {
                        return Err(DaemonError::InvalidParam(
                            "agent_message_expiry_disposition_requires_a_passed_expiry".into(),
                        ));
                    }
                    (
                        state,
                        Some(disposition),
                        EffectClassificationV1::ProvedNoEffect,
                        AttemptStateV1::Terminal,
                    )
                }
            };

        // ---- 1. fill the attempt's admission/effect evidence ------------
        //
        // Ordered FIRST because the transition and aggregate triggers both read
        // the attempt's sealed disposition in this same transaction.
        let attempt_rows = tx.execute(
            "UPDATE agent_message_delivery_attempts
                SET admission_classification=?3,
                    effect_classification=?4,
                    attempt_state=?5,
                    boundary_value=COALESCE(boundary_value,?6),
                    provider_error_class=?7,
                    admission_recorded_at=?8,
                    effect_possible_at=CASE WHEN ?4='effect_possible' THEN ?8 ELSE NULL END,
                    terminal_disposition=?9,
                    settlement_authority=CASE WHEN ?9 IS NULL THEN NULL ELSE 'dispatcher' END,
                    settlement_evidence_digest=CASE WHEN ?9 IS NULL THEN NULL ELSE ?10 END,
                    settled_at=CASE WHEN ?9 IS NULL THEN NULL ELSE ?8 END,
                    terminal_at=CASE WHEN ?9 IS NULL THEN NULL ELSE ?8 END,
                    updated_at=?8
              WHERE message_id=?1 AND attempt_number=?2
                -- `dispatching` is the ordinary state here: every attempt that
                -- actually reached the provider passed the pre-dispatch marker
                -- first, so TX-C lands on a `dispatching` row. `claimed` still
                -- matches because the pre-dispatch rejection exits record their
                -- admission without ever marking. Dropping either spelling
                -- would make this fill match zero rows and fail the attempt via
                -- `exactly_one_row` (H21-P2-R5-001).
                AND attempt_state IN ('claimed','dispatching')
                AND admission_classification IS NULL",
            params![
                fence.message_id.to_string(),
                fence.attempt_number,
                admission.classification.as_str(),
                effect_classification.as_str(),
                attempt_state.as_str(),
                admission.boundary_value(),
                admission.provider_error_class,
                now_string,
                terminal_disposition.map(AttemptTerminalDispositionV1::as_str),
                evidence_digest,
            ],
        )?;
        exactly_one_row("agent message attempt admission fill", attempt_rows)?;

        // ---- 2. insert the exact transition -----------------------------
        let transition_rows = tx.execute(
            "INSERT INTO agent_message_state_transitions (
                 message_id, state_version, from_state, to_state, attempt_number,
                 authority_kind, authority_id, evidence_digest, created_at)
             VALUES (?1,?2,'claimed',?3,?4,'dispatcher',?5,?6,?7)",
            params![
                fence.message_id.to_string(),
                next_state_version,
                to_state.as_str(),
                fence.attempt_number,
                authority_id.to_string(),
                evidence_digest,
                now_string,
            ],
        )?;
        exactly_one_row("agent message admission transition", transition_rows)?;

        // ---- 3. move the aggregate under its exact CAS ------------------
        //
        // The pointer deliberately STAYS on the sealed attempt across a requeue
        // (`attempt_count` is unchanged); it moves only when the next claim
        // inserts `attempt_count+1`.
        let safe_error_class = match to_state {
            AgentMessageStateV1::Failed => Some(
                admission
                    .provider_error_class
                    .clone()
                    .unwrap_or_else(|| "agent_message_provider_rejected".to_string()),
            ),
            _ => None,
        };
        let aggregate_rows = tx.execute(
            "UPDATE agent_messages
                SET state=?3,
                    state_version=?2,
                    safe_error_class=COALESCE(?4,safe_error_class),
                    updated_at=?5,
                    failed_at=CASE WHEN ?3='failed' THEN ?5 ELSE failed_at END,
                    expired_at=CASE WHEN ?3='expired' THEN ?5 ELSE expired_at END
              WHERE id=?1 AND state='claimed' AND state_version=?6
                AND current_attempt_number=?7",
            params![
                fence.message_id.to_string(),
                next_state_version,
                to_state.as_str(),
                safe_error_class,
                now_string,
                current.state_version,
                fence.attempt_number,
            ],
        )?;
        exactly_one_row("agent message admission aggregate CAS", aggregate_rows)?;

        tx.commit()?;
        Ok(RecordAdmissionOutcome::Recorded {
            state: to_state,
            state_version: next_state_version,
        })
    }

    /// P2-04: the ONE atomic acknowledgement transaction.
    ///
    /// In a single `BEGIN IMMEDIATE` it checks the aggregate pointer, the exact
    /// attempt fence, the delivery Session's current invocation, the genuine
    /// native turn where the boundary requires one, and the event's Session;
    /// then inserts the provider-originated event, fills the acknowledgement
    /// triple, seals the attempt `acknowledged`, inserts the exact transition,
    /// and CASes the aggregate `claimed|injected → acknowledged`. It returns the
    /// REAL event ID.
    ///
    /// `uncertain → acknowledged` is deliberately NOT reachable here. C-P2-22
    /// removes the generic `Injected|Uncertain` edge: only a specifically fenced
    /// unsealed `correlation_pending` AppServer attempt may late-ack from
    /// uncertain, through its own separate correlate-and-acknowledge operation.
    ///
    /// A daemon-created user/injection event is never acknowledgement proof, so
    /// a `Role::User` event is refused before anything is written.
    pub(crate) fn insert_event_and_acknowledge_agent_message(
        &self,
        fence: &MessageAttemptFenceV1,
        event: &ConversationEvent,
        late_admission: Option<&BoundaryAdmissionV1>,
        genuine_native_turn_id: Option<&str>,
        authority_id: Uuid,
    ) -> Result<AcknowledgeAgentMessageOutcome> {
        // The daemon writes the user/injection event that DELIVERS a message.
        // Accepting one as proof that the message was answered would let the
        // daemon acknowledge its own write.
        if event.role == Some(Role::User) {
            return Err(DaemonError::InvalidParam(
                "agent_message_user_event_is_never_acknowledgement_proof".into(),
            ));
        }
        if event.session_id != fence.delivery_session_id {
            return Err(DaemonError::InvalidParam(
                "agent_message_acknowledging_event_must_live_on_the_delivery_session".into(),
            ));
        }

        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;

        let Some(current) = read_acknowledgeable_attempt(&tx, fence)? else {
            return Ok(AcknowledgeAgentMessageOutcome::FenceLost);
        };

        // The delivery Session must still be running THIS invocation. A Session
        // that moved on cannot have produced this response.
        let live_invocation: Option<String> = tx
            .query_row(
                "SELECT model_invocation_id FROM sessions WHERE id=?1",
                params![fence.delivery_session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        if live_invocation.as_deref()
            != Some(fence.delivery_model_invocation_id.to_string().as_str())
        {
            return Ok(AcknowledgeAgentMessageOutcome::FenceLost);
        }

        // A native-turn boundary is only acknowledgeable once a GENUINE provider
        // turn ID exists — either already durable, or supplied now.
        let boundary_value = match current.boundary_kind.as_str() {
            "native_turn" => {
                let turn = genuine_native_turn_id
                    .map(str::to_string)
                    .or_else(|| current.boundary_value.clone());
                let Some(turn) = turn else {
                    return Err(DaemonError::InvalidParam(
                        "agent_message_native_turn_boundary_requires_a_genuine_turn_id".into(),
                    ));
                };
                if let Some(existing) = current.boundary_value.as_deref() {
                    if existing != turn {
                        return Ok(AcknowledgeAgentMessageOutcome::FenceLost);
                    }
                }
                Some(turn)
            }
            _ => Some(fence.delivery_model_invocation_id.to_string()),
        };

        // A still-`claimed` aggregate acknowledges only with the complete
        // provider fence that atomically fills its admitted/effect-possible
        // evidence: the response IS the proof the effect happened.
        let filling_admission = if current.state == AgentMessageStateV1::Claimed {
            let Some(admission) = late_admission else {
                return Err(DaemonError::InvalidParam(
                    "agent_message_claimed_acknowledgement_requires_the_complete_admission_fence"
                        .into(),
                ));
            };
            admission
                .validate()
                .map_err(|class| DaemonError::InvalidParam(class.to_string()))?;
            if admission.classification != BoundaryClassificationV1::AdmittedEffectPossible
                || admission.delivery_session_id != fence.delivery_session_id
                || admission.session_generation != fence.delivery_session_generation
                || admission.model_invocation_id != fence.delivery_model_invocation_id
            {
                return Err(DaemonError::InvalidParam(
                    "agent_message_admission_does_not_match_its_attempt_fence".into(),
                ));
            }
            true
        } else {
            false
        };

        let now = Utc::now();
        let now_string = now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let next_state_version = current
            .state_version
            .checked_add(1)
            .ok_or_else(|| DaemonError::Store("agent message state version overflow".into()))?;

        // ---- 1. insert the provider-originated event --------------------
        //
        // FIRST, because the attempt's ack-event-coherence trigger resolves the
        // acknowledgement triple against `conversation_events` in this same
        // transaction.
        let tool_input_json = event
            .tool_input
            .as_ref()
            .map(|value| serde_json::to_string(value))
            .transpose()
            .map_err(|error| {
                DaemonError::Store(format!("failed to serialize tool_input: {error}"))
            })?;
        let event_rows = tx.execute(
            "INSERT INTO conversation_events (
                 session_id, sequence, event_type, role, content,
                 tool_name, tool_input, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                event.session_id.to_string(),
                event.sequence,
                event_type_to_str(event.event_type),
                event.role.map(role_to_str),
                event.content,
                event.tool_name,
                tool_input_json,
                event.created_at.to_rfc3339(),
            ],
        )?;
        exactly_one_row("acknowledging conversation event insert", event_rows)?;
        let event_id = tx.last_insert_rowid();

        let evidence_digest = agent_message_digest(
            "acknowledgement",
            &[
                fence.message_id.as_bytes(),
                &fence.attempt_number.to_be_bytes(),
                fence.claim_token.as_bytes(),
                &event_id.to_be_bytes(),
            ],
        );

        // ---- 2. seal the attempt acknowledged ---------------------------
        let attempt_rows = tx.execute(
            "UPDATE agent_message_delivery_attempts
                SET boundary_value=?3,
                    admission_classification=COALESCE(admission_classification,
                        CASE WHEN ?4 THEN 'admitted_effect_possible' ELSE NULL END),
                    admission_recorded_at=COALESCE(admission_recorded_at,
                        CASE WHEN ?4 THEN ?5 ELSE NULL END),
                    effect_possible_at=COALESCE(effect_possible_at,
                        CASE WHEN ?4 THEN ?5 ELSE NULL END),
                    effect_classification='effect_acknowledged',
                    attempt_state='terminal',
                    terminal_disposition='acknowledged',
                    settlement_authority='acknowledgement_store',
                    settlement_evidence_digest=?6,
                    acknowledged_event_id=?7,
                    acknowledged_event_session_id=?8,
                    acknowledged_event_sequence=?9,
                    acknowledged_at=?5,
                    settled_at=?5,
                    terminal_at=?5,
                    updated_at=?5
              WHERE message_id=?1 AND attempt_number=?2
                -- `dispatching` belongs here: a provider can acknowledge a turn
                -- whose TX-C admission record failed or has not yet landed, and
                -- such a row is still `dispatching`. Omitting it would drop the
                -- acknowledgement of a message that WAS delivered.
                AND attempt_state IN
                    ('claimed','dispatching','effect_possible')
                AND acknowledged_event_id IS NULL",
            params![
                fence.message_id.to_string(),
                fence.attempt_number,
                boundary_value,
                filling_admission,
                now_string,
                evidence_digest,
                event_id,
                event.session_id.to_string(),
                event.sequence,
            ],
        )?;
        exactly_one_row("agent message attempt acknowledgement seal", attempt_rows)?;

        // ---- 3. insert the exact transition -----------------------------
        let transition_rows = tx.execute(
            "INSERT INTO agent_message_state_transitions (
                 message_id, state_version, from_state, to_state, attempt_number,
                 authority_kind, authority_id, evidence_digest, created_at)
             VALUES (?1,?2,?3,'acknowledged',?4,'acknowledgement_store',?5,?6,?7)",
            params![
                fence.message_id.to_string(),
                next_state_version,
                current.state.as_str(),
                fence.attempt_number,
                authority_id.to_string(),
                evidence_digest,
                now_string,
            ],
        )?;
        exactly_one_row("agent message acknowledgement transition", transition_rows)?;

        // ---- 4. CAS the aggregate to acknowledged -----------------------
        let aggregate_rows = tx.execute(
            "UPDATE agent_messages
                SET state='acknowledged', state_version=?2,
                    acknowledged_at=?3, updated_at=?3
              WHERE id=?1 AND state=?4 AND state_version=?5
                AND current_attempt_number=?6",
            params![
                fence.message_id.to_string(),
                next_state_version,
                now_string,
                current.state.as_str(),
                current.state_version,
                fence.attempt_number,
            ],
        )?;
        exactly_one_row(
            "agent message acknowledgement aggregate CAS",
            aggregate_rows,
        )?;

        tx.commit()?;
        Ok(AcknowledgeAgentMessageOutcome::Acknowledged {
            event_id,
            state_version: next_state_version,
        })
    }

    /// Commit one attempt's `correlation_pending → sealed_live_uncertain`
    /// transition — the durable half of the C-P2-15 control worker.
    ///
    /// This is the ONLY production writer of `sealed_live_uncertain`. It is a
    /// compare-and-swap: the `correlation_state='correlation_pending'`
    /// predicate lives in the `WHERE` clause, so the decision to seal and the
    /// seal itself are one statement. SQLite makes a single `UPDATE` atomic, so
    /// the seal is either durably recorded with both of its coupled evidence
    /// columns or not recorded at all — it can never be half-applied, and no
    /// explicit transaction is needed to make that true.
    ///
    /// Idempotency (C-P2-15): a second call for the same attempt matches zero
    /// rows, because the row is no longer `correlation_pending`. Re-scanning a
    /// latch therefore cannot double-settle or inflate any counter — only the
    /// FIRST call reports [`AppServerSealCommitV1::Committed`].
    ///
    /// The zero-row case is then classified by reading the durable state back,
    /// so the caller can distinguish "already done" from "a stronger fact won"
    /// from "no such row". That read is diagnostic only; the seal decision was
    /// already made atomically by the `UPDATE`.
    ///
    /// This performs NO provider I/O and starts no transaction, so it can never
    /// hold a SQLite write lock across an external effect.
    ///
    /// # Errors
    ///
    /// Propagates SQLite failures, including a V81 trigger or CHECK refusal. A
    /// refusal means the caller's belief was wrong and must never be worked
    /// around by loosening the constraint.
    pub(crate) fn commit_app_server_evidence_seal_v1(
        &self,
        message_id: Uuid,
        attempt_number: u32,
        suppression_class: &str,
        now: DateTime<Utc>,
    ) -> Result<AppServerSealCommitV1> {
        // The column is bounded by CHECK; a caller that exceeds it is a bug in
        // the caller, not something to truncate silently.
        debug_assert!(
            !suppression_class.is_empty()
                && suppression_class.len() <= AGENT_MESSAGE_MAX_ENUM_BYTES,
            "evidence_suppression_class must fit the frozen V81 ceiling"
        );
        let now_string = now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let rows = self.conn.execute(
            "UPDATE agent_message_delivery_attempts
                SET correlation_state='sealed_live_uncertain',
                    evidence_suppression_class=?3,
                    evidence_sealed_at=?4,
                    updated_at=?4
              WHERE message_id=?1 AND attempt_number=?2
                AND correlation_state='correlation_pending'",
            params![
                message_id.to_string(),
                attempt_number,
                suppression_class,
                now_string,
            ],
        )?;
        if rows == 1 {
            return Ok(AppServerSealCommitV1::Committed);
        }
        let durable: Option<String> = self
            .conn
            .query_row(
                "SELECT correlation_state FROM agent_message_delivery_attempts
                  WHERE message_id=?1 AND attempt_number=?2",
                params![message_id.to_string(), attempt_number],
                |row| row.get(0),
            )
            .optional()?;
        Ok(match durable.as_deref() {
            Some("sealed_live_uncertain") => AppServerSealCommitV1::AlreadySealed,
            Some(_) => AppServerSealCommitV1::Superseded,
            None => AppServerSealCommitV1::Unknown,
        })
    }

    /// Every durable AppServer attempt that is still live, for startup keyset
    /// reconciliation (C-P2-15).
    ///
    /// Terminal attempts are excluded: they are settled and must not re-consume
    /// the global 64-attempt cap. An already-`sealed_live_uncertain` attempt IS
    /// returned, so reconciliation can register it WITHOUT sealing it a second
    /// time — that is what stops a committed seal from being double-counted
    /// across a restart.
    ///
    /// # Errors
    ///
    /// Propagates SQLite failures.
    pub(crate) fn list_live_app_server_attempts_v1(&self) -> Result<Vec<LiveAppServerAttemptV1>> {
        let mut stmt = self.conn.prepare(
            "SELECT message_id, attempt_number, claim_token, delivery_boot_id,
                    delivery_session_id, delivery_session_generation,
                    delivery_model_invocation_id, correlation_state, boundary_value
               FROM agent_message_delivery_attempts
              WHERE capability_kind='native_multi_turn'
                AND attempt_state!='terminal'
              ORDER BY message_id, attempt_number
              LIMIT ?1",
        )?;
        let rows = stmt.query_map(
            params![i64::try_from(AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS).unwrap_or(i64::MAX)],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                ))
            },
        )?;
        let mut out = Vec::new();
        for row in rows {
            let (
                message_id,
                attempt_number,
                claim_token,
                delivery_boot_id,
                delivery_session_id,
                delivery_session_generation,
                delivery_model_invocation_id,
                correlation_state,
                provider_turn_id,
            ) = row?;
            let (
                Ok(message_id),
                Ok(attempt_number),
                Ok(claim_token),
                Ok(delivery_boot_id),
                Ok(delivery_session_id),
                Ok(delivery_model_invocation_id),
                Some(correlation),
            ) = (
                Uuid::parse_str(&message_id),
                u32::try_from(attempt_number),
                Uuid::parse_str(&claim_token),
                Uuid::parse_str(&delivery_boot_id),
                Uuid::parse_str(&delivery_session_id),
                Uuid::parse_str(&delivery_model_invocation_id),
                CorrelationStateV1::from_str_exact(&correlation_state),
            )
            else {
                continue;
            };
            out.push(LiveAppServerAttemptV1 {
                fence: MessageAttemptFenceV1 {
                    message_id,
                    attempt_number,
                    claim_token,
                    delivery_boot_id,
                    delivery_session_id,
                    delivery_session_generation,
                    delivery_model_invocation_id,
                },
                correlation,
                provider_turn_id,
            });
        }
        Ok(out)
    }

    /// P2-06a: every live delivery attempt whose owning daemon incarnation is
    /// gone, classified by the NARROWED crash-window rule.
    ///
    /// # THIS QUERY IS THE ONLY ENCODING OF THE NARROWED RULE
    ///
    /// The pre-dispatch marker (H21-P2-R5-001 option (i)) is the *entire*
    /// reason any crash-window inference is sound, and it licenses EXACTLY two
    /// readings and NOTHING WIDER:
    ///
    /// * `claimed` + foreign `delivery_boot_id` + no recorded admission
    ///   ⇒ **PROVED NO EFFECT.** The dead incarnation never reached the marker,
    ///   and [`Store::mark_agent_message_attempt_dispatching`] commits strictly
    ///   before the send while its `Err` arm refuses to send at all, so it
    ///   never reached the send. Requeue is permitted.
    /// * `dispatching` or anything later + foreign `delivery_boot_id`
    ///   ⇒ **UNCERTAIN.** The send may have completed and never been recorded.
    ///   Requeue is **FORBIDDEN**. These STRAND, and stranding is the CORRECT
    ///   answer: it is operator-recoverable, whereas a double-delivered paid
    ///   model turn is not.
    ///
    /// **DO NOT WIDEN THIS.** Lease expiry, a generic `Err`, a timeout, missing
    /// output, or process death is NEVER proof of no effect — that inference is
    /// the withdrawn defect H21-P2-R5-001 (HIGH) this whole campaign exists to
    /// prevent. Note what the `CASE` does NOT do: it does not read
    /// `claim_expires_at`, and there is deliberately no lease predicate
    /// anywhere in this statement.
    ///
    /// Defence in depth: the requeue writer
    /// [`Store::requeue_crashed_agent_message_attempt_v1`] re-checks the SAME
    /// conjunction in its own `WHERE`, so widening this classifier alone still
    /// cannot requeue a post-marker row — it fails loudly via
    /// `exactly_one_row` instead. Both encodings are pinned by
    /// `a_dispatching_row_with_a_foreign_boot_is_uncertain_and_never_requeues`.
    ///
    /// Only attempts the aggregate still points at are returned, and only while
    /// the aggregate is itself mid-delivery (`claimed`/`injected`): a message
    /// that already reached a terminal or `uncertain` state has been settled by
    /// evidence and must not be reopened by a boot-id observation.
    ///
    /// # Pagination — the bound is OBSERVABLE, never silent (P2-06b)
    ///
    /// One call returns at most [`AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS`] rows
    /// in `(message_id, attempt_number)` order, together with a continuation
    /// cursor. **`next_cursor.is_none()` is the ONLY evidence that recovery has
    /// seen the whole population**; a caller that stops after one page has not
    /// recovered anything it did not look at. Drive it to exhaustion:
    ///
    /// ```text
    /// let mut after = None;
    /// loop {
    ///     let page = store.list_crashed_agent_message_attempts_page_v1(boot, after)?;
    ///     for row in &page.attempts { /* requeue or mark uncertain */ }
    ///     match page.next_cursor { Some(next) => after = Some(next), None => break }
    /// }
    /// ```
    ///
    /// Keyset, never `OFFSET`. Recovery MUTATES the rows it walks — a requeue
    /// seals the attempt `terminal` and an uncertainty moves the aggregate to
    /// `uncertain`, and BOTH drop the row out of this query's predicate. Under
    /// `OFFSET` every processed row would shift the window and the scan would
    /// skip exactly as many rows as it had just fixed.
    ///
    /// **The cursor advances for every row REACHED, including a row this
    /// function declines to return.** The malformed-row `continue` below skips
    /// a row it cannot parse; if the cursor advanced only over *returned* rows,
    /// such a row would sit at the head of every subsequent page forever and
    /// starve everything behind it. This is the same rule, for the same reason,
    /// as [`Self::list_dispatchable_agent_messages`]'s ineligible-row cursor.
    ///
    /// A full page yields `Some(cursor)` even when it happened to be the last
    /// one, so a drain ends on one final empty page — matching the dispatch
    /// scan's `exhausted` contract rather than inventing a second one.
    ///
    /// Deliberately NOT time-bounded, unlike the dispatch scan: that scan
    /// resolves a lineage tip and reads delivery facts PER ROW, so its cost is
    /// unbounded per row and it needs [`AGENT_MESSAGE_DISPATCH_SCAN_MAX_MILLIS`].
    /// This is one indexed query with no per-row I/O amplification, so the row
    /// bound already bounds the transaction.
    ///
    /// # Errors
    ///
    /// Propagates SQLite failures, and refuses a row whose key cannot be parsed
    /// (see the loud-failure note in the body).
    pub(crate) fn list_crashed_agent_message_attempts_page_v1(
        &self,
        live_boot_id: Uuid,
        after: Option<CrashRecoveryCursorV1>,
    ) -> Result<CrashedAgentMessageAttemptPageV1> {
        let limit = AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS;
        let (after_message_id, after_attempt_number) = match after {
            Some(cursor) => (
                Some(cursor.message_id.to_string()),
                Some(i64::from(cursor.attempt_number)),
            ),
            None => (None, None),
        };

        let mut stmt = self.conn.prepare(CRASHED_AGENT_MESSAGE_ATTEMPT_SQL)?;
        let rows = stmt
            .query_map(
                params![
                    live_boot_id.to_string(),
                    i64::try_from(limit).unwrap_or(i64::MAX),
                    after_message_id,
                    after_attempt_number,
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let scanned = rows.len();
        let mut attempts = Vec::with_capacity(scanned);
        let mut next_cursor = None;

        for (message_id, attempt_number, state, state_version, attempt_state, verdict) in rows {
            // The KEY is parsed first and separately, and a key that will not
            // parse is a LOUD failure rather than a skip. A row we cannot key is
            // a row we cannot page past: skipping it would either abandon
            // everything behind it or re-serve it forever. Unreachable today —
            // `message_id` carries the uuid CHECK and `attempt_number>0` is an
            // INTEGER CHECK — but the alternative to failing here is exactly the
            // silent truncation this method was rewritten to remove.
            let (Ok(message_id), Ok(attempt_number)) =
                (Uuid::parse_str(&message_id), u32::try_from(attempt_number))
            else {
                return Err(DaemonError::Store(
                    "crashed agent message attempt has an unpageable key".into(),
                ));
            };
            next_cursor = Some(CrashRecoveryCursorV1 {
                message_id,
                attempt_number,
            });

            let (Some(aggregate_state), Some(attempt_state), Some(verdict)) = (
                AgentMessageStateV1::from_str_exact(&state),
                AttemptStateV1::from_str_exact(&attempt_state),
                CrashRecoveryVerdictV1::from_str_exact(&verdict),
            ) else {
                // The cursor above already moved past this row on purpose.
                continue;
            };
            attempts.push(CrashedAgentMessageAttemptV1 {
                message_id,
                attempt_number,
                aggregate_state,
                state_version,
                attempt_state,
                verdict,
            });
        }

        // A short page IS the end of the population; a full one may not be.
        let exhausted = scanned < limit;
        Ok(CrashedAgentMessageAttemptPageV1 {
            attempts,
            next_cursor: if exhausted { None } else { next_cursor },
        })
    }

    /// P2-06a: the `claimed → queued` requeue, under the NARROWED RULE ONLY.
    ///
    /// One exact-fence `BEGIN IMMEDIATE` transaction atomically records
    /// `admission_classification=rejected_before_effect`,
    /// `effect_classification=proved_no_effect`, terminal disposition
    /// `proved_no_effect_requeue`, the settlement authority and evidence
    /// digest, the transition, and the aggregate requeue. There is no partial
    /// requeue.
    ///
    /// The aggregate KEEPS `current_attempt_number` pointing at the sealed
    /// attempt until the next claim inserts `attempt_count+1`; no evidence is
    /// ever cleared.
    ///
    /// # The guard is the load-bearing line
    ///
    /// `AND attempt_state='claimed' AND admission_classification IS NULL AND
    /// delivery_boot_id!=?` is the narrowed rule re-encoded at the point of
    /// WRITE. A `dispatching` row — one that may have reached the provider —
    /// matches ZERO rows here and fails loudly through `exactly_one_row`
    /// rather than being requeued. **Lease expiry is NEVER this proof, and
    /// this statement deliberately cannot see `claim_expires_at`.**
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::Store`] if the attempt is not requeue-eligible
    /// under the narrowed rule, and propagates SQLite failures.
    pub(crate) fn requeue_crashed_agent_message_attempt_v1(
        &self,
        message_id: Uuid,
        attempt_number: u32,
        live_boot_id: Uuid,
        authority_id: Uuid,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let now_string = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let state_version = read_recoverable_aggregate_version(
            &tx,
            message_id,
            attempt_number,
            AgentMessageStateV1::Claimed,
        )?;
        let next_state_version = state_version
            .checked_add(1)
            .ok_or_else(|| DaemonError::Store("agent message state version overflow".into()))?;
        let evidence_digest = agent_message_digest(
            "restart_recovery_proved_no_effect_requeue",
            &[
                message_id.as_bytes(),
                &attempt_number.to_be_bytes(),
                live_boot_id.as_bytes(),
            ],
        );

        // ---- 1. seal the attempt proved-no-effect ------------------------
        let attempt_rows = tx.execute(
            "UPDATE agent_message_delivery_attempts
                SET admission_classification='rejected_before_effect',
                    effect_classification='proved_no_effect',
                    attempt_state='terminal',
                    terminal_disposition='proved_no_effect_requeue',
                    settlement_authority='restart_reconciler',
                    settlement_evidence_digest=?4,
                    admission_recorded_at=?3,
                    settled_at=?3,
                    terminal_at=?3,
                    updated_at=?3
              WHERE message_id=?1 AND attempt_number=?2
                -- THE NARROWED RULE, re-encoded at the point of write. A
                -- `dispatching` row matches zero rows here on purpose.
                AND attempt_state='claimed'
                AND admission_classification IS NULL
                AND delivery_boot_id!=?5",
            params![
                message_id.to_string(),
                i64::from(attempt_number),
                now_string,
                evidence_digest,
                live_boot_id.to_string(),
            ],
        )?;
        exactly_one_row(
            "restart recovery proved-no-effect requeue seal (narrowed rule refused a \
             post-marker row, or the attempt was not claimed on a foreign boot)",
            attempt_rows,
        )?;

        // ---- 2. insert the exact transition ------------------------------
        let transition_rows = tx.execute(
            "INSERT INTO agent_message_state_transitions (
                 message_id, state_version, from_state, to_state, attempt_number,
                 authority_kind, authority_id, evidence_digest, created_at)
             VALUES (?1,?2,'claimed','queued',?3,'restart_reconciler',?4,?5,?6)",
            params![
                message_id.to_string(),
                next_state_version,
                i64::from(attempt_number),
                authority_id.to_string(),
                evidence_digest,
                now_string,
            ],
        )?;
        exactly_one_row("restart recovery requeue transition", transition_rows)?;

        // ---- 3. requeue the aggregate, keeping its pointer ---------------
        let aggregate_rows = tx.execute(
            "UPDATE agent_messages
                SET state='queued', state_version=?2, updated_at=?3
              WHERE id=?1 AND state='claimed' AND state_version=?4
                AND current_attempt_number=?5",
            params![
                message_id.to_string(),
                next_state_version,
                now_string,
                state_version,
                i64::from(attempt_number),
            ],
        )?;
        exactly_one_row("restart recovery requeue aggregate CAS", aggregate_rows)?;

        tx.commit()?;
        Ok(())
    }

    /// P2-06a: advance a crashed `claimed`/`injected` aggregate to `uncertain`.
    ///
    /// This is the answer for every attempt the narrowed rule does NOT clear —
    /// above all a `dispatching` row, whose send may have completed and never
    /// been recorded. It does **not** requeue and it does **not** seal the
    /// attempt terminal: uncertainty is retained custody, not settlement.
    ///
    /// Conservative effect-possible evidence is filled first when the attempt
    /// never recorded admission, because `agent_messages_v81_cas_coherence`
    /// requires the current attempt to carry
    /// `effect_classification='effect_possible'` before the aggregate may be
    /// `uncertain` — the schema refuses to record uncertainty without the
    /// evidence that justifies it. Filling `effect_possible` is NOT sealing:
    /// `attempt_state` stops at `effect_possible`, `terminal_disposition`
    /// stays null, and the attempt remains live for later exact correlation.
    ///
    /// # The boot fence, symmetric with the requeue writer (R11 LOW-4)
    ///
    /// `live_boot_id` is the identity of the daemon incarnation calling this, and
    /// this writer refuses any attempt NOT stamped with a foreign one. Its sibling
    /// [`Self::requeue_crashed_agent_message_attempt_v1`] has always re-encoded
    /// `delivery_boot_id != ?` at its point of write; this one had no equivalent,
    /// so the narrowed rule's defence-in-depth was present on one crash writer and
    /// absent on the other. R11 cleared it as unreachable — the classifier already
    /// excludes live-boot rows and the whole pass runs under the process-wide store
    /// mutex — but P2-06c is the first code to call this writer at all, and an
    /// asymmetric guard is a trap for whoever calls it next.
    ///
    /// **This is strictly a NARROWING.** It can only ever REFUSE a row the
    /// previous code accepted; no row becomes more recoverable, and nothing about
    /// the conservative fill's `IN ('claimed','dispatching')` semantics changes.
    ///
    /// It is encoded TWICE, deliberately, and the two are not redundant:
    ///
    /// * as a **precondition read**, which is what actually refuses; and
    /// * at the **point of write**, mirroring the requeue writer's encoding.
    ///
    /// The point-of-write clause cannot do the refusing on its own here, and that
    /// is the one real asymmetry between the two writers: requeue demands
    /// `exactly_one_row` from its seal, so a zero-row match there IS a refusal.
    /// This writer's fill legitimately matches zero rows — an attempt that already
    /// carries its classifications needs no fill and must still be able to reach
    /// `uncertain` — so a zero-row match cannot be read as a refusal, and adding
    /// the fence there alone would let a live-boot row through to the aggregate
    /// CAS. Hence the explicit precondition.
    ///
    /// **The layering is ATTRIBUTED BY EXPERIMENT, not asserted.** Three mutations
    /// of this function, each applied and reverted from a sha256-verified copy:
    ///
    /// | mutation | observed |
    /// |---|---|
    /// | precondition removed | still REFUSED, but not by this fence — `the_uncertainty_writer_refuses_an_attempt_the_live_incarnation_owns` fails on its "must refuse first, naming why" assertion |
    /// | point-of-write clause removed | every test still passes — the precondition refuses first, so no test observes this clause alone |
    /// | **BOTH removed** | the writer **ACCEPTS** a live-owned `dispatching` attempt and marks it `uncertain` |
    ///
    /// So the second encoding is not decoration: the difference between the first
    /// row (refused) and the third (accepted) is exactly this clause. It is the
    /// same shape as the requeue writer's guard standing in front of the V82
    /// backstop CHECK — a layer whose job is to hold when the layer in front of it
    /// is removed, and which therefore cannot be observed while that layer stands.
    ///
    /// # Errors
    ///
    /// Propagates SQLite failures, refuses a row that is no longer live, and
    /// refuses an attempt that is missing or owned by the LIVE incarnation.
    pub(crate) fn mark_crashed_agent_message_attempt_uncertain_v1(
        &self,
        message_id: Uuid,
        attempt_number: u32,
        from_state: AgentMessageStateV1,
        live_boot_id: Uuid,
        authority_id: Uuid,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let now_string = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let state_version =
            read_recoverable_aggregate_version(&tx, message_id, attempt_number, from_state)?;
        let next_state_version = state_version
            .checked_add(1)
            .ok_or_else(|| DaemonError::Store("agent message state version overflow".into()))?;

        // ---- 0. the boot fence ------------------------------------------
        //
        // Crash recovery reasons only about DEAD incarnations. An attempt
        // carrying the live `delivery_boot_id` belongs to a delivery that is
        // running right now, and recording uncertainty about it would destroy
        // the admission evidence that delivery is about to write.
        let foreign_boot = tx
            .query_row(
                "SELECT 1 FROM agent_message_delivery_attempts
                  WHERE message_id=?1 AND attempt_number=?2 AND delivery_boot_id!=?3",
                params![
                    message_id.to_string(),
                    i64::from(attempt_number),
                    live_boot_id.to_string(),
                ],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !foreign_boot {
            return Err(DaemonError::Store(
                "restart recovery may only record uncertainty about an attempt left \
                 behind by a FOREIGN daemon incarnation; this attempt is missing or \
                 carries the LIVE delivery_boot_id"
                    .into(),
            ));
        }

        // ---- 1. conservative effect-possible fill, never a no-effect proof
        let filled = tx.execute(
            "UPDATE agent_message_delivery_attempts
                SET admission_classification='admitted_effect_possible',
                    effect_classification='effect_possible',
                    attempt_state='effect_possible',
                    admission_recorded_at=COALESCE(admission_recorded_at,?3),
                    effect_possible_at=COALESCE(effect_possible_at,?3),
                    updated_at=?3
              WHERE message_id=?1 AND attempt_number=?2
                AND admission_classification IS NULL
                AND effect_classification IS NULL
                -- `dispatching` is precisely the case this exists for. It must
                -- NEVER be narrowed to a no-effect proof here.
                AND attempt_state IN ('claimed','dispatching')
                -- The narrowed rule's boot fence, re-encoded at the point of
                -- write exactly as the requeue writer does it. Redundant with
                -- the precondition above BY DESIGN: either alone could be
                -- removed in isolation by a later author.
                AND delivery_boot_id!=?4",
            params![
                message_id.to_string(),
                i64::from(attempt_number),
                now_string,
                live_boot_id.to_string(),
            ],
        )?;
        if filled > 1 {
            return Err(DaemonError::Store(
                "restart recovery conservative effect-possible fill matched more than one row"
                    .into(),
            ));
        }

        let evidence_digest = agent_message_digest(
            "restart_recovery_effect_possible_uncertain",
            &[message_id.as_bytes(), &attempt_number.to_be_bytes()],
        );

        // ---- 2. insert the exact transition ------------------------------
        let transition_rows = tx.execute(
            "INSERT INTO agent_message_state_transitions (
                 message_id, state_version, from_state, to_state, attempt_number,
                 authority_kind, authority_id, evidence_digest, created_at)
             VALUES (?1,?2,?3,'uncertain',?4,'restart_reconciler',?5,?6,?7)",
            params![
                message_id.to_string(),
                next_state_version,
                from_state.as_str(),
                i64::from(attempt_number),
                authority_id.to_string(),
                evidence_digest,
                now_string,
            ],
        )?;
        exactly_one_row("restart recovery uncertain transition", transition_rows)?;

        // ---- 3. advance the aggregate ------------------------------------
        //
        // `safe_error_class` is deliberately NOT set: `uncertain` is not a
        // failure classification.
        let aggregate_rows = tx.execute(
            "UPDATE agent_messages
                SET state='uncertain', state_version=?2, uncertain_at=?3, updated_at=?3
              WHERE id=?1 AND state=?4 AND state_version=?5
                AND current_attempt_number=?6",
            params![
                message_id.to_string(),
                next_state_version,
                now_string,
                from_state.as_str(),
                state_version,
                i64::from(attempt_number),
            ],
        )?;
        exactly_one_row("restart recovery uncertain aggregate CAS", aggregate_rows)?;

        tx.commit()?;
        Ok(())
    }

    /// P2-06b / C-P2-09: the `expiry_reconciler`'s ONE edge — `queued → expired`
    /// with a NULL attempt.
    ///
    /// This fills the seat V81 built and left empty: `expiry_reconciler` was
    /// already an `authority_kind`, and
    /// `agent_message_transitions_v81_coherence` already REQUIRED that a
    /// `queued → expired` edge carry a null attempt AND exactly this authority.
    /// Nothing authored one. `list_dispatchable_agent_messages` has meanwhile
    /// been classifying durably-expired queued rows
    /// [`AgentMessageDispatchEligibility::Expired`] and holding them back, so
    /// until now they were held back by a reconciler that did not exist.
    ///
    /// # This writer can NEVER expire a live attempt, by construction
    ///
    /// It refuses anything but `queued`. That is not a convenience check, it is
    /// the same discriminator as the narrowed rule:
    ///
    /// * A `queued` message has NO live attempt to reason about. `attempt_count`
    ///   is either zero, or the pointer is a *historical* attempt already sealed
    ///   `proved_no_effect_requeue`. Expiring it can strand no effect because no
    ///   send is outstanding — hence the NULL attempt, and hence no attempt is
    ///   created (creating one would manufacture a delivery that never happened,
    ///   and `agent_message_transitions_v81_coherence` would refuse the edge).
    /// * A `claimed` message HAS a live attempt, and expiring it requires the
    ///   attempt to be proved no effect and sealed `proved_no_effect_expired` in
    ///   the same transaction. **Only the provider's own `RejectedBeforeEffect`
    ///   answer is that proof**, so that edge belongs exclusively to
    ///   [`Self::record_agent_message_admission`] (`NoEffectDisposition::Expired`)
    ///   and is unreachable from here.
    /// * A `dispatching` attempt therefore cannot be reached at all: it implies
    ///   the aggregate is `claimed`. **A wall-clock deadline is NEVER proof of
    ///   no effect** — that inference is the withdrawn defect H21-P2-R5-001.
    ///
    /// # `expires_at` is the MESSAGE's deadline, not a lease
    ///
    /// `agent_messages.expires_at` is set once at acceptance and frozen by
    /// `agent_messages_v81_acceptance_identity_immutable`. It is emphatically
    /// NOT `agent_message_delivery_attempts.claim_expires_at`, the delivery
    /// lease — this statement cannot see that column, and must never learn to.
    /// Expiry is a property of the row, so a caller whose deadline has not
    /// actually passed is refused, mirroring the identical guard in
    /// `record_agent_message_admission`.
    ///
    /// The historical requeue pointer is PRESERVED: `current_attempt_number` is
    /// not cleared, so the sealed attempt that caused the last requeue stays
    /// joinable from the expired aggregate.
    ///
    /// # Errors
    ///
    /// Returns [`DaemonError::Store`] if the message is not `queued`, or
    /// [`DaemonError::InvalidParam`] if its durable expiry has not passed, and
    /// propagates SQLite failures. A lost CAS is a loud error, not a silent
    /// no-op; the periodic worker (P2-06c) owns the retry policy.
    pub(crate) fn expire_queued_agent_message_v1(
        &self,
        message_id: Uuid,
        authority_id: Uuid,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let now = Utc::now();
        let now_string = now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);

        let found: Option<(String, i64, Option<String>)> = tx
            .query_row(
                "SELECT state, state_version, expires_at FROM agent_messages WHERE id=?1",
                params![message_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((state, state_version, expires_at)) = found else {
            return Err(DaemonError::Store(
                "expiry reconciliation target message does not exist".into(),
            ));
        };

        // The narrowed discriminator, re-encoded at the point of WRITE.
        if state != AgentMessageStateV1::Queued.as_str() {
            return Err(DaemonError::Store(format!(
                "expiry reconciliation refuses a {state} message: only `queued` has no live \
                 attempt to strand. A `claimed` message may be expired ONLY by the provider's \
                 own rejected-before-effect answer, and a wall-clock deadline is never proof \
                 of no effect"
            )));
        }

        let expires_at = expires_at
            .as_deref()
            .map(|value| parse_store_timestamp("agent message expires_at", value))
            .transpose()?;
        if !expires_at.is_some_and(|expiry| expiry <= now) {
            return Err(DaemonError::InvalidParam(
                "agent_message_expiry_requires_a_passed_expiry".into(),
            ));
        }

        let next_state_version = state_version
            .checked_add(1)
            .ok_or_else(|| DaemonError::Store("agent message state version overflow".into()))?;
        let evidence_digest = agent_message_digest(
            "expiry_reconciler_queued_expired",
            &[message_id.as_bytes(), now_string.as_bytes()],
        );

        // ---- 1. the NULL-attempt transition ------------------------------
        //
        // Ordered first: `agent_messages_v81_cas_coherence` reads this row in
        // the same transaction to authorize the aggregate move.
        let transition_rows = tx.execute(
            "INSERT INTO agent_message_state_transitions (
                 message_id, state_version, from_state, to_state, attempt_number,
                 authority_kind, authority_id, evidence_digest, created_at)
             VALUES (?1,?2,'queued','expired',NULL,'expiry_reconciler',?3,?4,?5)",
            params![
                message_id.to_string(),
                next_state_version,
                authority_id.to_string(),
                evidence_digest,
                now_string,
            ],
        )?;
        exactly_one_row("expiry reconciliation transition", transition_rows)?;

        // ---- 2. expire the aggregate, keeping its historical pointer -----
        //
        // `current_attempt_number` is deliberately untouched, and no attempt is
        // created. `safe_error_class` is not set either: expiry is not a
        // failure classification.
        let aggregate_rows = tx.execute(
            "UPDATE agent_messages
                SET state='expired', state_version=?2, expired_at=?3, updated_at=?3
              WHERE id=?1 AND state='queued' AND state_version=?4",
            params![
                message_id.to_string(),
                next_state_version,
                now_string,
                state_version,
            ],
        )?;
        exactly_one_row("expiry reconciliation aggregate CAS", aggregate_rows)?;

        tx.commit()?;
        Ok(())
    }

    /// Issue #46 Phase A: atomically settle one queued message whose current
    /// rotation-lineage tip became terminal before delivery.
    ///
    /// This is deliberately a separate writer from spawn settlement. A target
    /// Session exists and was deliverable when this mail was accepted; the
    /// evidence here is that its current lineage tip is now one of the five
    /// non-deliverable terminal statuses. The existing `restart_reconciler`
    /// authority truthfully identifies the worker performing that observation,
    /// and `authority_id` is the same live daemon boot UUID used by its crash
    /// recovery edges.
    ///
    /// The caller's selection is advisory. Under one `BEGIN IMMEDIATE`, this
    /// writer re-reads the aggregate, gives a passed durable expiry precedence,
    /// re-resolves the lineage tip, and re-authenticates the tip's Session and
    /// status. A live rotation successor therefore refuses the stale terminal
    /// selection and leaves the message queued.
    ///
    /// Only `queued` is reachable. No attempt is created or updated, the
    /// transition carries a NULL attempt, and a historical sealed-requeue
    /// `current_attempt_number` remains intact.
    pub(crate) fn fail_queued_agent_message_terminal_before_delivery_v1(
        &self,
        message_id: Uuid,
        authority_id: Uuid,
    ) -> Result<()> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let now = Utc::now();
        let now_string = now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);

        let found: Option<(String, i64, String, Option<String>)> = tx
            .query_row(
                "SELECT state, state_version, target_session_id, expires_at
                   FROM agent_messages WHERE id=?1",
                params![message_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((state, state_version, logical_root, expires_at)) = found else {
            return Err(DaemonError::Store(
                "terminal-before-delivery settlement target message does not exist".into(),
            ));
        };
        if state != AgentMessageStateV1::Queued.as_str() {
            return Err(DaemonError::Store(format!(
                "terminal-before-delivery settlement refuses a {state} message: only `queued` \
                 has no live delivery custody"
            )));
        }

        let expires_at = expires_at
            .as_deref()
            .map(|value| parse_store_timestamp("agent message expires_at", value))
            .transpose()?;
        if expires_at.is_some_and(|expiry| expiry <= now) {
            return Err(DaemonError::InvalidParam(
                "agent_message_terminal_settlement_yields_to_passed_expiry".into(),
            ));
        }

        let logical_root_session_id = parse_store_uuid("agent message target", &logical_root)?;
        let delivery_session_id = resolve_lineage_tip(&tx, logical_root_session_id)?;
        let delivery = read_delivery_session_facts(&tx, delivery_session_id)?.ok_or_else(|| {
            DaemonError::Store(
                "terminal-before-delivery settlement requires an existing delivery Session".into(),
            )
        })?;
        if !session_status_is_terminal_before_delivery(delivery.status) {
            return Err(DaemonError::Store(format!(
                "terminal-before-delivery settlement refuses delivery tip {delivery_session_id} \
                 in status {}",
                session_status_to_str(delivery.status)
            )));
        }
        if terminal_recovery_pending_tx(&tx, delivery_session_id)? {
            return Err(DaemonError::Store(format!(
                "agent_message_terminal_settlement_recovery_pending:{delivery_session_id}"
            )));
        }

        let next_state_version = state_version
            .checked_add(1)
            .ok_or_else(|| DaemonError::Store("agent message state version overflow".into()))?;
        let safe_error_class = TARGET_SESSION_TERMINAL_BEFORE_DELIVERY_ERROR_CLASS;
        let status = session_status_to_str(delivery.status);
        let evidence_digest = agent_message_digest(
            "restart_reconciler_terminal_before_delivery_failed",
            &[
                message_id.as_bytes(),
                logical_root_session_id.as_bytes(),
                delivery_session_id.as_bytes(),
                status.as_bytes(),
                safe_error_class.as_bytes(),
            ],
        );

        let transition_rows = tx.execute(
            "INSERT INTO agent_message_state_transitions (
                 message_id, state_version, from_state, to_state, attempt_number,
                 authority_kind, authority_id, evidence_digest, created_at)
             VALUES (?1,?2,'queued','failed',NULL,'restart_reconciler',?3,?4,?5)",
            params![
                message_id.to_string(),
                next_state_version,
                authority_id.to_string(),
                evidence_digest,
                now_string,
            ],
        )?;
        exactly_one_row(
            "terminal-before-delivery reconciliation transition",
            transition_rows,
        )?;

        let aggregate_rows = tx.execute(
            "UPDATE agent_messages
                SET state='failed', state_version=?2, failed_at=?3, updated_at=?3,
                    safe_error_class=?4
              WHERE id=?1 AND state='queued' AND state_version=?5",
            params![
                message_id.to_string(),
                next_state_version,
                now_string,
                safe_error_class,
                state_version,
            ],
        )?;
        exactly_one_row(
            "terminal-before-delivery reconciliation aggregate CAS",
            aggregate_rows,
        )?;

        tx.commit()?;
        Ok(())
    }

    /// C-P2-18: the ONLY permanent-failure writer for a reserved child.
    ///
    /// In one `BEGIN IMMEDIATE` transaction this CASes the exact live spawn
    /// request to `failed`, keyset-selects every message addressed to that
    /// reservation under the frozen 128-target cap, and settles every row —
    /// or rolls back all rows *and* the spawn failure together. There is no
    /// partial settlement: a permanently failed reservation must never leave
    /// a message queued behind it.
    ///
    /// Per-row disposition is decided by evidence, never by convenience:
    ///
    /// - `queued` takes a null-attempt `spawn_settlement` edge to `failed`.
    ///   No delivery was ever attempted, so no effect can exist.
    /// - `claimed` may fail ONLY when its exact current attempt is already
    ///   proved no effect (`rejected_before_effect` + `proved_no_effect`). It
    ///   is sealed `proved_no_effect_failed` and transitioned in this same
    ///   transaction.
    /// - Every other live row is **effect-possible** and becomes `uncertain`
    ///   with invocation/process custody retained. A `claimed` attempt that
    ///   never recorded admission cannot prove absence of effect, so this
    ///   fills the conservative `admitted_effect_possible`/`effect_possible`
    ///   evidence P2-04 mandates for exactly that case and advances to
    ///   `uncertain` — **never** back to `queued`. Downgrading an unprovable
    ///   attempt to a clean failure would be uncertainty erasure, which is the
    ///   failure class this schema exists to prevent.
    /// - `acknowledged` / `failed` / `expired` are immutable and untouched.
    ///
    /// Publication is the caller's job and happens strictly after commit; the
    /// returned outcome carries exactly what changed.
    ///
    /// # Errors
    ///
    /// Returns a `Store` error when the reservation is missing, already
    /// `launched`, exceeds the frozen target cap, or when any affected-row
    /// count is not exactly one — every one of which rolls the whole
    /// transaction back.
    pub(crate) fn settle_reserved_child_failure(
        &self,
        spawn_request_id: Uuid,
        safe_error_class: &str,
        authority_id: Uuid,
    ) -> Result<SettleReservedChildFailureOutcome> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;

        let request = tx
            .query_row(
                &format!("{SPAWN_REQUEST_SELECT} WHERE spawn_request_id=?1"),
                [spawn_request_id.to_string()],
                map_spawn_request_row,
            )
            .optional()?
            .ok_or_else(|| {
                DaemonError::Store(
                    "agent spawn request missing at reserved-child failure settlement".into(),
                )
            })?;

        // A launched child is a live session, not a reserved failure. Refusing
        // is the point: this writer must never be the thing that tears down a
        // child that actually started.
        if request.state == AgentSpawnStateV1::Launched {
            return Err(DaemonError::Store(
                "launched agent spawn request cannot be settled as a reserved-child failure".into(),
            ));
        }

        // Exact replay is read-only and settles nothing a second time.
        if request.state == AgentSpawnStateV1::Failed {
            tx.commit()?;
            return Ok(SettleReservedChildFailureOutcome {
                spawn_request_id,
                owner_session_id: request.owner_session_id,
                child_session_id: request.child_session_id,
                already_failed: true,
                rows: Vec::new(),
            });
        }

        let now = Utc::now();
        let now_string = now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);

        let spawn_rows = tx.execute(
            "UPDATE agent_spawn_requests
                SET state='failed', updated_at=?1, failed_at=COALESCE(failed_at,?1),
                    safe_error_class=?2
              WHERE spawn_request_id=?3 AND state IN ('reserved','queued','launching')",
            params![now_string, safe_error_class, spawn_request_id.to_string()],
        )?;
        exactly_one_row(
            "agent_spawn_requests reserved-child failure CAS",
            spawn_rows,
        )?;

        // Keyset-ordered read under the frozen cap. One extra row is requested
        // so overflow is DETECTED rather than silently truncated — a truncated
        // settlement would leave mail queued behind a dead reservation.
        let scan_limit = i64::try_from(AGENT_MESSAGE_SPAWN_SETTLEMENT_MAX_TARGETS)
            .unwrap_or(i64::MAX)
            .saturating_add(1);
        let mut stmt = tx.prepare(
            "SELECT id, state, state_version, current_attempt_number
               FROM agent_messages
              WHERE target_spawn_request_id=?1
              ORDER BY created_at, id
              LIMIT ?2",
        )?;
        let mut selected: Vec<(Uuid, String, i64, Option<u32>)> = Vec::new();
        let mut cursor = stmt.query(params![spawn_request_id.to_string(), scan_limit])?;
        while let Some(row) = cursor.next()? {
            let id: String = row.get(0)?;
            let message_id = Uuid::parse_str(&id).map_err(|_| {
                DaemonError::Store("agent message id is not a canonical UUID".into())
            })?;
            let attempt: Option<i64> = row.get(3)?;
            let attempt = attempt
                .map(u32::try_from)
                .transpose()
                .map_err(|_| DaemonError::Store("agent message attempt pointer overflow".into()))?;
            selected.push((message_id, row.get(1)?, row.get(2)?, attempt));
        }
        drop(cursor);
        drop(stmt);

        if selected.len() > AGENT_MESSAGE_SPAWN_SETTLEMENT_MAX_TARGETS {
            return Err(DaemonError::Store(format!(
                "agent spawn settlement exceeds the frozen {AGENT_MESSAGE_SPAWN_SETTLEMENT_MAX_TARGETS}-target cap; \
                 refusing to settle partially"
            )));
        }

        let mut rows = Vec::with_capacity(selected.len());
        for (message_id, state, state_version, current_attempt_number) in selected {
            let disposition = settle_one_spawn_settlement_row(
                &tx,
                message_id,
                &state,
                state_version,
                current_attempt_number,
                safe_error_class,
                authority_id,
                &now_string,
            )?;
            rows.push(SpawnSettlementRowOutcome {
                message_id,
                disposition,
                state_version,
            });
        }

        tx.commit()?;
        Ok(SettleReservedChildFailureOutcome {
            spawn_request_id,
            owner_session_id: request.owner_session_id,
            child_session_id: request.child_session_id,
            already_failed: false,
            rows,
        })
    }

    // =====================================================================
    // P2-07 U1 — exact-turn gate lifecycle (C-P2-17).
    //
    // Three single-statement compare-and-swaps over the V81 gate catalog.
    // The shape is `commit_app_server_evidence_seal_v1`'s: the decision
    // predicate lives in the `WHERE`, so deciding and acting are ONE atomic
    // statement and no explicit transaction is needed; a zero-row result is
    // then classified by a diagnostic read-back.
    //
    // Every method here is a synchronous `fn`. That is structural, not
    // stylistic: a `fn` cannot suspend, so "an external provider effect
    // inside a SQLite transaction" is unreachable rather than merely absent.
    // =====================================================================

    /// Open the exact-turn gate for one acknowledged AppServer delivery
    /// attempt, returning the generation it was opened at.
    ///
    /// **The `WHERE EXISTS` is the CAS, and it is the only coherence guard
    /// this table has.** Unlike the provider-request ledger, the gate table
    /// carries NO `BEFORE INSERT` trigger — its five V81 triggers are all
    /// `BEFORE DELETE`/`BEFORE UPDATE`. So the predicate below is the entire
    /// admissibility check, and it is deliberately the SAME predicate
    /// `agent_message_request_effects_v81_evidence_coherence` applies to
    /// request inserts: the attempt must exist at this exact delivery
    /// invocation, its boundary value must BE this provider turn, and it must
    /// already be `correlated` and `effect_acknowledged`. A gate opened
    /// without it would be inert (no request could ever join it) but it could
    /// still be closed, which writes terminal authority.
    ///
    /// `gate_generation` is derived as `MAX(...)+1` inside the insert. That
    /// relies on SQLite serializing writers, which holds for this single
    /// `Store` connection and would NOT hold under multi-writer WAL. If a
    /// second writer is ever added, the generation must come from the arbiter
    /// grant instead of being derived here.
    ///
    /// # Errors
    ///
    /// Propagates SQLite failures, including a V81 CHECK refusal. A refusal
    /// means the caller's belief was wrong and must never be worked around by
    /// loosening the constraint.
    pub(crate) fn open_app_server_turn_gate_v1(
        &self,
        fence: &TurnGateFenceV1,
        opened_at: DateTime<Utc>,
    ) -> Result<GateOpenOutcomeV1> {
        let opened = opened_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let rows = self.conn.execute(
            "INSERT INTO agent_message_provider_turn_gates (
                 message_id, attempt_number, delivery_model_invocation_id,
                 provider_turn_id, gate_generation, gate_state, admission_sequence,
                 issued_permit_count, settled_permit_count, opened_at, updated_at)
             SELECT ?1, ?2, ?3, ?4,
                    COALESCE((SELECT MAX(g.gate_generation)+1
                                FROM agent_message_provider_turn_gates g
                               WHERE g.message_id=?1 AND g.attempt_number=?2
                                 AND g.delivery_model_invocation_id=?3
                                 AND g.provider_turn_id=?4), 0),
                    'open', 0, 0, 0, ?5, ?5
              WHERE EXISTS(SELECT 1 FROM agent_message_delivery_attempts a
                            WHERE a.message_id=?1 AND a.attempt_number=?2
                              AND a.delivery_model_invocation_id=?3
                              AND a.boundary_value=?4
                              AND a.correlation_state='correlated'
                              AND a.effect_classification='effect_acknowledged')",
            params![
                fence.message_id.to_string(),
                fence.attempt_number,
                fence.delivery_model_invocation_id.to_string(),
                fence.provider_turn_id.as_str(),
                opened,
            ],
        )?;
        if rows == 0 {
            return Ok(GateOpenOutcomeV1::AttemptNotAdmissible);
        }
        let gate_generation: i64 = self.conn.query_row(
            "SELECT MAX(gate_generation) FROM agent_message_provider_turn_gates
              WHERE message_id=?1 AND attempt_number=?2
                AND delivery_model_invocation_id=?3 AND provider_turn_id=?4",
            params![
                fence.message_id.to_string(),
                fence.attempt_number,
                fence.delivery_model_invocation_id.to_string(),
                fence.provider_turn_id.as_str(),
            ],
            |row| row.get(0),
        )?;
        Ok(GateOpenOutcomeV1::Opened { gate_generation })
    }

    /// Move the exact-turn gate `open → closing`, sealing terminal authority.
    ///
    /// **This is THE compare-and-swap, and it is the single production writer
    /// of `gate_state='closing'`.** Both the request-wins path and the
    /// terminal-wins path call this one method; that is what makes the race
    /// between them decidable at all, and a source scan in the test suite
    /// pins that `SET gate_state='closing'` appears exactly once in the
    /// production zone of this file.
    ///
    /// The `gate_state='open'` predicate in the `WHERE` is the whole race
    /// resolution: the first caller matches one row and wins, every later
    /// caller matches zero and is told, via the diagnostic read-back, that it
    /// lost AND which authority beat it. A wrong or missing provider turn
    /// matches zero rows for free, because the turn is part of the primary
    /// key — no special-casing.
    ///
    /// # Errors
    ///
    /// Propagates SQLite failures, including the V81 `..._v81_forward` and
    /// `..._admission_coherence` refusals that independently re-encode
    /// fill-once terminal authority.
    pub(crate) fn close_app_server_turn_gate_to_closing_v1(
        &self,
        fence: &TurnGateFenceV1,
        gate_generation: i64,
        terminal_authority: &str,
        terminal_evidence_digest: &str,
        closing_at: DateTime<Utc>,
    ) -> Result<GateClosingOutcomeV1> {
        debug_assert!(
            !terminal_authority.is_empty()
                && terminal_authority.len() <= AGENT_MESSAGE_MAX_ENUM_BYTES,
            "terminal_authority must fit the frozen V81 ceiling"
        );
        let closing = closing_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let rows = self.conn.execute(
            "UPDATE agent_message_provider_turn_gates
                SET gate_state='closing', terminal_authority=?6,
                    terminal_evidence_digest=?7, closing_at=?8, updated_at=?8
              WHERE message_id=?1 AND attempt_number=?2
                AND delivery_model_invocation_id=?3 AND provider_turn_id=?4
                AND gate_generation=?5 AND gate_state='open'",
            params![
                fence.message_id.to_string(),
                fence.attempt_number,
                fence.delivery_model_invocation_id.to_string(),
                fence.provider_turn_id.as_str(),
                gate_generation,
                terminal_authority,
                terminal_evidence_digest,
                closing,
            ],
        )?;
        if rows == 1 {
            return Ok(GateClosingOutcomeV1::Closing);
        }
        match self.read_turn_gate_state_row_v1(fence, gate_generation)? {
            None => Ok(GateClosingOutcomeV1::NoSuchGate),
            Some((state, authority)) => match state.as_str() {
                "closing" => Ok(GateClosingOutcomeV1::AlreadyClosing {
                    terminal_authority: authority.unwrap_or_default(),
                }),
                "closed" => Ok(GateClosingOutcomeV1::AlreadyClosed {
                    terminal_authority: authority.unwrap_or_default(),
                }),
                other => Err(DaemonError::Store(format!(
                    "V81 exact-turn gate closing CAS matched zero rows while the durable gate \
                     state is {other:?}; a single-connection Store cannot reach this and the \
                     durable state must not be guessed"
                ))),
            },
        }
    }

    /// Settle the exact-turn gate `closing → closed`.
    ///
    /// The `settled_permit_count=issued_permit_count` predicate and the V81
    /// `..._close_requires_settled_permits` trigger are TWO INDEPENDENT
    /// encodings of the same fact: the predicate reads the gate's counters,
    /// the trigger re-derives the truth from the permit rows themselves. They
    /// deliberately disagree about `uncertain_unjoined` — the counter treats
    /// it as settled, the trigger does not — and **the trigger is
    /// authoritative**. That asymmetry is why a miscounted gate can only
    /// wedge shut, never close early.
    ///
    /// # Errors
    ///
    /// Propagates SQLite failures, including the close-requires-settled
    /// trigger refusal.
    pub(crate) fn settle_app_server_turn_gate_closed_v1(
        &self,
        fence: &TurnGateFenceV1,
        gate_generation: i64,
        closed_at: DateTime<Utc>,
    ) -> Result<GateClosedOutcomeV1> {
        let closed = closed_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let rows = self.conn.execute(
            "UPDATE agent_message_provider_turn_gates
                SET gate_state='closed', closed_at=?6, updated_at=?6
              WHERE message_id=?1 AND attempt_number=?2
                AND delivery_model_invocation_id=?3 AND provider_turn_id=?4
                AND gate_generation=?5 AND gate_state='closing'
                AND settled_permit_count=issued_permit_count",
            params![
                fence.message_id.to_string(),
                fence.attempt_number,
                fence.delivery_model_invocation_id.to_string(),
                fence.provider_turn_id.as_str(),
                gate_generation,
                closed,
            ],
        )?;
        if rows == 1 {
            return Ok(GateClosedOutcomeV1::Closed);
        }
        match self.read_turn_gate_state_row_v1(fence, gate_generation)? {
            None => Ok(GateClosedOutcomeV1::NoSuchGate),
            Some((state, _)) => match state.as_str() {
                "open" => Ok(GateClosedOutcomeV1::StillOpen),
                "closing" => Ok(GateClosedOutcomeV1::PermitsOutstanding),
                "closed" => Ok(GateClosedOutcomeV1::AlreadyClosed),
                other => Err(DaemonError::Store(format!(
                    "V81 exact-turn gate carries the unknown durable state {other:?}"
                ))),
            },
        }
    }

    /// The gate's durable `(gate_state, terminal_authority)`, for classifying
    /// a zero-row CAS. Diagnostic only — the decision was already made
    /// atomically by the `UPDATE` that returned zero rows.
    fn read_turn_gate_state_row_v1(
        &self,
        fence: &TurnGateFenceV1,
        gate_generation: i64,
    ) -> Result<Option<(String, Option<String>)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT gate_state, terminal_authority
                   FROM agent_message_provider_turn_gates
                  WHERE message_id=?1 AND attempt_number=?2
                    AND delivery_model_invocation_id=?3 AND provider_turn_id=?4
                    AND gate_generation=?5",
                params![
                    fence.message_id.to_string(),
                    fence.attempt_number,
                    fence.delivery_model_invocation_id.to_string(),
                    fence.provider_turn_id.as_str(),
                    gate_generation,
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    // =====================================================================
    // P2-07 U2 — started-effect permit issue and settlement (C-P2-17,
    // discharging H21-P2-P202-002).
    // =====================================================================

    /// Issue one started-effect permit and move its request to
    /// `handler_started`, atomically.
    ///
    /// # THE COUNTER BUMP IS THE ADMISSION FENCE. DO NOT SEPARATE IT.
    ///
    /// The `issued_permit_count + 1` below is **not bookkeeping**. It is the
    /// only thing in the entire system that refuses a new external effect
    /// against a gate whose terminal has already won.
    ///
    /// The permit table has **no `BEFORE INSERT` trigger** — its four V81
    /// triggers are `no_delete`, `identity_immutable`, `forward`, and
    /// `evidence_coherence`, all `BEFORE DELETE`/`BEFORE UPDATE`. Nothing in
    /// the schema stops a permit row being inserted against a `closing` gate.
    /// The refusal is produced HERE, by two independent encodings:
    ///
    /// 1. this method's own `AND gate_state='open'` predicate plus the
    ///    exactly-one-row check on statement (2), and
    /// 2. `agent_message_turn_gates_v81_admission_coherence`, which aborts
    ///    any `issued_permit_count` change while `OLD.gate_state!='open'` —
    ///    the backstop against a future writer that bumps the counter WITHOUT
    ///    the predicate.
    ///
    /// So a caller that inserts a permit without bumping the counter in this
    /// same transaction does not get a refusal from anywhere: the insert
    /// succeeds, the request moves to `handler_started`, and a new external
    /// effect starts after the terminal won — silently. **Exactly one `+1`
    /// per exactly one permit insert, in one transaction. Never batch this.**
    ///
    /// Statement order is forced, not chosen: the permit must exist before
    /// the request moves to `handler_started`, because
    /// `..._v81_permit_coherence` refuses that move otherwise.
    ///
    /// `provider_reply` permits are out of scope for P2-07 and are made
    /// structurally unreachable by [`EffectPermitKindV1`] carrying only the
    /// two handler kinds.
    ///
    /// # Errors
    ///
    /// Propagates SQLite failures. A V81 trigger or CHECK refusal means the
    /// caller's belief was wrong and must never be worked around.
    pub(crate) fn issue_effect_permit_v1(
        &self,
        issue: &EffectPermitIssueV1,
    ) -> Result<PermitIssueOutcomeV1> {
        let issued = issue
            .issued_at
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let tx = self.conn.unchecked_transaction()?;

        // (1) The permit row. Must precede the request's move to
        //     `handler_started` (`..._v81_permit_coherence`).
        let inserted = tx.execute(
            "INSERT INTO agent_message_provider_effect_permits (
                 permit_id, message_id, attempt_number, turn_start_request_id,
                 provider_turn_id, provider_request_id, provider_request_kind,
                 provider_request_digest, delivery_model_invocation_id,
                 turn_gate_generation, permit_kind, request_phase, permit_state,
                 executor_boot_id, external_join_id, issued_at, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'handler_started','issued',
                     ?12,?13,?14,?14)",
            params![
                issue.permit_id.to_string(),
                issue.fence.message_id.to_string(),
                issue.fence.attempt_number,
                issue.turn_start_request_id.as_str(),
                issue.fence.provider_turn_id.as_str(),
                issue.provider_request_id.as_str(),
                issue.provider_request_kind.as_str(),
                issue.provider_request_digest.as_str(),
                issue.fence.delivery_model_invocation_id.to_string(),
                issue.gate_generation,
                issue.permit_kind.as_str(),
                issue.executor_boot_id.to_string(),
                issue.external_join_id.to_string(),
                issued,
            ],
        );
        match inserted {
            Ok(_) => {}
            // An exact retransmit of a request that already holds its permit.
            // The UNIQUE key supplies the abort; this method supplies the
            // classification, and it must NEVER remint the join handles — the
            // original `external_join_id` is the only thing a restart can
            // rejoin the running effect on.
            Err(error) if is_unique_or_primary_key_violation(&error) => {
                drop(tx);
                return match self.read_issued_permit_identity_v1(issue)? {
                    Some(existing) => Ok(existing),
                    None => Err(DaemonError::Store(
                        "V81 effect permit insert hit a uniqueness refusal but no durable permit \
                         exists for that request identity; the durable state must not be guessed"
                            .to_string(),
                    )),
                };
            }
            Err(error) => return Err(error.into()),
        }

        // (2) THE ADMISSION FENCE. See this method's doc comment.
        let admitted = tx.execute(
            "UPDATE agent_message_provider_turn_gates
                SET issued_permit_count=issued_permit_count+1, updated_at=?6
              WHERE message_id=?1 AND attempt_number=?2
                AND delivery_model_invocation_id=?3 AND provider_turn_id=?4
                AND gate_generation=?5 AND gate_state='open'",
            params![
                issue.fence.message_id.to_string(),
                issue.fence.attempt_number,
                issue.fence.delivery_model_invocation_id.to_string(),
                issue.fence.provider_turn_id.as_str(),
                issue.gate_generation,
                issued,
            ],
        )?;
        if admitted != 1 {
            drop(tx);
            return Ok(PermitIssueOutcomeV1::GateNotOpen);
        }

        // (3) The request's own phase advance, now that its permit exists.
        let started = tx.execute(
            "UPDATE agent_message_provider_request_effects
                SET handler_phase='handler_started', handler_started_at=?8, updated_at=?8
              WHERE message_id=?1 AND attempt_number=?2 AND turn_start_request_id=?3
                AND provider_turn_id=?4 AND provider_request_id=?5
                AND provider_request_kind=?6 AND provider_request_digest=?7
                AND handler_phase='handler_authorized'",
            params![
                issue.fence.message_id.to_string(),
                issue.fence.attempt_number,
                issue.turn_start_request_id.as_str(),
                issue.fence.provider_turn_id.as_str(),
                issue.provider_request_id.as_str(),
                issue.provider_request_kind.as_str(),
                issue.provider_request_digest.as_str(),
                issued,
            ],
        )?;
        if started != 1 {
            drop(tx);
            return Ok(PermitIssueOutcomeV1::RequestNotAuthorized);
        }

        tx.commit()?;
        Ok(PermitIssueOutcomeV1::Issued)
    }

    /// The durable permit identity for one request, used to classify an exact
    /// retransmit. Never mints anything.
    fn read_issued_permit_identity_v1(
        &self,
        issue: &EffectPermitIssueV1,
    ) -> Result<Option<PermitIssueOutcomeV1>> {
        Ok(self
            .conn
            .query_row(
                "SELECT permit_id, external_join_id, executor_boot_id
                   FROM agent_message_provider_effect_permits
                  WHERE message_id=?1 AND attempt_number=?2 AND turn_start_request_id=?3
                    AND provider_turn_id=?4 AND provider_request_id=?5
                    AND provider_request_kind=?6 AND provider_request_digest=?7
                    AND permit_kind=?8",
                params![
                    issue.fence.message_id.to_string(),
                    issue.fence.attempt_number,
                    issue.turn_start_request_id.as_str(),
                    issue.fence.provider_turn_id.as_str(),
                    issue.provider_request_id.as_str(),
                    issue.provider_request_kind.as_str(),
                    issue.provider_request_digest.as_str(),
                    issue.permit_kind.as_str(),
                ],
                |row| {
                    Ok(PermitIssueOutcomeV1::AlreadyIssued {
                        permit_id: row.get::<_, String>(0)?,
                        external_join_id: row.get::<_, String>(1)?,
                        executor_boot_id: row.get::<_, String>(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Settle one started-effect permit out of `issued`, or upgrade an
    /// already-settled `uncertain_unjoined` permit with exact join evidence.
    ///
    /// # The `issued` guard on the counter bump is the trap.
    ///
    /// The V81 CHECK `(permit_state='issued')=(settled_at IS NULL)` means
    /// `uncertain_unjoined` is ALREADY a settled state — it stamps
    /// `settled_at` on the way in. So `settled_permit_count` must bump on the
    /// move **out of `issued`** and must **not** bump again when an
    /// `uncertain_unjoined` permit is later upgraded to `uncertain_joined`.
    /// Double-bumping inflates the counter past `issued_permit_count`, and
    /// `..._close_requires_settled_permits` then refuses the close forever:
    /// the gate wedges shut and the session is stranded. That is a liveness
    /// bug, not a safety bug — an inflated count can never close a gate
    /// early, because the trigger re-derives the truth from the permit rows.
    ///
    /// Settling to `uncertain_unjoined` is the **strand** path. It is
    /// correct, it is permanent, and nothing cleans it up.
    ///
    /// This method writes ONLY permit and gate rows. It never touches
    /// `agent_messages.state`, `attempt_state`, or `terminal_disposition`, so
    /// it adds no second `claimed → expired` writer (TX-C keeps that edge)
    /// and can never requeue anything — the narrowed rule is untouched.
    ///
    /// # Errors
    ///
    /// Propagates SQLite failures, including the V81 forward-only and
    /// fill-once evidence refusals.
    pub(crate) fn settle_effect_permit_v1(
        &self,
        permit_id: Uuid,
        settlement: &PermitSettlementV1,
        settled_at: DateTime<Utc>,
    ) -> Result<PermitSettleOutcomeV1> {
        let now = settled_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        let target = settlement.target_state();
        let tx = self.conn.unchecked_transaction()?;

        let durable: Option<(String, String, i64, String, String, i64)> = tx
            .query_row(
                "SELECT permit_state, message_id, attempt_number,
                        delivery_model_invocation_id, provider_turn_id, turn_gate_generation
                   FROM agent_message_provider_effect_permits
                  WHERE permit_id=?1",
                params![permit_id.to_string()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((prior, message_id, attempt_number, invocation_id, turn, generation)) = durable
        else {
            return Ok(PermitSettleOutcomeV1::NoSuchPermit);
        };
        if prior == target {
            return Ok(PermitSettleOutcomeV1::AlreadySettled);
        }

        // THE TRAP: bump only on the move OUT OF `issued`. `settled_at` is
        // fill-once (`..._v81_evidence_coherence`), so the upgrade path must
        // not restamp it either.
        let leaves_issued = prior == "issued";
        let settled_column = if leaves_issued { ", settled_at=?3" } else { "" };
        let rows = tx.execute(
            &format!(
                "UPDATE agent_message_provider_effect_permits
                    SET permit_state=?4, updated_at=?3{settled_column}{evidence}
                  WHERE permit_id=?1 AND permit_state=?2",
                evidence = settlement.evidence_assignments()
            ),
            rusqlite::params_from_iter(settlement.bind(permit_id, &prior, &now, target)),
        )?;
        if rows != 1 {
            drop(tx);
            return Ok(PermitSettleOutcomeV1::IllegalTransition {
                from: prior,
                to: target.to_string(),
            });
        }

        if leaves_issued {
            let bumped = tx.execute(
                "UPDATE agent_message_provider_turn_gates
                    SET settled_permit_count=settled_permit_count+1, updated_at=?6
                  WHERE message_id=?1 AND attempt_number=?2
                    AND delivery_model_invocation_id=?3 AND provider_turn_id=?4
                    AND gate_generation=?5",
                params![
                    message_id,
                    attempt_number,
                    invocation_id,
                    turn,
                    generation,
                    now,
                ],
            )?;
            exactly_one_row("V81 effect permit settlement gate counter", bumped)?;
        }

        tx.commit()?;
        Ok(PermitSettleOutcomeV1::Settled {
            counter_bumped: leaves_issued,
        })
    }

    // =====================================================================
    // P2-07 U3 — exact gate state and invocation-scoped release reads.
    // =====================================================================

    /// Read one exact V81 gate by its full primary key.
    ///
    /// This is a synchronous, read-only point lookup. Durable state text and
    /// counters are validated instead of being guessed, collapsed, or cast
    /// with wrapping/saturation semantics.
    pub(crate) fn app_server_turn_gate_state_v1(
        &self,
        fence: &TurnGateFenceV1,
        gate_generation: i64,
    ) -> Result<Option<TurnGateStateV1>> {
        validate_app_server_turn_gate_key_v1(fence, gate_generation)?;

        let durable: Option<TurnGateStateRowV1> = self
            .conn
            .query_row(
                APP_SERVER_TURN_GATE_STATE_SQL_V1,
                params![
                    fence.message_id.to_string(),
                    fence.attempt_number,
                    fence.delivery_model_invocation_id.to_string(),
                    fence.provider_turn_id.as_str(),
                    gate_generation,
                ],
                |row| {
                    Ok(TurnGateStateRowV1 {
                        message_id: row.get(0)?,
                        attempt_number: row.get(1)?,
                        delivery_model_invocation_id: row.get(2)?,
                        provider_turn_id: row.get(3)?,
                        gate_generation: row.get(4)?,
                        gate_state: row.get(5)?,
                        issued_permit_count: row.get(6)?,
                        settled_permit_count: row.get(7)?,
                    })
                },
            )
            .optional()?;

        durable
            .map(|row| decode_app_server_turn_gate_state_row_v1(fence, gate_generation, row))
            .transpose()
    }

    /// Decide whether one AppServer invocation has released all gate and
    /// started-effect custody.
    ///
    /// Both counts execute through one deferred read transaction. SQLite
    /// establishes the snapshot on the first count and retains it through the
    /// second, so a concurrent writer cannot make the verdict combine facts
    /// from different database states. The two statements are the only reads
    /// in the transaction and both are forced through the existing V81
    /// custody indexes.
    pub(crate) fn app_server_effect_release_verdict_v1(
        &self,
        delivery_model_invocation_id: Uuid,
    ) -> Result<EffectReleaseVerdictV1> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let invocation_id = delivery_model_invocation_id.to_string();
        let open_or_closing_gates: i64 = tx.query_row(
            APP_SERVER_OPEN_OR_CLOSING_GATE_COUNT_SQL_V1,
            params![invocation_id.as_str()],
            |row| row.get(0),
        )?;
        let unresolved_permits: i64 = tx.query_row(
            APP_SERVER_UNRESOLVED_PERMIT_COUNT_SQL_V1,
            params![invocation_id.as_str()],
            |row| row.get(0),
        )?;
        let open_or_closing_gates =
            nonnegative_count_v1(open_or_closing_gates, "open_or_closing_gates")?;
        let unresolved_permits = nonnegative_count_v1(unresolved_permits, "unresolved_permits")?;
        tx.commit()?;

        if open_or_closing_gates == 0 && unresolved_permits == 0 {
            Ok(EffectReleaseVerdictV1::Released)
        } else {
            Ok(EffectReleaseVerdictV1::BlockedClosing {
                open_or_closing_gates,
                unresolved_permits,
            })
        }
    }

    /// Read the durable continuation cursor `AgentContinueChild` fences on.
    ///
    /// All three components are read inside ONE deferred read transaction so
    /// the tuple cannot combine facts from different database states: SQLite
    /// establishes the snapshot on the first read and retains it across the
    /// rest. A cursor assembled from three independent reads could witness a
    /// pre-rotation tip against a post-rotation sequence and would then admit
    /// exactly the stale continuation this verb exists to refuse.
    ///
    /// The tip resolution, the sequence read, and the custody-generation read
    /// deliberately mirror `progress_row`, so a caller may take all three
    /// components straight from an `AgentGetProgress` snapshot and check
    /// them without a second round trip.
    ///
    /// `custody_generation` is `None` for an unsandboxed session and for one
    /// whose projection has not been published yet; both are legitimate states
    /// and are distinguished from a present generation by full equality in
    /// [`AgentContinuationCursorV1::satisfies`].
    pub(crate) fn agent_continuation_cursor(
        &self,
        session_id: Uuid,
    ) -> Result<AgentContinuationCursorV1> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let cursor = continuation_cursor_tx(&tx, session_id)?;
        tx.commit()?;
        Ok(cursor)
    }
}

/// The body of [`Store::agent_continuation_cursor`] for a caller that already
/// holds a transaction, so the cursor is read in that caller's snapshot.
fn continuation_cursor_tx(
    tx: &Transaction<'_>,
    session_id: Uuid,
) -> Result<AgentContinuationCursorV1> {
    let tip_session_id = resolve_lineage_tip(tx, session_id)?;
    let event_sequence: i64 = tx.query_row(
        "SELECT COALESCE(MAX(sequence),0) FROM conversation_events WHERE session_id=?1",
        [tip_session_id.to_string()],
        |row| row.get(0),
    )?;
    let custody_generation: Option<i64> = tx
        .query_row(
            "SELECT custody_generation FROM session_execution_projections WHERE session_id=?1",
            [tip_session_id.to_string()],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten();
    Ok(AgentContinuationCursorV1 {
        tip_session_id,
        event_sequence,
        custody_generation,
    })
}

/// Exact production point lookup for one complete V81 turn-gate primary key.
pub(crate) const APP_SERVER_TURN_GATE_STATE_SQL_V1: &str = "SELECT
            message_id, attempt_number, delivery_model_invocation_id,
            provider_turn_id, gate_generation, gate_state,
            issued_permit_count, settled_permit_count
       FROM agent_message_provider_turn_gates
      WHERE message_id=?1 AND attempt_number=?2
        AND delivery_model_invocation_id=?3 AND provider_turn_id=?4
        AND gate_generation=?5";

/// Exact production predicate for invocation-scoped non-closed gates.
///
/// V81 constrains the durable domain to `open`, `closing`, and `closed`, so
/// this `IN` is semantically identical to `gate_state!='closed'`. The spelling
/// is load-bearing: on the live V82 catalog SQLite reports `SEARCH` for this
/// form while the inequality degrades to an index `SCAN`.
pub(crate) const APP_SERVER_OPEN_OR_CLOSING_GATE_COUNT_SQL_V1: &str = "SELECT COUNT(*)
       FROM agent_message_provider_turn_gates
            INDEXED BY idx_agent_message_turn_gates_v81_closing
      WHERE gate_state IN ('open','closing')
        AND delivery_model_invocation_id=?1";

/// Exact production predicate for invocation-scoped unresolved permits.
pub(crate) const APP_SERVER_UNRESOLVED_PERMIT_COUNT_SQL_V1: &str = "SELECT COUNT(*)
       FROM agent_message_provider_effect_permits
            INDEXED BY idx_agent_message_effect_permits_v81_unresolved
      WHERE permit_state IN ('issued','uncertain_unjoined')
        AND delivery_model_invocation_id=?1";

fn nonnegative_count_v1(value: i64, durable_name: &'static str) -> Result<u64> {
    u64::try_from(value).map_err(|_| {
        DaemonError::Store(format!(
            "V81 {durable_name} carried invalid negative count {value}"
        ))
    })
}

fn validate_app_server_turn_gate_key_v1(
    fence: &TurnGateFenceV1,
    gate_generation: i64,
) -> Result<()> {
    if fence.message_id.is_nil() {
        return Err(DaemonError::Store(
            "V81 exact-turn gate request has a nil message_id".to_string(),
        ));
    }
    if fence.attempt_number == 0 {
        return Err(DaemonError::Store(
            "V81 exact-turn gate request attempt_number must be positive".to_string(),
        ));
    }
    if fence.delivery_model_invocation_id.is_nil() {
        return Err(DaemonError::Store(
            "V81 exact-turn gate request has a nil delivery_model_invocation_id".to_string(),
        ));
    }
    validate_provider_turn_id_v1(
        &fence.provider_turn_id,
        "exact-turn gate request provider_turn_id",
    )?;
    if gate_generation < 0 {
        return Err(DaemonError::Store(format!(
            "V81 exact-turn gate request gate_generation must be nonnegative, got {gate_generation}"
        )));
    }
    Ok(())
}

fn validate_provider_turn_id_v1(value: &str, durable_name: &'static str) -> Result<()> {
    let raw_bytes = value.len();
    if !(1..=AGENT_MESSAGE_MAX_PROVIDER_TURN_BYTES).contains(&raw_bytes) {
        return Err(DaemonError::Store(format!(
            "V81 {durable_name} has invalid raw UTF-8 byte length {raw_bytes}; expected 1..={AGENT_MESSAGE_MAX_PROVIDER_TURN_BYTES}"
        )));
    }
    Ok(())
}

fn parse_canonical_turn_gate_uuid_v1(durable_name: &'static str, value: &str) -> Result<Uuid> {
    let parsed = Uuid::parse_str(value).map_err(|error| {
        DaemonError::Store(format!(
            "V81 exact-turn gate {durable_name} is not a UUID: {error}"
        ))
    })?;
    if parsed.is_nil() || parsed.to_string() != value {
        return Err(DaemonError::Store(format!(
            "V81 exact-turn gate {durable_name} is not a lowercase canonical non-nil UUID"
        )));
    }
    Ok(parsed)
}

pub(crate) fn decode_app_server_turn_gate_state_row_v1(
    requested_fence: &TurnGateFenceV1,
    requested_generation: i64,
    durable: TurnGateStateRowV1,
) -> Result<TurnGateStateV1> {
    let message_id = parse_canonical_turn_gate_uuid_v1("message_id", &durable.message_id)?;
    if durable.attempt_number <= 0 {
        return Err(DaemonError::Store(format!(
            "V81 exact-turn gate durable attempt_number must be positive, got {}",
            durable.attempt_number
        )));
    }
    let attempt_number = u32::try_from(durable.attempt_number).map_err(|_| {
        DaemonError::Store(format!(
            "V81 exact-turn gate durable attempt_number {} exceeds the typed key domain",
            durable.attempt_number
        ))
    })?;
    let delivery_model_invocation_id = parse_canonical_turn_gate_uuid_v1(
        "delivery_model_invocation_id",
        &durable.delivery_model_invocation_id,
    )?;
    validate_provider_turn_id_v1(
        &durable.provider_turn_id,
        "exact-turn gate durable provider_turn_id",
    )?;
    if durable.gate_generation < 0 {
        return Err(DaemonError::Store(format!(
            "V81 exact-turn gate durable gate_generation must be nonnegative, got {}",
            durable.gate_generation
        )));
    }

    if message_id != requested_fence.message_id
        || attempt_number != requested_fence.attempt_number
        || delivery_model_invocation_id != requested_fence.delivery_model_invocation_id
        || durable.provider_turn_id != requested_fence.provider_turn_id
        || durable.gate_generation != requested_generation
    {
        return Err(DaemonError::Store(
            "V81 exact-turn gate durable key does not equal the requested key".to_string(),
        ));
    }

    Ok(TurnGateStateV1 {
        gate_state: TurnGateLifecycleStateV1::from_durable(&durable.gate_state)?,
        issued_permit_count: nonnegative_count_v1(
            durable.issued_permit_count,
            "issued_permit_count",
        )?,
        settled_permit_count: nonnegative_count_v1(
            durable.settled_permit_count,
            "settled_permit_count",
        )?,
    })
}

/// Is this SQLite failure a uniqueness refusal specifically?
///
/// Matched on the EXTENDED code so a `RAISE(ABORT,...)` trigger refusal —
/// which is also `ConstraintViolation`, but extended code
/// `SQLITE_CONSTRAINT_TRIGGER` — can never be mistaken for an idempotent
/// retransmit and silently swallowed.
fn is_unique_or_primary_key_violation(error: &rusqlite::Error) -> bool {
    error.sqlite_error().is_some_and(|sqlite_error| {
        matches!(
            sqlite_error.extended_code,
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE | rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
        )
    })
}

// =========================================================================
// P2-07 gate / permit DTOs (C-P2-17).
// =========================================================================

/// The exact-turn fence naming one provider turn gate.
///
/// These four columns plus `gate_generation` ARE the gate's primary key, so a
/// wrong or missing provider turn matches zero rows in every CAS without any
/// special-casing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnGateFenceV1 {
    pub(crate) message_id: Uuid,
    pub(crate) attempt_number: u32,
    pub(crate) delivery_model_invocation_id: Uuid,
    pub(crate) provider_turn_id: String,
}

/// Raw durable columns selected by the exact V81 turn-gate point lookup.
///
/// This remains distinct from [`TurnGateStateV1`] so every persisted key and
/// value is checked before a caller can observe a plausible typed DTO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnGateStateRowV1 {
    pub(crate) message_id: String,
    pub(crate) attempt_number: i64,
    pub(crate) delivery_model_invocation_id: String,
    pub(crate) provider_turn_id: String,
    pub(crate) gate_generation: i64,
    pub(crate) gate_state: String,
    pub(crate) issued_permit_count: i64,
    pub(crate) settled_permit_count: i64,
}

/// Outcome of opening one exact-turn gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateOpenOutcomeV1 {
    /// The gate was opened at this generation.
    Opened { gate_generation: i64 },
    /// No `correlated` + `effect_acknowledged` delivery attempt exists at this
    /// exact invocation AND provider turn, so no gate may exist for it.
    AttemptNotAdmissible,
}

/// Outcome of the `open → closing` terminal CAS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateClosingOutcomeV1 {
    /// This caller won the race and sealed terminal authority.
    Closing,
    /// A previous caller already won; `terminal_authority` is the winner's.
    AlreadyClosing { terminal_authority: String },
    /// The gate already reached `closed` under the named authority.
    AlreadyClosed { terminal_authority: String },
    /// No gate exists at this fence and generation.
    NoSuchGate,
}

/// Outcome of the `closing → closed` settlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GateClosedOutcomeV1 {
    Closed,
    /// Terminality has not been claimed yet; `closing` comes first.
    StillOpen,
    /// At least one issued permit is still unsettled.
    PermitsOutstanding,
    AlreadyClosed,
    NoSuchGate,
}

/// The exact durable lifecycle state of one V81 turn gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnGateLifecycleStateV1 {
    Open,
    Closing,
    Closed,
}

impl TurnGateLifecycleStateV1 {
    fn from_durable(value: &str) -> Result<Self> {
        match value {
            "open" => Ok(Self::Open),
            "closing" => Ok(Self::Closing),
            "closed" => Ok(Self::Closed),
            other => Err(DaemonError::Store(format!(
                "V81 exact-turn gate carries unknown durable state {other:?}"
            ))),
        }
    }
}

/// Lossless read-only facts for one exact V81 gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TurnGateStateV1 {
    pub(crate) gate_state: TurnGateLifecycleStateV1,
    pub(crate) issued_permit_count: u64,
    pub(crate) settled_permit_count: u64,
}

/// Invocation-scoped custody that decides whether terminal cleanup may
/// release the AppServer invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EffectReleaseVerdictV1 {
    Released,
    BlockedClosing {
        open_or_closing_gates: u64,
        unresolved_permits: u64,
    },
}

/// The two permit kinds P2-07 issues.
///
/// `provider_reply` is deliberately ABSENT: it binds to the writer-receipt
/// path Family B owns, and omitting the variant makes issuing one
/// structurally unreachable rather than merely forbidden by convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EffectPermitKindV1 {
    ToolExecution,
    ApprovalPresentation,
}

impl EffectPermitKindV1 {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ToolExecution => "tool_execution",
            Self::ApprovalPresentation => "approval_presentation",
        }
    }
}

/// One started-effect permit to issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EffectPermitIssueV1 {
    pub(crate) permit_id: Uuid,
    pub(crate) fence: TurnGateFenceV1,
    pub(crate) gate_generation: i64,
    pub(crate) turn_start_request_id: String,
    pub(crate) provider_request_id: String,
    pub(crate) provider_request_kind: String,
    pub(crate) provider_request_digest: String,
    pub(crate) permit_kind: EffectPermitKindV1,
    /// The boot that owns the running effect. Immutable once issued.
    pub(crate) executor_boot_id: Uuid,
    /// The ONLY handle a restart can rejoin the running effect on. Immutable
    /// once issued, and never reminted on retransmit.
    pub(crate) external_join_id: Uuid,
    pub(crate) issued_at: DateTime<Utc>,
}

/// Outcome of issuing one started-effect permit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PermitIssueOutcomeV1 {
    Issued,
    /// An exact retransmit: this request already holds its permit. The
    /// ORIGINAL join handles are returned so a restart rejoins the running
    /// effect instead of replaying it.
    AlreadyIssued {
        permit_id: String,
        external_join_id: String,
        executor_boot_id: String,
    },
    /// The gate is no longer `open`, so no new external effect may start —
    /// the terminal already won. This is the admission fence refusing.
    GateNotOpen,
    /// The request is not in `handler_authorized`, so no capability to start.
    RequestNotAuthorized,
}

/// How one started-effect permit settles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PermitSettlementV1 {
    Completed {
        completion_evidence_digest: String,
    },
    /// The **strand** path: the effect's outcome is unknown and no join
    /// handle resolved it. Permanent, correct, and never cleaned up.
    UncertainUnjoined {
        uncertainty_evidence_digest: String,
    },
    UncertainJoined {
        uncertainty_evidence_digest: String,
        join_evidence_digest: String,
    },
    CancelConfirmed {
        cancel_requested_at: DateTime<Utc>,
        cancel_confirmed_at: DateTime<Utc>,
        cancel_evidence_digest: String,
    },
}

impl PermitSettlementV1 {
    pub(crate) const fn target_state(&self) -> &'static str {
        match self {
            Self::Completed { .. } => "completed",
            Self::UncertainUnjoined { .. } => "uncertain_unjoined",
            Self::UncertainJoined { .. } => "uncertain_joined",
            Self::CancelConfirmed { .. } => "cancel_confirmed",
        }
    }

    /// The evidence columns this settlement fills, as `SET` fragments bound
    /// positionally from `?5` onward.
    const fn evidence_assignments(&self) -> &'static str {
        match self {
            Self::Completed { .. } => ", completion_evidence_digest=?5",
            Self::UncertainUnjoined { .. } => ", uncertainty_evidence_digest=?5",
            Self::UncertainJoined { .. } => {
                ", uncertainty_evidence_digest=?5, join_evidence_digest=?6"
            }
            Self::CancelConfirmed { .. } => {
                ", cancel_requested_at=?5, cancel_confirmed_at=?6, cancel_evidence_digest=?7"
            }
        }
    }

    /// Positional bindings: `?1` permit, `?2` expected prior state, `?3` now,
    /// `?4` target state, then this settlement's evidence.
    fn bind(&self, permit_id: Uuid, prior: &str, now: &str, target: &str) -> Vec<String> {
        let mut bound = vec![
            permit_id.to_string(),
            prior.to_string(),
            now.to_string(),
            target.to_string(),
        ];
        match self {
            Self::Completed {
                completion_evidence_digest,
            } => bound.push(completion_evidence_digest.clone()),
            Self::UncertainUnjoined {
                uncertainty_evidence_digest,
            } => bound.push(uncertainty_evidence_digest.clone()),
            Self::UncertainJoined {
                uncertainty_evidence_digest,
                join_evidence_digest,
            } => {
                bound.push(uncertainty_evidence_digest.clone());
                bound.push(join_evidence_digest.clone());
            }
            Self::CancelConfirmed {
                cancel_requested_at,
                cancel_confirmed_at,
                cancel_evidence_digest,
            } => {
                bound.push(cancel_requested_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true));
                bound.push(cancel_confirmed_at.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true));
                bound.push(cancel_evidence_digest.clone());
            }
        }
        bound
    }
}

/// Outcome of settling one started-effect permit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PermitSettleOutcomeV1 {
    /// `counter_bumped` is true exactly when this call moved the permit OUT
    /// of `issued`. An `uncertain_unjoined → uncertain_joined` upgrade
    /// settles nothing new and must report `false`.
    Settled {
        counter_bumped: bool,
    },
    /// The permit already holds the requested state.
    AlreadySettled,
    /// The durable prior state does not admit this settlement.
    IllegalTransition {
        from: String,
        to: String,
    },
    NoSuchPermit,
}

/// C-P2-18's frozen per-transaction target cap. Chosen so one settlement is a
/// bounded unit of work; overflow is an error, never a silent truncation.
pub(crate) const AGENT_MESSAGE_SPAWN_SETTLEMENT_MAX_TARGETS: usize = 128;

/// How one message addressed to a failed reservation was settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpawnSettlementDisposition {
    /// Never delivered: a null-attempt `spawn_settlement` edge to `failed`.
    QueuedFailed,
    /// The exact current attempt was already proved no effect, so it was
    /// sealed `proved_no_effect_failed` and the aggregate failed.
    ClaimedProvedNoEffectFailed,
    /// Effect could not be excluded. The aggregate advanced to `uncertain`
    /// and invocation/process custody was retained.
    EffectPossibleUncertain,
    /// `acknowledged` / `failed` / `expired`: immutable, untouched.
    AlreadyTerminalUntouched,
}

/// One settled row, reported for post-commit publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SpawnSettlementRowOutcome {
    pub(crate) message_id: Uuid,
    pub(crate) disposition: SpawnSettlementDisposition,
    /// The aggregate version this row carried *before* settlement.
    pub(crate) state_version: i64,
}

/// What one `settle_reserved_child_failure` call durably did.
#[derive(Debug, Clone)]
pub(crate) struct SettleReservedChildFailureOutcome {
    pub(crate) spawn_request_id: Uuid,
    pub(crate) owner_session_id: Uuid,
    pub(crate) child_session_id: Uuid,
    /// True when the reservation was already `failed`; the call was a
    /// read-only exact replay and settled nothing.
    pub(crate) already_failed: bool,
    pub(crate) rows: Vec<SpawnSettlementRowOutcome>,
}

/// Settle exactly one message row inside the caller's open transaction.
///
/// Every write verifies its affected-row count, so any stale version, missing
/// attempt, or refused trigger propagates as an error and rolls the entire
/// settlement — including the spawn failure — back.
#[allow(clippy::too_many_arguments)]
fn settle_one_spawn_settlement_row(
    tx: &Transaction<'_>,
    message_id: Uuid,
    state: &str,
    state_version: i64,
    current_attempt_number: Option<u32>,
    safe_error_class: &str,
    authority_id: Uuid,
    now_string: &str,
) -> Result<SpawnSettlementDisposition> {
    // Immutable terminal states are never rewritten.
    if matches!(state, "acknowledged" | "failed" | "expired") {
        return Ok(SpawnSettlementDisposition::AlreadyTerminalUntouched);
    }

    let next_state_version = state_version
        .checked_add(1)
        .ok_or_else(|| DaemonError::Store("agent message state version overflow".into()))?;

    // `queued`: no attempt exists, so no effect can exist.
    if state == "queued" {
        let evidence_digest = agent_message_digest(
            "spawn_settlement_queued_failed",
            &[message_id.as_bytes(), safe_error_class.as_bytes()],
        );
        let transition_rows = tx.execute(
            "INSERT INTO agent_message_state_transitions (
                 message_id, state_version, from_state, to_state, attempt_number,
                 authority_kind, authority_id, evidence_digest, created_at)
             VALUES (?1,?2,'queued','failed',NULL,'spawn_settlement',?3,?4,?5)",
            params![
                message_id.to_string(),
                next_state_version,
                authority_id.to_string(),
                evidence_digest,
                now_string,
            ],
        )?;
        exactly_one_row("spawn settlement queued transition", transition_rows)?;

        let aggregate_rows = tx.execute(
            "UPDATE agent_messages
                SET state='failed', state_version=?2, failed_at=?3, updated_at=?3,
                    safe_error_class=?4
              WHERE id=?1 AND state='queued' AND state_version=?5",
            params![
                message_id.to_string(),
                next_state_version,
                now_string,
                safe_error_class,
                state_version,
            ],
        )?;
        exactly_one_row("spawn settlement queued aggregate CAS", aggregate_rows)?;
        return Ok(SpawnSettlementDisposition::QueuedFailed);
    }

    // Every remaining live state (`claimed`, `injected`) is attempt-bound.
    let attempt_number = current_attempt_number.ok_or_else(|| {
        DaemonError::Store(format!(
            "agent message {message_id} is {state} without a current attempt pointer"
        ))
    })?;

    let (attempt_state, admission_classification, effect_classification): (
        String,
        Option<String>,
        Option<String>,
    ) = tx
        .query_row(
            "SELECT attempt_state, admission_classification, effect_classification
               FROM agent_message_delivery_attempts
              WHERE message_id=?1 AND attempt_number=?2",
            params![message_id.to_string(), i64::from(attempt_number)],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
        .ok_or_else(|| {
            DaemonError::Store(format!(
                "agent message {message_id} points at a missing attempt {attempt_number}"
            ))
        })?;

    let proved_no_effect = admission_classification.as_deref() == Some("rejected_before_effect")
        && effect_classification.as_deref() == Some("proved_no_effect");

    if proved_no_effect {
        // Seal the attempt terminal, then fail the aggregate. The frozen
        // terminal pointer/disposition join in `agent_messages_v81_cas_coherence`
        // requires the seal to already be committed in this transaction.
        if attempt_state != "terminal" {
            let settlement_digest = agent_message_digest(
                "spawn_settlement_attempt_seal",
                &[
                    message_id.as_bytes(),
                    &attempt_number.to_be_bytes(),
                    safe_error_class.as_bytes(),
                ],
            );
            let sealed = tx.execute(
                "UPDATE agent_message_delivery_attempts
                    SET attempt_state='terminal',
                        terminal_disposition='proved_no_effect_failed',
                        settlement_authority='spawn_settlement',
                        settlement_evidence_digest=?3,
                        settled_at=COALESCE(settled_at,?4),
                        terminal_at=COALESCE(terminal_at,?4),
                        updated_at=?4
                  WHERE message_id=?1 AND attempt_number=?2
                    AND attempt_state!='terminal'
                    AND admission_classification='rejected_before_effect'
                    AND effect_classification='proved_no_effect'",
                params![
                    message_id.to_string(),
                    i64::from(attempt_number),
                    settlement_digest,
                    now_string,
                ],
            )?;
            exactly_one_row("spawn settlement proved-no-effect attempt seal", sealed)?;
        }

        let evidence_digest = agent_message_digest(
            "spawn_settlement_proved_no_effect_failed",
            &[
                message_id.as_bytes(),
                &attempt_number.to_be_bytes(),
                safe_error_class.as_bytes(),
            ],
        );
        let transition_rows = tx.execute(
            "INSERT INTO agent_message_state_transitions (
                 message_id, state_version, from_state, to_state, attempt_number,
                 authority_kind, authority_id, evidence_digest, created_at)
             VALUES (?1,?2,?3,'failed',?4,'spawn_settlement',?5,?6,?7)",
            params![
                message_id.to_string(),
                next_state_version,
                state,
                i64::from(attempt_number),
                authority_id.to_string(),
                evidence_digest,
                now_string,
            ],
        )?;
        exactly_one_row(
            "spawn settlement proved-no-effect transition",
            transition_rows,
        )?;

        let aggregate_rows = tx.execute(
            "UPDATE agent_messages
                SET state='failed', state_version=?2, failed_at=?3, updated_at=?3,
                    safe_error_class=?4
              WHERE id=?1 AND state=?5 AND state_version=?6",
            params![
                message_id.to_string(),
                next_state_version,
                now_string,
                safe_error_class,
                state,
                state_version,
            ],
        )?;
        exactly_one_row(
            "spawn settlement proved-no-effect aggregate CAS",
            aggregate_rows,
        )?;
        return Ok(SpawnSettlementDisposition::ClaimedProvedNoEffectFailed);
    }

    // Effect cannot be excluded. Fill conservative effect-possible evidence
    // when the attempt never recorded admission, then advance to `uncertain`
    // keeping invocation/process custody. Never back to `queued`.
    if effect_classification.is_none() {
        let filled = tx.execute(
            "UPDATE agent_message_delivery_attempts
                SET admission_classification='admitted_effect_possible',
                    effect_classification='effect_possible',
                    attempt_state='effect_possible',
                    admission_recorded_at=COALESCE(admission_recorded_at,?3),
                    effect_possible_at=COALESCE(effect_possible_at,?3),
                    updated_at=?3
              WHERE message_id=?1 AND attempt_number=?2
                AND admission_classification IS NULL
                AND effect_classification IS NULL
                -- A `dispatching` attempt is the case this conservative fill
                -- exists for: the send may have completed and never been
                -- recorded, so `effect_possible` is the only safe answer. It
                -- must NOT be narrowed to a no-effect proof here — that is
                -- P2-06's blocked inference, not this path's.
                AND attempt_state IN ('claimed','dispatching')",
            params![
                message_id.to_string(),
                i64::from(attempt_number),
                now_string,
            ],
        )?;
        exactly_one_row("spawn settlement conservative effect-possible fill", filled)?;
    }

    let evidence_digest = agent_message_digest(
        "spawn_settlement_effect_possible_uncertain",
        &[
            message_id.as_bytes(),
            &attempt_number.to_be_bytes(),
            safe_error_class.as_bytes(),
        ],
    );
    let transition_rows = tx.execute(
        "INSERT INTO agent_message_state_transitions (
             message_id, state_version, from_state, to_state, attempt_number,
             authority_kind, authority_id, evidence_digest, created_at)
         VALUES (?1,?2,?3,'uncertain',?4,'spawn_settlement',?5,?6,?7)",
        params![
            message_id.to_string(),
            next_state_version,
            state,
            i64::from(attempt_number),
            authority_id.to_string(),
            evidence_digest,
            now_string,
        ],
    )?;
    exactly_one_row("spawn settlement uncertain transition", transition_rows)?;

    // `safe_error_class` is deliberately NOT set: `uncertain` is not a failure
    // classification, and the schema reserves that column's mandatory pairing
    // for `failed`.
    let aggregate_rows = tx.execute(
        "UPDATE agent_messages
            SET state='uncertain', state_version=?2, uncertain_at=?3, updated_at=?3
          WHERE id=?1 AND state=?4 AND state_version=?5",
        params![
            message_id.to_string(),
            next_state_version,
            now_string,
            state,
            state_version,
        ],
    )?;
    exactly_one_row("spawn settlement uncertain aggregate CAS", aggregate_rows)?;
    Ok(SpawnSettlementDisposition::EffectPossibleUncertain)
}

/// Outcome of the control worker's durable seal CAS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppServerSealCommitV1 {
    /// This call performed the one and only
    /// `correlation_pending → sealed_live_uncertain` transition.
    Committed,
    /// A previous call already sealed this exact attempt. The durable intent is
    /// present, so the latch is settled; nothing is counted twice.
    AlreadySealed,
    /// The attempt durably left `correlation_pending` by a STRONGER route (it
    /// correlated). The forward-only trigger refuses the seal, and it must not
    /// be retried.
    Superseded,
    /// No durable attempt row exists for this key.
    Unknown,
}

/// One durable live AppServer attempt replayed at startup reconciliation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveAppServerAttemptV1 {
    pub(crate) fence: MessageAttemptFenceV1,
    pub(crate) correlation: CorrelationStateV1,
    pub(crate) provider_turn_id: Option<String>,
}

/// The acknowledgeable projection of one live attempt.
struct AcknowledgeableAttempt {
    state: AgentMessageStateV1,
    state_version: i64,
    boundary_kind: String,
    boundary_value: Option<String>,
}

/// Read the exact attempt an acknowledgement may seal.
///
/// Admits `claimed` and `injected` only — `uncertain` is reachable exclusively
/// through the AppServer correlate-and-acknowledge path (C-P2-22).
fn read_acknowledgeable_attempt(
    tx: &Transaction<'_>,
    fence: &MessageAttemptFenceV1,
) -> Result<Option<AcknowledgeableAttempt>> {
    let row: Option<(String, i64, String, Option<String>)> = tx
        .query_row(
            "SELECT m.state, m.state_version, a.boundary_kind, a.boundary_value
               FROM agent_messages m
               JOIN agent_message_delivery_attempts a
                 ON a.message_id=m.id AND a.attempt_number=m.current_attempt_number
              WHERE m.id=?1
                AND m.state IN ('claimed','injected')
                AND a.attempt_number=?2
                AND a.claim_token=?3
                AND a.delivery_boot_id=?4
                AND a.delivery_session_id=?5
                AND a.delivery_session_generation=?6
                AND a.delivery_model_invocation_id=?7
                AND a.attempt_state!='terminal'",
            params![
                fence.message_id.to_string(),
                fence.attempt_number,
                fence.claim_token.to_string(),
                fence.delivery_boot_id.to_string(),
                fence.delivery_session_id.to_string(),
                fence.delivery_session_generation,
                fence.delivery_model_invocation_id.to_string(),
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((state, state_version, boundary_kind, boundary_value)) = row else {
        return Ok(None);
    };
    let state = AgentMessageStateV1::from_str_exact(&state)
        .ok_or_else(|| DaemonError::Store(format!("invalid agent message state: {state}")))?;
    Ok(Some(AcknowledgeableAttempt {
        state,
        state_version,
        boundary_kind,
        boundary_value,
    }))
}

/// Outcome of one acknowledgement transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AcknowledgeAgentMessageOutcome {
    Acknowledged {
        /// The REAL `conversation_events.id`, never a placeholder.
        event_id: i64,
        state_version: i64,
    },
    /// The fence no longer matches durable state; nothing was written.
    FenceLost,
}

/// The claimed-attempt projection every post-claim mutation fences against.
struct ClaimedAttemptFence {
    state_version: i64,
    expires_at: Option<DateTime<Utc>>,
}

/// Verify the full attempt fence against durable state: the aggregate must
/// still be `claimed` on THIS exact attempt, and the attempt must still carry
/// the caller's exact token, boot, delivery Session, generation, and invocation.
///
/// Returns `None` for any mismatch — a lost fence, which the caller must treat
/// as "someone else already settled this" rather than as an error.
fn read_claimed_attempt_fence(
    tx: &Transaction<'_>,
    fence: &MessageAttemptFenceV1,
) -> Result<Option<ClaimedAttemptFence>> {
    let row: Option<(i64, Option<String>)> = tx
        .query_row(
            "SELECT m.state_version, m.expires_at
               FROM agent_messages m
               JOIN agent_message_delivery_attempts a
                 ON a.message_id=m.id AND a.attempt_number=m.current_attempt_number
              WHERE m.id=?1
                AND m.state='claimed'
                AND a.attempt_number=?2
                AND a.claim_token=?3
                AND a.delivery_boot_id=?4
                AND a.delivery_session_id=?5
                AND a.delivery_session_generation=?6
                AND a.delivery_model_invocation_id=?7",
            params![
                fence.message_id.to_string(),
                fence.attempt_number,
                fence.claim_token.to_string(),
                fence.delivery_boot_id.to_string(),
                fence.delivery_session_id.to_string(),
                fence.delivery_session_generation,
                fence.delivery_model_invocation_id.to_string(),
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((state_version, expires_at)) = row else {
        return Ok(None);
    };
    let expires_at = expires_at
        .as_deref()
        .map(parse_timestamp)
        .transpose()
        .map_err(DaemonError::Store)?;
    Ok(Some(ClaimedAttemptFence {
        state_version,
        expires_at,
    }))
}

/// Which no-effect settlement a PROVED rejection takes (P2-04).
///
/// Only reachable from `rejected_before_effect`: lease expiry alone never
/// authorizes any of these, because none of them is proof that the provider was
/// not called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoEffectDisposition {
    /// Return the message to the queue for a later attempt.
    Requeue,
    /// The message's durable expiry has passed; verified in-transaction.
    Expired,
    /// Permanent failure; the aggregate carries the safe error class.
    Failed,
}

/// Outcome of one admission-recording transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecordAdmissionOutcome {
    Recorded {
        state: AgentMessageStateV1,
        state_version: i64,
    },
    /// The attempt fence no longer matches durable state, so nothing was
    /// written. Something else already settled this attempt.
    FenceLost,
}

/// Exactly-one-row assertion for every P2-04 mutation (C-P2-21).
///
/// Zero is the caller's typed CAS loss; more than one means the WHERE clause
/// failed to identify a single row, which is an integrity failure and must not
/// be allowed to commit.
fn exactly_one_row(what: &str, affected: usize) -> Result<()> {
    match affected {
        1 => Ok(()),
        0 => Err(DaemonError::Store(format!(
            "agent_message_cas_lost: {what} affected zero rows"
        ))),
        n => Err(DaemonError::Store(format!(
            "agent_message_integrity_failure: {what} affected {n} rows, expected exactly one"
        ))),
    }
}

/// C-P2-23's per-transaction row bound for the dispatcher's selection scan.
pub(crate) const AGENT_MESSAGE_DISPATCH_SCAN_MAX_ROWS: usize = 64;

/// C-P2-23's per-transaction time bound for the dispatcher's selection scan.
///
/// Tip resolution walks the `continued_from` lineage per row, so a page's cost
/// is not a function of its row count alone. The scan therefore stops on
/// whichever bound trips first and hands back a cursor.
pub(crate) const AGENT_MESSAGE_DISPATCH_SCAN_MAX_MILLIS: u64 = 10;

/// Keyset position in the global `(created_at,id)` FIFO over queued mail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AgentMessageDispatchCursor {
    pub created_at: DateTime<Utc>,
    pub message_id: Uuid,
}

/// Sender-authored message text, in the only shape the Store hands it out.
///
/// The inner `String` is private and there is no accessor that returns it for
/// a provider prompt. The single rendering method is
/// [`Self::render_for_delivery`], which is
/// [`rsi_common::daemon_message::wrap_agent_message`] — the neutralizing
/// wrapper that rewrites every `<` beginning a plausible envelope tag to the
/// inert `&lt;`, so the first closing tag after the header is provably the real
/// one.
///
/// This is a newtype rather than a plain `String` for one reason: the payload
/// is authored by another agent and is therefore attacker-shaped. Without the
/// wrapper, a sender can close its own envelope early and open a second one
/// attributed to a trusted source such as `terminal-watch`, promoting its text
/// from advisory peer mail the receiver need not obey into a daemon
/// notification the receiver is explicitly instructed to act on. Making the
/// wrapper the only way out of this type means a future dispatcher cannot route
/// a delivered payload around it by accident — doing so requires deliberately
/// editing this file, which is exactly the review surface such a change
/// deserves.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct DeliverablePayload(String);

impl DeliverablePayload {
    /// The one provider-ready rendering: attributed, and structurally incapable
    /// of carrying an envelope boundary.
    #[must_use]
    pub(crate) fn render_for_delivery(&self, message_id: Uuid, owner_session_id: Uuid) -> String {
        rsi_common::daemon_message::wrap_agent_message(message_id, owner_session_id, &self.0)
    }

    /// Raw text, for tests that must prove the stored payload really is
    /// attacker-shaped and was not sanitized on the way in.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn raw_for_test(&self) -> &str {
        &self.0
    }

    /// Build a payload directly, for tests that exercise a consumer of a
    /// selected row without standing up a Store. Test-only on purpose: the
    /// production path must obtain payloads from the selection scan, so that
    /// this type's private inner `String` stays the only door.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn new_for_test(text: impl Into<String>) -> Self {
        Self(text.into())
    }
}

/// Redacted: the plan forbids logging message payload bytes, and
/// `DispatchableAgentMessage` derives `Debug`, so an unredacted inner `String`
/// would put attacker-authored text into any diagnostic that prints a selected
/// row.
impl std::fmt::Debug for DeliverablePayload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "DeliverablePayload({} bytes, redacted)",
            self.0.len()
        )
    }
}

/// The delivery-Session facts a claim is fenced on, read in one snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeliverySessionFacts {
    pub status: SessionStatus,
    /// `sessions.rotation_depth`. Constant per row — see
    /// [`Store::list_dispatchable_agent_messages`] for why that is correct and
    /// what actually carries the rotated-away case.
    pub generation: i64,
    pub prior_model_invocation_id: Option<Uuid>,
    pub provider_kind: BoundaryProviderKindV1,
}

/// Why a selected row may or may not be claimed right now.
///
/// Mirrors the claim's own typed CAS-loss classes deliberately: the selection
/// pass pre-filters on exactly the properties the claim re-checks under the
/// write lock, so a `Ready` row that then loses its CAS is a real race rather
/// than a category the dispatcher forgot to consider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentMessageDispatchEligibility {
    /// Every fence the claim will check held at selection time.
    Ready,
    /// The durable expiry has passed; C-P2-09 gives this row to
    /// `expiry_reconciler`, never to the dispatcher.
    Expired,
    /// The resolved rotation tip has no `sessions` row.
    DeliverySessionMissing,
    /// The tip is terminal but an exact durable C5 marker or open capacity
    /// incident still owns recovery. Dispatch and terminal settlement both
    /// skip it until that owner resolves or admits a successor.
    RecoveryPending(SessionStatus),
    /// The tip exists but its status does not admit delivery. This is the guard
    /// that carries the rotated-away case.
    DeliverySessionNotLive(SessionStatus),
}

/// Stable safe error class for Issue #46 Phase A's terminal-before-delivery
/// aggregate settlement. This is durable operator-facing evidence, not a raw
/// provider or SQLite error.
pub(crate) const TARGET_SESSION_TERMINAL_BEFORE_DELIVERY_ERROR_CLASS: &str =
    "target_session_terminal_before_delivery";

/// One queued message joined to its resolved delivery tip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DispatchableAgentMessage {
    pub message_id: Uuid,
    pub owner_session_id: Uuid,
    /// The immutable logical root the sender addressed. Progress and queue caps
    /// stay rooted here; only delivery follows the tip.
    pub logical_root_session_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub state_version: i64,
    pub current_attempt_number: Option<u32>,
    /// Sender-authored text. Attacker-shaped by construction, so it is handed
    /// out only as a [`DeliverablePayload`], whose sole rendering is the
    /// neutralizing envelope wrapper.
    pub payload: DeliverablePayload,
    /// The resolved rotation tip, never the logical root.
    pub delivery_session_id: Uuid,
    /// `None` exactly when the tip has no `sessions` row.
    pub delivery: Option<DeliverySessionFacts>,
    pub eligibility: AgentMessageDispatchEligibility,
}

/// One bounded page of the dispatcher's selection scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DispatchableAgentMessagePage {
    pub messages: Vec<DispatchableAgentMessage>,
    /// `Some` while the queue may hold more rows past this page; `None` once
    /// the scan provably reached the end.
    pub next_cursor: Option<AgentMessageDispatchCursor>,
}

fn parse_store_uuid(what: &str, value: &str) -> Result<Uuid> {
    Uuid::parse_str(value)
        .map_err(|error| DaemonError::Store(format!("invalid {what} UUID: {error}")))
}

fn parse_store_timestamp(what: &str, value: &str) -> Result<DateTime<Utc>> {
    parse_timestamp(value).map_err(|error| DaemonError::Store(format!("invalid {what}: {error}")))
}

/// Read the exact delivery facts a claim will be fenced on.
///
/// Returns `None` for a missing row rather than erroring: a reserved target
/// whose child session was never admitted is an ordinary, expected state, and
/// the dispatcher classifies it rather than failing the whole scan.
fn read_delivery_session_facts(
    tx: &Transaction<'_>,
    delivery_session_id: Uuid,
) -> Result<Option<DeliverySessionFacts>> {
    let row: Option<(String, i64, Option<String>, Option<String>)> = tx
        .query_row(
            "SELECT status, rotation_depth, model_invocation_id, provider
               FROM sessions WHERE id=?1",
            params![delivery_session_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((status_text, generation, prior_invocation, provider_text)) = row else {
        return Ok(None);
    };
    // A NULL provider is a pre-provider-column legacy row; `str_to_session_provider`
    // already resolves unknown/removed providers to the Claude CLI default, and
    // matching that here keeps one provider-resolution rule in the daemon.
    let provider = match provider_text.as_deref() {
        Some(text) => str_to_session_provider(text)?,
        None => rsi_common::types::SessionProvider::default(),
    };
    Ok(Some(DeliverySessionFacts {
        status: str_to_session_status(&status_text)?,
        generation,
        // A malformed stored invocation id is treated as "no prior binding", the
        // same reading `claim_agent_message_exact_inner` uses, so selection and
        // claim cannot disagree about the expectation being fenced.
        prior_model_invocation_id: prior_invocation
            .as_deref()
            .and_then(|value| Uuid::parse_str(value).ok()),
        provider_kind: BoundaryProviderKindV1::from_session_provider(provider),
    }))
}

/// The Session statuses that admit a message delivery (C-P2-21's "live/
/// admissible"). A rotated-away, terminal, archived, or deleted tip is not a
/// delivery target: rotation replaces the row rather than mutating it, so a
/// stale tip is caught here even though its own `rotation_depth` never moves.
const fn session_status_admits_delivery(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingApproval
    )
}

/// The exhaustive complement of the three delivery-admitting states for the
/// current durable SessionStatus catalog. `Deleted` is terminal for message
/// custody even though the shared `SessionStatus::is_terminal` helper excludes
/// it for lifecycle callers.
const fn session_status_is_terminal_before_delivery(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Completed
            | SessionStatus::Failed
            | SessionStatus::Interrupted
            | SessionStatus::Archived
            | SessionStatus::Deleted
    )
}

/// Whether either accepted durable recovery owner still has custody of this
/// exact delivery tip in the caller's SQLite snapshot.
///
/// Marker existence is authoritative even when its payload is malformed: C5
/// owns disposition until the exact key is resolved. Capacity owns disposition
/// only while an open incident's latest capacity invocation belongs to this
/// exact Session. Controller ancestry is deliberately absent from this query.
fn terminal_recovery_pending_tx(tx: &Transaction<'_>, delivery_session_id: Uuid) -> Result<bool> {
    let pending_key = super::daemon_settings::c5_autofile_pending_key(delivery_session_id);
    let c5_pending: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM daemon_settings WHERE key=?1)",
        [pending_key],
        |row| row.get(0),
    )?;
    if c5_pending {
        return Ok(true);
    }

    tx.query_row(
        "SELECT EXISTS(
             SELECT 1
               FROM master_no_idle_capacity_incidents AS incident
               JOIN model_invocations AS invocation
                 ON invocation.id=incident.last_capacity_model_invocation_id
              WHERE incident.state='open'
                AND invocation.session_id=?1
         )",
        [delivery_session_id.to_string()],
        |row| row.get(0),
    )
    .map_err(DaemonError::Database)
}

/// C-P2-19's single Store watch helper, in-transaction.
///
/// Natural key `(owner_session_id, delivery_session_id)`. An existing ENABLED
/// row deduplicates; consumed/disabled history deliberately does not satisfy
/// the arm, because a disabled row will never fire again. The row shape matches
/// `build_agent_scheduled_job`'s `on_terminal` output exactly — a recurring
/// 60-second row — so `watch_service` cannot tell these rows apart from
/// interactively armed ones.
pub(super) fn arm_agent_message_watch_in_tx(
    tx: &Transaction<'_>,
    owner_session_id: Uuid,
    delivery_session_id: Uuid,
    now: DateTime<Utc>,
) -> Result<bool> {
    let wake_mode = format!("on_terminal:{delivery_session_id}");
    let already_enabled: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM scheduled_jobs
                        WHERE enabled=1 AND wake_session_id=?1 AND wake_mode=?2)",
        params![owner_session_id.to_string(), wake_mode],
        |row| row.get(0),
    )?;
    if already_enabled {
        return Ok(false);
    }

    let schedule_json = serde_json::to_string(&ScheduleSpec {
        recurrence: Recurrence::EverySeconds(60),
        anchor: now,
    })
    .map_err(|error| DaemonError::Store(error.to_string()))?;
    let now_string = now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let job_id = Uuid::new_v4();
    let rows = tx.execute(
        "INSERT INTO scheduled_jobs (
             id, name, message, schedule_json, last_fired_at, next_fire_at,
             enabled, working_dir, provider, model, project_id,
             created_at, updated_at, wake_mode, wake_session_id)
         SELECT ?1, ?2, ?3, ?4, NULL, ?5, 1, s.working_dir, s.provider, s.model,
                s.project_id, ?5, ?5, ?6, ?7
           FROM sessions s WHERE s.id=?7",
        params![
            job_id.to_string(),
            format!("agent-message-{delivery_session_id}"),
            format!("agent message delivery terminal watch {delivery_session_id}"),
            schedule_json,
            now_string,
            wake_mode,
            owner_session_id.to_string(),
        ],
    )?;
    exactly_one_row("agent message terminal watch arm", rows)?;
    super::source_worktree_v120::refresh_scheduled_job_path_projection(tx, &job_id.to_string())?;
    Ok(true)
}

/// Closed CAS-loss classes for [`Store::claim_agent_message_exact`].
///
/// Each names the exact fence component that moved, so the dispatcher can log
/// and settle precisely rather than retrying blind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentMessageClaimCasLoss {
    MessageMissing,
    MessageNotQueued,
    StateVersionMismatch,
    CurrentAttemptMismatch,
    DeliverySessionMissing,
    DeliverySessionNotLive,
    SessionGenerationMismatch,
    SessionInvocationMismatch,
    ModelInvocationMissing,
}

impl AgentMessageClaimCasLoss {
    #[must_use]
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::MessageMissing => "agent_message_claim_message_missing",
            Self::MessageNotQueued => "agent_message_claim_message_not_queued",
            Self::StateVersionMismatch => "agent_message_claim_state_version_mismatch",
            Self::CurrentAttemptMismatch => "agent_message_claim_current_attempt_mismatch",
            Self::DeliverySessionMissing => "agent_message_claim_delivery_session_missing",
            Self::DeliverySessionNotLive => "agent_message_claim_delivery_session_not_live",
            Self::SessionGenerationMismatch => "agent_message_claim_session_generation_mismatch",
            Self::SessionInvocationMismatch => "agent_message_claim_session_invocation_mismatch",
            Self::ModelInvocationMissing => "agent_message_claim_model_invocation_missing",
        }
    }
}

/// Outcome of one claim attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClaimAgentMessageOutcome {
    /// The claim committed. The fence is the ONLY authority for dispatch and
    /// for every later mutation of this attempt.
    Claimed(MessageAttemptFenceV1),
    /// Nothing was written. No attempt, no transition, no Session binding, no
    /// watch, and — because dispatch follows the commit — no external effect.
    CasLost(AgentMessageClaimCasLoss),
}

/// Exact fence inputs for one claim (C-P2-21).
///
/// `capability_kind` and `boundary_kind` are deliberately absent: they are
/// derived from `provider_kind` through the shared closed mapping, so a caller
/// cannot construct an incoherent provider/capability/boundary triple.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaimAgentMessageRequest {
    pub message_id: Uuid,
    /// The owner whose terminal watch is armed. Message progress stays rooted
    /// on the logical target; this is the WAKE target.
    pub owner_session_id: Uuid,
    /// The immutable logical root the owner addressed.
    pub logical_root_session_id: Uuid,
    pub expected_state_version: i64,
    /// `k`: the aggregate's current attempt number, `None` before any attempt.
    pub expected_current_attempt_number: Option<u32>,
    /// The EXACT live rotation tip resolved by the dispatcher, never the root.
    pub delivery_session_id: Uuid,
    /// The delivery Session's `rotation_depth` — its lineage generation. See
    /// [`session_status_admits_delivery`] for why status carries the
    /// rotated-away case and this carries wrong-tip detection.
    pub expected_session_generation: i64,
    pub expected_prior_model_invocation_id: Option<Uuid>,
    /// The already-admitted durable invocation this attempt will consume.
    pub delivery_model_invocation_id: Uuid,
    pub delivery_boot_id: Uuid,
    pub claim_token: Uuid,
    pub provider_kind: BoundaryProviderKindV1,
    pub claim_expires_at: DateTime<Utc>,
    /// The dispatcher capability that authored this claim.
    pub authority_id: Uuid,
}

/// Derive the stable message UUID from the canonical idempotency digest.
///
/// Deterministic by construction, so an exact replay recomputes the same ID
/// before any Store read and two callers can never collide on one key.
#[must_use]
pub(crate) fn agent_message_id_from_idempotency_digest(digest: &str) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(b"agent-message-v1:message-id:");
    hasher.update(digest.as_bytes());
    let bytes: [u8; 32] = hasher.finalize().into();
    let mut truncated = [0u8; 16];
    truncated.copy_from_slice(&bytes[..16]);
    uuid::Builder::from_bytes(truncated)
        .with_version(uuid::Version::Sha1)
        .with_variant(uuid::Variant::RFC4122)
        .into_uuid()
}

/// Outcome of one acceptance attempt. Both arms carry the SAME deterministic
/// receipt, so a replaying caller cannot distinguish itself into a second send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AcceptAgentMessageOutcome {
    Accepted(AgentSendMessageResultV1),
    Replayed(AgentSendMessageResultV1),
}

impl AcceptAgentMessageOutcome {
    #[must_use]
    pub(crate) const fn receipt(&self) -> &AgentSendMessageResultV1 {
        match self {
            Self::Accepted(receipt) | Self::Replayed(receipt) => receipt,
        }
    }

    #[must_use]
    pub(crate) const fn deduplicated(&self) -> bool {
        matches!(self, Self::Replayed(_))
    }
}

/// The acceptance-relevant projection of one aggregate row. Deliberately does
/// NOT load the payload: acceptance and progress never scan message bodies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentMessageAcceptanceRecord {
    pub message_id: Uuid,
    pub target_session_id: Uuid,
    pub target_spawn_request_id: Option<Uuid>,
    pub request_fingerprint: String,
    pub state: AgentMessageStateV1,
    pub state_version: i64,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub deduplicated: bool,
}

impl AgentMessageAcceptanceRecord {
    fn into_receipt(self) -> AgentSendMessageResultV1 {
        AgentSendMessageResultV1 {
            message_id: self.message_id,
            target_session_id: self.target_session_id,
            target_spawn_request_id: self.target_spawn_request_id,
            state: self.state,
            state_version: self.state_version,
            deduplicated: self.deduplicated,
            created_at: self.created_at,
            expires_at: self.expires_at,
        }
    }
}

fn get_agent_message_by_owner_digest(
    tx: &Transaction<'_>,
    owner_session_id: Uuid,
    idempotency_digest: &str,
) -> Result<Option<AgentMessageAcceptanceRecord>> {
    tx.query_row(
        "SELECT id, target_session_id, target_spawn_request_id, request_fingerprint,
                state, state_version, created_at, expires_at
         FROM agent_messages
         WHERE owner_session_id=?1 AND idempotency_digest=?2",
        params![owner_session_id.to_string(), idempotency_digest],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        },
    )
    .optional()?
    .map(
        |(id, target, reservation, fingerprint, state, version, created, expires)| {
            let message_id = Uuid::parse_str(&id)
                .map_err(|_| DaemonError::Store(format!("malformed agent message id {id}")))?;
            let target_session_id = Uuid::parse_str(&target).map_err(|_| {
                DaemonError::Store(format!("malformed agent message target {target}"))
            })?;
            let target_spawn_request_id = reservation
                .map(|value| {
                    Uuid::parse_str(&value).map_err(|_| {
                        DaemonError::Store(format!("malformed agent message reservation {value}"))
                    })
                })
                .transpose()?;
            let state = AgentMessageStateV1::from_str_exact(&state).ok_or_else(|| {
                DaemonError::Store(format!("unknown agent message state {state}"))
            })?;
            Ok(AgentMessageAcceptanceRecord {
                message_id,
                target_session_id,
                target_spawn_request_id,
                request_fingerprint: fingerprint,
                state,
                state_version: version,
                created_at: parse_timestamp(&created).map_err(|error| {
                    DaemonError::Store(format!("malformed agent message created_at: {error}"))
                })?,
                expires_at: expires
                    .as_deref()
                    .map(parse_timestamp)
                    .transpose()
                    .map_err(|error| {
                        DaemonError::Store(format!("malformed agent message expires_at: {error}"))
                    })?,
                // Set by the caller: a fresh insert is not a deduplication.
                deduplicated: true,
            })
        },
    )
    .transpose()
}

/// Count PENDING (non-terminal) aggregate rows for one owner or logical root.
///
/// `column` is a caller-controlled literal, never caller input.
fn count_pending_agent_messages(
    tx: &Transaction<'_>,
    column: &'static str,
    id: Uuid,
) -> Result<u32> {
    let sql = format!(
        "SELECT COUNT(*) FROM agent_messages
         WHERE {column}=?1 AND state IN ('queued','claimed','injected','uncertain')"
    );
    let count: i64 = tx.query_row(&sql, [id.to_string()], |row| row.get(0))?;
    u32::try_from(count)
        .map_err(|_| DaemonError::Store("pending agent message count overflowed u32".into()))
}

fn authorized_child_ids(tx: &Transaction<'_>, caller: Uuid) -> Result<BTreeSet<Uuid>> {
    if tx
        .query_row(
            "SELECT 1 FROM sessions WHERE id=?1",
            [caller.to_string()],
            |_| Ok(()),
        )
        .optional()?
        .is_none()
    {
        return Err(DaemonError::SessionNotFound(caller));
    }
    let mut statement = tx.prepare(
        "SELECT id FROM sessions
         WHERE id!=?1 AND (
           parent_id=?1 OR parent_id IN (
             SELECT id FROM sessions WHERE session_kind='Epic' AND lead_session_id=?1
           )
         )
         UNION
         SELECT child_session_id FROM agent_spawn_requests WHERE owner_session_id=?1
         ORDER BY 1",
    )?;
    let raw = statement
        .query_map([caller.to_string()], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raw.into_iter()
        .map(|value| {
            Uuid::parse_str(&value)
                .map_err(|error| DaemonError::Store(format!("invalid cohort UUID: {error}")))
        })
        .collect()
}

fn progress_row(
    tx: &Transaction<'_>,
    caller: Uuid,
    session_id: Uuid,
    observed_at: DateTime<Utc>,
) -> Result<AgentProgressRowV1> {
    let spawn = tx
        .query_row(
            &format!("{SPAWN_REQUEST_SELECT} WHERE owner_session_id=?1 AND child_session_id=?2"),
            params![caller.to_string(), session_id.to_string()],
            map_spawn_request_row,
        )
        .optional()?;
    let base_commit = tx
        .query_row(
            "SELECT custody.source_commit
             FROM sessions logical_child
             LEFT JOIN sandbox_custody_roots custody
               ON custody.custody_id=logical_child.sandbox_custody_id
             WHERE logical_child.id=?1",
            [session_id.to_string()],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let lineage_tip_id = resolve_lineage_tip(tx, session_id)?;
    let session_row: Option<(String, String)> = tx
        .query_row(
            "SELECT status,updated_at FROM sessions WHERE id=?1",
            [lineage_tip_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (status, status_updated_at, obligation) =
        if let Some((raw_status, updated_at)) = session_row {
            let session_status = str_to_session_status(&raw_status)?;
            let obligation = match session_status {
                SessionStatus::Failed => Some(AgentProgressObligationV1::HarvestFailed),
                SessionStatus::Interrupted => Some(AgentProgressObligationV1::HarvestInterrupted),
                _ => None,
            };
            (
                progress_status(session_status),
                parse_timestamp(&updated_at).map_err(DaemonError::Store)?,
                obligation,
            )
        } else {
            let reservation = spawn.as_ref().ok_or_else(|| {
                DaemonError::Store(format!(
                    "progress cohort row {session_id} has no durable source"
                ))
            })?;
            let status = if reservation.state == AgentSpawnStateV1::Failed {
                AgentProgressStatusV1::Failed
            } else {
                AgentProgressStatusV1::Reserved
            };
            (status, reservation.updated_at, None)
        };
    let (event_sequence, last_event_at_raw): (i64, Option<String>) = tx.query_row(
        "SELECT COALESCE(MAX(sequence),0),MAX(created_at) FROM conversation_events WHERE session_id=?1",
        [lineage_tip_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    // The custody generation is read against the SAME resolved tip, inside the
    // SAME snapshot transaction, as `agent_continuation_cursor`. That is what
    // makes the published cursor directly usable through
    // `AgentContinueChild` — a generation combined from a different database
    // state could witness a pre-reallocation custody against a post-rotation
    // tip. `None` is legitimate: the tip has no sandbox, or its projection
    // publishes no generation yet.
    let custody_generation: Option<i64> = tx
        .query_row(
            "SELECT custody_generation FROM session_execution_projections WHERE session_id=?1",
            [lineage_tip_id.to_string()],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten();
    let last_event_at = last_event_at_raw
        .as_deref()
        .map(parse_timestamp)
        .transpose()
        .map_err(DaemonError::Store)?;
    let cursor_updated_at = last_event_at
        .filter(|event_at| *event_at > status_updated_at)
        .unwrap_or(status_updated_at);
    let staleness_ms = u64::try_from(
        observed_at
            .signed_duration_since(cursor_updated_at)
            .num_milliseconds()
            .max(0),
    )
    .unwrap_or(0);
    let wake_mode = format!("on_terminal:{session_id}");
    let watch_counts: (i64, i64) = tx.query_row(
        "SELECT COUNT(*),COALESCE(MAX(enabled),0) FROM scheduled_jobs WHERE wake_session_id=?1 AND wake_mode=?2",
        params![caller.to_string(), wake_mode],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let watch_state = match watch_counts {
        (0, _) => AgentWatchStateV1::Missing,
        (_, 0) => AgentWatchStateV1::Disabled,
        _ => AgentWatchStateV1::Enabled,
    };
    let mut messages = AgentMessageCountsV1::default();
    let mut message_stmt = tx.prepare(
        "SELECT state,COUNT(*) FROM agent_messages WHERE target_session_id=?1 GROUP BY state",
    )?;
    // P2-03: message counts root on the IMMUTABLE logical target, never the
    // delivery tip. Acceptance stores `target_session_id` exactly as the owner
    // named it, so counting against `lineage_tip_id` made an owner's whole
    // mailbox read as empty the moment its child rotated. `session_id` is the
    // cohort member the caller is authorized on, which is that logical root.
    for entry in message_stmt.query_map([session_id.to_string()], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?))
    })? {
        let (state, count) = entry?;
        match state.as_str() {
            "queued" => messages.queued = count,
            "claimed" => messages.claimed = count,
            "injected" => messages.injected = count,
            "acknowledged" => messages.acknowledged = count,
            "uncertain" => messages.uncertain = count,
            "failed" => messages.failed = count,
            "expired" => messages.expired = count,
            _ => {
                return Err(DaemonError::Store(format!(
                    "invalid agent message state: {state}"
                )));
            }
        }
    }
    Ok(AgentProgressRowV1 {
        spawn_request_id: spawn.as_ref().map(|request| request.spawn_request_id),
        spawn_state: spawn.as_ref().map(|request| request.state),
        spawn_safe_error_class: spawn
            .as_ref()
            .and_then(|request| request.safe_error_class.clone()),
        base_commit,
        cursor: AgentProgressCursorV1 {
            session_id,
            lineage_tip_id,
            event_sequence,
            custody_generation,
            last_event_at,
            status,
            status_updated_at,
        },
        freshness: AgentProgressFreshnessV1 {
            cursor_updated_at,
            staleness_ms,
        },
        watch_state,
        messages,
        message_queue: message_queue_summary(tx, session_id, observed_at)?,
        obligation,
    })
}

/// Bounded mailbox detail for one logical target (P2-03).
///
/// Three narrow reads, none of which touches `payload`: the oldest pending
/// age, the most recently updated aggregate's state/version/attempt fields,
/// and that aggregate's current-attempt acknowledgement cursor. Returns `None`
/// when the target has no messages, so a Phase 1 snapshot is unchanged.
fn message_queue_summary(
    tx: &Transaction<'_>,
    target_session_id: Uuid,
    observed_at: DateTime<Utc>,
) -> Result<Option<AgentMessageQueueSummaryV1>> {
    // "Pending" is exactly the non-terminal set the acceptance caps count, so
    // queue age and queue-full pressure can never disagree about membership.
    let oldest_pending_raw: Option<String> = tx.query_row(
        "SELECT MIN(created_at) FROM agent_messages
         WHERE target_session_id=?1 AND state IN ('queued','claimed','injected','uncertain')",
        [target_session_id.to_string()],
        |row| row.get(0),
    )?;
    let oldest_pending_age_ms = oldest_pending_raw
        .as_deref()
        .map(parse_timestamp)
        .transpose()
        .map_err(DaemonError::Store)?
        .map(|created_at| {
            u64::try_from(
                observed_at
                    .signed_duration_since(created_at)
                    .num_milliseconds()
                    .max(0),
            )
            .unwrap_or(u64::MAX)
        });

    // The latest aggregate by durable update order. `id` breaks ties so the
    // choice is deterministic for two rows updated in the same nanosecond.
    let latest: Option<(String, String, i64, i64, Option<i64>, Option<String>)> = tx
        .query_row(
            "SELECT id,state,state_version,attempt_count,current_attempt_number,safe_error_class
             FROM agent_messages WHERE target_session_id=?1
             ORDER BY updated_at DESC,id DESC LIMIT 1",
            [target_session_id.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((
        message_id,
        raw_state,
        latest_state_version,
        attempt_count,
        current_attempt_number,
        latest_safe_error_class,
    )) = latest
    else {
        return Ok(None);
    };
    let latest_state = AgentMessageStateV1::from_str_exact(&raw_state)
        .ok_or_else(|| DaemonError::Store(format!("invalid agent message state: {raw_state}")))?;
    let latest_attempt_count = u32::try_from(attempt_count)
        .map_err(|_| DaemonError::Store("agent message attempt_count overflowed u32".into()))?;
    let latest_current_attempt_number = current_attempt_number
        .map(u32::try_from)
        .transpose()
        .map_err(|_| {
            DaemonError::Store("agent message current_attempt_number overflowed u32".into())
        })?;

    // The acknowledgement cursor is durable evidence on the CURRENT attempt.
    // Absent until an acknowledgement transaction fills the triple (P2-04), so
    // an unacknowledged message legitimately reports `None` rather than zero.
    let acknowledgement_cursor = match latest_current_attempt_number {
        None => None,
        Some(attempt_number) => tx
            .query_row(
                "SELECT acknowledged_event_id,acknowledged_event_session_id,
                        acknowledged_event_sequence
                 FROM agent_message_delivery_attempts
                 WHERE message_id=?1 AND attempt_number=?2",
                params![message_id, i64::from(attempt_number)],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .optional()?
            .and_then(|(event_id, event_session_id, event_sequence)| {
                // The triple is filled atomically; a partial triple is not a
                // cursor and must not be reported as one.
                match (event_id, event_session_id, event_sequence) {
                    (Some(event_id), Some(session), Some(event_sequence)) => {
                        Uuid::parse_str(&session).ok().map(|event_session_id| {
                            AgentMessageAckCursorV1 {
                                event_id,
                                event_session_id,
                                event_sequence,
                            }
                        })
                    }
                    _ => None,
                }
            }),
    };

    Ok(Some(AgentMessageQueueSummaryV1 {
        oldest_pending_age_ms,
        latest_state,
        latest_state_version,
        latest_attempt_count,
        latest_current_attempt_number,
        latest_safe_error_class,
        acknowledgement_cursor,
    }))
}

fn validate_agent_message_live_target(
    tx: &Transaction<'_>,
    owner_session_id: Uuid,
    target_session_id: Uuid,
    manager_authorized: bool,
) -> Result<()> {
    let root_exists = tx
        .query_row(
            "SELECT 1 FROM sessions WHERE id=?1",
            [target_session_id.to_string()],
            |_| Ok(()),
        )
        .optional()?
        .is_some();

    // A reserved child without a Session row is still a valid pre-launch
    // mailbox target. Its initial authorization was checked against the
    // caller-owned reservation; make sure that custody has not since failed.
    if !root_exists {
        let reserved: bool = tx.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM agent_spawn_requests
                  WHERE child_session_id=?1 AND owner_session_id=?2
                    AND state!='failed'
             )",
            params![target_session_id.to_string(), owner_session_id.to_string()],
            |row| row.get(0),
        )?;
        if reserved {
            return Ok(());
        }
        return Err(crate::error::agent_message_error(
            AgentMessageErrorCodeV1::TargetUnknown,
            Some(target_session_id.to_string()),
            None,
            None,
            None,
        ));
    }

    let delivery_tip = resolve_lineage_tip(tx, target_session_id)?;
    let target: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT status, parent_id FROM sessions WHERE id=?1",
            [delivery_tip.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((status_text, parent_id)) = target else {
        return Err(crate::error::agent_message_error(
            AgentMessageErrorCodeV1::TargetUnknown,
            Some(delivery_tip.to_string()),
            None,
            None,
            None,
        ));
    };

    let authorized = parent_id.as_deref() == Some(owner_session_id.to_string().as_str())
        || tx.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sessions AS parent
                  WHERE parent.id=?1 AND parent.session_kind='Epic'
                    AND parent.lead_session_id=?2
             )",
            params![parent_id, owner_session_id.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
    if !authorized && !manager_authorized {
        return Err(crate::error::agent_message_error(
            AgentMessageErrorCodeV1::TargetNotAuthorized,
            Some(format!("{owner_session_id}->{delivery_tip}")),
            None,
            None,
            None,
        ));
    }

    let status = str_to_session_status(&status_text)?;
    if !matches!(
        status,
        SessionStatus::Starting | SessionStatus::Running | SessionStatus::WaitingApproval
    ) {
        return Err(crate::error::agent_message_error(
            AgentMessageErrorCodeV1::TargetTerminal,
            Some(format!("{delivery_tip}:{status_text}")),
            None,
            None,
            None,
        ));
    }
    Ok(())
}

fn resolve_lineage_tip(tx: &Transaction<'_>, origin: Uuid) -> Result<Uuid> {
    let chain = lineage_chain(tx, origin)?;
    Ok(chain.last().copied().unwrap_or(origin))
}

/// The ordered lineage `origin, successor, ..., tip` that
/// [`resolve_lineage_tip`] walks. An unknown origin yields `[origin]`.
fn lineage_chain(tx: &Transaction<'_>, origin: Uuid) -> Result<Vec<Uuid>> {
    let exists = tx
        .query_row(
            "SELECT 1 FROM sessions WHERE id=?1",
            [origin.to_string()],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !exists {
        return Ok(vec![origin]);
    }
    let mut chain = vec![origin];
    let mut tip = origin;
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(tip) {
            return Err(DaemonError::Store(format!(
                "agent_message_lineage_cycle:{tip}"
            )));
        }
        let next = tx
            .query_row(
                "SELECT id FROM sessions WHERE continued_from=?1 ORDER BY created_at DESC,id DESC LIMIT 1",
                [tip.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let Some(next) = next else { return Ok(chain) };
        tip = Uuid::parse_str(&next)
            .map_err(|error| DaemonError::Store(format!("invalid lineage UUID: {error}")))?;
        chain.push(tip);
    }
}

#[allow(clippy::match_wildcard_for_single_variants)]
fn progress_status(status: SessionStatus) -> AgentProgressStatusV1 {
    match status {
        SessionStatus::Starting => AgentProgressStatusV1::Starting,
        SessionStatus::Running => AgentProgressStatusV1::Running,
        SessionStatus::WaitingApproval => AgentProgressStatusV1::WaitingApproval,
        SessionStatus::Completed => AgentProgressStatusV1::Completed,
        SessionStatus::Failed => AgentProgressStatusV1::Failed,
        SessionStatus::Interrupted => AgentProgressStatusV1::Interrupted,
        SessionStatus::Archived => AgentProgressStatusV1::Archived,
        SessionStatus::Deleted => AgentProgressStatusV1::Deleted,
        _ => AgentProgressStatusV1::Failed,
    }
}

// #241 attributed read scope: which sessions a token-attributed caller may
// read through the READ_VERBS session/conversation surface.

/// Bound on a `continued_from` rotation walk for attributed read scope. A
/// chain that still continues past this many links is treated as corrupt
/// lineage and grants nothing.
const AGENT_READ_LINEAGE_WALK_LIMIT: usize = 256;

/// Bound on a `parent_id` walk; equal to `MAX_OWNING_EPIC_HIERARCHY_DEPTH`.
/// A chain that still continues past this many links is corrupt topology.
const AGENT_READ_PARENT_WALK_LIMIT: usize = 64;

/// What an attributed (tokened) read exposes about its target (#241).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentReadClass {
    /// Session row, summaries, child listing. Additionally admits the
    /// caller's owning Epic and that Epic's Group (never an intermediate
    /// leaf ancestor).
    Metadata,
    /// Conversation events and turn metrics. No container rule.
    Content,
}

/// Caller-side facts for attributed read scope, resolved once per request so
/// a multi-row read (child listing, conversation batch) does not re-walk the
/// caller for every row.
#[derive(Debug, Clone)]
pub(crate) struct AgentReadScope {
    caller: Uuid,
    /// The caller's own `continued_from` or `parent_id` chain is cyclic or
    /// overflows its bound: every admission is denied.
    corrupt: bool,
    /// The caller's rotation predecessors (`continued_from` chain).
    predecessors: HashSet<Uuid>,
    /// The caller's owning Epic and its Group, or the Group the caller sits
    /// directly under. Metadata-only grant.
    containers: HashSet<Uuid>,
}

#[derive(Clone, Copy)]
enum AgentReadChainLink {
    Parent,
    ContinuedFrom,
}

impl AgentReadChainLink {
    const fn column(self) -> &'static str {
        match self {
            Self::Parent => "parent_id",
            Self::ContinuedFrom => "continued_from",
        }
    }

    const fn limit(self) -> usize {
        match self {
            Self::Parent => AGENT_READ_PARENT_WALK_LIMIT,
            Self::ContinuedFrom => AGENT_READ_LINEAGE_WALK_LIMIT,
        }
    }
}

/// One durable row on a read-scope chain walk.
struct AgentReadChainRow {
    id: Uuid,
    kind: String,
    lead: Option<Uuid>,
}

/// Result of a bounded chain walk. `Corrupt` (a repeated id, a link past the
/// walk bound, or an unparseable link) denies the whole admission; it is
/// never read as "chain ended here".
enum AgentReadChain {
    /// `start` first (absent when `start` does not exist), then each linked
    /// row nearest-first, ending at a null or dangling link.
    Intact(Vec<AgentReadChainRow>),
    Corrupt,
}

impl Store {
    /// Walk one link column from `start`, detecting cycles and depth
    /// overflow instead of silently truncating them.
    fn agent_read_chain(&self, start: Uuid, link: AgentReadChainLink) -> Result<AgentReadChain> {
        let column = link.column();
        let sql =
            format!("SELECT id, {column}, session_kind, lead_session_id FROM sessions WHERE id=?1");
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        let mut cursor = Some(start);
        while let Some(id) = cursor {
            if !seen.insert(id) {
                return Ok(AgentReadChain::Corrupt);
            }
            // `start` plus at most `limit` links.
            if rows.len() > link.limit() {
                return Ok(AgentReadChain::Corrupt);
            }
            let Some((next, kind, lead)) = stmt
                .query_row([id.to_string()], |row| {
                    Ok((
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                })
                .optional()?
            else {
                break;
            };
            cursor = match next {
                None => None,
                Some(next) => match Uuid::parse_str(&next) {
                    Ok(next) => Some(next),
                    Err(_) => return Ok(AgentReadChain::Corrupt),
                },
            };
            rows.push(AgentReadChainRow {
                id,
                kind,
                lead: lead.and_then(|lead| Uuid::parse_str(&lead).ok()),
            });
        }
        Ok(AgentReadChain::Intact(rows))
    }

    /// Resolve the caller-side facts of attributed read scope (#241).
    pub(crate) fn agent_read_scope(&self, caller: Uuid) -> Result<AgentReadScope> {
        let mut scope = AgentReadScope {
            caller,
            corrupt: false,
            predecessors: HashSet::new(),
            containers: HashSet::new(),
        };
        let (AgentReadChain::Intact(lineage), AgentReadChain::Intact(parents)) = (
            self.agent_read_chain(caller, AgentReadChainLink::ContinuedFrom)?,
            self.agent_read_chain(caller, AgentReadChainLink::Parent)?,
        ) else {
            scope.corrupt = true;
            return Ok(scope);
        };
        scope.predecessors = lineage.iter().skip(1).map(|row| row.id).collect();
        let above = parents.get(1..).unwrap_or_default();
        if let Some(parent) = above.first()
            && parent.kind == "Group"
        {
            scope.containers.insert(parent.id);
        }
        if let Some(epic_at) = above.iter().position(|row| row.kind == "Epic") {
            scope.containers.insert(above[epic_at].id);
            if let Some(group) = above.get(epic_at + 1)
                && group.kind == "Group"
            {
                scope.containers.insert(group.id);
            }
        }
        Ok(scope)
    }

    /// Whether a session-attributed caller may read `target` (#241).
    ///
    /// Admitted: the caller itself; its rotation lineage in either direction;
    /// its control subtree (the caller is on the target's parent chain, or
    /// an Epic at or above the target is led by the caller); the appointed
    /// manager's reach (`manager_session_scope`); and, for
    /// [`AgentReadClass::Metadata`] only, the caller's owning Epic and its
    /// Group. A cyclic or over-deep chain on either side denies the whole
    /// admission. A missing target is simply not admitted, so denial never
    /// reveals whether a foreign session exists.
    pub(crate) fn agent_read_scope_admits(
        &self,
        scope: &AgentReadScope,
        target: Uuid,
        class: AgentReadClass,
    ) -> Result<bool> {
        if scope.corrupt {
            return Ok(false);
        }
        let (AgentReadChain::Intact(lineage), AgentReadChain::Intact(parents)) = (
            self.agent_read_chain(target, AgentReadChainLink::ContinuedFrom)?,
            self.agent_read_chain(target, AgentReadChainLink::Parent)?,
        ) else {
            return Ok(false);
        };
        let caller = scope.caller;
        if target == caller || scope.predecessors.contains(&target) {
            return Ok(true);
        }
        if class == AgentReadClass::Metadata && scope.containers.contains(&target) {
            return Ok(true);
        }
        // Rotation successor: the caller sits on the target's lineage chain.
        if lineage.iter().skip(1).any(|row| row.id == caller) {
            return Ok(true);
        }
        // Control subtree: the caller is a parent-chain ancestor of the
        // target, or leads an Epic at or above it (the target included).
        if parents.iter().skip(1).any(|row| row.id == caller)
            || parents
                .iter()
                .any(|row| row.kind == "Epic" && row.lead == Some(caller))
        {
            return Ok(true);
        }
        Ok(self.manager_session_scope(caller, target)?.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::agent_verbs::tests::test_session;
    use crate::store::sandbox_custody::{CustodyCause, NewCustodyRoot};
    use rsi_common::types::{SandboxCleanupState, SandboxKind, SessionKind, SessionStatus};

    #[allow(clippy::expect_used)]
    fn progress_owner_and_child(
        store: &Store,
        owner_id: Uuid,
        child_id: Uuid,
        status: SessionStatus,
    ) {
        let mut owner = test_session(owner_id, std::path::PathBuf::from("/tmp"));
        owner.status = SessionStatus::Running;
        store.insert_session(&owner).expect("insert progress owner");
        let mut child = test_session(child_id, std::path::PathBuf::from("/tmp"));
        child.parent_id = Some(owner_id);
        child.status = status;
        store.insert_session(&child).expect("insert progress child");
    }

    fn accepted_v82_fingerprint_at_head(store: &Store) -> String {
        let live_version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read live schema head");
        store
            .conn
            .pragma_update(None, "user_version", AGENT_MESSAGE_V82_USER_VERSION)
            .expect("project the mailbox catalog to V82");
        let fingerprint = v81_schema_fingerprint(&store.conn);
        store
            .conn
            .pragma_update(None, "user_version", live_version)
            .expect("restore live schema head");
        fingerprint.expect("compute accepted V82 mailbox fingerprint")
    }

    #[test]
    fn v80_catalog_fingerprint_is_pinned() {
        // The accepted V80 fingerprint constant is PRESERVED verbatim and is
        // still load-bearing: it is the exact preflight V81 requires before
        // its first DDL statement (P2-02). What it can no longer be evaluated
        // against is a CURRENT store, because V81 deliberately rebuilds the
        // mailbox domain and therefore supersedes the six V80 mailbox objects
        // (`agent_messages` plus its two indexes and four triggers). The nine
        // surviving objects are exactly the accepted spawn, progress, and
        // watch objects that C-P2-03 forbids editing.
        let store = Store::open_in_memory().unwrap();
        let error = v80_schema_fingerprint(&store.conn)
            .expect_err("V81 supersedes the V80 mailbox catalog");
        assert!(
            error.to_string().contains("expected 15 objects, found 9"),
            "unexpected V80 inventory error: {error}"
        );

        // Every accepted V80 spawn/progress/watch object survives untouched.
        for name in [
            "agent_spawn_requests",
            "idx_agent_spawn_requests_owner_cohort",
            "idx_agent_spawn_requests_child_state",
            "idx_scheduled_jobs_agent_watch_natural",
            "agent_spawn_requests_no_delete",
            "agent_spawn_requests_identity_immutable",
            "agent_spawn_requests_v80_validate_insert",
            "agent_spawn_requests_v80_validate_update",
        ] {
            let present: i64 = store
                .conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name=?1",
                    [name],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(present, 1, "accepted V80 object {name} must survive V81");
        }

        // The V80 mailbox objects are exactly the ones V81 replaces.
        for name in [
            "idx_agent_messages_owner_state",
            "idx_agent_messages_target_fifo",
            "agent_messages_no_delete",
            "agent_messages_identity_immutable",
            "agent_messages_v80_validate_insert",
            "agent_messages_v80_validate_update",
        ] {
            let present: i64 = store
                .conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name=?1",
                    [name],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(present, 0, "V80 mailbox object {name} must be superseded");
        }
    }

    /// The V82 inventory, RECOUNTED against the live catalog.
    ///
    /// The constant arrays cannot prove themselves: asserting
    /// `AGENT_MESSAGE_V81_TABLES.len() == 6` only restates the literal. This
    /// counts `sqlite_master` at the V82 head instead, so a rebuild that
    /// silently dropped an index or left a scratch relation behind is caught.
    #[test]
    fn v82_catalog_inventory_is_unchanged_and_recounted() {
        let store = Store::open_in_memory().unwrap();

        // The rebuild's scratch relation must not survive it.
        let scratch: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name LIKE '%v82_rebuild_scratch%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(scratch, 0, "the V82 rebuild scratch relation leaked");

        let count = |kind: &str, prefixes: [&str; 2]| -> i64 {
            store
                .conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master
                      WHERE type=?1 AND (name LIKE ?2 OR name LIKE ?3)
                        AND name NOT LIKE 'sqlite_autoindex%'",
                    rusqlite::params![kind, prefixes[0], prefixes[1]],
                    |row| row.get(0),
                )
                .unwrap()
        };

        // 6 tables / 18 indexes / 32 triggers, unchanged by V82: a table-level
        // CHECK is not a catalog object, and the CAS trigger was replaced under
        // its own name rather than added.
        assert_eq!(
            count("index", ["idx_agent_message%", "idx_agent_messages%"]),
            18,
            "V82 index inventory moved"
        );
        assert_eq!(
            count("trigger", ["agent_message%", "agent_messages%"]),
            32,
            "V82 trigger inventory moved"
        );
        // R10 L-1: the TABLE count must come from the live catalog too. Asserting
        // only `AGENT_MESSAGE_V81_TABLES.len()` restates a literal, so that arm of
        // the "recounted" claim was not recounting anything — the attribution doc
        // said all three types were read from `sqlite_master` while only indexes
        // and triggers were.
        assert_eq!(
            count("table", ["agent_message%", "agent_messages%"]),
            6,
            "V82 table inventory moved"
        );
        assert_eq!(AGENT_MESSAGE_V81_TABLES.len(), 6);
        assert_eq!(AGENT_MESSAGE_V81_INDEXES.len(), 18);
        assert_eq!(AGENT_MESSAGE_V81_TRIGGERS.len(), 32);

        // The named backstop is present exactly once, in the attempts DDL.
        let attempts_sql: String = store
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='agent_message_delivery_attempts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            attempts_sql
                .matches("agent_message_attempts_v82_reconciler_no_effect_backstop")
                .count(),
            1,
            "the V82 backstop CHECK is not in the live attempts DDL exactly once"
        );

        // And the amended CAS trigger really carries the GAP 1 conjunct.
        let cas_sql: String = store
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='agent_messages_v81_cas_coherence'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            cas_sql.contains("a.correlation_state!='sealed_live_uncertain'"),
            "the V82 sealed-uncertain guard is not in the live CAS trigger"
        );
    }

    #[test]
    fn v81_catalog_fingerprint_is_pinned_and_rejects_the_rejected_digest() {
        let store = Store::open_in_memory().unwrap();
        let fingerprint = accepted_v82_fingerprint_at_head(&store);
        // The digest absorbs `PRAGMA user_version`, so a later schema head must
        // be projected to V82 to prove the shipped mailbox catalog still has
        // V82's accepted shape.
        assert_eq!(fingerprint, AGENT_MESSAGE_V82_PINNED_FINGERPRINT);
        assert_ne!(fingerprint, AGENT_MESSAGE_REJECTED_V81_SCHEMA_FINGERPRINT);

        // Exact inventory: six tables, 18 indexes, 32 triggers, no wildcard
        // discovery and no count-only acceptance.
        assert_eq!(AGENT_MESSAGE_V81_TABLES.len(), 6);
        assert_eq!(AGENT_MESSAGE_V81_INDEXES.len(), 18);
        assert_eq!(AGENT_MESSAGE_V81_TRIGGERS.len(), 32);
        for (kind, names) in [
            ("table", AGENT_MESSAGE_V81_TABLES.as_slice()),
            ("index", AGENT_MESSAGE_V81_INDEXES.as_slice()),
            ("trigger", AGENT_MESSAGE_V81_TRIGGERS.as_slice()),
        ] {
            for name in names {
                let found: String = store
                    .conn
                    .query_row(
                        "SELECT type FROM sqlite_master WHERE name=?1",
                        [name],
                        |row| row.get(0),
                    )
                    .unwrap_or_else(|_| panic!("missing V81 {kind} {name}"));
                assert_eq!(&found, kind, "V81 object {name} has the wrong type");
            }
        }
    }

    // ---- P2-03 acceptance ---------------------------------------------------

    /// Insert one Session row so the V81 `agent_messages_v81_target_custody`
    /// trigger can prove custody. That trigger requires the target to be
    /// EXACTLY one of an existing Session row or a live spawn reservation, so
    /// the direct-child acceptance path below inserts a real target row rather
    /// than a bare UUID.
    fn acceptance_session(store: &Store, id: Uuid) {
        let mut row = test_session(id, std::path::PathBuf::from("/tmp"));
        row.session_kind = SessionKind::Task;
        row.status = SessionStatus::Running;
        store.insert_session(&row).expect("insert session");
    }

    fn acceptance_owner(store: &Store, owner: Uuid) {
        acceptance_session(store, owner);
    }

    fn send_request(target: Uuid, key: &str, message: &str) -> AgentSendMessageRequestV1 {
        AgentSendMessageRequestV1 {
            target_session_id: target,
            message: message.to_string(),
            idempotency_key: key.to_string(),
            expires_at: None,
        }
    }

    // ---- H21-P2-INT-REV-002 trigger regression fixtures --------------------

    /// A timestamp in the exact shape `sql_timestamp` demands.
    const TRIGGER_TS: &str = "2026-08-03T00:00:00.000000000Z";

    fn trigger_digest(fill: &str) -> String {
        format!("sha256:{}", fill.repeat(64))
    }

    /// Accept a message and drive it to `claimed` on attempt 1, leaving that
    /// attempt UNSEALED (`attempt_state='claimed'`, null terminal disposition).
    ///
    /// Uses raw SQL deliberately: these are schema-level guards, and the point
    /// is that they hold against ANY writer, including one that bypasses the
    /// Store helpers. The claim CAS below is legitimate and must be admitted —
    /// if it ever starts failing, the tightening has over-reached.
    fn claimed_message_with_unsealed_attempt(
        store: &Store,
        owner: Uuid,
        target: Uuid,
        key: &str,
    ) -> Uuid {
        let message_id = store
            .accept_agent_message(owner, None, &send_request(target, key, "hello"))
            .expect("acceptance")
            .receipt()
            .message_id;

        let invocation_id = Uuid::new_v4();
        store
            .conn
            .execute(
                "INSERT INTO model_invocations
                 (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,provider,model,backend,model_tier,effort,trigger_source,session_id,created_at)
                 VALUES (?1,'agent.deliver','orchestration','foreground','paid_capable','admitted','running','Claude','claude-sonnet-5','Claude','premium','high','test',?2,?3)",
                params![invocation_id.to_string(), target.to_string(), TRIGGER_TS],
            )
            .expect("seed delivery invocation");

        store
            .conn
            .execute(
                "INSERT INTO agent_message_delivery_attempts (
                     message_id, attempt_number, claim_token, delivery_boot_id,
                     logical_root_session_id, delivery_session_id,
                     delivery_session_generation, delivery_model_invocation_id,
                     provider_kind, capability_kind, boundary_kind,
                     claimed_at, claim_expires_at, correlation_state,
                     attempt_state, updated_at)
                 VALUES (?1,1,?2,?3,?4,?5,0,?6,'harness','terminal_one_turn',
                         'native_turn',?7,?7,'not_applicable','claimed',?7)",
                params![
                    message_id.to_string(),
                    Uuid::new_v4().to_string(),
                    Uuid::new_v4().to_string(),
                    target.to_string(),
                    target.to_string(),
                    invocation_id.to_string(),
                    TRIGGER_TS,
                ],
            )
            .expect("insert unsealed claimed attempt");

        store
            .conn
            .execute(
                "INSERT INTO agent_message_state_transitions (
                     message_id, state_version, from_state, to_state, attempt_number,
                     authority_kind, authority_id, evidence_digest, created_at)
                 VALUES (?1,1,'queued','claimed',1,'dispatcher',?2,?3,?4)",
                params![
                    message_id.to_string(),
                    Uuid::new_v4().to_string(),
                    trigger_digest("a"),
                    TRIGGER_TS,
                ],
            )
            .expect("claim transition");

        store
            .conn
            .execute(
                "UPDATE agent_messages
                    SET state='claimed', state_version=1, attempt_count=1,
                        current_attempt_number=1, updated_at=?2
                  WHERE id=?1",
                params![message_id.to_string(), TRIGGER_TS],
            )
            .expect("a legitimate attempt-linked claim CAS must still be admitted");

        message_id
    }

    /// Seal an attempt terminal with `disposition`, filling every field the
    /// coupled CHECKs require.
    fn seal_attempt(store: &Store, message_id: Uuid, attempt: i64, disposition: &str) {
        let admission = if disposition == "unsupported" {
            "unsupported"
        } else {
            "rejected_before_effect"
        };
        store
            .conn
            .execute(
                "UPDATE agent_message_delivery_attempts
                    SET attempt_state='terminal',
                        admission_classification=?4,
                        effect_classification='proved_no_effect',
                        terminal_disposition=?3,
                        settlement_authority='dispatcher',
                        settlement_evidence_digest=?5,
                        admission_recorded_at=?6, settled_at=?6,
                        terminal_at=?6, updated_at=?6
                  WHERE message_id=?1 AND attempt_number=?2",
                params![
                    message_id.to_string(),
                    attempt,
                    disposition,
                    admission,
                    trigger_digest("b"),
                    TRIGGER_TS,
                ],
            )
            .expect("seal attempt");
    }

    fn insert_transition(
        store: &Store,
        message_id: Uuid,
        version: i64,
        from: &str,
        to: &str,
        attempt: Option<i64>,
    ) -> rusqlite::Result<usize> {
        store.conn.execute(
            "INSERT INTO agent_message_state_transitions (
                 message_id, state_version, from_state, to_state, attempt_number,
                 authority_kind, authority_id, evidence_digest, created_at)
             VALUES (?1,?2,?3,?4,?5,'dispatcher',?6,?7,?8)",
            params![
                message_id.to_string(),
                version,
                from,
                to,
                attempt,
                Uuid::new_v4().to_string(),
                trigger_digest("c"),
                TRIGGER_TS,
            ],
        )
    }

    fn settle_aggregate(store: &Store, message_id: Uuid, state: &str) -> rusqlite::Result<usize> {
        store.conn.execute(
            "UPDATE agent_messages
                SET state=?2, state_version=2, safe_error_class='agent_message_test',
                    updated_at=?3,
                    failed_at=CASE WHEN ?2='failed' THEN ?3 ELSE failed_at END,
                    expired_at=CASE WHEN ?2='expired' THEN ?3 ELSE expired_at END
              WHERE id=?1",
            params![message_id.to_string(), state, TRIGGER_TS],
        )
    }

    /// H21-P2-INT-REV-002. The aggregate CAS trigger applied NO pointer or
    /// disposition check for `failed`/`expired`, so a `claimed` message whose
    /// current attempt was still live could be durably recorded terminal with
    /// no sealed disposition anywhere — uncertainty erasure, the exact failure
    /// class Phase 2 exists to prevent, unguarded in the layer the plan
    /// designates as its guard (plan `:2202-2209`).
    #[test]
    fn a_terminal_aggregate_state_requires_a_sealed_matching_attempt_disposition() {
        let store = Store::open_in_memory().expect("open V81 store");
        let owner = Uuid::new_v4();
        let target = Uuid::new_v4();
        acceptance_owner(&store, owner);
        acceptance_session(&store, target);

        // ---- attack 1: attempt-linked terminal over an UNSEALED attempt ----
        let unsealed = claimed_message_with_unsealed_attempt(&store, owner, target, "k-linked");
        insert_transition(&store, unsealed, 2, "claimed", "failed", Some(1))
            .expect("the transition itself is well-formed");
        let error = settle_aggregate(&store, unsealed, "failed")
            .expect_err("a live attempt must not be settled failed without a sealed disposition");
        assert!(
            error.to_string().contains("CAS coherence"),
            "unexpected error: {error}"
        );

        // ---- attack 2: launder the same erasure through a NULL attempt -----
        // Claiming the transition carries no attempt must not buy an escape:
        // the aggregate still points at a live attempt.
        let laundered = claimed_message_with_unsealed_attempt(&store, owner, target, "k-null");
        insert_transition(&store, laundered, 2, "claimed", "failed", None)
            .expect("the transition itself is well-formed");
        let error = settle_aggregate(&store, laundered, "failed")
            .expect_err("a null-attempt transition must not erase a live attempt");
        assert!(
            error.to_string().contains("CAS coherence"),
            "unexpected error: {error}"
        );

        // ---- attack 3: sealed, but with a disposition `failed` forbids -----
        //
        // Direction matters here, and the reason is worth recording rather
        // than engineering around. The MIRROR of this case — settling
        // `expired` over an attempt sealed `proved_no_effect_failed` — never
        // reaches the CAS trigger at all: `agent_message_transitions_v81_
        // coherence` already refuses to record a `claimed→expired` edge whose
        // attempt is not sealed `proved_no_effect_expired`, so the transition
        // INSERT aborts first. That is correct layering, not redundancy.
        // `claimed→failed` has no such transition-level rule, so it is the
        // direction where the CAS trigger is the ONLY guard — which is exactly
        // what this asserts.
        let mismatched = claimed_message_with_unsealed_attempt(&store, owner, target, "k-mismatch");
        seal_attempt(&store, mismatched, 1, "proved_no_effect_expired");
        insert_transition(&store, mismatched, 2, "claimed", "failed", Some(1))
            .expect("the transitions trigger has no rule for claimed->failed");
        let error = settle_aggregate(&store, mismatched, "failed")
            .expect_err("failed must not accept a disposition sealed for expiry");
        assert!(
            error.to_string().contains("CAS coherence"),
            "unexpected error: {error}"
        );

        // ---- the authorized combinations still pass --------------------
        let failed = claimed_message_with_unsealed_attempt(&store, owner, target, "k-ok-failed");
        seal_attempt(&store, failed, 1, "proved_no_effect_failed");
        insert_transition(&store, failed, 2, "claimed", "failed", Some(1)).expect("transition");
        assert_eq!(
            settle_aggregate(&store, failed, "failed").expect("authorized settlement"),
            1
        );

        let expired = claimed_message_with_unsealed_attempt(&store, owner, target, "k-ok-expired");
        seal_attempt(&store, expired, 1, "proved_no_effect_expired");
        insert_transition(&store, expired, 2, "claimed", "expired", Some(1)).expect("transition");
        assert_eq!(
            settle_aggregate(&store, expired, "expired").expect("authorized settlement"),
            1
        );

        let unsupported = claimed_message_with_unsealed_attempt(&store, owner, target, "k-unsup");
        seal_attempt(&store, unsupported, 1, "unsupported");
        insert_transition(&store, unsupported, 2, "claimed", "failed", Some(1))
            .expect("transition");
        assert_eq!(
            settle_aggregate(&store, unsupported, "failed").expect("authorized settlement"),
            1
        );
    }

    /// H21-P2-INT-REV-002. The required transition was joined only on
    /// `(message_id, state_version, from_state, to_state)`, never on
    /// `attempt_number`, so a transition could name a different attempt than
    /// the aggregate's current pointer and still satisfy the CAS.
    #[test]
    fn a_transition_naming_a_different_attempt_than_the_current_pointer_is_refused() {
        let store = Store::open_in_memory().expect("open V81 store");
        let owner = Uuid::new_v4();
        let target = Uuid::new_v4();
        acceptance_owner(&store, owner);
        acceptance_session(&store, target);

        let message_id = claimed_message_with_unsealed_attempt(&store, owner, target, "k-wrong");
        seal_attempt(&store, message_id, 1, "proved_no_effect_failed");

        // A second, separately sealed attempt the aggregate does NOT point at.
        let invocation_id: String = store
            .conn
            .query_row(
                "SELECT delivery_model_invocation_id FROM agent_message_delivery_attempts
                  WHERE message_id=?1 AND attempt_number=1",
                [message_id.to_string()],
                |row| row.get(0),
            )
            .expect("read invocation");
        store
            .conn
            .execute(
                "INSERT INTO agent_message_delivery_attempts (
                     message_id, attempt_number, claim_token, delivery_boot_id,
                     logical_root_session_id, delivery_session_id,
                     delivery_session_generation, delivery_model_invocation_id,
                     provider_kind, capability_kind, boundary_kind,
                     claimed_at, claim_expires_at, correlation_state,
                     attempt_state, updated_at)
                 VALUES (?1,2,?2,?3,?4,?5,0,?6,'harness','terminal_one_turn',
                         'native_turn',?7,?7,'not_applicable','claimed',?7)",
                params![
                    message_id.to_string(),
                    Uuid::new_v4().to_string(),
                    Uuid::new_v4().to_string(),
                    target.to_string(),
                    target.to_string(),
                    invocation_id,
                    TRIGGER_TS,
                ],
            )
            .expect("insert second attempt");
        seal_attempt(&store, message_id, 2, "proved_no_effect_failed");

        // The aggregate still points at attempt 1; the transition names 2.
        insert_transition(&store, message_id, 2, "claimed", "failed", Some(2))
            .expect("the transition itself is well-formed");
        let error = settle_aggregate(&store, message_id, "failed")
            .expect_err("a transition must name the aggregate's exact current attempt");
        assert!(
            error.to_string().contains("CAS coherence"),
            "unexpected error: {error}"
        );
    }

    /// H21-P2-INT-REV-002. `'uncertain'` was absent from the transitions
    /// trigger's must-name-an-attempt list, so an uncertainty edge could be
    /// recorded without naming the attempt whose effect is in doubt — which is
    /// the only custody that state exists to carry.
    #[test]
    fn an_uncertain_transition_must_name_its_exact_attempt() {
        let store = Store::open_in_memory().expect("open V81 store");
        let owner = Uuid::new_v4();
        let target = Uuid::new_v4();
        acceptance_owner(&store, owner);
        acceptance_session(&store, target);

        let message_id = claimed_message_with_unsealed_attempt(&store, owner, target, "k-uncert");

        let error = insert_transition(&store, message_id, 2, "claimed", "uncertain", None)
            .expect_err("an uncertainty edge must name its attempt");
        assert!(
            error.to_string().contains("transition attempt linkage"),
            "unexpected error: {error}"
        );

        // Naming the exact attempt is accepted.
        insert_transition(&store, message_id, 2, "claimed", "uncertain", Some(1))
            .expect("an attempt-linked uncertainty edge is well-formed");
    }

    #[test]
    fn acceptance_inserts_the_aggregate_and_its_immutable_version_zero_edge() {
        let store = Store::open_in_memory().expect("open V81 store");
        let owner = Uuid::new_v4();
        let target = Uuid::new_v4();
        acceptance_owner(&store, owner);
        acceptance_session(&store, target);

        let outcome = store
            .accept_agent_message(owner, None, &send_request(target, "k-1", "hello"))
            .expect("first acceptance succeeds");
        assert!(matches!(outcome, AcceptAgentMessageOutcome::Accepted(_)));
        let receipt = outcome.receipt();
        assert_eq!(receipt.state, AgentMessageStateV1::Queued);
        assert_eq!(receipt.state_version, 0);
        assert!(
            !receipt.deduplicated,
            "an original acceptance must not report as a replay"
        );
        assert_eq!(
            receipt.target_session_id, target,
            "the receipt carries the immutable logical root"
        );

        // Exactly one version-0 acceptance edge, carrying no attempt.
        let (version, from, to, attempt, authority): (i64, String, String, Option<i64>, String) =
            store
                .conn
                .query_row(
                    "SELECT state_version, from_state, to_state, attempt_number, authority_kind
                     FROM agent_message_state_transitions WHERE message_id=?1",
                    [receipt.message_id.to_string()],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .expect("exactly one acceptance transition");
        assert_eq!((version, from.as_str(), to.as_str()), (0, "none", "queued"));
        assert_eq!(attempt, None, "no delivery can precede acceptance");
        assert_eq!(authority, "acceptance");

        // Acceptance invents zero delivery attempts.
        let attempts: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM agent_message_delivery_attempts WHERE message_id=?1",
                [receipt.message_id.to_string()],
                |row| row.get(0),
            )
            .expect("count attempts");
        assert_eq!(attempts, 0);
    }

    #[test]
    fn exact_replay_returns_the_original_receipt_and_writes_nothing_new() {
        let store = Store::open_in_memory().expect("open V81 store");
        let owner = Uuid::new_v4();
        let target = Uuid::new_v4();
        acceptance_owner(&store, owner);
        acceptance_session(&store, target);
        let request = send_request(target, "k-1", "hello");

        let first = store
            .accept_agent_message(owner, None, &request)
            .expect("first acceptance");
        let second = store
            .accept_agent_message(owner, None, &request)
            .expect("exact replay");

        assert!(matches!(second, AcceptAgentMessageOutcome::Replayed(_)));
        assert!(second.deduplicated());
        assert_eq!(
            second.receipt().message_id,
            first.receipt().message_id,
            "replay returns the SAME stable message UUID"
        );
        assert_eq!(second.receipt().state_version, 0);
        assert_eq!(
            second.receipt().expires_at,
            first.receipt().expires_at,
            "an exact replay preserves the original acceptance deadline"
        );

        let rows: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM agent_messages", [], |row| row.get(0))
            .expect("count messages");
        assert_eq!(rows, 1, "a replay must not insert a second aggregate");
        let edges: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM agent_message_state_transitions",
                [],
                |row| row.get(0),
            )
            .expect("count transitions");
        assert_eq!(edges, 1, "a replay must not author a second transition");
    }

    #[test]
    fn omitted_message_expiry_is_finite_and_keeps_the_original_request_fingerprint() {
        let store = Store::open_in_memory().expect("open V81 store");
        let owner = Uuid::new_v4();
        let target = Uuid::new_v4();
        acceptance_owner(&store, owner);
        acceptance_session(&store, target);
        let request = send_request(target, "default-expiry", "hello");

        let accepted = store
            .accept_agent_message(owner, None, &request)
            .expect("first acceptance");
        let receipt = accepted.receipt();
        assert_eq!(
            receipt.expires_at,
            Some(receipt.created_at + chrono::Duration::minutes(30))
        );
        let (stored_expiry, stored_fingerprint): (String, String) = store
            .conn
            .query_row(
                "SELECT expires_at, request_fingerprint FROM agent_messages WHERE id=?1",
                [receipt.message_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read accepted row");
        assert_eq!(
            parse_store_timestamp("agent message expires_at", &stored_expiry).unwrap(),
            receipt.expires_at.unwrap()
        );
        assert_eq!(
            stored_fingerprint,
            agent_message_request_fingerprint(owner, &request),
            "the fingerprint covers the original omitted expiry"
        );

        let replayed = store
            .accept_agent_message(owner, None, &request)
            .expect("exact replay");
        assert!(replayed.deduplicated());
        assert_eq!(replayed.receipt().message_id, receipt.message_id);
        assert_eq!(replayed.receipt().expires_at, receipt.expires_at);
    }

    #[test]
    fn default_deadline_expires_queued_mail_while_target_recovery_is_pending() {
        let store = Store::open_in_memory().expect("open V81 store");
        let owner = Uuid::new_v4();
        let target = Uuid::new_v4();
        acceptance_owner(&store, owner);
        acceptance_session(&store, target);
        let request = send_request(target, "held-default-expiry", "hello");
        let accepted_at = Utc::now() - chrono::Duration::minutes(31);
        let accepted = store
            .accept_agent_message_inner(owner, None, None, &request, false, || accepted_at)
            .expect("accept omitted expiry at the controlled test clock");
        let message_id = accepted.receipt().message_id;
        let original_deadline = accepted.receipt().expires_at;
        let (stored_created, stored_expiry): (String, String) = store
            .conn
            .query_row(
                "SELECT created_at, expires_at FROM agent_messages WHERE id=?1",
                [message_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read default deadline");
        assert_eq!(
            parse_store_timestamp("agent message created_at", &stored_created).unwrap(),
            accepted_at
        );
        assert_eq!(
            parse_store_timestamp("agent message expires_at", &stored_expiry).unwrap(),
            accepted_at + AGENT_MESSAGE_DEFAULT_EXPIRY
        );

        let still_held = store
            .accept_agent_message(
                owner,
                None,
                &send_request(target, "held-fresh-expiry", "hello"),
            )
            .expect("accept fresh default deadline")
            .receipt()
            .message_id;
        store
            .update_failed_and_stage_c5_autofile(
                target,
                crate::store::daemon_settings::AutofileCause::ProcessDied,
            )
            .expect("stage exact recovery owner");

        let page = store
            .list_dispatchable_agent_messages(None, 64)
            .expect("select held mail");
        assert_eq!(page.messages.len(), 2);
        let eligibility = |id| {
            page.messages
                .iter()
                .find(|message| message.message_id == id)
                .expect("selected message")
                .eligibility
        };
        assert_eq!(
            eligibility(message_id),
            AgentMessageDispatchEligibility::Expired
        );
        assert_eq!(
            eligibility(still_held),
            AgentMessageDispatchEligibility::RecoveryPending(SessionStatus::Failed)
        );

        let report = crate::session::agent_message_reconciler::reconcile_agent_messages_pass(
            &store,
            Uuid::new_v4(),
            crate::session::agent_message_reconciler::ReconciliationPassBudget::default(),
        );
        assert_eq!(
            (report.expired, report.terminal_failed, report.errors),
            (1, 0, 0)
        );
        let (state, attempt_count): (String, i64) = store
            .conn
            .query_row(
                "SELECT state, attempt_count FROM agent_messages WHERE id=?1",
                [message_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read expired held mail");
        assert_eq!((state.as_str(), attempt_count), ("expired", 0));
        let (attempt_number, authority): (Option<i64>, String) = store
            .conn
            .query_row(
                "SELECT attempt_number, authority_kind
                   FROM agent_message_state_transitions
                  WHERE message_id=?1 AND to_state='expired'",
                [message_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read expiry transition");
        assert_eq!(
            (attempt_number, authority.as_str()),
            (None, "expiry_reconciler")
        );
        let held_state: String = store
            .conn
            .query_row(
                "SELECT state FROM agent_messages WHERE id=?1",
                [still_held.to_string()],
                |row| row.get(0),
            )
            .expect("read unexpired held mail");
        assert_eq!(held_state, "queued");

        let replayed = store
            .accept_agent_message_inner(owner, None, None, &request, false, || {
                panic!("an exact replay must not sample a new acceptance clock")
            })
            .expect("exact replay after expiry");
        assert!(replayed.deduplicated());
        assert_eq!(replayed.receipt().message_id, message_id);
        assert_eq!(replayed.receipt().expires_at, original_deadline);
    }

    #[test]
    fn explicit_message_expiries_are_preserved_on_both_sides_of_the_default() {
        let store = Store::open_in_memory().expect("open V81 store");
        let owner = Uuid::new_v4();
        let target = Uuid::new_v4();
        acceptance_owner(&store, owner);
        acceptance_session(&store, target);

        for (key, expiry) in [
            ("early-expiry", Utc::now() + chrono::Duration::minutes(5)),
            ("late-expiry", Utc::now() + chrono::Duration::hours(2)),
        ] {
            let mut request = send_request(target, key, "hello");
            request.expires_at = Some(expiry);
            let accepted = store
                .accept_agent_message(owner, None, &request)
                .expect("first acceptance");
            assert_eq!(accepted.receipt().expires_at, Some(expiry));
            let replayed = store
                .accept_agent_message(owner, None, &request)
                .expect("exact replay");
            assert_eq!(replayed.receipt().expires_at, Some(expiry));
        }
    }

    #[test]
    fn the_same_key_with_changed_target_payload_or_expiry_conflicts() {
        let store = Store::open_in_memory().expect("open V81 store");
        let owner = Uuid::new_v4();
        let target = Uuid::new_v4();
        acceptance_owner(&store, owner);
        acceptance_session(&store, target);
        store
            .accept_agent_message(owner, None, &send_request(target, "k-1", "hello"))
            .expect("first acceptance");

        // Changed payload under the same key.
        let error = store
            .accept_agent_message(owner, None, &send_request(target, "k-1", "DIFFERENT"))
            .expect_err("changed payload conflicts");
        assert!(
            error
                .to_string()
                .contains(AgentMessageErrorCodeV1::IdempotencyConflict.as_str()),
            "unexpected error: {error}"
        );

        // Changed expiry under the same key and payload.
        let mut with_expiry = send_request(target, "k-1", "hello");
        with_expiry.expires_at = Some(Utc::now());
        let error = store
            .accept_agent_message(owner, None, &with_expiry)
            .expect_err("changed expiry conflicts");
        assert!(
            error
                .to_string()
                .contains(AgentMessageErrorCodeV1::IdempotencyConflict.as_str()),
            "unexpected error: {error}"
        );

        // Changed TARGET under the same key. This is the arm the test name
        // has always claimed and the contract has always required: replay is
        // scoped to (caller,key), so a retry whose target resolution drifted
        // — for example a child ID recomputed across a rotation — must surface
        // the drift to the caller rather than quietly delivering twice.
        let other_target = Uuid::new_v4();
        acceptance_session(&store, other_target);
        let error = store
            .accept_agent_message(owner, None, &send_request(other_target, "k-1", "hello"))
            .expect_err("changed target conflicts");
        assert!(
            error
                .to_string()
                .contains(AgentMessageErrorCodeV1::IdempotencyConflict.as_str()),
            "unexpected error: {error}"
        );

        // Exactly one message exists under this key: the conflict refused the
        // write rather than merely reporting it.
        let messages: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM agent_messages WHERE owner_session_id=?1",
                [owner.to_string()],
                |row| row.get(0),
            )
            .expect("count messages");
        assert_eq!(messages, 1, "a conflicting send must not insert a message");
    }

    #[test]
    fn acceptance_enforces_the_frozen_per_target_pending_cap() {
        let store = Store::open_in_memory().expect("open V81 store");
        let owner = Uuid::new_v4();
        let target = Uuid::new_v4();
        acceptance_owner(&store, owner);
        acceptance_session(&store, target);

        for index in 0..AGENT_MESSAGE_MAX_PENDING_PER_TARGET {
            store
                .accept_agent_message(
                    owner,
                    None,
                    &send_request(target, &format!("k-{index}"), "hello"),
                )
                .unwrap_or_else(|error| panic!("message {index} must fit the cap: {error}"));
        }
        let error = store
            .accept_agent_message(owner, None, &send_request(target, "k-overflow", "hello"))
            .expect_err("the 129th pending message to one target is refused");
        assert!(
            error
                .to_string()
                .contains(AgentMessageErrorCodeV1::TargetQueueFull.as_str()),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn acceptance_enforces_the_frozen_per_owner_pending_cap() {
        let store = Store::open_in_memory().expect("open V81 store");
        let owner = Uuid::new_v4();
        acceptance_owner(&store, owner);

        const TARGET_BASE: u128 = 0x0A00_0000;

        // Spread across enough targets that the per-target cap never fires
        // first, isolating the owner cap.
        let per_target = AGENT_MESSAGE_MAX_PENDING_PER_TARGET as usize;
        let mut sent = 0u32;
        let mut target_index = 0usize;
        while sent < AGENT_MESSAGE_MAX_PENDING_PER_OWNER {
            let target = Uuid::from_u128(TARGET_BASE + target_index as u128);
            acceptance_session(&store, target);
            for slot in 0..per_target {
                if sent >= AGENT_MESSAGE_MAX_PENDING_PER_OWNER {
                    break;
                }
                store
                    .accept_agent_message(
                        owner,
                        None,
                        &send_request(target, &format!("k-{target_index}-{slot}"), "hello"),
                    )
                    .unwrap_or_else(|error| panic!("message {sent} must fit the cap: {error}"));
                sent += 1;
            }
            target_index += 1;
        }

        let overflow_target = Uuid::from_u128(TARGET_BASE + 9_999);
        acceptance_session(&store, overflow_target);
        let error = store
            .accept_agent_message(owner, None, &send_request(overflow_target, "k-of", "hello"))
            .expect_err("the 513th pending message from one owner is refused");
        assert!(
            error
                .to_string()
                .contains(AgentMessageErrorCodeV1::OwnerQueueFull.as_str()),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn the_stable_message_uuid_is_deterministic_and_binds_exactly_caller_and_key() {
        let owner = Uuid::from_u128(1);
        let derive = |owner: Uuid, key: &str| {
            agent_message_id_from_idempotency_digest(&agent_message_idempotency_digest(owner, key))
        };
        let base = derive(owner, "k");
        assert_eq!(base, derive(owner, "k"), "derivation is stable");
        assert_ne!(base, derive(Uuid::from_u128(9), "k"), "caller binds");
        assert_ne!(base, derive(owner, "other"), "key binds");
        assert!(!base.is_nil());
    }

    #[test]
    fn a_bounded_utf8_payload_and_128_byte_key_are_accepted_verbatim() {
        let store = Store::open_in_memory().expect("open V81 store");
        let owner = Uuid::new_v4();
        let target = Uuid::new_v4();
        acceptance_owner(&store, owner);
        acceptance_session(&store, target);

        let key = "k".repeat(AGENT_MESSAGE_MAX_IDEMPOTENCY_KEY_BYTES);
        let payload = "héllo → 🌍";
        let receipt = store
            .accept_agent_message(owner, None, &send_request(target, &key, payload))
            .expect("bounded multibyte payload is accepted");

        let stored: String = store
            .conn
            .query_row(
                "SELECT payload FROM agent_messages WHERE id=?1",
                [receipt.receipt().message_id.to_string()],
                |row| row.get(0),
            )
            .expect("read payload");
        assert_eq!(stored, payload, "the payload persists byte-exact");
    }

    #[test]
    fn agent_get_progress_one_64_256_and_typed_overload() {
        let store = Store::open_in_memory().expect("open V80 store");
        let caller = Uuid::new_v4();
        let mut owner = test_session(caller, std::path::PathBuf::from("/tmp"));
        owner.session_kind = SessionKind::Task;
        owner.status = SessionStatus::Running;
        store.insert_session(&owner).expect("insert owner");

        let mut ids = Vec::new();
        for index in 0..257 {
            let id = Uuid::new_v4();
            let mut child = test_session(id, std::path::PathBuf::from("/tmp"));
            child.parent_id = Some(caller);
            child.session_kind = SessionKind::Task;
            child.status = if index == 0 {
                SessionStatus::Running
            } else {
                SessionStatus::Completed
            };
            store.insert_session(&child).expect("insert child");
            ids.push(id);
        }

        for size in [1usize, 64, 256] {
            let result = store
                .agent_get_progress_snapshot(caller, &ids[..size])
                .expect("bounded snapshot");
            assert_eq!(result.cohort_size, size as u32);
            assert_eq!(result.rows.len(), size);
            assert!(
                result
                    .rows
                    .windows(2)
                    .all(|rows| rows[0].cursor.session_id < rows[1].cursor.session_id),
                "rows must be UUID-sorted"
            );
            assert_eq!(
                result.status_counts.running + result.status_counts.completed,
                size as u32
            );
        }

        let error = store
            .agent_get_progress_snapshot(caller, &[])
            .expect_err("257-row full cohort must not truncate");
        let DaemonError::StructuredRpc { data, .. } = error else {
            panic!("expected typed overload error");
        };
        assert_eq!(data["code"], "cohort_too_large");
        assert_eq!(data["cohort_size"], 257);
        assert_eq!(data["max_cohort_size"], 256);
    }

    #[test]
    fn agent_get_progress_resolves_rotation_tip_and_event_cursor() {
        let store = Store::open_in_memory().expect("open V80 store");
        let caller = Uuid::new_v4();
        let origin_id = Uuid::new_v4();
        let tip_id = Uuid::new_v4();
        let mut owner = test_session(caller, std::path::PathBuf::from("/tmp"));
        owner.status = SessionStatus::Running;
        store.insert_session(&owner).expect("insert owner");
        let mut origin = test_session(origin_id, std::path::PathBuf::from("/tmp"));
        origin.parent_id = Some(caller);
        origin.status = SessionStatus::Completed;
        store.insert_session(&origin).expect("insert origin");
        let mut tip = test_session(tip_id, std::path::PathBuf::from("/tmp"));
        tip.parent_id = Some(caller);
        tip.continued_from = Some(origin_id);
        tip.status = SessionStatus::Running;
        store.insert_session(&tip).expect("insert tip");
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "INSERT INTO conversation_events (session_id,event_type,content,sequence,created_at) VALUES (?1,'assistant','progress',7,?2)",
                params![tip_id.to_string(), now],
            )
            .expect("insert durable event");

        let result = store
            .agent_get_progress_snapshot(caller, &[origin_id])
            .expect("snapshot");
        assert_eq!(result.rows.len(), 1);
        let cursor = &result.rows[0].cursor;
        assert_eq!(cursor.session_id, origin_id);
        assert_eq!(cursor.lineage_tip_id, tip_id);
        assert_eq!(cursor.event_sequence, 7);
        assert!(cursor.last_event_at.is_some());
        assert_eq!(cursor.status, AgentProgressStatusV1::Running);
    }

    #[test]
    fn agent_get_progress_projects_immutable_sandbox_base_commit() {
        let mut store = Store::open_in_memory().expect("open V80 store");
        let caller = Uuid::new_v4();
        let origin_id = Uuid::new_v4();
        let tip_id = Uuid::new_v4();
        let ordinary_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let reservation_id = Uuid::new_v4();
        let reserved_child_id = Uuid::new_v4();
        let base_commit = "a".repeat(64);

        let mut owner = test_session(caller, std::path::PathBuf::from("/tmp"));
        owner.status = SessionStatus::Running;
        store.insert_session(&owner).expect("insert owner");

        let mut origin = test_session(origin_id, std::path::PathBuf::from("/tmp/progress-base"));
        origin.parent_id = Some(caller);
        origin.status = SessionStatus::Completed;
        origin.sandbox_kind = Some(SandboxKind::GitWorktree);
        origin.sandbox_root = Some(std::path::PathBuf::from("/tmp/progress-base-sandbox"));
        origin.sandbox_branch = Some("rsi/progress-base".into());
        origin.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        store
            .insert_session_with_custody(
                &origin,
                SessionCustodyBinding::New(NewCustodyRoot {
                    custody_id: Uuid::new_v4(),
                    canonical_repo_dir: origin.working_dir.display().to_string(),
                    sandbox_root: origin.sandbox_root.as_ref().unwrap().display().to_string(),
                    sandbox_branch: origin.sandbox_branch.clone().unwrap(),
                    repository_identity: "progress-base-test-repository".into(),
                    source_commit: base_commit.clone(),
                    cause: CustodyCause::AgentSpawnChild,
                }),
            )
            .expect("insert sandboxed child");

        let mut tip = test_session(tip_id, std::path::PathBuf::from("/tmp"));
        tip.parent_id = Some(caller);
        tip.continued_from = Some(origin_id);
        tip.status = SessionStatus::Running;
        store.insert_session(&tip).expect("insert rotation tip");

        let mut ordinary = test_session(ordinary_id, std::path::PathBuf::from("/tmp"));
        ordinary.parent_id = Some(caller);
        store
            .insert_session(&ordinary)
            .expect("insert ordinary child");

        let mut epic = test_session(epic_id, std::path::PathBuf::from("/tmp"));
        epic.session_kind = SessionKind::Epic;
        epic.lead_session_id = Some(caller);
        store.insert_session(&epic).expect("insert epic");
        let request = AgentSpawnChildRequestV1 {
            kind: SessionKind::Task,
            provider: None,
            model: None,
            effort: None,
            query: "reserved child".into(),
            agent_role: None,
            topology_node: None,
            iteration: None,
            tags: None,
            idempotency_key: "base-commit-reservation".into(),
        };
        store
            .reserve_agent_spawn_request(
                caller,
                &format!("sha256:{}", "c".repeat(64)),
                &format!("sha256:{}", "d".repeat(64)),
                &request,
                epic_id,
                reservation_id,
                reserved_child_id,
            )
            .expect("reserve child without a Session");

        let result = store
            .agent_get_progress_snapshot(caller, &[origin_id, ordinary_id, reserved_child_id])
            .expect("progress snapshot");
        let origin_row = result
            .rows
            .iter()
            .find(|row| row.cursor.session_id == origin_id)
            .expect("sandboxed child row");
        assert_eq!(
            origin_row.base_commit.as_deref(),
            Some(base_commit.as_str())
        );
        assert_eq!(origin_row.cursor.lineage_tip_id, tip_id);
        assert_eq!(
            result
                .rows
                .iter()
                .find(|row| row.cursor.session_id == ordinary_id)
                .expect("ordinary child row")
                .base_commit,
            None
        );
        let reserved_row = result
            .rows
            .iter()
            .find(|row| row.cursor.session_id == reserved_child_id)
            .expect("reserved child row");
        assert_eq!(reserved_row.base_commit, None);
        assert!(
            serde_json::to_value(reserved_row)
                .expect("serialize reserved row")
                .get("base_commit")
                .is_none(),
            "rows with no custody root must omit the field"
        );
    }

    /// The progress snapshot publishes the SAME custody generation the
    /// continuation cursor fences on, so an owner can take
    /// `(lineage_tip_id, event_sequence, custody_generation)` straight from
    /// `AgentGetProgress` and continue the child without a second round trip.
    ///
    /// Before this fix the snapshot omitted the generation entirely while
    /// `AgentContinueChild` fenced on it, so the advertised one-round-trip
    /// path could only ever be walked by a caller that guessed absence.
    #[test]
    fn agent_get_progress_publishes_the_continuation_custody_generation() {
        let mut store = Store::open_in_memory().expect("open V80 store");
        let caller = Uuid::new_v4();
        let sandboxed_id = Uuid::new_v4();
        let ordinary_id = Uuid::new_v4();

        let mut owner = test_session(caller, std::path::PathBuf::from("/tmp"));
        owner.status = SessionStatus::Running;
        store.insert_session(&owner).expect("insert owner");

        let mut sandboxed = test_session(
            sandboxed_id,
            std::path::PathBuf::from("/tmp/progress-custody"),
        );
        sandboxed.parent_id = Some(caller);
        sandboxed.status = SessionStatus::Running;
        sandboxed.sandbox_kind = Some(SandboxKind::GitWorktree);
        sandboxed.sandbox_root = Some(std::path::PathBuf::from("/tmp/progress-custody-sandbox"));
        sandboxed.sandbox_branch = Some("rsi/progress-custody".into());
        sandboxed.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        store
            .insert_session_with_custody(
                &sandboxed,
                SessionCustodyBinding::New(NewCustodyRoot {
                    custody_id: Uuid::new_v4(),
                    canonical_repo_dir: sandboxed.working_dir.display().to_string(),
                    sandbox_root: sandboxed
                        .sandbox_root
                        .as_ref()
                        .unwrap()
                        .display()
                        .to_string(),
                    sandbox_branch: sandboxed.sandbox_branch.clone().unwrap(),
                    repository_identity: "progress-custody-test-repository".into(),
                    source_commit: "b".repeat(40),
                    cause: CustodyCause::AgentSpawnChild,
                }),
            )
            .expect("insert sandboxed child");

        let mut ordinary = test_session(ordinary_id, std::path::PathBuf::from("/tmp"));
        ordinary.parent_id = Some(caller);
        ordinary.status = SessionStatus::Running;
        store
            .insert_session(&ordinary)
            .expect("insert unsandboxed child");

        let result = store
            .agent_get_progress_snapshot(caller, &[sandboxed_id, ordinary_id])
            .expect("progress snapshot");
        let sandboxed_row = result
            .rows
            .iter()
            .find(|row| row.cursor.session_id == sandboxed_id)
            .expect("sandboxed child row");
        let generation = sandboxed_row
            .cursor
            .custody_generation
            .expect("a sandboxed child publishes its custody generation");
        assert!(
            generation > 0,
            "custody generations are positive; got {generation}"
        );

        // The snapshot and the continuation read must agree component for
        // component: they are the same durable facts read the same way.
        let continuation = store
            .agent_continuation_cursor(sandboxed_id)
            .expect("continuation cursor");
        assert_eq!(
            continuation,
            AgentContinuationCursorV1 {
                tip_session_id: sandboxed_row.cursor.lineage_tip_id,
                event_sequence: sandboxed_row.cursor.event_sequence,
                custody_generation: sandboxed_row.cursor.custody_generation,
            }
        );

        // The advertised one-round-trip path: build the staleness-fenced request straight
        // from the published cursor and confirm the observed tuple clears it.
        let request = rsi_common::agent_coordination::AgentContinueChildRequestV1 {
            target_session_id: sandboxed_id,
            query: "resume the stage".into(),
            expected_tip_session_id: sandboxed_row.cursor.lineage_tip_id,
            expected_event_sequence: sandboxed_row.cursor.event_sequence,
            expected_custody_generation: sandboxed_row.cursor.custody_generation,
            idempotency_key: None,
        };
        request
            .validate()
            .expect("a progress-derived request is in bounds");
        assert!(
            continuation.satisfies(&request),
            "a cursor taken from AgentGetProgress must clear AgentContinueChild"
        );

        // Absence stays absent: an unsandboxed child omits the field, so its
        // snapshot is byte-identical to one taken before the field existed.
        let ordinary_row = result
            .rows
            .iter()
            .find(|row| row.cursor.session_id == ordinary_id)
            .expect("unsandboxed child row");
        assert_eq!(ordinary_row.cursor.custody_generation, None);
        assert!(
            serde_json::to_value(&ordinary_row.cursor)
                .expect("serialize unsandboxed cursor")
                .get("custody_generation")
                .is_none(),
            "a tip with no custody must omit the field"
        );
    }

    #[test]
    fn agent_get_progress_reports_failed_pre_session_spawn_as_failed() {
        let store = Store::open_in_memory().expect("open V80 store");
        let caller = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let mut owner = test_session(caller, std::path::PathBuf::from("/tmp"));
        owner.status = SessionStatus::Running;
        store.insert_session(&owner).expect("insert owner");
        let mut epic = test_session(epic_id, std::path::PathBuf::from("/tmp"));
        epic.session_kind = SessionKind::Epic;
        epic.lead_session_id = Some(caller);
        store.insert_session(&epic).expect("insert epic");
        let request = AgentSpawnChildRequestV1 {
            kind: SessionKind::Task,
            provider: None,
            model: None,
            effort: None,
            query: "work".into(),
            agent_role: None,
            topology_node: None,
            iteration: None,
            tags: None,
            idempotency_key: "failed-before-session".into(),
        };
        store
            .reserve_agent_spawn_request(
                caller,
                &crate::model_control::hash_request_fingerprint(&[
                    "agent-spawn-idempotency-v1",
                    &caller.to_string(),
                    &request.idempotency_key,
                ]),
                &crate::model_control::hash_request_fingerprint(&[
                    "agent-spawn-request-v1",
                    &caller.to_string(),
                    &serde_json::to_string(&request).expect("serialize request"),
                ]),
                &request,
                epic_id,
                request_id,
                child_id,
            )
            .expect("reserve spawn");
        store
            .mark_agent_spawn_failed(request_id, "sandbox_custody.source_worktree_dirty")
            .expect("settle failed spawn");

        let result = store
            .agent_get_progress_snapshot(caller, &[child_id])
            .expect("failed snapshot");
        assert_eq!(result.rows[0].cursor.status, AgentProgressStatusV1::Failed);
        assert_eq!(result.rows[0].spawn_state, Some(AgentSpawnStateV1::Failed));
        assert_eq!(
            result.rows[0].spawn_safe_error_class.as_deref(),
            Some("sandbox_custody.source_worktree_dirty")
        );
        assert_eq!(result.status_counts.failed, 1);
        assert_eq!(result.status_counts.reserved, 0);
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn agent_get_progress_marks_failed_and_interrupted_children_as_harvest_obligations() {
        let store = Store::open_in_memory().expect("open store");
        let caller = Uuid::new_v4();
        let failed_id = Uuid::new_v4();
        let interrupted_id = Uuid::new_v4();
        progress_owner_and_child(&store, caller, failed_id, SessionStatus::Failed);
        let mut interrupted_owner = test_session(Uuid::new_v4(), std::path::PathBuf::from("/tmp"));
        interrupted_owner.status = SessionStatus::Running;
        store
            .insert_session(&interrupted_owner)
            .expect("insert second owner");
        let mut interrupted = test_session(interrupted_id, std::path::PathBuf::from("/tmp"));
        interrupted.parent_id = Some(interrupted_owner.id);
        interrupted.status = SessionStatus::Interrupted;
        store
            .insert_session(&interrupted)
            .expect("insert interrupted child");

        let failed = store
            .agent_get_progress_snapshot(caller, &[failed_id])
            .expect("failed progress snapshot");
        assert_eq!(failed.rows[0].cursor.status, AgentProgressStatusV1::Failed);
        assert_eq!(
            failed.rows[0].obligation,
            Some(AgentProgressObligationV1::HarvestFailed)
        );
        assert_eq!(failed.unhandled_terminal_children, 1);

        let interrupted = store
            .agent_get_progress_snapshot(interrupted_owner.id, &[interrupted_id])
            .expect("interrupted progress snapshot");
        assert_eq!(
            interrupted.rows[0].cursor.status,
            AgentProgressStatusV1::Interrupted
        );
        assert_eq!(
            interrupted.rows[0].obligation,
            Some(AgentProgressObligationV1::HarvestInterrupted)
        );
        assert_eq!(interrupted.unhandled_terminal_children, 1);
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn agent_get_progress_failed_reservation_reports_spawn_failed_without_obligation_count() {
        let store = Store::open_in_memory().expect("open store");
        let caller = Uuid::new_v4();
        let failed_child = Uuid::new_v4();
        let reservation_child = Uuid::new_v4();
        let request_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        progress_owner_and_child(&store, caller, failed_child, SessionStatus::Failed);
        let mut epic = test_session(epic_id, std::path::PathBuf::from("/tmp"));
        epic.session_kind = SessionKind::Epic;
        epic.lead_session_id = Some(caller);
        store.insert_session(&epic).expect("insert Epic");
        let request = AgentSpawnChildRequestV1 {
            kind: SessionKind::Task,
            provider: None,
            model: None,
            effort: None,
            query: "work".into(),
            agent_role: None,
            topology_node: None,
            iteration: None,
            tags: None,
            idempotency_key: "failed-reservation-no-obligation".into(),
        };
        store
            .reserve_agent_spawn_request(
                caller,
                &crate::model_control::hash_request_fingerprint(&[
                    "agent-spawn-idempotency-v1",
                    &caller.to_string(),
                    &request.idempotency_key,
                ]),
                &crate::model_control::hash_request_fingerprint(&[
                    "agent-spawn-request-v1",
                    &caller.to_string(),
                    &serde_json::to_string(&request).expect("serialize request"),
                ]),
                &request,
                epic_id,
                request_id,
                reservation_child,
            )
            .expect("reserve spawn");
        store
            .mark_agent_spawn_failed(request_id, "sandbox_custody.source_worktree_dirty")
            .expect("settle failed spawn");

        let result = store
            .agent_get_progress_snapshot(caller, &[reservation_child, failed_child])
            .expect("progress snapshot");
        let reservation = result
            .rows
            .iter()
            .find(|row| row.cursor.session_id == reservation_child)
            .expect("reservation row");
        let session = result
            .rows
            .iter()
            .find(|row| row.cursor.session_id == failed_child)
            .expect("failed child session row");
        assert_eq!(reservation.cursor.status, AgentProgressStatusV1::Failed);
        assert_eq!(reservation.spawn_state, Some(AgentSpawnStateV1::Failed));
        assert_eq!(reservation.obligation, None);
        assert_eq!(
            session.obligation,
            Some(AgentProgressObligationV1::HarvestFailed)
        );
        assert_eq!(result.status_counts.failed, 2);
        assert_eq!(result.unhandled_terminal_children, 1);
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn agent_get_progress_archived_child_reports_archived_status_and_zero_unhandled() {
        let store = Store::open_in_memory().expect("open store");
        let caller = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        progress_owner_and_child(&store, caller, child_id, SessionStatus::Archived);
        let result = store
            .agent_get_progress_snapshot(caller, &[child_id])
            .expect("archived progress snapshot");
        assert_eq!(
            result.rows[0].cursor.status,
            AgentProgressStatusV1::Archived
        );
        assert_eq!(result.rows[0].obligation, None);
        assert_eq!(result.unhandled_terminal_children, 0);
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn agent_get_progress_relaunched_child_reports_running_and_zero_unhandled() {
        let store = Store::open_in_memory().expect("open store");
        let caller = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        progress_owner_and_child(&store, caller, child_id, SessionStatus::Running);
        let result = store
            .agent_get_progress_snapshot(caller, &[child_id])
            .expect("running progress snapshot");
        assert_eq!(result.rows[0].cursor.status, AgentProgressStatusV1::Running);
        assert_eq!(result.rows[0].obligation, None);
        assert_eq!(result.unhandled_terminal_children, 0);
    }

    #[test]
    #[allow(clippy::expect_used, clippy::items_after_statements)]
    fn agent_get_progress_snapshot_without_terminal_children_is_byte_identical() {
        let store = Store::open_in_memory().expect("open store");
        let caller = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        progress_owner_and_child(&store, caller, child_id, SessionStatus::Running);
        let result = store
            .agent_get_progress_snapshot(caller, &[child_id])
            .expect("running progress snapshot");
        assert_eq!(result.unhandled_terminal_children, 0);
        assert_eq!(result.rows[0].obligation, None);
        #[derive(serde::Serialize)]
        struct LegacyProgressRow<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            spawn_request_id: Option<&'a Uuid>,
            #[serde(skip_serializing_if = "Option::is_none")]
            spawn_state: Option<&'a AgentSpawnStateV1>,
            #[serde(skip_serializing_if = "Option::is_none")]
            spawn_safe_error_class: Option<&'a String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            base_commit: Option<&'a String>,
            cursor: &'a AgentProgressCursorV1,
            freshness: &'a AgentProgressFreshnessV1,
            watch_state: &'a AgentWatchStateV1,
            messages: &'a AgentMessageCountsV1,
            #[serde(skip_serializing_if = "Option::is_none")]
            message_queue: Option<&'a AgentMessageQueueSummaryV1>,
        }
        #[derive(serde::Serialize)]
        struct LegacyProgressResult<'a> {
            observed_at: &'a DateTime<Utc>,
            cohort_size: u32,
            status_counts: &'a AgentProgressStatusCountsV1,
            rows: Vec<LegacyProgressRow<'a>>,
        }
        let legacy = LegacyProgressResult {
            observed_at: &result.observed_at,
            cohort_size: result.cohort_size,
            status_counts: &result.status_counts,
            rows: result
                .rows
                .iter()
                .map(|row| LegacyProgressRow {
                    spawn_request_id: row.spawn_request_id.as_ref(),
                    spawn_state: row.spawn_state.as_ref(),
                    spawn_safe_error_class: row.spawn_safe_error_class.as_ref(),
                    base_commit: row.base_commit.as_ref(),
                    cursor: &row.cursor,
                    freshness: &row.freshness,
                    watch_state: &row.watch_state,
                    messages: &row.messages,
                    message_queue: row.message_queue.as_ref(),
                })
                .collect(),
        };
        assert_eq!(
            serde_json::to_vec(&result).expect("serialize current response"),
            serde_json::to_vec(&legacy).expect("serialize previous wire shape")
        );
    }

    #[test]
    fn agent_spawn_child_admission_commits_child_and_settlement_atomically() {
        let store = Store::open_in_memory().expect("open V80 store");
        let owner_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();
        let invocation_id = Uuid::new_v4();
        let mut owner = test_session(owner_id, std::path::PathBuf::from("/tmp"));
        owner.status = SessionStatus::Running;
        let mut epic = test_session(epic_id, std::path::PathBuf::from("/tmp"));
        epic.session_kind = SessionKind::Epic;
        epic.lead_session_id = Some(owner_id);
        store.insert_session(&owner).expect("insert owner");
        store.insert_session(&epic).expect("insert epic");
        let request = AgentSpawnChildRequestV1 {
            kind: SessionKind::Task,
            provider: None,
            model: None,
            effort: None,
            query: "work".into(),
            agent_role: None,
            topology_node: None,
            iteration: None,
            tags: None,
            idempotency_key: "atomic-admission".into(),
        };
        store
            .reserve_agent_spawn_request(
                owner_id,
                &format!("sha256:{}", "c".repeat(64)),
                &format!("sha256:{}", "d".repeat(64)),
                &request,
                epic_id,
                request_id,
                child_id,
            )
            .expect("reserve");
        store.mark_agent_spawn_queued(request_id).expect("queue");
        store
            .mark_agent_spawn_launching(request_id)
            .expect("launching");
        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "INSERT INTO model_invocations
                 (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,provider,model,backend,model_tier,effort,trigger_source,session_id,created_at)
                 VALUES (?1,'agent.spawn_child','orchestration','foreground','paid_capable','admitted','running','Claude','claude-sonnet-5','Claude','premium','high','test',?2,?3)",
                params![invocation_id.to_string(), owner_id.to_string(), now],
            )
            .expect("seed invocation");
        let mut child = test_session(child_id, std::path::PathBuf::from("/tmp"));
        child.session_kind = SessionKind::Task;
        child.parent_id = Some(epic_id);
        child.status = SessionStatus::Starting;
        let reserved = store.get_agent_spawn_request(request_id).unwrap().unwrap();
        child.agent_role = reserved.request.agent_role;
        child.epic_spawn_ordinal = Some(reserved.epic_spawn_ordinal);
        store
            .admit_agent_child_session(
                &child,
                invocation_id,
                request_id,
                owner_id,
                SessionCustodyBinding::Ordinary,
            )
            .expect("atomic child admission");
        assert!(store.get_session(child_id).unwrap().is_some());
        assert_eq!(
            store
                .get_agent_spawn_request(request_id)
                .unwrap()
                .unwrap()
                .state,
            AgentSpawnStateV1::Launched
        );
        let arm_intent: String = store
            .conn
            .query_row(
                "SELECT state FROM agent_child_watch_witness
                 WHERE owner_session_id=?1 AND child_session_id=?2",
                params![owner_id.to_string(), child_id.to_string()],
                |row| row.get(0),
            )
            .expect("launch commits watch arm intent");
        assert_eq!(arm_intent, "pending");

        let duplicate_request_id = Uuid::new_v4();
        store
            .reserve_agent_spawn_request(
                owner_id,
                &format!("sha256:{}", "e".repeat(64)),
                &format!("sha256:{}", "f".repeat(64)),
                &AgentSpawnChildRequestV1 {
                    idempotency_key: "atomic-rollback".into(),
                    ..request
                },
                epic_id,
                duplicate_request_id,
                child_id,
            )
            .expect_err("reserved child identity is globally unique");
        assert!(
            store
                .get_agent_spawn_request(duplicate_request_id)
                .unwrap()
                .is_none(),
            "failed reservation must leave no partial request row"
        );

        let rollback_request_id = Uuid::new_v4();
        let rollback_child_id = Uuid::new_v4();
        let rollback_request = AgentSpawnChildRequestV1 {
            kind: SessionKind::Task,
            provider: None,
            model: None,
            effort: None,
            query: "work".into(),
            agent_role: None,
            topology_node: None,
            iteration: None,
            tags: None,
            idempotency_key: "admission-rollback".into(),
        };
        store
            .reserve_agent_spawn_request(
                owner_id,
                &format!("sha256:{}", "1".repeat(64)),
                &format!("sha256:{}", "2".repeat(64)),
                &rollback_request,
                epic_id,
                rollback_request_id,
                rollback_child_id,
            )
            .expect("reserve rollback case");
        store
            .mark_agent_spawn_queued(rollback_request_id)
            .expect("queue rollback case");
        let mut conflicting_child =
            test_session(rollback_child_id, std::path::PathBuf::from("/tmp"));
        conflicting_child.parent_id = Some(epic_id);
        conflicting_child.session_kind = SessionKind::Task;
        let rollback_reserved = store
            .get_agent_spawn_request(rollback_request_id)
            .unwrap()
            .unwrap();
        conflicting_child.agent_role = rollback_reserved.request.agent_role;
        conflicting_child.epic_spawn_ordinal = Some(rollback_reserved.epic_spawn_ordinal);
        store
            .insert_session(&conflicting_child)
            .expect("seed conflicting session");
        assert!(
            store
                .admit_agent_child_session(
                    &conflicting_child,
                    invocation_id,
                    rollback_request_id,
                    owner_id,
                    SessionCustodyBinding::Ordinary,
                )
                .is_err()
        );
        assert_eq!(
            store
                .get_agent_spawn_request(rollback_request_id)
                .unwrap()
                .unwrap()
                .state,
            AgentSpawnStateV1::Queued,
            "failed child insert must roll back launch settlement"
        );
        let rolled_back_witnesses: i64 = store
            .conn
            .query_row(
                "SELECT count(*) FROM agent_child_watch_witness
                 WHERE child_session_id=?1",
                [rollback_child_id.to_string()],
                |row| row.get(0),
            )
            .expect("count rollback witnesses");
        assert_eq!(rolled_back_witnesses, 0);
    }

    #[test]
    fn post_bind_agent_child_failure_fences_exact_launch_and_retains_owned_root() {
        let mut store = Store::open_in_memory().expect("open store");
        let owner_id = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();
        let invocation_id = Uuid::new_v4();
        let custody_id = Uuid::new_v4();
        let sandbox_root = std::path::PathBuf::from(format!("/repo/sandboxes/{child_id}"));

        let mut owner = test_session(owner_id, std::path::PathBuf::from("/repo"));
        owner.status = SessionStatus::Running;
        let mut epic = test_session(epic_id, std::path::PathBuf::from("/repo"));
        epic.session_kind = SessionKind::Epic;
        epic.lead_session_id = Some(owner_id);
        store.insert_session(&owner).expect("insert owner");
        store.insert_session(&epic).expect("insert epic");

        let request = AgentSpawnChildRequestV1 {
            kind: SessionKind::Task,
            provider: None,
            model: None,
            effort: None,
            query: "work".into(),
            agent_role: Some("implementer".into()),
            topology_node: None,
            iteration: None,
            tags: None,
            idempotency_key: "post-bind-failure".into(),
        };
        store
            .reserve_agent_spawn_request(
                owner_id,
                &format!("sha256:{}", "7".repeat(64)),
                &format!("sha256:{}", "8".repeat(64)),
                &request,
                epic_id,
                request_id,
                child_id,
            )
            .expect("reserve child");
        store
            .mark_agent_spawn_launching(request_id)
            .expect("mark launching");

        let now = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
        store
            .conn
            .execute(
                "INSERT INTO model_invocations
                 (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,provider,model,backend,model_tier,effort,trigger_source,session_id,created_at)
                 VALUES (?1,'agent.spawn_child','orchestration','foreground','paid_capable','admitted','running','Claude','claude-sonnet-5','Claude','premium','high','test',?2,?3)",
                params![invocation_id.to_string(), owner_id.to_string(), now],
            )
            .expect("seed invocation");

        let reserved = store
            .get_agent_spawn_request(request_id)
            .expect("read reservation")
            .expect("reservation exists");
        let mut child = test_session(child_id, std::path::PathBuf::from("/repo"));
        child.session_kind = SessionKind::Task;
        child.parent_id = Some(epic_id);
        child.status = SessionStatus::Starting;
        child.agent_role = reserved.request.agent_role;
        child.epic_spawn_ordinal = Some(reserved.epic_spawn_ordinal);
        child.sandbox_kind = Some(SandboxKind::GitWorktree);
        child.sandbox_root = Some(sandbox_root.clone());
        child.sandbox_branch = Some(format!("rsi/{child_id}"));
        child.sandbox_cleanup_state = Some(SandboxCleanupState::Live);
        store
            .admit_agent_child_session(
                &child,
                invocation_id,
                request_id,
                owner_id,
                SessionCustodyBinding::New(NewCustodyRoot {
                    custody_id,
                    canonical_repo_dir: "/repo".into(),
                    sandbox_root: sandbox_root.display().to_string(),
                    sandbox_branch: child.sandbox_branch.clone().expect("branch"),
                    repository_identity: "post-bind-agent-child-test".into(),
                    source_commit: "a".repeat(40),
                    cause: CustodyCause::AgentSpawnChild,
                }),
            )
            .expect("admit bound child");
        store
            .conn
            .execute(
                "UPDATE model_invocations SET session_id=?1 WHERE id=?2",
                params![child_id.to_string(), invocation_id.to_string()],
            )
            .expect("bind invocation to child");

        let mut prelaunch_template = child.clone();
        prelaunch_template.status = SessionStatus::Failed;
        prelaunch_template.stop_reason = Some("sandbox_custody:worktree_mismatch".into());
        prelaunch_template.sandbox_kind = None;
        prelaunch_template.sandbox_root = None;
        prelaunch_template.sandbox_branch = None;
        prelaunch_template.sandbox_cleanup_state = None;
        assert!(
            store
                .settle_failed_agent_spawn_custody(
                    request_id,
                    &prelaunch_template,
                    SandboxCustodyErrorCodeV1::WorktreeMismatch,
                )
                .is_err(),
            "the prelaunch helper must reject an already-Launched request"
        );

        store
            .settle_post_bind_agent_child_failure(
                request_id,
                child_id,
                invocation_id,
                "execution_scratch_unavailable",
            )
            .expect("settle post-bind failure");

        let failed_request = store
            .get_agent_spawn_request(request_id)
            .expect("read failed request")
            .expect("failed request exists");
        assert_eq!(failed_request.state, AgentSpawnStateV1::Failed);
        assert_eq!(
            failed_request.safe_error_class.as_deref(),
            Some("execution_scratch_unavailable")
        );
        let failed_child = store
            .get_session(child_id)
            .expect("read failed child")
            .expect("failed child exists");
        assert_eq!(failed_child.status, SessionStatus::Failed);
        assert_eq!(
            failed_child.stop_reason.as_deref(),
            Some("sandbox_custody:execution_scratch_unavailable")
        );

        let projection: (String, String, Option<String>, Option<String>, Option<i64>, String) =
            store
                .conn
                .query_row(
                    "SELECT execution_state,freshness,effective_cwd,custody_id,custody_generation,error_code
                     FROM session_execution_projections WHERE session_id=?1",
                    [child_id.to_string()],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ))
                    },
                )
                .expect("read failed projection");
        assert_eq!(projection.0, "invalid");
        assert_eq!(projection.1, "invalid");
        assert_eq!(projection.2, None);
        assert_eq!(projection.3, Some(custody_id.to_string()));
        assert_eq!(projection.4, Some(1));
        assert_eq!(projection.5, "execution_scratch_unavailable");

        let root: (String, String, String, i64) = store
            .conn
            .query_row(
                "SELECT state,owner_session_id,allocation_id,generation
                 FROM sandbox_custody_roots WHERE custody_id=?1",
                [custody_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("read retained custody root");
        assert_eq!(root.0, "live");
        assert_eq!(root.1, child_id.to_string());
        assert_eq!(root.2, child_id.to_string());
        assert_eq!(root.3, 1);
    }

    // ---- P2-03 progress rooting and mailbox summary -------------------------

    /// Progress must count mail against the IMMUTABLE logical target, never the
    /// delivery tip.
    ///
    /// Before this fix `progress_row` counted against `lineage_tip_id`, so the
    /// instant a child rotated (a new `sessions` row with
    /// `continued_from = <root>`), the owner's entire mailbox read as empty
    /// while the durable rows were untouched. Acceptance stores
    /// `target_session_id` exactly as the owner named it, so the tip is by
    /// construction the wrong key.
    #[test]
    fn progress_counts_mail_against_the_logical_root_not_the_rotation_tip() {
        let store = Store::open_in_memory().expect("open V81 store");
        let owner = Uuid::new_v4();
        let root = Uuid::new_v4();
        let tip = Uuid::new_v4();
        acceptance_owner(&store, owner);

        // The target is a direct child of the owner, so it is in the cohort.
        let mut root_row = test_session(root, std::path::PathBuf::from("/tmp"));
        root_row.session_kind = SessionKind::Task;
        root_row.status = SessionStatus::Running;
        root_row.parent_id = Some(owner);
        store.insert_session(&root_row).expect("insert root");

        store
            .accept_agent_message(owner, None, &send_request(root, "k-root", "hello"))
            .expect("accept against the logical root");

        // Sanity: with no rotation the count is visible.
        let before = store
            .agent_get_progress_snapshot(owner, &[root])
            .expect("snapshot before rotation");
        assert_eq!(before.rows[0].messages.queued, 1);
        assert_eq!(before.rows[0].cursor.lineage_tip_id, root);

        // Rotate: a new row continues from the root. The tip moves; the
        // mailbox key must not.
        let mut tip_row = test_session(tip, std::path::PathBuf::from("/tmp"));
        tip_row.session_kind = SessionKind::Task;
        tip_row.status = SessionStatus::Running;
        tip_row.parent_id = Some(owner);
        tip_row.continued_from = Some(root);
        store.insert_session(&tip_row).expect("insert rotation tip");

        let after = store
            .agent_get_progress_snapshot(owner, &[root])
            .expect("snapshot after rotation");
        let row = &after.rows[0];
        assert_eq!(
            row.cursor.lineage_tip_id, tip,
            "the cursor legitimately follows the rotation tip"
        );
        assert_eq!(
            row.messages.queued, 1,
            "mail must remain visible after the target rotates"
        );
        assert_eq!(
            row.message_queue
                .as_ref()
                .expect("summary present")
                .latest_state,
            AgentMessageStateV1::Queued
        );
    }

    /// A target with no mail reports no summary at all, so accepted Phase 1
    /// progress snapshots are byte-identical.
    #[test]
    fn progress_omits_the_mailbox_summary_when_a_target_has_no_mail() {
        let store = Store::open_in_memory().expect("open V81 store");
        let owner = Uuid::new_v4();
        let child = Uuid::new_v4();
        acceptance_owner(&store, owner);
        let mut child_row = test_session(child, std::path::PathBuf::from("/tmp"));
        child_row.session_kind = SessionKind::Task;
        child_row.status = SessionStatus::Running;
        child_row.parent_id = Some(owner);
        store.insert_session(&child_row).expect("insert child");

        let snapshot = store
            .agent_get_progress_snapshot(owner, &[child])
            .expect("snapshot");
        assert!(snapshot.rows[0].message_queue.is_none());
        let encoded = serde_json::to_value(&snapshot.rows[0]).expect("encode row");
        assert!(
            encoded.get("message_queue").is_none(),
            "an absent summary must not appear on the wire: {encoded}"
        );
    }

    /// The summary reports queue age, the latest state/version, and an absent
    /// acknowledgement cursor before any delivery attempt exists — and it
    /// never loads a payload.
    #[test]
    fn progress_summary_reports_queue_age_and_latest_state_without_loading_payloads() {
        let store = Store::open_in_memory().expect("open V81 store");
        let owner = Uuid::new_v4();
        let child = Uuid::new_v4();
        acceptance_owner(&store, owner);
        let mut child_row = test_session(child, std::path::PathBuf::from("/tmp"));
        child_row.session_kind = SessionKind::Task;
        child_row.status = SessionStatus::Running;
        child_row.parent_id = Some(owner);
        store.insert_session(&child_row).expect("insert child");

        let secret = "top-secret-payload-body";
        for key in ["k-1", "k-2", "k-3"] {
            store
                .accept_agent_message(owner, None, &send_request(child, key, secret))
                .expect("accept");
        }

        let snapshot = store
            .agent_get_progress_snapshot(owner, &[child])
            .expect("snapshot");
        let row = &snapshot.rows[0];
        assert_eq!(row.messages.queued, 3);
        let summary = row.message_queue.as_ref().expect("summary present");
        assert!(
            summary.oldest_pending_age_ms.is_some(),
            "three pending messages must report a queue age"
        );
        assert_eq!(summary.latest_state, AgentMessageStateV1::Queued);
        assert_eq!(summary.latest_state_version, 0);
        assert_eq!(summary.latest_attempt_count, 0);
        assert_eq!(summary.latest_current_attempt_number, None);
        assert_eq!(summary.latest_safe_error_class, None);
        assert_eq!(
            summary.acknowledgement_cursor, None,
            "no attempt exists yet, so there is no acknowledgement cursor"
        );

        // Progress never exposes message bodies.
        let encoded = serde_json::to_string(&snapshot).expect("encode snapshot");
        assert!(
            !encoded.contains(secret),
            "progress must never expose a message payload"
        );
    }

    // =====================================================================
    // P2-02 strict trigger matrix.
    //
    // Every one of the 32 named V81 triggers is pinned by a runtime test that
    // drives a REAL `Store` connection with raw SQL — the point being that
    // these are schema-level guards that must hold against any writer,
    // including one that bypasses every Store helper.
    //
    // Two disciplines make these tests non-vacuous rather than merely green:
    //
    // 1. **Refusals assert the exact trigger message.** `refused()` matches the
    //    `RAISE(ABORT,...)` text of the specific trigger under test, so a row
    //    rejected one layer down by a CHECK or a foreign key FAILS the test
    //    instead of silently satisfying it. Several of these fixtures were
    //    built by first observing which layer actually rejected the write.
    // 2. **Allowed edges are asserted too.** A trigger that refuses everything
    //    is exactly as broken as one that refuses nothing, so wherever the
    //    contract permits a same-state or forward edge, that edge is executed
    //    with `.expect(...)` and must succeed.
    // =====================================================================
    mod strict_trigger_matrix {
        use super::*;

        /// The provider turn this world is correlated to. Deliberately NOT a
        /// UUID: `boundary_kind='native_turn'` carries a provider-shaped turn
        /// ID, and the coupled CHECK only forces equality with the invocation
        /// UUID for `model_invocation` boundaries.
        const TURN: &str = "turn-p202-alpha";
        const TURN_START_ID: &str = "n:1";
        const REQUEST_ID: &str = "n:2";
        const REQUEST_KIND: &str = "tool_call";
        const EVENT_SEQUENCE: i64 = 7;

        fn request_digest() -> String {
            trigger_digest("d")
        }

        /// Assert a raw write was refused BY THE NAMED TRIGGER.
        ///
        /// Matching the exact abort text is what stops a test from passing for
        /// the wrong reason: if the trigger were dropped and a CHECK or FK
        /// caught the write instead, the fragment would not match and the
        /// assertion would fail — which is precisely the non-vacuity proof.
        #[track_caller]
        fn refused(result: rusqlite::Result<usize>, fragment: &str, what: &str) {
            let error = match result {
                Ok(rows) => panic!("{what}: expected a refusal, but {rows} row(s) were written"),
                Err(error) => error.to_string(),
            };
            assert!(
                error.contains(fragment),
                "{what}: expected the trigger abort {fragment:?}, got {error:?}"
            );
        }

        /// A fully scaffolded, VALID V81 world: an accepted message, a claimed
        /// then correlated and acknowledged AppServer delivery attempt, an open
        /// exact-turn gate, one provider-request ledger row, and one issued
        /// handler permit.
        ///
        /// Building this legitimately is itself evidence: every insert below
        /// passes through the same triggers under test, so a trigger that
        /// over-refuses breaks the fixture rather than hiding behind
        /// negative-only assertions.
        struct World {
            owner: Uuid,
            target: Uuid,
            message_id: Uuid,
            invocation_id: Uuid,
            event_id: i64,
            permit_id: Uuid,
        }

        /// Stage one of the scaffold: an accepted message driven to `injected`
        /// over a correlated attempt whose effect is merely POSSIBLE, not yet
        /// acknowledged.
        ///
        /// This split exists because SQLite leaves the firing order of several
        /// BEFORE UPDATE triggers on one table UNDEFINED. To pin a specific
        /// trigger, a test must reach a state where only that trigger can
        /// object — `agent_messages_v81_state_forward` is pinned from here,
        /// because at `injected` over an effect-possible attempt the
        /// `cas_coherence` conditions for `claimed` are all satisfiable.
        fn scaffold_injected(store: &Store, key: &str) -> World {
            let owner = Uuid::new_v4();
            let target = Uuid::new_v4();
            acceptance_session(store, owner);
            acceptance_session(store, target);

            let message_id = store
                .accept_agent_message(owner, None, &send_request(target, key, "hello"))
                .expect("acceptance")
                .receipt()
                .message_id;

            let invocation_id = Uuid::new_v4();
            store
                .conn
                .execute(
                    "INSERT INTO model_invocations
                     (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,provider,model,backend,model_tier,effort,trigger_source,session_id,created_at)
                     VALUES (?1,'agent.deliver','orchestration','foreground','paid_capable','admitted','running','Claude','claude-sonnet-5','Claude','premium','high','test',?2,?3)",
                    params![invocation_id.to_string(), target.to_string(), TRIGGER_TS],
                )
                .expect("seed delivery invocation");

            store
                .conn
                .execute(
                    "INSERT INTO conversation_events (session_id,event_type,content,sequence,created_at)
                     VALUES (?1,'assistant','ack',?2,?3)",
                    params![target.to_string(), EVENT_SEQUENCE, TRIGGER_TS],
                )
                .expect("seed acknowledging event");
            let event_id = store.conn.last_insert_rowid();

            // Attempt 1, AppServer-shaped so correlation custody is reachable
            // at all (`capability_kind='native_multi_turn'` is the only shape
            // whose correlation_state may leave `not_applicable`).
            store
                .conn
                .execute(
                    "INSERT INTO agent_message_delivery_attempts (
                         message_id, attempt_number, claim_token, delivery_boot_id,
                         logical_root_session_id, delivery_session_id,
                         delivery_session_generation, delivery_model_invocation_id,
                         provider_kind, capability_kind, boundary_kind,
                         claimed_at, claim_expires_at, correlation_state,
                         attempt_state, updated_at)
                     VALUES (?1,1,?2,?3,?4,?5,0,?6,'codex_app_server','native_multi_turn',
                             'native_turn',?7,?7,'correlation_pending','claimed',?7)",
                    params![
                        message_id.to_string(),
                        Uuid::new_v4().to_string(),
                        Uuid::new_v4().to_string(),
                        target.to_string(),
                        target.to_string(),
                        invocation_id.to_string(),
                        TRIGGER_TS,
                    ],
                )
                .expect("insert claimed attempt");

            insert_transition(store, message_id, 1, "queued", "claimed", Some(1))
                .expect("a legitimate attempt-linked claim transition must be admitted");
            store
                .conn
                .execute(
                    "UPDATE agent_messages
                        SET state='claimed', state_version=1, attempt_count=1,
                            current_attempt_number=1, updated_at=?2
                      WHERE id=?1",
                    params![message_id.to_string(), TRIGGER_TS],
                )
                .expect("a legitimate claim CAS must be admitted");

            // correlation_pending -> correlated, and admission recorded.
            store
                .conn
                .execute(
                    "UPDATE agent_message_delivery_attempts
                        SET correlation_state='correlated', boundary_value=?3,
                            correlation_source='app_server_response', correlated_at=?4,
                            admission_classification='admitted_effect_possible',
                            effect_classification='effect_possible',
                            attempt_state='effect_possible',
                            admission_recorded_at=?4, effect_possible_at=?4, updated_at=?4
                      WHERE message_id=?1 AND attempt_number=?2",
                    params![message_id.to_string(), 1i64, TURN, TRIGGER_TS],
                )
                .expect("a legitimate correlation fill must be admitted");

            insert_transition(store, message_id, 2, "claimed", "injected", Some(1))
                .expect("injection transition");
            store
                .conn
                .execute(
                    "UPDATE agent_messages
                        SET state='injected', state_version=2, updated_at=?2 WHERE id=?1",
                    params![message_id.to_string(), TRIGGER_TS],
                )
                .expect("a legitimate injection CAS must be admitted");

            World {
                owner,
                target,
                message_id,
                invocation_id,
                event_id,
                permit_id: Uuid::nil(),
            }
        }

        /// Stage two: acknowledge the attempt's effect, open the exact-turn
        /// gate, and land one provider-request row plus its handler permit.
        fn scaffold(store: &Store) -> World {
            let World {
                owner,
                target,
                message_id,
                invocation_id,
                event_id,
                ..
            } = scaffold_injected(store, "p202-matrix");

            // effect_possible -> effect_acknowledged is the ONE strengthening
            // the monotonic-evidence trigger carves out, plus its event triple.
            store
                .conn
                .execute(
                    "UPDATE agent_message_delivery_attempts
                        SET effect_classification='effect_acknowledged',
                            acknowledged_event_id=?3, acknowledged_event_session_id=?4,
                            acknowledged_event_sequence=?5, acknowledged_at=?6, updated_at=?6
                      WHERE message_id=?1 AND attempt_number=?2",
                    params![
                        message_id.to_string(),
                        1i64,
                        event_id,
                        target.to_string(),
                        EVENT_SEQUENCE,
                        TRIGGER_TS,
                    ],
                )
                .expect("a legitimate acknowledgement fill must be admitted");

            open_gate(store, message_id, invocation_id, 0).expect("open the exact-turn gate");
            insert_request(store, message_id, invocation_id, event_id, target)
                .expect("a fully scaffolded provider-request row must be admitted");

            let permit_id = Uuid::new_v4();
            issue_permit(
                store,
                message_id,
                invocation_id,
                permit_id,
                "tool_execution",
                "handler_started",
            )
            .expect("a legitimate handler permit must be admitted");

            World {
                owner,
                target,
                message_id,
                invocation_id,
                event_id,
                permit_id,
            }
        }

        fn open_gate(
            store: &Store,
            message_id: Uuid,
            invocation_id: Uuid,
            generation: i64,
        ) -> rusqlite::Result<usize> {
            store.conn.execute(
                "INSERT INTO agent_message_provider_turn_gates (
                     message_id, attempt_number, delivery_model_invocation_id,
                     provider_turn_id, gate_generation, gate_state, admission_sequence,
                     issued_permit_count, settled_permit_count, opened_at, updated_at)
                 VALUES (?1,1,?2,?3,?4,'open',0,0,0,?5,?5)",
                params![
                    message_id.to_string(),
                    invocation_id.to_string(),
                    TURN,
                    generation,
                    TRIGGER_TS,
                ],
            )
        }

        fn insert_request(
            store: &Store,
            message_id: Uuid,
            invocation_id: Uuid,
            event_id: i64,
            event_session: Uuid,
        ) -> rusqlite::Result<usize> {
            insert_request_as(
                store,
                message_id,
                invocation_id,
                event_id,
                event_session,
                REQUEST_ID,
                "tool_execution",
                TURN,
                0,
            )
        }

        /// The general provider-request insert. `request_id`, `turn`, and
        /// `generation` are parameterised so the BEFORE INSERT evidence guard
        /// can be probed off its happy path, and `handler_kind` because only
        /// `approval_presentation` rows may hold approval custody at all.
        #[allow(clippy::too_many_arguments)]
        fn insert_request_as(
            store: &Store,
            message_id: Uuid,
            invocation_id: Uuid,
            event_id: i64,
            event_session: Uuid,
            request_id: &str,
            handler_kind: &str,
            turn: &str,
            generation: i64,
        ) -> rusqlite::Result<usize> {
            store.conn.execute(
                "INSERT INTO agent_message_provider_request_effects (
                     message_id, attempt_number, turn_start_request_id, provider_turn_id,
                     provider_request_id, provider_request_kind, provider_request_digest,
                     delivery_model_invocation_id, turn_gate_generation,
                     evidence_event_id, evidence_event_session_id, evidence_event_sequence,
                     handler_kind, evidence_committed_at, created_at,
                     handler_phase, reply_phase, request_disposition, updated_at)
                 VALUES (?1,1,?2,?3,?4,?5,?6,?7,?12,?8,?9,?10,?13,?11,?11,
                         'evidence_committed','not_authorized','live',?11)",
                params![
                    message_id.to_string(),
                    TURN_START_ID,
                    turn,
                    request_id,
                    REQUEST_KIND,
                    request_digest(),
                    invocation_id.to_string(),
                    event_id,
                    event_session.to_string(),
                    EVENT_SEQUENCE,
                    TRIGGER_TS,
                    generation,
                    handler_kind,
                ],
            )
        }

        fn issue_permit(
            store: &Store,
            message_id: Uuid,
            invocation_id: Uuid,
            permit_id: Uuid,
            permit_kind: &str,
            request_phase: &str,
        ) -> rusqlite::Result<usize> {
            store.conn.execute(
                "INSERT INTO agent_message_provider_effect_permits (
                     permit_id, message_id, attempt_number, turn_start_request_id,
                     provider_turn_id, provider_request_id, provider_request_kind,
                     provider_request_digest, delivery_model_invocation_id,
                     turn_gate_generation, permit_kind, request_phase, permit_state,
                     executor_boot_id, external_join_id, issued_at, updated_at)
                 VALUES (?1,?2,1,?3,?4,?5,?6,?7,?8,0,?9,?10,'issued',?11,?12,?13,?13)",
                params![
                    permit_id.to_string(),
                    message_id.to_string(),
                    TURN_START_ID,
                    TURN,
                    REQUEST_ID,
                    REQUEST_KIND,
                    request_digest(),
                    invocation_id.to_string(),
                    permit_kind,
                    request_phase,
                    Uuid::new_v4().to_string(),
                    Uuid::new_v4().to_string(),
                    TRIGGER_TS,
                ],
            )
        }

        /// The scaffold itself is the first assertion: a legitimate world must
        /// be constructible through the live trigger set. If any trigger
        /// over-refuses, this fails before a single negative test runs.
        #[test]
        fn p202_a_fully_legitimate_v81_world_is_admitted_end_to_end() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            let counts: Vec<i64> = [
                "SELECT COUNT(*) FROM agent_messages",
                "SELECT COUNT(*) FROM agent_message_delivery_attempts",
                "SELECT COUNT(*) FROM agent_message_state_transitions",
                "SELECT COUNT(*) FROM agent_message_provider_turn_gates",
                "SELECT COUNT(*) FROM agent_message_provider_request_effects",
                "SELECT COUNT(*) FROM agent_message_provider_effect_permits",
            ]
            .iter()
            .map(|sql| store.conn.query_row(sql, [], |r| r.get(0)).expect("count"))
            .collect();
            // 3 transitions: version 0 acceptance, 1 claim, 2 injection.
            assert_eq!(counts, vec![1, 1, 3, 1, 1, 1], "scaffold shape");

            let (state, version, pointer): (String, i64, Option<i64>) = store
                .conn
                .query_row(
                    "SELECT state,state_version,current_attempt_number FROM agent_messages WHERE id=?1",
                    params![world.message_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("aggregate");
            assert_eq!((state.as_str(), version, pointer), ("injected", 2, Some(1)));

            // The whole world is referentially sound. Draining the pragma IS
            // the evidence; preparing it is not.
            let mut stmt = store
                .conn
                .prepare("PRAGMA foreign_key_check")
                .expect("prepare foreign_key_check");
            let violations: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .expect("run foreign_key_check")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("drain foreign_key_check");
            assert!(
                violations.is_empty(),
                "foreign_key_check reported violations: {violations:?}"
            );
        }

        /// Seal attempt 1 terminal with `acknowledged`, the one disposition
        /// that carries genuine effect evidence rather than a no-effect proof.
        fn seal_acknowledged(store: &Store, message_id: Uuid) {
            store
                .conn
                .execute(
                    "UPDATE agent_message_delivery_attempts
                        SET attempt_state='terminal', terminal_disposition='acknowledged',
                            settlement_authority='acknowledgement_store',
                            settlement_evidence_digest=?3,
                            settled_at=?4, terminal_at=?4, updated_at=?4
                      WHERE message_id=?1 AND attempt_number=?2",
                    params![
                        message_id.to_string(),
                        1i64,
                        trigger_digest("e"),
                        TRIGGER_TS
                    ],
                )
                .expect("sealing an acknowledged attempt is a legitimate forward edge");
        }

        // ==================================================================
        // Aggregate: agent_messages (6 triggers)
        // ==================================================================

        /// Trigger 1/32 — `agent_messages_v81_no_delete`.
        #[test]
        fn p202_agent_messages_v81_no_delete() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            refused(
                store.conn.execute(
                    "DELETE FROM agent_messages WHERE id=?1",
                    params![world.message_id.to_string()],
                ),
                "agent message history cannot be deleted",
                "deleting an accepted agent message",
            );
            // An unqualified sweep is refused for the same reason.
            refused(
                store.conn.execute("DELETE FROM agent_messages", []),
                "agent message history cannot be deleted",
                "sweeping the whole aggregate table",
            );

            let remaining: i64 = store
                .conn
                .query_row("SELECT COUNT(*) FROM agent_messages", [], |r| r.get(0))
                .expect("count");
            assert_eq!(remaining, 1, "the aggregate row survived every delete");
        }

        /// Trigger 2/32 — `agent_messages_v81_target_custody`.
        ///
        /// Covers BOTH halves of the dual insert truth (existing Session, and
        /// an exact live reservation tuple) in the allowed direction, plus the
        /// unknown target, the wrong-owner reservation, and the acceptance
        /// shape clause in the refused direction.
        #[test]
        fn p202_agent_messages_v81_target_custody() {
            let store = Store::open_in_memory().unwrap();
            let owner = Uuid::new_v4();
            let target = Uuid::new_v4();
            acceptance_session(&store, owner);
            acceptance_session(&store, target);

            // Raw insert helper: deliberately bypasses `accept_agent_message`
            // so the trigger, not the Rust writer, is what is under test.
            let insert = |id: Uuid,
                          owner: Uuid,
                          target: Uuid,
                          reservation: Option<Uuid>,
                          state: &str,
                          version: i64,
                          attempts: i64,
                          key: &str| {
                store.conn.execute(
                    "INSERT INTO agent_messages (
                         id, owner_session_id, target_session_id, target_spawn_request_id,
                         idempotency_digest, request_fingerprint, payload_digest, payload,
                         created_at, state, state_version, attempt_count, updated_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,'hi',?8,?9,?10,?11,?8)",
                    params![
                        id.to_string(),
                        owner.to_string(),
                        target.to_string(),
                        reservation.map(|r| r.to_string()),
                        trigger_digest(key),
                        trigger_digest("1"),
                        trigger_digest("2"),
                        TRIGGER_TS,
                        state,
                        version,
                        attempts,
                    ],
                )
            };

            // --- ALLOWED: the target already exists as a Session ------------
            insert(Uuid::new_v4(), owner, target, None, "queued", 0, 0, "a")
                .expect("Session-custody acceptance must be admitted");

            // --- REFUSED: the target is neither a Session nor a reservation --
            refused(
                insert(
                    Uuid::new_v4(),
                    owner,
                    Uuid::new_v4(),
                    None,
                    "queued",
                    0,
                    0,
                    "b",
                ),
                "V81 agent message target custody is not exactly one of Session or live reservation",
                "accepting mail for a target that does not exist",
            );

            // --- REFUSED: acceptance must start queued / version 0 / 0 attempts
            for (state, version, attempts, what) in [
                ("claimed", 0i64, 0i64, "accepting directly into claimed"),
                ("queued", 1, 0, "accepting at a nonzero state version"),
                ("queued", 0, 1, "accepting with a nonzero attempt count"),
            ] {
                refused(
                    insert(
                        Uuid::new_v4(),
                        owner,
                        target,
                        None,
                        state,
                        version,
                        attempts,
                        "c",
                    ),
                    "V81 agent message target custody is not exactly one of Session or live reservation",
                    what,
                );
            }

            // --- The reservation half of the dual truth ---------------------
            let child = Uuid::new_v4();
            let spawn_request = Uuid::new_v4();
            store
                .conn
                .execute(
                    "INSERT INTO agent_spawn_requests (
                         spawn_request_id, owner_session_id, idempotency_digest,
                         request_fingerprint, request_json, child_session_id, epic_id,
                         kind, state, reserved_at, updated_at, epic_spawn_ordinal)
                     VALUES (?1,?2,?3,?4,'{}',?5,?6,'Task','reserved',?7,?7,1)",
                    params![
                        spawn_request.to_string(),
                        owner.to_string(),
                        trigger_digest("9"),
                        trigger_digest("8"),
                        child.to_string(),
                        owner.to_string(),
                        TRIGGER_TS,
                    ],
                )
                .expect("seed a live reservation");

            // ALLOWED: the exact owner/request/child tuple, target still a
            // reserved UUID with no Session row of its own.
            insert(
                Uuid::new_v4(),
                owner,
                child,
                Some(spawn_request),
                "queued",
                0,
                0,
                "d",
            )
            .expect("reservation-custody acceptance must be admitted");

            // REFUSED: same reservation, but the child does not match.
            refused(
                insert(
                    Uuid::new_v4(),
                    owner,
                    Uuid::new_v4(),
                    Some(spawn_request),
                    "queued",
                    0,
                    0,
                    "e",
                ),
                "V81 agent message target custody is not exactly one of Session or live reservation",
                "a reservation whose child_session_id is not the target",
            );

            // REFUSED: same reservation and child, but a different owner.
            let stranger = Uuid::new_v4();
            acceptance_session(&store, stranger);
            refused(
                insert(
                    Uuid::new_v4(),
                    stranger,
                    child,
                    Some(spawn_request),
                    "queued",
                    0,
                    0,
                    "f",
                ),
                "V81 agent message target custody is not exactly one of Session or live reservation",
                "a reservation belonging to a different owner",
            );

            // REFUSED: a FAILED reservation is not a live one.
            let dead_child = Uuid::new_v4();
            let dead_request = Uuid::new_v4();
            store
                .conn
                .execute(
                    "INSERT INTO agent_spawn_requests (
                         spawn_request_id, owner_session_id, idempotency_digest,
                         request_fingerprint, request_json, child_session_id, epic_id,
                         kind, state, safe_error_class, reserved_at, updated_at, failed_at,
                         epic_spawn_ordinal)
                     VALUES (?1,?2,?3,?4,'{}',?5,?6,'Task','failed','spawn.denied',?7,?7,?7,2)",
                    params![
                        dead_request.to_string(),
                        owner.to_string(),
                        trigger_digest("7"),
                        trigger_digest("6"),
                        dead_child.to_string(),
                        owner.to_string(),
                        TRIGGER_TS,
                    ],
                )
                .expect("seed a failed reservation");
            refused(
                insert(
                    Uuid::new_v4(),
                    owner,
                    dead_child,
                    Some(dead_request),
                    "queued",
                    0,
                    0,
                    "0",
                ),
                "V81 agent message target custody is not exactly one of Session or live reservation",
                "a permanently failed reservation",
            );
        }

        /// Trigger 3/32 — `agent_messages_v81_identity_immutable`.
        #[test]
        fn p202_agent_messages_v81_identity_immutable() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);
            let id = world.message_id.to_string();

            // Every immutable acceptance-identity column, including the two
            // NULLABLE ones whose guard must use `IS NOT` rather than `!=`
            // (a `!=` comparison against NULL is NULL, i.e. never true, so a
            // NULL-to-value rewrite would slip straight through).
            let rewrites: [(&str, Box<dyn rusqlite::ToSql>); 10] = [
                ("id", Box::new(Uuid::new_v4().to_string())),
                ("owner_session_id", Box::new(world.target.to_string())),
                ("target_session_id", Box::new(world.owner.to_string())),
                ("idempotency_digest", Box::new(trigger_digest("9"))),
                ("request_fingerprint", Box::new(trigger_digest("8"))),
                ("payload_digest", Box::new(trigger_digest("7"))),
                ("payload", Box::new("tampered".to_string())),
                ("created_at", Box::new("2027-01-01T00:00:00.000000000Z")),
                (
                    "target_spawn_request_id",
                    Box::new(Uuid::new_v4().to_string()),
                ),
                ("expires_at", Box::new("2027-01-01T00:00:00.000000000Z")),
            ];
            for (column, value) in rewrites {
                refused(
                    store.conn.execute(
                        &format!("UPDATE agent_messages SET {column}=?2 WHERE id=?1"),
                        params![id, value],
                    ),
                    "V81 agent message acceptance identity and expiry are immutable",
                    &format!("rewriting immutable column {column}"),
                );
            }

            // ALLOWED: the mutable aggregate columns still move. Without this
            // the trigger could be guarding every column and still pass above.
            store
                .conn
                .execute(
                    "UPDATE agent_messages SET updated_at=?2 WHERE id=?1",
                    params![id, "2026-08-04T00:00:00.000000000Z"],
                )
                .expect("a mutable-column update must still be admitted");
        }

        /// Trigger 4/32 — `agent_messages_v81_state_forward`.
        ///
        /// `injected -> claimed` is the edge that isolates this trigger: at
        /// `injected` over an effect-possible attempt, EVERY `cas_coherence`
        /// clause for a `claimed` target is satisfiable (the transition row
        /// names the exact current attempt, the attempt exists, the count does
        /// not rewind), so `state_forward` is the only layer left that can
        /// object. Because SQLite leaves multi-trigger firing order undefined,
        /// that isolation — not source order — is what makes this assertion
        /// pin the intended trigger.
        #[test]
        fn p202_agent_messages_v81_state_forward() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold_injected(&store, "p202-forward");
            let id = world.message_id.to_string();

            // The transition row that would make `cas_coherence` content.
            insert_transition(&store, world.message_id, 3, "injected", "claimed", Some(1))
                .expect("a backward transition row is itself accepted; the aggregate edge is not");

            refused(
                store.conn.execute(
                    "UPDATE agent_messages
                        SET state='claimed', state_version=3, updated_at=?2 WHERE id=?1",
                    params![id, TRIGGER_TS],
                ),
                "V81 agent message state transition is not a legal forward edge",
                "driving injected -> claimed with an otherwise coherent CAS",
            );

            // ALLOWED: injected -> uncertain IS a legal forward edge, and over
            // an effect-possible attempt it satisfies CAS coherence too, so it
            // must actually succeed. This is the direction that proves the
            // trigger is not simply refusing every state change. A fresh world
            // is used because version 3 is already consumed above and
            // transitions are append-only and immutable.
            let world2 = scaffold_injected(&store, "p202-forward-ok");
            insert_transition(
                &store,
                world2.message_id,
                3,
                "injected",
                "uncertain",
                Some(1),
            )
            .expect("uncertainty transition");
            store
                .conn
                .execute(
                    "UPDATE agent_messages
                        SET state='uncertain', state_version=3, uncertain_at=?2, updated_at=?2
                      WHERE id=?1",
                    params![world2.message_id.to_string(), TRIGGER_TS],
                )
                .expect("injected -> uncertain must be admitted");

            // A terminal-ish frozen check: `uncertain` may only go to
            // acknowledged or failed, never back to injected.
            insert_transition(
                &store,
                world2.message_id,
                4,
                "uncertain",
                "injected",
                Some(1),
            )
            .expect("transition row");
            refused(
                store.conn.execute(
                    "UPDATE agent_messages
                        SET state='injected', state_version=4, updated_at=?2 WHERE id=?1",
                    params![world2.message_id.to_string(), TRIGGER_TS],
                ),
                "V81 agent message state transition is not a legal forward edge",
                "resurrecting uncertain -> injected",
            );
        }

        /// Trigger 5/32 — `agent_messages_v81_cas_coherence`.
        ///
        /// The existing `a_terminal_aggregate_state_requires_a_sealed_matching_attempt_disposition`
        /// and `a_transition_naming_a_different_attempt_than_the_current_pointer_is_refused`
        /// tests pin the terminal pointer/disposition join. This adds the
        /// version-arithmetic and required-transition-row halves.
        #[test]
        fn p202_agent_messages_v81_cas_coherence() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);
            let id = world.message_id.to_string();

            // REFUSED: a state change with NO transition row at the new
            // version. `injected -> acknowledged` is a legal forward edge, so
            // `state_forward` admits it and this trigger is the layer that
            // must reject it.
            seal_acknowledged(&store, world.message_id);
            refused(
                store.conn.execute(
                    "UPDATE agent_messages
                        SET state='acknowledged', state_version=3, acknowledged_at=?2, updated_at=?2
                      WHERE id=?1",
                    params![id, TRIGGER_TS],
                ),
                "V81 agent message CAS coherence violated",
                "an acknowledgement with no transition row",
            );

            // REFUSED: a version that skips, even with a transition row.
            insert_transition(
                &store,
                world.message_id,
                3,
                "injected",
                "acknowledged",
                Some(1),
            )
            .expect("acknowledgement transition at version 3");
            refused(
                store.conn.execute(
                    "UPDATE agent_messages
                        SET state='acknowledged', state_version=4, acknowledged_at=?2, updated_at=?2
                      WHERE id=?1",
                    params![id, TRIGGER_TS],
                ),
                "V81 agent message CAS coherence violated",
                "a state version that skips ahead",
            );

            // REFUSED: bumping the version without changing state at all.
            refused(
                store.conn.execute(
                    "UPDATE agent_messages SET state_version=3, updated_at=?2 WHERE id=?1",
                    params![id, TRIGGER_TS],
                ),
                "V81 agent message CAS coherence violated",
                "bumping the version with no matching same-state transition",
            );

            // REFUSED: the aggregate may not decrease its attempt count.
            refused(
                store.conn.execute(
                    "UPDATE agent_messages
                        SET state='acknowledged', state_version=3, attempt_count=0,
                            current_attempt_number=NULL, acknowledged_at=?2, updated_at=?2
                      WHERE id=?1",
                    params![id, TRIGGER_TS],
                ),
                "V81 agent message CAS coherence violated",
                "rewinding the attempt count",
            );

            // ALLOWED: same-state, same-version replay is read-only and must
            // pass without any transition row at all.
            store
                .conn
                .execute(
                    "UPDATE agent_messages SET updated_at=?2 WHERE id=?1",
                    params![id, "2026-08-04T00:00:00.000000000Z"],
                )
                .expect("same-state replay must be admitted");

            // ALLOWED: the exact +1 bump with its matching transition row.
            store
                .conn
                .execute(
                    "UPDATE agent_messages
                        SET state='acknowledged', state_version=3, acknowledged_at=?2, updated_at=?2
                      WHERE id=?1",
                    params![id, TRIGGER_TS],
                )
                .expect("a coherent +1 CAS with its transition row must be admitted");
        }

        // ==================================================================
        // P2-06d (V82) acceptance
        // ==================================================================

        /// V82 item (a) — the R7 structural backstop, in the DATABASE.
        ///
        /// R7 proved the narrowed rule was protected by PROSE plus exactly ONE
        /// test and by no database object at all: widening only the requeue
        /// writer's guard to `IN ('claimed','dispatching')` fired NO trigger,
        /// and the seal, the transition insert and the aggregate CAS all
        /// committed. This is the layer that now refuses it, by constraint
        /// name.
        ///
        /// The two ADMITTED cases are the point, not decoration: a guard that
        /// passes by refusing everything is not a guard. They pin the exact
        /// scope -- the dispatcher path and the pre-marker reconciler path both
        /// still work.
        #[test]
        fn p206d_v82_reconciler_no_effect_backstop_refuses_only_the_reconciler() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);
            let id = world.message_id.to_string();

            for attempt in [2i64, 3, 4] {
                insert_attempt(&store, &world, attempt, "correlation_pending")
                    .expect("a fresh claimed attempt");
            }
            // Attempts 2 and 3 pass the pre-dispatch marker and carry the
            // durable stamp; attempt 4 never leaves `claimed`.
            for attempt in [2i64, 3] {
                store
                    .conn
                    .execute(
                        "UPDATE agent_message_delivery_attempts
                            SET attempt_state='dispatching', dispatching_at=?3, updated_at=?3
                          WHERE message_id=?1 AND attempt_number=?2",
                        params![id, attempt, TRIGGER_TS],
                    )
                    .expect("claimed -> dispatching stamps the durable marker");
            }

            let seal = |attempt: i64, authority: &str| {
                store.conn.execute(
                    "UPDATE agent_message_delivery_attempts
                        SET attempt_state='terminal',
                            admission_classification='rejected_before_effect',
                            effect_classification='proved_no_effect',
                            terminal_disposition='proved_no_effect_requeue',
                            settlement_authority=?3,
                            settlement_evidence_digest=?4,
                            settled_at=?5, terminal_at=?5, updated_at=?5
                      WHERE message_id=?1 AND attempt_number=?2",
                    params![id, attempt, authority, trigger_digest("a"), TRIGGER_TS],
                )
            };

            // REFUSED: the restart reconciler runs when no dispatcher is alive,
            // so it can NEVER hold the provider's answer. A durable
            // `dispatching_at` means the send window had already opened and
            // no-effect is unprovable. STRANDING IS CORRECT here.
            refused(
                seal(2, "restart_reconciler"),
                "agent_message_attempts_v82_reconciler_no_effect_backstop",
                "the restart reconciler sealing a post-marker attempt proved-no-effect",
            );

            // ADMITTED: the dispatcher seals the SAME post-marker shape when it
            // holds a real RejectedBeforeEffect. A blanket
            // `dispatching_at IS NULL` rule would have broken this.
            seal(3, "dispatcher")
                .expect("ALLOWED: the dispatcher still seals a post-marker attempt");

            // ADMITTED: the reconciler on a PRE-marker attempt -- the narrowed
            // rule this backstop protects, not one it forbids.
            seal(4, "restart_reconciler")
                .expect("ALLOWED: the reconciler still seals a pre-marker attempt");
        }

        /// Drive a world whose CURRENT attempt is sealed `acknowledged` under
        /// the requested correlation custody, then return the aggregate CAS to
        /// `acknowledged`.
        fn cas_to_acknowledged_under_custody(custody: &str) -> rusqlite::Result<usize> {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);
            let id = world.message_id.to_string();

            // Correlation custody is AppServer-only, so `not_applicable` has to
            // arrive on a terminal-one-turn attempt -- which is exactly the
            // shape every non-AppServer provider in the system has.
            let (provider, capability, initial) = match custody {
                "not_applicable" => ("harness", "terminal_one_turn", "not_applicable"),
                _ => (
                    "codex_app_server",
                    "native_multi_turn",
                    "correlation_pending",
                ),
            };
            store
                .conn
                .execute(
                    "INSERT INTO agent_message_delivery_attempts (
                         message_id, attempt_number, claim_token, delivery_boot_id,
                         logical_root_session_id, delivery_session_id,
                         delivery_session_generation, delivery_model_invocation_id,
                         provider_kind, capability_kind, boundary_kind,
                         claimed_at, claim_expires_at, correlation_state,
                         attempt_state, updated_at)
                     VALUES (?1,2,?2,?3,?4,?5,0,?6,?7,?8,'native_turn',?9,?9,?10,'claimed',?9)",
                    params![
                        id,
                        Uuid::new_v4().to_string(),
                        Uuid::new_v4().to_string(),
                        world.target.to_string(),
                        world.target.to_string(),
                        world.invocation_id.to_string(),
                        provider,
                        capability,
                        TRIGGER_TS,
                        initial,
                    ],
                )
                .expect("a second attempt under the requested custody");

            match custody {
                "correlated" => {
                    store
                        .conn
                        .execute(
                            "UPDATE agent_message_delivery_attempts
                                SET correlation_state='correlated', boundary_value=?3,
                                    correlation_source='provider_response',
                                    correlated_at=?4, updated_at=?4
                              WHERE message_id=?1 AND attempt_number=?2",
                            params![id, 2i64, TURN, TRIGGER_TS],
                        )
                        .expect("correlation_pending -> correlated");
                }
                "sealed_live_uncertain" => {
                    store
                        .conn
                        .execute(
                            "UPDATE agent_message_delivery_attempts
                                SET correlation_state='sealed_live_uncertain',
                                    evidence_suppression_class='response_lost',
                                    evidence_sealed_at=?3, updated_at=?3
                              WHERE message_id=?1 AND attempt_number=?2",
                            params![id, 2i64, TRIGGER_TS],
                        )
                        .expect("correlation_pending -> sealed_live_uncertain");
                }
                _ => {}
            }

            // Attempt 2 needs its OWN acknowledging event: the ack-event index
            // is UNIQUE, so one provider event can never acknowledge two
            // attempts.
            store
                .conn
                .execute(
                    "INSERT INTO conversation_events (session_id,event_type,content,sequence,created_at)
                     VALUES (?1,'assistant','ack',?2,?3)",
                    params![world.target.to_string(), EVENT_SEQUENCE + 1, TRIGGER_TS],
                )
                .expect("seed a second acknowledging event");
            let event_id = store.conn.last_insert_rowid();

            // claimed -> effect_possible -> terminal(acknowledged). Evidence
            // fills monotonically, so the intermediate step is required.
            store
                .conn
                .execute(
                    "UPDATE agent_message_delivery_attempts
                        SET attempt_state='effect_possible',
                            admission_classification='admitted_effect_possible',
                            effect_classification='effect_possible',
                            admission_recorded_at=?3, effect_possible_at=?3, updated_at=?3
                      WHERE message_id=?1 AND attempt_number=?2",
                    params![id, 2i64, TRIGGER_TS],
                )
                .expect("claimed -> effect_possible");
            store
                .conn
                .execute(
                    "UPDATE agent_message_delivery_attempts
                        SET attempt_state='terminal', effect_classification='effect_acknowledged',
                            terminal_disposition='acknowledged',
                            settlement_authority='acknowledgement_store',
                            settlement_evidence_digest=?3,
                            acknowledged_event_id=?4, acknowledged_event_session_id=?5,
                            acknowledged_event_sequence=?6,
                            acknowledged_at=?7, settled_at=?7, terminal_at=?7, updated_at=?7
                      WHERE message_id=?1 AND attempt_number=?2",
                    params![
                        id,
                        2i64,
                        trigger_digest("b"),
                        event_id,
                        world.target.to_string(),
                        EVENT_SEQUENCE + 1,
                        TRIGGER_TS,
                    ],
                )
                .expect("effect_possible -> terminal(acknowledged)");

            // Point the aggregate at attempt 2 without touching state or
            // version, which the CAS trigger treats as read-only replay. The
            // pointer is coupled to `attempt_count` by CHECK, so both move.
            store
                .conn
                .execute(
                    "UPDATE agent_messages SET attempt_count=2, current_attempt_number=2
                      WHERE id=?1",
                    params![id],
                )
                .expect("repoint the current attempt");
            insert_transition(
                &store,
                world.message_id,
                3,
                "injected",
                "acknowledged",
                Some(2),
            )
            .expect("acknowledgement transition at version 3");

            store.conn.execute(
                "UPDATE agent_messages
                    SET state='acknowledged', state_version=3, acknowledged_at=?2, updated_at=?2
                  WHERE id=?1",
                params![id, TRIGGER_TS],
            )
        }

        /// V82 item (b) — GAP 1: an attempt durably sealed
        /// `sealed_live_uncertain` can never be acknowledged.
        ///
        /// The NEGATIVE formulation is load-bearing and this test is what pins
        /// it. Gating on `correlation_state='correlation_pending'` -- the
        /// obvious positive reading -- would have refused BOTH admitted cases
        /// below, and `not_applicable` is EVERY non-AppServer provider in the
        /// system. That trap was verified real by R7.
        #[test]
        fn p206d_v82_sealed_live_uncertain_attempt_can_never_be_acknowledged() {
            // ADMITTED: every non-AppServer provider. Permanently
            // `not_applicable`, and acknowledgement must keep working.
            cas_to_acknowledged_under_custody("not_applicable")
                .expect("ALLOWED: a non-AppServer provider still acknowledges");

            // ADMITTED: an AppServer attempt that genuinely correlated.
            cas_to_acknowledged_under_custody("correlated")
                .expect("ALLOWED: a correlated AppServer attempt still acknowledges");

            // REFUSED: sealing is irreversible and permits only lifecycle
            // settlement -- a late response can never ack it.
            refused(
                cas_to_acknowledged_under_custody("sealed_live_uncertain"),
                "V81 agent message CAS coherence violated",
                "acknowledging an attempt already sealed live-uncertain",
            );
        }

        /// Trigger 6/32 — `agent_messages_v81_requeue_requires_no_effect`.
        #[test]
        fn p202_agent_messages_v81_requeue_requires_no_effect() {
            let store = Store::open_in_memory().unwrap();
            let owner = Uuid::new_v4();
            let target = Uuid::new_v4();
            acceptance_owner(&store, owner);
            acceptance_session(&store, target);
            let message_id =
                claimed_message_with_unsealed_attempt(&store, owner, target, "p202-requeue");
            let id = message_id.to_string();

            insert_transition(&store, message_id, 2, "claimed", "queued", Some(1))
                .expect("requeue transition");

            // REFUSED: the current attempt is still UNSEALED. Lease expiry
            // alone is never proof that no effect occurred.
            refused(
                store.conn.execute(
                    "UPDATE agent_messages
                        SET state='queued', state_version=2, updated_at=?2 WHERE id=?1",
                    params![id, TRIGGER_TS],
                ),
                "V81 agent message requeue requires a sealed proved-no-effect attempt",
                "requeueing over an unsealed in-flight attempt",
            );

            // REFUSED: sealed, but with a disposition that does NOT prove
            // absence of effect — `unsupported` seals the attempt and satisfies
            // every CHECK, yet must not license a redelivery.
            seal_attempt(&store, message_id, 1, "unsupported");
            refused(
                store.conn.execute(
                    "UPDATE agent_messages
                        SET state='queued', state_version=2, updated_at=?2 WHERE id=?1",
                    params![id, TRIGGER_TS],
                ),
                "V81 agent message requeue requires a sealed proved-no-effect attempt",
                "requeueing over an attempt sealed `unsupported`",
            );

            // ALLOWED: a genuinely sealed proved_no_effect_requeue attempt.
            let clean = Uuid::new_v4();
            acceptance_session(&store, clean);
            let ok_id =
                claimed_message_with_unsealed_attempt(&store, owner, clean, "p202-requeue-ok");
            seal_attempt(&store, ok_id, 1, "proved_no_effect_requeue");
            insert_transition(&store, ok_id, 2, "claimed", "queued", Some(1))
                .expect("requeue transition");
            store
                .conn
                .execute(
                    "UPDATE agent_messages
                        SET state='queued', state_version=2, updated_at=?2 WHERE id=?1",
                    params![ok_id.to_string(), TRIGGER_TS],
                )
                .expect("requeue over a sealed proved-no-effect attempt must be admitted");
        }

        // ==================================================================
        // Delivery attempts: agent_message_delivery_attempts (6 triggers)
        // ==================================================================

        /// Insert an extra attempt row directly. No trigger guards attempt
        /// INSERT (all six attempt triggers are DELETE/UPDATE), so this
        /// exercises only the coupled CHECKs — which is exactly why the
        /// UPDATE-side guards below carry the whole burden.
        fn insert_attempt(
            store: &Store,
            world: &World,
            attempt_number: i64,
            correlation_state: &str,
        ) -> rusqlite::Result<usize> {
            store.conn.execute(
                "INSERT INTO agent_message_delivery_attempts (
                     message_id, attempt_number, claim_token, delivery_boot_id,
                     logical_root_session_id, delivery_session_id,
                     delivery_session_generation, delivery_model_invocation_id,
                     provider_kind, capability_kind, boundary_kind,
                     claimed_at, claim_expires_at, correlation_state,
                     attempt_state, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,0,?7,'codex_app_server','native_multi_turn',
                         'native_turn',?8,?8,?9,'claimed',?8)",
                params![
                    world.message_id.to_string(),
                    attempt_number,
                    Uuid::new_v4().to_string(),
                    Uuid::new_v4().to_string(),
                    world.target.to_string(),
                    world.target.to_string(),
                    world.invocation_id.to_string(),
                    TRIGGER_TS,
                    correlation_state,
                ],
            )
        }

        /// Trigger 7/32 — `agent_message_attempts_v81_no_delete`.
        #[test]
        fn p202_agent_message_attempts_v81_no_delete() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            for (sql, what) in [
                (
                    "DELETE FROM agent_message_delivery_attempts WHERE message_id=?1",
                    "deleting one attempt",
                ),
                (
                    "DELETE FROM agent_message_delivery_attempts WHERE ?1 IS NOT NULL",
                    "sweeping every attempt",
                ),
            ] {
                refused(
                    store
                        .conn
                        .execute(sql, params![world.message_id.to_string()]),
                    "agent message delivery attempts are append-only",
                    what,
                );
            }

            let remaining: i64 = store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM agent_message_delivery_attempts",
                    [],
                    |r| r.get(0),
                )
                .expect("count");
            assert_eq!(remaining, 1, "the attempt row survived every delete");
        }

        /// Trigger 8/32 — `agent_message_attempts_v81_identity_immutable`.
        #[test]
        fn p202_agent_message_attempts_v81_identity_immutable() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            // Every column of the frozen claim identity.
            let rewrites: [(&str, Box<dyn rusqlite::ToSql>); 13] = [
                ("message_id", Box::new(Uuid::new_v4().to_string())),
                ("attempt_number", Box::new(2i64)),
                ("claim_token", Box::new(Uuid::new_v4().to_string())),
                ("delivery_boot_id", Box::new(Uuid::new_v4().to_string())),
                (
                    "logical_root_session_id",
                    Box::new(Uuid::new_v4().to_string()),
                ),
                ("delivery_session_id", Box::new(world.owner.to_string())),
                ("delivery_session_generation", Box::new(9i64)),
                (
                    "delivery_model_invocation_id",
                    Box::new(Uuid::new_v4().to_string()),
                ),
                ("provider_kind", Box::new("harness")),
                ("capability_kind", Box::new("terminal_one_turn")),
                ("boundary_kind", Box::new("model_invocation")),
                ("claimed_at", Box::new("2027-01-01T00:00:00.000000000Z")),
                (
                    "claim_expires_at",
                    Box::new("2027-01-01T00:00:00.000000000Z"),
                ),
            ];
            for (column, value) in rewrites {
                refused(
                    store.conn.execute(
                        &format!(
                            "UPDATE agent_message_delivery_attempts SET {column}=?2
                             WHERE message_id=?1 AND attempt_number=1"
                        ),
                        params![world.message_id.to_string(), value],
                    ),
                    "V81 delivery attempt claim identity is immutable",
                    &format!("rewriting immutable claim column {column}"),
                );
            }

            // ALLOWED: a mutable evidence column still moves.
            store
                .conn
                .execute(
                    "UPDATE agent_message_delivery_attempts SET updated_at=?2
                     WHERE message_id=?1 AND attempt_number=1",
                    params![
                        world.message_id.to_string(),
                        "2026-08-04T00:00:00.000000000Z"
                    ],
                )
                .expect("a mutable-column update must still be admitted");
        }

        /// A message accepted and CLAIMED, stopping short of any dispatch.
        ///
        /// `scaffold_injected` drives straight past `claimed` to
        /// `effect_possible`, so it cannot exercise the pre-dispatch marker at
        /// all. This stops where the marker actually applies.
        fn scaffold_claimed(store: &Store, key: &str) -> (Uuid, Uuid) {
            scaffold_claimed_expiring(store, key, None)
        }

        /// An accepted message carrying a durable `expires_at`, never claimed.
        ///
        /// `expires_at` is frozen at acceptance by
        /// `agent_messages_v81_acceptance_identity_immutable`, so a test cannot
        /// back-date it afterwards — it has to be supplied here.
        fn accept_expiring(
            store: &Store,
            key: &str,
            expires_at: Option<chrono::DateTime<Utc>>,
        ) -> (Uuid, Uuid) {
            let owner = Uuid::new_v4();
            let target = Uuid::new_v4();
            acceptance_session(store, owner);
            acceptance_session(store, target);
            let mut request = send_request(target, key, "hello");
            request.expires_at = expires_at;
            let message_id = store
                .accept_agent_message(owner, None, &request)
                .expect("acceptance")
                .receipt()
                .message_id;
            (message_id, target)
        }

        /// As [`scaffold_claimed`], optionally carrying a durable expiry.
        fn scaffold_claimed_expiring(
            store: &Store,
            key: &str,
            expires_at: Option<chrono::DateTime<Utc>>,
        ) -> (Uuid, Uuid) {
            let owner = Uuid::new_v4();
            let target = Uuid::new_v4();
            acceptance_session(store, owner);
            acceptance_session(store, target);

            let mut request = send_request(target, key, "hello");
            request.expires_at = expires_at;
            let message_id = store
                .accept_agent_message(owner, None, &request)
                .expect("acceptance")
                .receipt()
                .message_id;

            let invocation_id = Uuid::new_v4();
            store
                .conn
                .execute(
                    "INSERT INTO model_invocations
                     (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,provider,model,backend,model_tier,effort,trigger_source,session_id,created_at)
                     VALUES (?1,'agent.deliver','orchestration','foreground','paid_capable','admitted','running','Claude','claude-sonnet-5','Claude','premium','high','test',?2,?3)",
                    params![invocation_id.to_string(), target.to_string(), TRIGGER_TS],
                )
                .expect("seed delivery invocation");

            store
                .conn
                .execute(
                    "INSERT INTO agent_message_delivery_attempts (
                         message_id, attempt_number, claim_token, delivery_boot_id,
                         logical_root_session_id, delivery_session_id,
                         delivery_session_generation, delivery_model_invocation_id,
                         provider_kind, capability_kind, boundary_kind,
                         claimed_at, claim_expires_at, correlation_state,
                         attempt_state, updated_at)
                     VALUES (?1,1,?2,?3,?4,?5,0,?6,'codex_app_server','native_multi_turn',
                             'native_turn',?7,?7,'correlation_pending','claimed',?7)",
                    params![
                        message_id.to_string(),
                        Uuid::new_v4().to_string(),
                        Uuid::new_v4().to_string(),
                        target.to_string(),
                        target.to_string(),
                        invocation_id.to_string(),
                        TRIGGER_TS,
                    ],
                )
                .expect("insert claimed attempt");

            insert_transition(store, message_id, 1, "queued", "claimed", Some(1))
                .expect("a legitimate attempt-linked claim transition must be admitted");
            store
                .conn
                .execute(
                    "UPDATE agent_messages
                        SET state='claimed', state_version=1, attempt_count=1,
                            current_attempt_number=1, updated_at=?2
                      WHERE id=?1",
                    params![message_id.to_string(), TRIGGER_TS],
                )
                .expect("a legitimate claim CAS must be admitted");

            (message_id, target)
        }

        /// The PRE-DISPATCH MARKER's durable constraints (H21-P2-R5-001).
        ///
        /// The marker is only worth anything if the database refuses to let it
        /// lie. Each assertion below corresponds to a way a false marker would
        /// make P2-06's recovery unsound again:
        ///
        /// * a `dispatching` row with no stamp, or a stamp on a still-`claimed`
        ///   row, would be a marker that half-exists — the state and the stamp
        ///   must agree or neither can be trusted;
        /// * a restamped `dispatching_at` would move the window's opening time
        ///   after the fact;
        /// * a rewind out of `dispatching` would let an attempt that may have
        ///   sent look like one that provably did not, which is exactly the
        ///   redelivery this slice exists to prevent.
        #[test]
        fn p202_agent_message_attempts_v81_dispatching_marker_is_durable() {
            let store = Store::open_in_memory().unwrap();
            let (message_id, _) = scaffold_claimed(&store, "p202-dispatching");
            let id = message_id.to_string();
            let update = |set: &str, value: Box<dyn rusqlite::ToSql>| {
                store.conn.execute(
                    &format!(
                        "UPDATE agent_message_delivery_attempts SET {set}=?2
                         WHERE message_id=?1 AND attempt_number=1"
                    ),
                    params![id, value],
                )
            };

            // REFUSED: the state without its stamp. Isolated deliberately —
            // this moves attempt_state alone, so the forward-only trigger is
            // satisfied (claimed -> dispatching is legal) and only the CHECK
            // can object.
            refused(
                update("attempt_state", Box::new("dispatching")),
                "CHECK constraint failed",
                "marking dispatching without stamping dispatching_at",
            );

            // REFUSED: the stamp without the state.
            refused(
                update("dispatching_at", Box::new(TRIGGER_TS)),
                "CHECK constraint failed",
                "stamping dispatching_at on a still-claimed attempt",
            );

            // ALLOWED: state and stamp together, which is what the delivery
            // path's marker transaction writes.
            store
                .conn
                .execute(
                    "UPDATE agent_message_delivery_attempts
                        SET attempt_state='dispatching', dispatching_at=?2, updated_at=?2
                      WHERE message_id=?1 AND attempt_number=1",
                    params![id, TRIGGER_TS],
                )
                .expect("the legitimate claimed -> dispatching marker must be admitted");

            // REFUSED: the stamp is fill-once, so the window's opening time
            // cannot be moved after the fact.
            refused(
                update("dispatching_at", Box::new("2027-01-01T00:00:00.000000000Z")),
                "V81 delivery attempt evidence is fill-once and forward-only",
                "restamping a filled dispatching_at",
            );

            // REFUSED: no rewind out of the marker. `claimed` is the dangerous
            // one — it is the state P2-06 is entitled to read as proof of no
            // effect, so a rewind would license redelivering a message that may
            // already have been sent.
            for backward in ["claimed"] {
                refused(
                    update("attempt_state", Box::new(backward)),
                    "V81 delivery attempt evidence is fill-once and forward-only",
                    &format!("rewinding attempt_state dispatching -> {backward}"),
                );
            }

            // ALLOWED: the forward edge TX-C actually takes after a successful
            // dispatch. Asserted so a graph that refused it would fail here
            // rather than silently stranding every delivered message.
            store
                .conn
                .execute(
                    "UPDATE agent_message_delivery_attempts
                        SET attempt_state='effect_possible',
                            admission_classification='admitted_effect_possible',
                            effect_classification='effect_possible',
                            admission_recorded_at=?2, effect_possible_at=?2, updated_at=?2
                      WHERE message_id=?1 AND attempt_number=1",
                    params![id, TRIGGER_TS],
                )
                .expect("the legitimate dispatching -> effect_possible edge must be admitted");
        }

        /// `mark_agent_message_attempt_dispatching` opens the window ONCE.
        ///
        /// A second successful call would restamp `dispatching_at` and move the
        /// recorded opening time of a window that is already open, so the guard
        /// must make the retry an error rather than a silent no-op — a no-op
        /// would let a caller believe it had freshly marked an attempt that had
        /// in fact already been handed to the provider.
        #[test]
        fn the_pre_dispatch_marker_opens_its_window_exactly_once() {
            let store = Store::open_in_memory().unwrap();
            let (message_id, target) = scaffold_claimed(&store, "p202-marker-once");
            let fence = MessageAttemptFenceV1 {
                message_id,
                attempt_number: 1,
                claim_token: Uuid::new_v4(),
                delivery_boot_id: Uuid::new_v4(),
                delivery_session_id: target,
                delivery_session_generation: 0,
                delivery_model_invocation_id: Uuid::new_v4(),
            };

            store
                .mark_agent_message_attempt_dispatching(&fence)
                .expect("the first marker on a claimed attempt must commit");

            let (state, stamp): (String, Option<String>) = store
                .conn
                .query_row(
                    "SELECT attempt_state, dispatching_at
                       FROM agent_message_delivery_attempts
                      WHERE message_id=?1 AND attempt_number=1",
                    params![message_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("read the marked attempt");
            assert_eq!(state, AttemptStateV1::Dispatching.as_str());
            assert!(
                stamp.is_some(),
                "the marker must stamp when the send window opened"
            );

            store
                .mark_agent_message_attempt_dispatching(&fence)
                .expect_err("a second marker on an already-dispatching attempt must be refused");
        }

        /// **P2-06a — THE NARROWED RULE. This test exists to FAIL if anyone
        /// widens it.**
        ///
        /// A `dispatching` attempt on a foreign boot with no recorded admission
        /// is the case where a paid model turn MAY ALREADY HAVE GONE OUT: the
        /// marker commits before the send, so reaching `dispatching` means the
        /// send was attempted, and TX-C's record lands only afterwards.
        /// Requeuing it would redeliver an already-sent paid model turn — the
        /// exact defect H21-P2-R5-001 (HIGH) withdrew.
        ///
        /// Both encodings of the rule are asserted, because either one alone
        /// could be widened in isolation:
        ///   1. the classifier `CRASHED_AGENT_MESSAGE_ATTEMPT_SQL` must answer
        ///      `Uncertain`, and
        ///   2. the requeue WRITER must REFUSE the row even when called
        ///      directly, so a widened classifier still cannot requeue it.
        #[test]
        fn a_dispatching_row_with_a_foreign_boot_is_uncertain_and_never_requeues() {
            let store = Store::open_in_memory().unwrap();
            let (message_id, _) = scaffold_claimed(&store, "narrowed-rule");
            let live_boot = Uuid::new_v4();

            // Open the send window exactly as the delivery path does.
            store
                .conn
                .execute(
                    "UPDATE agent_message_delivery_attempts
                        SET attempt_state='dispatching', dispatching_at=?2, updated_at=?2
                      WHERE message_id=?1 AND attempt_number=1",
                    params![message_id.to_string(), TRIGGER_TS],
                )
                .expect("open the pre-dispatch window");

            // 1. The classifier must answer UNCERTAIN, never a requeue.
            let crashed = store
                .list_crashed_agent_message_attempts_page_v1(live_boot, None)
                .expect("classify crashed attempts");
            let row = crashed
                .attempts
                .iter()
                .find(|row| row.message_id == message_id)
                .expect("a dispatching attempt on a dead boot is still live and must be surfaced");
            assert_eq!(
                row.attempt_state,
                AttemptStateV1::Dispatching,
                "precondition: the attempt is past the marker"
            );
            assert_eq!(
                row.verdict,
                CrashRecoveryVerdictV1::Uncertain,
                "WIDENED NARROWED RULE: a `dispatching` row may already have sent a paid \
                 model turn. Classifying it as proved-no-effect licenses redelivery \
                 (H21-P2-R5-001). Lease expiry, timeouts, and process death are NEVER \
                 proof of no effect."
            );

            // 2. The WRITER must refuse it too, even called directly.
            let requeue =
                store.requeue_crashed_agent_message_attempt_v1(message_id, 1, live_boot, live_boot);
            // R10 L-2: record WHICH layer refused, not merely that something did.
            // Before V82 this guard WAS the last line of defence; now the named
            // CHECK `agent_message_attempts_v82_reconciler_no_effect_backstop` is,
            // and the guard is the first. If this ever starts failing with the
            // CHECK's message instead, the guard has been widened and the V82
            // backstop is all that still stands between us and redelivering a
            // paid model turn.
            let requeue_err = requeue
                .expect_err(
                    "WIDENED NARROWED RULE: the requeue writer accepted a `dispatching` row. \
                     Its `attempt_state='claimed'` guard is the FIRST line of defence against \
                     redelivering an already-sent paid model turn; the V82 backstop CHECK is \
                     the second.",
                )
                .to_string();
            assert!(
                !requeue_err.contains("agent_message_attempts_v82_reconciler_no_effect_backstop"),
                "the writer's own guard should refuse first; the V82 backstop CHECK fired \
                 instead, which means the guard was widened: {requeue_err}"
            );

            // 3. Nothing moved: no requeue, no seal.
            let (state, attempt_state, disposition): (String, String, Option<String>) = store
                .conn
                .query_row(
                    "SELECT m.state, a.attempt_state, a.terminal_disposition
                       FROM agent_messages m
                       JOIN agent_message_delivery_attempts a
                         ON a.message_id=m.id AND a.attempt_number=1
                      WHERE m.id=?1",
                    params![message_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("read back");
            assert_eq!(
                state, "claimed",
                "the refused requeue must not have moved the aggregate"
            );
            assert_eq!(
                attempt_state, "dispatching",
                "the refused requeue must not have sealed the attempt"
            );
            assert_eq!(
                disposition, None,
                "no disposition may be invented for an uncertain attempt"
            );

            // 4. The correct answer — uncertain — is reachable, retains custody,
            //    and does NOT seal the attempt terminal.
            store
                .mark_crashed_agent_message_attempt_uncertain_v1(
                    message_id,
                    1,
                    AgentMessageStateV1::Claimed,
                    live_boot,
                    live_boot,
                )
                .expect("a dispatching crash must be recordable as uncertain");
            let (state, attempt_state, disposition): (String, String, Option<String>) = store
                .conn
                .query_row(
                    "SELECT m.state, a.attempt_state, a.terminal_disposition
                       FROM agent_messages m
                       JOIN agent_message_delivery_attempts a
                         ON a.message_id=m.id AND a.attempt_number=1
                      WHERE m.id=?1",
                    params![message_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("read back");
            assert_eq!(state, "uncertain");
            assert_eq!(
                attempt_state, "effect_possible",
                "uncertainty is retained custody: conservative effect-possible evidence, \
                 NOT a terminal seal"
            );
            assert_eq!(
                disposition, None,
                "an uncertain attempt must NOT be sealed — sealing it would erase the \
                 uncertainty this state exists to record"
            );
        }

        /// **R11 LOW-4 — the uncertainty writer's boot fence, now symmetric with
        /// the requeue writer's.**
        ///
        /// `requeue_crashed_agent_message_attempt_v1` has always re-encoded
        /// `delivery_boot_id != ?` at its point of write.
        /// `mark_crashed_agent_message_attempt_uncertain_v1` had no equivalent, so
        /// the narrowed rule's defence-in-depth was present on one crash writer
        /// and missing on its sibling. Unreachable through the reconciler — the
        /// classifier already excludes live-boot rows, and the whole pass is
        /// serialised by the process-wide store mutex — but P2-06c is the first
        /// code to call this writer at all, and the next caller should not have to
        /// discover the asymmetry.
        ///
        /// Both directions are asserted here, because a guard is only correct if
        /// it refuses the right rows AND still admits the rest:
        ///
        ///   1. an attempt carrying the LIVE `delivery_boot_id` is REFUSED, and
        ///      nothing about it moves; and
        ///   2. an attempt on a FOREIGN boot is still admitted — proving the
        ///      change is a strict NARROWING and not a refuse-everything
        ///      "fix" that would make crash recovery useless.
        #[test]
        fn the_uncertainty_writer_refuses_an_attempt_the_live_incarnation_owns() {
            let store = Store::open_in_memory().unwrap();

            // ---- 1. a row this incarnation OWNS ---------------------------
            let (live_row, _) = scaffold_claimed(&store, "low4-live-boot");

            // The scaffold stamps a random `delivery_boot_id`, and a test CANNOT
            // re-stamp it onto another incarnation: V81 makes the claim identity
            // immutable by trigger ("V81 delivery attempt claim identity is
            // immutable"). That is a stronger guarantee than "no writer updates
            // it" — the schema refuses. So this test approaches the scenario from
            // the other side: read the identity the row actually carries and let
            // THAT be the live incarnation. The fence compares exactly these two
            // values, so the two framings are the same scenario.
            let owning_boot: Uuid = store
                .conn
                .query_row(
                    "SELECT delivery_boot_id FROM agent_message_delivery_attempts
                      WHERE message_id=?1 AND attempt_number=1",
                    params![live_row.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .map(|raw| Uuid::parse_str(&raw).expect("a stored boot id is a uuid"))
                .expect("read the attempt's own delivery identity");

            // `dispatching` is exactly the case the uncertainty writer exists
            // for, so it is the row this writer would most want to act on.
            store
                .conn
                .execute(
                    "UPDATE agent_message_delivery_attempts
                        SET attempt_state='dispatching', dispatching_at=?2, updated_at=?2
                      WHERE message_id=?1 AND attempt_number=1",
                    params![live_row.to_string(), TRIGGER_TS],
                )
                .expect("open the pre-dispatch window");

            let crashed = store
                .list_crashed_agent_message_attempts_page_v1(owning_boot, None)
                .expect("classify crashed attempts");
            assert!(
                crashed
                    .attempts
                    .iter()
                    .all(|row| row.message_id != live_row),
                "precondition: the classifier already excludes an attempt carrying the \
                 live delivery_boot_id, which is why this writer's own fence is \
                 defence-in-depth rather than the only thing standing here"
            );

            let refused = store
                .mark_crashed_agent_message_attempt_uncertain_v1(
                    live_row,
                    1,
                    AgentMessageStateV1::Claimed,
                    owning_boot,
                    owning_boot,
                )
                .expect_err(
                    "the uncertainty writer accepted an attempt owned by the LIVE \
                     incarnation. Recording uncertainty about a delivery that is running \
                     right now destroys the admission evidence that delivery is about to \
                     write, and strands a message whose provider effect is still \
                     knowable exactly.",
                )
                .to_string();
            assert!(
                refused.contains("FOREIGN"),
                "the writer's own boot fence should refuse first, naming why; got: {refused}"
            );

            let (state, attempt_state, admission): (String, String, Option<String>) = store
                .conn
                .query_row(
                    "SELECT m.state, a.attempt_state, a.admission_classification
                       FROM agent_messages m
                       JOIN agent_message_delivery_attempts a
                         ON a.message_id=m.id AND a.attempt_number=1
                      WHERE m.id=?1",
                    params![live_row.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("read back");
            assert_eq!(
                state, "claimed",
                "the refused row's aggregate must not move"
            );
            assert_eq!(
                attempt_state, "dispatching",
                "the refused row's attempt must not move: the live delivery still owns it"
            );
            assert_eq!(
                admission, None,
                "no conservative effect-possible fill may be invented for a row this \
                 incarnation is still delivering"
            );

            // ---- 2. STRICT NARROWING: a foreign-boot row still works ------
            // A fresh incarnation id, foreign to whatever the scaffold stamped.
            let live_boot = Uuid::new_v4();
            let (foreign_row, _) = scaffold_claimed(&store, "low4-foreign-boot");
            store
                .conn
                .execute(
                    "UPDATE agent_message_delivery_attempts
                        SET attempt_state='dispatching', dispatching_at=?2, updated_at=?2
                      WHERE message_id=?1 AND attempt_number=1",
                    params![foreign_row.to_string(), TRIGGER_TS],
                )
                .expect("open the pre-dispatch window");
            store
                .mark_crashed_agent_message_attempt_uncertain_v1(
                    foreign_row,
                    1,
                    AgentMessageStateV1::Claimed,
                    live_boot,
                    live_boot,
                )
                .expect(
                    "the boot fence must be a strict NARROWING: a `dispatching` attempt \
                     left behind by a DEAD incarnation is precisely what this writer is \
                     for, and it must still be able to reach `uncertain`",
                );
            let (state, attempt_state): (String, String) = store
                .conn
                .query_row(
                    "SELECT m.state, a.attempt_state
                       FROM agent_messages m
                       JOIN agent_message_delivery_attempts a
                         ON a.message_id=m.id AND a.attempt_number=1
                      WHERE m.id=?1",
                    params![foreign_row.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("read back");
            assert_eq!(state, "uncertain");
            assert_eq!(
                attempt_state, "effect_possible",
                "uncertainty is retained custody, not a terminal seal — the fence must \
                 not have changed what a legitimate call produces"
            );
        }

        /// P2-06a — the other half of the narrowed rule: a `claimed` attempt
        /// that never reached the marker genuinely proves no effect, so the
        /// requeue is permitted and lands atomically.
        ///
        /// Without this, the rule could be "corrected" by refusing everything,
        /// which is safe but useless.
        #[test]
        fn a_claimed_row_with_a_foreign_boot_proves_no_effect_and_requeues() {
            let store = Store::open_in_memory().unwrap();
            let (message_id, _) = scaffold_claimed(&store, "proved-no-effect");
            let live_boot = Uuid::new_v4();

            let crashed = store
                .list_crashed_agent_message_attempts_page_v1(live_boot, None)
                .expect("classify crashed attempts");
            let row = crashed
                .attempts
                .iter()
                .find(|row| row.message_id == message_id)
                .expect("a claimed attempt on a dead boot must be surfaced");
            assert_eq!(row.attempt_state, AttemptStateV1::Claimed);
            assert_eq!(
                row.verdict,
                CrashRecoveryVerdictV1::ProvedNoEffectRequeue,
                "a never-marked claimed attempt provably never reached the send"
            );

            store
                .requeue_crashed_agent_message_attempt_v1(message_id, 1, live_boot, live_boot)
                .expect("the narrowed rule permits exactly this requeue");

            let (state, version, pointer, attempt_state, disposition, admission, effect): (
                String,
                i64,
                Option<i64>,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
            ) = store
                .conn
                .query_row(
                    "SELECT m.state, m.state_version, m.current_attempt_number,
                            a.attempt_state, a.terminal_disposition,
                            a.admission_classification, a.effect_classification
                       FROM agent_messages m
                       JOIN agent_message_delivery_attempts a
                         ON a.message_id=m.id AND a.attempt_number=1
                      WHERE m.id=?1",
                    params![message_id.to_string()],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                        ))
                    },
                )
                .expect("read back");
            assert_eq!(state, "queued");
            assert_eq!(version, 2, "the requeue bumps the version exactly once");
            assert_eq!(
                pointer,
                Some(1),
                "the aggregate KEEPS its pointer to the sealed attempt until the next claim"
            );
            assert_eq!(attempt_state, "terminal");
            assert_eq!(disposition.as_deref(), Some("proved_no_effect_requeue"));
            assert_eq!(admission.as_deref(), Some("rejected_before_effect"));
            assert_eq!(effect.as_deref(), Some("proved_no_effect"));

            // The transition is attempt-linked and attributed to the reconciler,
            // not relabelled as a dispatcher edge.
            let (authority, attempt): (String, Option<i64>) = store
                .conn
                .query_row(
                    "SELECT authority_kind, attempt_number
                       FROM agent_message_state_transitions
                      WHERE message_id=?1 AND state_version=2",
                    params![message_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("read the requeue transition");
            assert_eq!(authority, "restart_reconciler");
            assert_eq!(attempt, Some(1));
        }

        /// P2-06a — an attempt stamped with the LIVE boot id is not crashed at
        /// all and must never be surfaced to recovery.
        ///
        /// Without this the recovery would requeue attempts belonging to the
        /// running incarnation, racing the live dispatcher.
        #[test]
        fn a_live_boot_attempt_is_never_offered_to_crash_recovery() {
            let store = Store::open_in_memory().unwrap();
            let (message_id, _) = scaffold_claimed(&store, "live-boot");
            let live_boot: String = store
                .conn
                .query_row(
                    "SELECT delivery_boot_id FROM agent_message_delivery_attempts
                      WHERE message_id=?1 AND attempt_number=1",
                    params![message_id.to_string()],
                    |row| row.get(0),
                )
                .expect("read the attempt's own boot id");
            let live_boot = Uuid::parse_str(&live_boot).expect("boot uuid");

            let crashed = store
                .list_crashed_agent_message_attempts_page_v1(live_boot, None)
                .expect("classify crashed attempts");
            assert!(
                !crashed
                    .attempts
                    .iter()
                    .any(|row| row.message_id == message_id),
                "an attempt owned by the LIVE incarnation is not a crash and must not be \
                 recovered underneath the running dispatcher"
            );
        }

        /// **P2-06b — THE DRAIN. This test exists to FAIL if the page bound
        /// ever becomes silent again.**
        ///
        /// Before the cursor, a restart holding more than
        /// `AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS` crashed attempts recovered
        /// the first page and abandoned the rest: they were neither requeued
        /// nor marked uncertain, their aggregates were never re-dispatched and
        /// never stranded-with-evidence, and NOTHING reported it — no error, no
        /// log line, no short page. They were simply lost. That is the defect
        /// this test closes, so it asserts all three halves:
        ///
        ///   1. the bound is OBSERVABLE — a full page offers a continuation;
        ///   2. the walk is EXACT — no row is served twice and none is skipped;
        ///   3. the walk is USEFUL — a real processing drain recovers every
        ///      member of a more-than-one-page population, not just the head.
        ///
        /// The population is deliberately one MORE than a page, so a
        /// re-introduced silent cap cannot pass by accident.
        #[test]
        fn a_population_larger_than_one_page_is_fully_drained_by_the_cursor() {
            let store = Store::open_in_memory().unwrap();
            let live_boot = Uuid::new_v4();

            let population = AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS + 1;
            let mut expected = std::collections::BTreeSet::new();
            for index in 0..population {
                let (message_id, _) = scaffold_claimed(&store, &format!("drain-{index}"));
                expected.insert(message_id);
            }
            assert_eq!(
                expected.len(),
                population,
                "precondition: the scaffold produced distinct messages"
            );

            // 1. The bound is observable. A page that fills MUST say so.
            let first = store
                .list_crashed_agent_message_attempts_page_v1(live_boot, None)
                .expect("first page");
            assert_eq!(
                first.attempts.len(),
                AGENT_MESSAGE_QUARANTINE_MAX_ATTEMPTS,
                "precondition: one page genuinely cannot hold this population"
            );
            assert!(
                first.next_cursor.is_some(),
                "SILENT CAP: a full page reported exhaustion. A caller that believes this \
                 has finished recovery abandons every attempt past the first page — \
                 neither requeued nor marked uncertain, with no error and no log line."
            );

            // 2. The walk is exact: strictly increasing, no repeat, no gap.
            let mut seen: Vec<(Uuid, u32)> = Vec::new();
            let mut after = None;
            let mut pages = 0;
            loop {
                let page = store
                    .list_crashed_agent_message_attempts_page_v1(live_boot, after)
                    .expect("drain page");
                pages += 1;
                assert!(
                    pages <= 8,
                    "the drain did not converge in {pages} pages over {population} rows: \
                     the cursor is not advancing past every row it reaches"
                );
                for row in &page.attempts {
                    if let Some(previous) = seen.last() {
                        assert!(
                            (row.message_id, row.attempt_number) > *previous,
                            "the keyset order is not strictly increasing across pages, so \
                             the cursor cannot bound what has already been recovered"
                        );
                    }
                    seen.push((row.message_id, row.attempt_number));
                }
                match page.next_cursor {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }

            let unique: std::collections::BTreeSet<_> = seen.iter().copied().collect();
            assert_eq!(
                unique.len(),
                seen.len(),
                "the cursor re-served a row it had already offered; recovery would try to \
                 settle the same attempt twice"
            );
            assert_eq!(
                unique.len(),
                population,
                "the drain visited {} of {population} crashed attempts — the rest were \
                 SILENTLY SKIPPED",
                unique.len()
            );
            let drained: std::collections::BTreeSet<Uuid> =
                unique.iter().map(|(id, _)| *id).collect();
            assert_eq!(
                drained, expected,
                "the drain must reach EXACTLY the crashed population"
            );

            // 3. The walk is useful: a real processing drain, which MUTATES
            //    every row it touches, still reaches the whole population.
            //    This is the shape the periodic worker (P2-06c) will run, and
            //    it is why the walk is keyset rather than OFFSET — each
            //    requeue drops its row out of the query's own predicate.
            let mut after = None;
            let mut recovered = 0;
            loop {
                let page = store
                    .list_crashed_agent_message_attempts_page_v1(live_boot, after)
                    .expect("processing drain page");
                for row in &page.attempts {
                    assert_eq!(
                        row.verdict,
                        CrashRecoveryVerdictV1::ProvedNoEffectRequeue,
                        "precondition: every scaffolded row is a never-marked claimed attempt"
                    );
                    store
                        .requeue_crashed_agent_message_attempt_v1(
                            row.message_id,
                            row.attempt_number,
                            live_boot,
                            live_boot,
                        )
                        .expect("the narrowed rule permits exactly this requeue");
                    recovered += 1;
                }
                match page.next_cursor {
                    Some(next) => after = Some(next),
                    None => break,
                }
            }
            assert_eq!(
                recovered, population,
                "a processing drain recovered {recovered} of {population}"
            );

            let queued: i64 = store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM agent_messages WHERE state='queued'",
                    [],
                    |row| row.get(0),
                )
                .expect("count requeued aggregates");
            assert_eq!(
                queued,
                i64::try_from(population).unwrap(),
                "every crashed message in a more-than-one-page population must end up \
                 re-deliverable; any shortfall is a message lost to the silent cap"
            );
            let still_live: i64 = store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM agent_message_delivery_attempts
                      WHERE attempt_state!='terminal'",
                    [],
                    |row| row.get(0),
                )
                .expect("count unsealed attempts");
            assert_eq!(
                still_live, 0,
                "an attempt left unsealed is one the drain never reached"
            );
        }

        /// P2-06b — the `expiry_reconciler`'s one legal edge, in both of the
        /// pointer shapes a `queued` message can be in.
        ///
        /// C-P2-09 froze the rule that `queued → expired` carries a NULL
        /// attempt and creates none. The transition trigger has required it
        /// since V81; nothing authored one until now. Both halves are asserted
        /// because they fail differently: a never-claimed message has a NULL
        /// pointer, while a requeued one still points at its sealed
        /// `proved_no_effect_requeue` attempt, and that historical pointer must
        /// SURVIVE the expiry rather than be cleared to make the write easier.
        #[test]
        fn a_durably_expired_queued_message_expires_with_a_null_attempt() {
            let store = Store::open_in_memory().unwrap();
            let past = Utc::now() - chrono::Duration::seconds(60);

            // --- shape 1: never claimed, so the pointer is NULL -----------
            let (never_claimed, _) = accept_expiring(&store, "expiry-fresh", Some(past));
            store
                .expire_queued_agent_message_v1(never_claimed, Uuid::new_v4())
                .expect("a durably expired queued message must be expirable");

            let (state, version, pointer, count, expired_at): (
                String,
                i64,
                Option<i64>,
                i64,
                Option<String>,
            ) = store
                .conn
                .query_row(
                    "SELECT state, state_version, current_attempt_number, attempt_count, expired_at
                       FROM agent_messages WHERE id=?1",
                    params![never_claimed.to_string()],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .expect("read back");
            assert_eq!(state, "expired");
            assert_eq!(version, 1, "the expiry bumps the version exactly once");
            assert_eq!(pointer, None);
            assert_eq!(count, 0, "expiry must not invent a delivery attempt");
            assert!(
                expired_at.is_some(),
                "an expired aggregate stamps expired_at"
            );

            let attempts: i64 = store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM agent_message_delivery_attempts WHERE message_id=?1",
                    params![never_claimed.to_string()],
                    |row| row.get(0),
                )
                .expect("count attempts");
            assert_eq!(
                attempts, 0,
                "expiry created an attempt. An attempt asserts a delivery was made; \
                 manufacturing one to satisfy a join would fabricate custody for a send \
                 that never happened"
            );

            let (authority, attempt): (String, Option<i64>) = store
                .conn
                .query_row(
                    "SELECT authority_kind, attempt_number
                       FROM agent_message_state_transitions
                      WHERE message_id=?1 AND state_version=1",
                    params![never_claimed.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("read the expiry transition");
            assert_eq!(
                authority, "expiry_reconciler",
                "the edge must be attributed to the authority the schema names for it, \
                 not relabelled onto the dispatcher"
            );
            assert_eq!(
                attempt, None,
                "a `queued -> expired` edge names NO attempt: there is no live delivery \
                 to take custody of"
            );

            // --- shape 2: requeued, so a historical pointer must survive ---
            let live_boot = Uuid::new_v4();
            let (requeued, _) = scaffold_claimed_expiring(&store, "expiry-requeued", Some(past));
            store
                .requeue_crashed_agent_message_attempt_v1(requeued, 1, live_boot, live_boot)
                .expect("the narrowed rule permits this requeue");
            store
                .expire_queued_agent_message_v1(requeued, Uuid::new_v4())
                .expect("a requeued, durably expired message must still be expirable");

            let (state, pointer, count, disposition): (String, Option<i64>, i64, Option<String>) =
                store
                    .conn
                    .query_row(
                        "SELECT m.state, m.current_attempt_number, m.attempt_count,
                                a.terminal_disposition
                           FROM agent_messages m
                           JOIN agent_message_delivery_attempts a
                             ON a.message_id=m.id AND a.attempt_number=1
                          WHERE m.id=?1",
                        params![requeued.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .expect("read back");
            assert_eq!(state, "expired");
            assert_eq!(
                pointer,
                Some(1),
                "the historical requeue pointer must SURVIVE expiry — clearing it would \
                 orphan the sealed attempt that records why the message was requeued"
            );
            assert_eq!(count, 1, "expiry must not create a second attempt");
            assert_eq!(
                disposition.as_deref(),
                Some("proved_no_effect_requeue"),
                "the historical seal is untouched: expiry re-settles nothing"
            );

            let attempt: Option<i64> = store
                .conn
                .query_row(
                    "SELECT attempt_number FROM agent_message_state_transitions
                      WHERE message_id=?1 AND to_state='expired'",
                    params![requeued.to_string()],
                    |row| row.get(0),
                )
                .expect("read the expiry transition");
            assert_eq!(
                attempt, None,
                "even with a live historical pointer the expiry edge names NO attempt"
            );
        }

        /// **P2-06b — THE DISCRIMINATOR. This test exists to FAIL if the
        /// expiry reconciler is ever widened past `queued`.**
        ///
        /// A wall-clock deadline is NEVER proof of no effect. That is the same
        /// inference the narrowed rule forbids, and it is the withdrawn defect
        /// H21-P2-R5-001 (HIGH) wearing a different hat: a `claimed` message
        /// has a LIVE attempt, and a `dispatching` one may already have spent a
        /// paid model turn. Expiring either as proved-no-effect would seal away
        /// a possible effect on nothing but the passage of time.
        ///
        /// `claimed → expired` is legal in the schema, but only for the
        /// dispatcher holding the provider's own rejected-before-effect answer
        /// (`record_agent_message_admission` + `NoEffectDisposition::Expired`).
        /// The reconciler holds no such answer and must never reach that edge.
        #[test]
        fn the_expiry_reconciler_refuses_every_message_with_a_live_attempt() {
            let store = Store::open_in_memory().unwrap();
            let past = Utc::now() - chrono::Duration::seconds(60);

            for (key, open_send_window) in [("expiry-claimed", false), ("expiry-marked", true)] {
                let (message_id, _) = scaffold_claimed_expiring(&store, key, Some(past));
                if open_send_window {
                    store
                        .conn
                        .execute(
                            "UPDATE agent_message_delivery_attempts
                                SET attempt_state='dispatching', dispatching_at=?2, updated_at=?2
                              WHERE message_id=?1 AND attempt_number=1",
                            params![message_id.to_string(), TRIGGER_TS],
                        )
                        .expect("open the pre-dispatch window");
                }

                let refusal = store
                    .expire_queued_agent_message_v1(message_id, Uuid::new_v4())
                    .expect_err(
                        "WIDENED: the expiry reconciler accepted a message with a LIVE \
                         attempt. A passed deadline is not evidence about whether the \
                         provider was reached — expiring it as proved-no-effect seals \
                         away an effect that may already have been paid for.",
                    );
                assert!(
                    refusal.to_string().contains("only `queued`"),
                    "expected the queued-only discriminator to refuse it, got: {refusal}"
                );

                // Nothing moved: no expiry, no seal, no invented disposition.
                let (state, attempt_state, disposition, expired_at): (
                    String,
                    String,
                    Option<String>,
                    Option<String>,
                ) = store
                    .conn
                    .query_row(
                        "SELECT m.state, a.attempt_state, a.terminal_disposition, m.expired_at
                           FROM agent_messages m
                           JOIN agent_message_delivery_attempts a
                             ON a.message_id=m.id AND a.attempt_number=1
                          WHERE m.id=?1",
                        params![message_id.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .expect("read back");
                assert_eq!(state, "claimed", "the refused expiry moved the aggregate");
                assert_eq!(
                    attempt_state,
                    if open_send_window {
                        "dispatching"
                    } else {
                        "claimed"
                    },
                    "the refused expiry disturbed the attempt"
                );
                assert_eq!(
                    disposition, None,
                    "no `proved_no_effect_expired` may be invented from a wall-clock \
                     deadline — that is exactly the withdrawn inference"
                );
                assert_eq!(expired_at, None);
            }
        }

        /// P2-06b — expiry is a property of the ROW, not of the reconciler's
        /// opinion, and `expires_at` is the MESSAGE's frozen acceptance
        /// deadline — never the delivery lease.
        ///
        /// Mirrors the identical guard already enforced by
        /// `record_agent_message_admission`. Without it the reconciler could
        /// expire live mail on a scheduling accident.
        #[test]
        fn the_expiry_reconciler_refuses_a_deadline_that_has_not_passed() {
            let store = Store::open_in_memory().unwrap();

            // An explicit future deadline and an omitted deadline both remain
            // queued until their persisted deadlines pass.
            for (key, expires_at) in [
                (
                    "expiry-future",
                    Some(Utc::now() + chrono::Duration::days(1)),
                ),
                ("expiry-absent", None),
            ] {
                let (message_id, _) = accept_expiring(&store, key, expires_at);
                let refusal = store
                    .expire_queued_agent_message_v1(message_id, Uuid::new_v4())
                    .expect_err("a deadline that has not passed is not an expiry");
                assert!(
                    refusal
                        .to_string()
                        .contains("agent_message_expiry_requires_a_passed_expiry"),
                    "unexpected error for {key}: {refusal}"
                );

                let state: String = store
                    .conn
                    .query_row(
                        "SELECT state FROM agent_messages WHERE id=?1",
                        params![message_id.to_string()],
                        |row| row.get(0),
                    )
                    .expect("read back");
                assert_eq!(state, "queued", "live mail must stay deliverable");
            }
        }

        /// Trigger 9/32 — `agent_message_attempts_v81_evidence_monotonic`.
        #[test]
        fn p202_agent_message_attempts_v81_evidence_monotonic() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);
            let id = world.message_id.to_string();
            let update = |set: &str, value: Box<dyn rusqlite::ToSql>| {
                store.conn.execute(
                    &format!(
                        "UPDATE agent_message_delivery_attempts SET {set}=?2
                         WHERE message_id=?1 AND attempt_number=1"
                    ),
                    params![id, value],
                )
            };

            // A nonnull evidence scalar never changes and never CLEARS. The
            // clearing half matters most: `IS NOT` rather than `!=` is what
            // makes a value->NULL rewrite visible to the guard at all.
            let frozen: [(&str, Box<dyn rusqlite::ToSql>); 8] = [
                ("boundary_value", Box::new("turn-somewhere-else")),
                ("boundary_value", Box::new(Option::<String>::None)),
                ("admission_classification", Box::new("unsupported")),
                ("admission_classification", Box::new(Option::<String>::None)),
                (
                    "admission_recorded_at",
                    Box::new("2027-01-01T00:00:00.000000000Z"),
                ),
                (
                    "effect_possible_at",
                    Box::new("2027-01-01T00:00:00.000000000Z"),
                ),
                (
                    "acknowledged_at",
                    Box::new("2027-01-01T00:00:00.000000000Z"),
                ),
                ("acknowledged_at", Box::new(Option::<String>::None)),
            ];
            for (column, value) in frozen {
                refused(
                    update(column, value),
                    "V81 delivery attempt evidence is fill-once and forward-only",
                    &format!("rewriting or clearing filled evidence {column}"),
                );
            }

            // effect_classification may ONLY strengthen effect_possible ->
            // effect_acknowledged. It is already acknowledged here, so every
            // move is a weakening.
            for weaker in ["effect_possible", "proved_no_effect"] {
                refused(
                    update("effect_classification", Box::new(weaker)),
                    "V81 delivery attempt evidence is fill-once and forward-only",
                    &format!("weakening effect_acknowledged -> {weaker}"),
                );
            }

            // attempt_state is forward-only. `dispatching` is the meaningful
            // second edge (R7 LOW-1): the pre-dispatch marker sits between
            // `claimed` and everything downstream, so `effect_possible ->
            // dispatching` is a rewind BACK ACROSS the marker — it would make
            // an attempt whose effect is already known possible look like one
            // still inside the send window. It replaced `admission_recorded`,
            // a spelling P2-06a removed from the `attempt_state` enum, whose
            // iteration had become redundant with `claimed`.
            for backward in ["claimed", "dispatching"] {
                refused(
                    update("attempt_state", Box::new(backward)),
                    "V81 delivery attempt evidence is fill-once and forward-only",
                    &format!("rewinding attempt_state effect_possible -> {backward}"),
                );
            }

            // ALLOWED: the scaffold already performed the one carved-out
            // strengthening (effect_possible -> effect_acknowledged). The other
            // legal edge is effect_possible -> terminal, asserted here.
            seal_acknowledged(&store, world.message_id);
            let state: String = store
                .conn
                .query_row(
                    "SELECT attempt_state FROM agent_message_delivery_attempts
                     WHERE message_id=?1 AND attempt_number=1",
                    params![id],
                    |r| r.get(0),
                )
                .expect("read attempt state");
            assert_eq!(state, "terminal", "the forward seal must be admitted");
        }

        /// Trigger 10/32 — `agent_message_attempts_v81_correlation_coherence`.
        #[test]
        fn p202_agent_message_attempts_v81_correlation_coherence() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);
            let id = world.message_id.to_string();

            // Attempt 1 is already `correlated`. Correlation is forward-only
            // and sealing is irreversible, so nothing leaves `correlated` --
            // not even to the other terminal correlation state. Every field the
            // coupled CHECKs would demand is supplied, so the TRIGGER is the
            // only layer that can object.
            refused(
                store.conn.execute(
                    "UPDATE agent_message_delivery_attempts
                        SET correlation_state='correlation_pending'
                      WHERE message_id=?1 AND attempt_number=1",
                    params![id],
                ),
                "V81 correlation custody is forward-only and sealing is irreversible",
                "reverting correlated -> correlation_pending",
            );
            refused(
                store.conn.execute(
                    "UPDATE agent_message_delivery_attempts
                        SET correlation_state='sealed_live_uncertain',
                            evidence_suppression_class='response_lost',
                            evidence_sealed_at=?2
                      WHERE message_id=?1 AND attempt_number=1",
                    params![id, TRIGGER_TS],
                ),
                "V81 correlation custody is forward-only and sealing is irreversible",
                "sealing an already-correlated attempt",
            );
            refused(
                store.conn.execute(
                    "UPDATE agent_message_delivery_attempts
                        SET correlation_state='not_applicable'
                      WHERE message_id=?1 AND attempt_number=1",
                    params![id],
                ),
                "V81 correlation custody is forward-only and sealing is irreversible",
                "demoting correlated -> not_applicable",
            );

            // A separately sealed attempt is equally frozen: sealing is
            // irreversible in the other direction too.
            insert_attempt(&store, &world, 2, "correlation_pending").expect("second attempt");
            store
                .conn
                .execute(
                    "UPDATE agent_message_delivery_attempts
                        SET correlation_state='sealed_live_uncertain',
                            evidence_suppression_class='response_lost',
                            evidence_sealed_at=?2
                      WHERE message_id=?1 AND attempt_number=2",
                    params![id, TRIGGER_TS],
                )
                .expect("ALLOWED: correlation_pending -> sealed_live_uncertain");
            refused(
                store.conn.execute(
                    "UPDATE agent_message_delivery_attempts
                        SET correlation_state='correlated', boundary_value=?2,
                            correlation_source='late_response', correlated_at=?3
                      WHERE message_id=?1 AND attempt_number=2",
                    params![id, TURN, TRIGGER_TS],
                ),
                "V81 correlation custody is forward-only and sealing is irreversible",
                "correlating an attempt that already sealed",
            );

            // ALLOWED: the scaffold performed correlation_pending ->
            // correlated, and a same-state write is untouched.
            store
                .conn
                .execute(
                    "UPDATE agent_message_delivery_attempts
                        SET correlation_state='correlated', updated_at=?2
                      WHERE message_id=?1 AND attempt_number=1",
                    params![id, "2026-08-04T00:00:00.000000000Z"],
                )
                .expect("a same-state correlation write must be admitted");
        }

        /// Trigger 11/32 — `agent_message_attempts_v81_ack_event_coherence`.
        ///
        /// Records an accurate SCOPE observation: this trigger is BEFORE
        /// UPDATE only. The all-null/all-nonnull half of the acknowledgement
        /// contract is a coupled CHECK (`sql_all_or_none`) that covers INSERT
        /// too, and `acknowledged_event_id` carries a real foreign key, but the
        /// session/sequence RESOLUTION is enforced on UPDATE only. That is the
        /// production shape -- attempts are inserted at claim time with a null
        /// triple and acknowledged later by update -- and it is asserted here
        /// rather than assumed.
        #[test]
        fn p202_agent_message_attempts_v81_ack_event_coherence() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);
            let id = world.message_id.to_string();

            // The scaffold already filled a RESOLVING triple in the allowed
            // direction. A triple naming the wrong sequence must be refused.
            refused(
                store.conn.execute(
                    "UPDATE agent_message_delivery_attempts
                        SET acknowledged_event_sequence=?2
                      WHERE message_id=?1 AND attempt_number=1",
                    params![id, EVENT_SEQUENCE + 1],
                ),
                "V81 acknowledgement event triple does not resolve to one exact event",
                "an acknowledgement triple naming the wrong sequence",
            );

            // ... or the wrong Session, even though that Session exists and the
            // event ID is real. This is the cross-row truth no CHECK can hold.
            refused(
                store.conn.execute(
                    "UPDATE agent_message_delivery_attempts
                        SET acknowledged_event_session_id=?2
                      WHERE message_id=?1 AND attempt_number=1",
                    params![id, world.owner.to_string()],
                ),
                "V81 acknowledgement event triple does not resolve to one exact event",
                "an acknowledgement triple naming the wrong session",
            );

            // ... or an event ID that does not exist at all.
            refused(
                store.conn.execute(
                    "UPDATE agent_message_delivery_attempts
                        SET acknowledged_event_id=?2
                      WHERE message_id=?1 AND attempt_number=1",
                    params![id, world.event_id + 9_999],
                ),
                "V81 acknowledgement event triple does not resolve to one exact event",
                "an acknowledgement triple naming a nonexistent event",
            );

            // ALLOWED: a second attempt may fill its own RESOLVING triple --
            // over its OWN event. Reusing attempt 1's event is refused by the
            // unique partial `idx_agent_message_attempts_v81_ack_event` index,
            // which is the separate guarantee that one acknowledging event
            // settles exactly one attempt; that layering is asserted here
            // rather than worked around.
            insert_attempt(&store, &world, 2, "correlation_pending").expect("second attempt");
            let reuse = store.conn.execute(
                "UPDATE agent_message_delivery_attempts
                    SET acknowledged_event_id=?2, acknowledged_event_session_id=?3,
                        acknowledged_event_sequence=?4
                  WHERE message_id=?1 AND attempt_number=2",
                params![id, world.event_id, world.target.to_string(), EVENT_SEQUENCE],
            );
            let reuse_error = reuse
                .expect_err("one event may not acknowledge two attempts")
                .to_string();
            assert!(
                reuse_error.contains("UNIQUE constraint failed")
                    && reuse_error.contains("acknowledged_event_id"),
                "expected the unique ack-event index, got {reuse_error:?}"
            );

            store
                .conn
                .execute(
                    "INSERT INTO conversation_events (session_id,event_type,content,sequence,created_at)
                     VALUES (?1,'assistant','ack-2',?2,?3)",
                    params![world.target.to_string(), EVENT_SEQUENCE + 1, TRIGGER_TS],
                )
                .expect("seed a second acknowledging event");
            let second_event = store.conn.last_insert_rowid();
            store
                .conn
                .execute(
                    "UPDATE agent_message_delivery_attempts
                        SET acknowledged_event_id=?2, acknowledged_event_session_id=?3,
                            acknowledged_event_sequence=?4
                      WHERE message_id=?1 AND attempt_number=2",
                    params![
                        id,
                        second_event,
                        world.target.to_string(),
                        EVENT_SEQUENCE + 1
                    ],
                )
                .expect("a resolving acknowledgement triple must be admitted");
        }

        /// Trigger 12/32 — `agent_message_attempts_v81_terminal_immutable`.
        #[test]
        fn p202_agent_message_attempts_v81_terminal_immutable() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);
            let id = world.message_id.to_string();
            seal_acknowledged(&store, world.message_id);

            let rewrites: [(&str, Box<dyn rusqlite::ToSql>); 8] = [
                ("terminal_disposition", Box::new("settled_failed")),
                ("settlement_authority", Box::new("operator_settlement")),
                ("settlement_evidence_digest", Box::new(trigger_digest("3"))),
                ("settled_at", Box::new("2027-01-01T00:00:00.000000000Z")),
                ("terminal_at", Box::new("2027-01-01T00:00:00.000000000Z")),
                ("attempt_state", Box::new("effect_possible")),
                ("effect_classification", Box::new("effect_possible")),
                ("acknowledged_event_id", Box::new(Option::<i64>::None)),
            ];
            for (column, value) in rewrites {
                refused(
                    store.conn.execute(
                        &format!(
                            "UPDATE agent_message_delivery_attempts SET {column}=?2
                             WHERE message_id=?1 AND attempt_number=1"
                        ),
                        params![id, value],
                    ),
                    "V81 terminal delivery attempt evidence is immutable",
                    &format!("rewriting sealed settlement column {column}"),
                );
            }

            // ALLOWED: a column outside the frozen settlement set still moves,
            // so the freeze is scoped rather than total.
            store
                .conn
                .execute(
                    "UPDATE agent_message_delivery_attempts SET updated_at=?2
                     WHERE message_id=?1 AND attempt_number=1",
                    params![id, "2026-08-04T00:00:00.000000000Z"],
                )
                .expect("a non-settlement column must still move on a sealed attempt");
        }

        // ==================================================================
        // Transitions: agent_message_state_transitions (3 triggers)
        // ==================================================================

        fn insert_transition_with_authority(
            store: &Store,
            message_id: Uuid,
            version: i64,
            from: &str,
            to: &str,
            attempt: Option<i64>,
            authority: &str,
        ) -> rusqlite::Result<usize> {
            store.conn.execute(
                "INSERT INTO agent_message_state_transitions (
                     message_id, state_version, from_state, to_state, attempt_number,
                     authority_kind, authority_id, evidence_digest, created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    message_id.to_string(),
                    version,
                    from,
                    to,
                    attempt,
                    authority,
                    Uuid::new_v4().to_string(),
                    trigger_digest("c"),
                    TRIGGER_TS,
                ],
            )
        }

        /// Trigger 13/32 — `agent_message_transitions_v81_no_delete`.
        #[test]
        fn p202_agent_message_transitions_v81_no_delete() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            for (sql, what) in [
                (
                    "DELETE FROM agent_message_state_transitions WHERE message_id=?1",
                    "deleting a message's transition history",
                ),
                (
                    "DELETE FROM agent_message_state_transitions WHERE ?1 IS NOT NULL",
                    "sweeping the whole transition log",
                ),
            ] {
                refused(
                    store
                        .conn
                        .execute(sql, params![world.message_id.to_string()]),
                    "agent message state transitions are append-only",
                    what,
                );
            }

            let remaining: i64 = store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM agent_message_state_transitions",
                    [],
                    |r| r.get(0),
                )
                .expect("count");
            assert_eq!(remaining, 3, "every transition survived");
        }

        /// Trigger 14/32 — `agent_message_transitions_v81_identity_immutable`.
        ///
        /// This trigger carries NO `WHEN` clause: a transition row is wholly
        /// immutable, so there is deliberately no allowed UPDATE to assert.
        /// The non-refusal direction is covered by the INSERT path instead --
        /// appending new rows must still work, which is asserted here so the
        /// trigger cannot be silently over-scoped to block inserts too.
        #[test]
        fn p202_agent_message_transitions_v81_identity_immutable() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);
            let id = world.message_id.to_string();

            for column in [
                "state_version",
                "from_state",
                "to_state",
                "attempt_number",
                "authority_kind",
                "authority_id",
                "evidence_digest",
                "created_at",
            ] {
                refused(
                    store.conn.execute(
                        &format!(
                            "UPDATE agent_message_state_transitions SET {column}=NULL
                             WHERE message_id=?1 AND state_version=1"
                        ),
                        params![id],
                    ),
                    "agent message state transitions are immutable",
                    &format!("clearing transition column {column}"),
                );
            }

            // Even a no-op self-assignment is refused -- the trigger is
            // unconditional by design.
            refused(
                store.conn.execute(
                    "UPDATE agent_message_state_transitions SET to_state=to_state
                     WHERE message_id=?1 AND state_version=1",
                    params![id],
                ),
                "agent message state transitions are immutable",
                "a no-op self-assignment on a transition row",
            );

            // ALLOWED: appending is still permitted.
            insert_transition(
                &store,
                world.message_id,
                3,
                "injected",
                "uncertain",
                Some(1),
            )
            .expect("appending a new transition must still be admitted");
        }

        /// Trigger 15/32 — `agent_message_transitions_v81_coherence`.
        #[test]
        fn p202_agent_message_transitions_v81_coherence() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);
            let message_id = world.message_id;

            // An active/attempt-linked edge must name its exact attempt.
            for to_state in ["claimed", "injected", "uncertain", "acknowledged"] {
                refused(
                    insert_transition(&store, message_id, 9, "injected", to_state, None),
                    "V81 transition attempt linkage violates terminal coherence",
                    &format!("an unnamed attempt on a {to_state} edge"),
                );
            }

            // A named attempt that does not exist on this message is refused.
            refused(
                insert_transition(&store, message_id, 9, "injected", "acknowledged", Some(7)),
                "V81 transition attempt linkage violates terminal coherence",
                "naming an attempt that does not exist",
            );

            // Queued expiry is a NULL-attempt edge under expiry authority.
            refused(
                insert_transition_with_authority(
                    &store,
                    message_id,
                    9,
                    "queued",
                    "expired",
                    Some(1),
                    "expiry_reconciler",
                ),
                "V81 transition attempt linkage violates terminal coherence",
                "queued expiry that names an attempt",
            );
            refused(
                insert_transition_with_authority(
                    &store,
                    message_id,
                    9,
                    "queued",
                    "expired",
                    None,
                    "dispatcher",
                ),
                "V81 transition attempt linkage violates terminal coherence",
                "queued expiry under the wrong authority",
            );

            // Queued launch failure is also a NULL-attempt edge.
            refused(
                insert_transition(&store, message_id, 9, "queued", "failed", Some(1)),
                "V81 transition attempt linkage violates terminal coherence",
                "queued launch failure that names an attempt",
            );

            // Claimed expiry must name an attempt ALREADY sealed
            // proved_no_effect_expired. Attempt 1 here is sealed
            // `acknowledged`, which is a real seal but the wrong one.
            seal_acknowledged(&store, message_id);
            refused(
                insert_transition(&store, message_id, 9, "claimed", "expired", Some(1)),
                "V81 transition attempt linkage violates terminal coherence",
                "claimed expiry over an attempt not sealed proved_no_effect_expired",
            );

            // --- ALLOWED edges ---------------------------------------------
            store
                .conn
                .execute(
                    "UPDATE agent_message_state_transitions SET to_state=to_state WHERE 1=0",
                    [],
                )
                .expect("no-op guard");

            // A NULL-attempt queued expiry under expiry authority.
            insert_transition_with_authority(
                &store,
                message_id,
                9,
                "queued",
                "expired",
                None,
                "expiry_reconciler",
            )
            .expect("a null-attempt queued expiry must be admitted");

            // A NULL-attempt queued launch failure.
            insert_transition_with_authority(
                &store,
                message_id,
                10,
                "queued",
                "failed",
                None,
                "spawn_settlement",
            )
            .expect("a null-attempt queued launch failure must be admitted");

            // A claimed expiry over a correctly sealed attempt.
            let other = Uuid::new_v4();
            acceptance_session(&store, other);
            let expired_id = claimed_message_with_unsealed_attempt(
                &store,
                world.owner,
                other,
                "p202-claimed-expiry",
            );
            seal_attempt(&store, expired_id, 1, "proved_no_effect_expired");
            insert_transition_with_authority(
                &store,
                expired_id,
                2,
                "claimed",
                "expired",
                Some(1),
                "expiry_reconciler",
            )
            .expect("claimed expiry over a sealed proved_no_effect_expired attempt is legal");
        }

        // ==================================================================
        // Provider-request ledger: agent_message_provider_request_effects
        // (8 triggers)
        // ==================================================================

        /// Update the scaffold's single request row.
        fn update_request(store: &Store, world: &World, set: &str) -> rusqlite::Result<usize> {
            store.conn.execute(
                &format!(
                    "UPDATE agent_message_provider_request_effects SET {set}
                     WHERE message_id=?1 AND provider_request_id=?2"
                ),
                params![world.message_id.to_string(), REQUEST_ID],
            )
        }

        /// Drive the scaffold's request row to `handler_started`, which its
        /// pre-issued handler permit authorises.
        fn start_handler(store: &Store, world: &World) {
            update_request(
                store,
                world,
                &format!(
                    "handler_phase='handler_authorized', handler_capability_id='{}',
                     handler_authorized_at='{TRIGGER_TS}'",
                    Uuid::new_v4()
                ),
            )
            .expect("evidence_committed -> handler_authorized is a legal forward edge");
            update_request(
                store,
                world,
                &format!("handler_phase='handler_started', handler_started_at='{TRIGGER_TS}'"),
            )
            .expect("handler_authorized -> handler_started over an issued permit is legal");
        }

        /// Trigger 16/32 — `agent_message_request_effects_v81_no_delete`.
        #[test]
        fn p202_agent_message_request_effects_v81_no_delete() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            for (sql, what) in [
                (
                    "DELETE FROM agent_message_provider_request_effects WHERE message_id=?1",
                    "deleting a provider-request evidence row",
                ),
                (
                    "DELETE FROM agent_message_provider_request_effects WHERE ?1 IS NOT NULL",
                    "sweeping the provider-request ledger",
                ),
            ] {
                refused(
                    store
                        .conn
                        .execute(sql, params![world.message_id.to_string()]),
                    "agent message provider request effects are append-only",
                    what,
                );
            }
        }

        /// Trigger 17/32 — `agent_message_request_effects_v81_identity_immutable`.
        #[test]
        fn p202_agent_message_request_effects_v81_identity_immutable() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            let rewrites: [(&str, Box<dyn rusqlite::ToSql>); 15] = [
                ("message_id", Box::new(Uuid::new_v4().to_string())),
                ("attempt_number", Box::new(2i64)),
                ("turn_start_request_id", Box::new("n:99")),
                ("provider_turn_id", Box::new("turn-elsewhere")),
                ("provider_request_id", Box::new("n:98")),
                ("provider_request_kind", Box::new("other_call")),
                ("provider_request_digest", Box::new(trigger_digest("4"))),
                (
                    "delivery_model_invocation_id",
                    Box::new(Uuid::new_v4().to_string()),
                ),
                ("turn_gate_generation", Box::new(1i64)),
                ("evidence_event_id", Box::new(world.event_id + 1)),
                (
                    "evidence_event_session_id",
                    Box::new(world.owner.to_string()),
                ),
                ("evidence_event_sequence", Box::new(99i64)),
                ("handler_kind", Box::new("approval_presentation")),
                (
                    "evidence_committed_at",
                    Box::new("2027-01-01T00:00:00.000000000Z"),
                ),
                ("created_at", Box::new("2027-01-01T00:00:00.000000000Z")),
            ];
            for (column, value) in rewrites {
                refused(
                    store.conn.execute(
                        &format!(
                            "UPDATE agent_message_provider_request_effects SET {column}=?2
                             WHERE message_id=?1 AND provider_request_id=?3"
                        ),
                        params![world.message_id.to_string(), value, REQUEST_ID],
                    ),
                    "V81 provider request identity and evidence custody are immutable",
                    &format!("rewriting immutable request column {column}"),
                );
            }

            // ALLOWED: a phase column outside the frozen identity still moves.
            update_request(&store, &world, &format!("updated_at='{TRIGGER_TS}'"))
                .expect("a mutable column must still move");
        }

        /// Trigger 18/32 — `agent_message_request_effects_v81_evidence_coherence`.
        #[test]
        fn p202_agent_message_request_effects_v81_evidence_coherence() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);
            let msg = world.message_id;

            // A provider turn that is NOT the attempt's correlated boundary
            // value. A gate is opened for it so the missing link is exactly the
            // attempt/turn join and nothing else.
            open_gate(&store, msg, world.invocation_id, 0).expect_err(
                "the gate primary key already exists for this exact turn and generation",
            );
            store
                .conn
                .execute(
                    "INSERT INTO agent_message_provider_turn_gates (
                         message_id, attempt_number, delivery_model_invocation_id,
                         provider_turn_id, gate_generation, gate_state, admission_sequence,
                         issued_permit_count, settled_permit_count, opened_at, updated_at)
                     VALUES (?1,1,?2,'turn-foreign',0,'open',0,0,0,?3,?3)",
                    params![msg.to_string(), world.invocation_id.to_string(), TRIGGER_TS],
                )
                .expect("open a gate on a foreign turn");
            refused(
                insert_request_as(
                    &store,
                    msg,
                    world.invocation_id,
                    world.event_id,
                    world.target,
                    "n:31",
                    "tool_execution",
                    "turn-foreign",
                    0,
                ),
                "V81 provider request evidence requires an acknowledged correlated attempt and an open gate",
                "a request whose provider turn is not the attempt's boundary value",
            );

            // A generation with no gate at all.
            refused(
                insert_request_as(
                    &store,
                    msg,
                    world.invocation_id,
                    world.event_id,
                    world.target,
                    "n:32",
                    "tool_execution",
                    TURN,
                    7,
                ),
                "V81 provider request evidence requires an acknowledged correlated attempt and an open gate",
                "a request naming a gate generation that was never opened",
            );

            // A gate that has begun CLOSING admits nothing further. This is the
            // terminal admission fence.
            store
                .conn
                .execute(
                    "UPDATE agent_message_provider_turn_gates
                        SET gate_state='closing', terminal_authority='dispatcher',
                            terminal_evidence_digest=?3, closing_at=?4, updated_at=?4
                      WHERE message_id=?1 AND provider_turn_id=?2 AND gate_generation=0",
                    params![msg.to_string(), TURN, trigger_digest("5"), TRIGGER_TS],
                )
                .expect("open -> closing is a legal gate edge");
            refused(
                insert_request_as(
                    &store,
                    msg,
                    world.invocation_id,
                    world.event_id,
                    world.target,
                    "n:33",
                    "tool_execution",
                    TURN,
                    0,
                ),
                "V81 provider request evidence requires an acknowledged correlated attempt and an open gate",
                "a request admitted after its gate began closing",
            );

            // An attempt that is not yet acknowledged cannot carry evidence at
            // all: a fresh world stopped at effect_possible.
            let pending = scaffold_injected(&store, "p202-evidence-pending");
            store
                .conn
                .execute(
                    "INSERT INTO agent_message_provider_turn_gates (
                         message_id, attempt_number, delivery_model_invocation_id,
                         provider_turn_id, gate_generation, gate_state, admission_sequence,
                         issued_permit_count, settled_permit_count, opened_at, updated_at)
                     VALUES (?1,1,?2,?3,0,'open',0,0,0,?4,?4)",
                    params![
                        pending.message_id.to_string(),
                        pending.invocation_id.to_string(),
                        TURN,
                        TRIGGER_TS
                    ],
                )
                .expect("open a gate over the effect-possible attempt");
            refused(
                insert_request(
                    &store,
                    pending.message_id,
                    pending.invocation_id,
                    pending.event_id,
                    pending.target,
                ),
                "V81 provider request evidence requires an acknowledged correlated attempt and an open gate",
                "a request over an attempt that is only effect_possible",
            );

            // ALLOWED: the scaffold's own row was admitted through this exact
            // trigger, and a SECOND request on the same open gate is too.
            let ok = scaffold(&store);
            insert_request_as(
                &store,
                ok.message_id,
                ok.invocation_id,
                ok.event_id,
                ok.target,
                "n:34",
                "approval_presentation",
                TURN,
                0,
            )
            .expect("a second fully scaffolded request on an open gate must be admitted");
        }

        /// Trigger 19/32 — `agent_message_request_effects_v81_handler_forward`.
        #[test]
        fn p202_agent_message_request_effects_v81_handler_forward() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            // A skip. The permit that `permit_coherence` demands for
            // `handler_started` already exists, so this trigger is the only
            // layer that can object.
            refused(
                update_request(
                    &store,
                    &world,
                    &format!("handler_phase='handler_started', handler_started_at='{TRIGGER_TS}'"),
                ),
                "V81 provider request handler phase is forward-only",
                "skipping evidence_committed -> handler_started",
            );

            // ALLOWED: the exact forward edges.
            start_handler(&store, &world);

            // A rewind out of handler_started.
            for backward in ["evidence_committed", "handler_authorized"] {
                refused(
                    update_request(&store, &world, &format!("handler_phase='{backward}'")),
                    "V81 provider request handler phase is forward-only",
                    &format!("rewinding handler_started -> {backward}"),
                );
            }

            // handler_uncertain is a legal sink, and nothing leaves it.
            update_request(
                &store,
                &world,
                &format!("handler_phase='handler_uncertain', handler_uncertain_at='{TRIGGER_TS}'"),
            )
            .expect("handler_started -> handler_uncertain must be admitted");
            for onward in ["handler_started", "handler_completed", "pending_decision"] {
                refused(
                    update_request(&store, &world, &format!("handler_phase='{onward}'")),
                    "V81 provider request handler phase is forward-only",
                    &format!("retrying handler_uncertain -> {onward}"),
                );
            }
        }

        /// Trigger 20/32 — `agent_message_request_effects_v81_approval_coherence`.
        ///
        /// Approval custody is reachable ONLY on an `approval_presentation`
        /// row: a coupled CHECK forbids a `tool_execution` row from holding an
        /// approval ID, a pending decision, or a recorded decision at all. That
        /// layering is why this test builds its own request row rather than
        /// reusing the scaffold's.
        #[test]
        fn p202_agent_message_request_effects_v81_approval_coherence() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);
            const APPROVAL_REQUEST: &str = "n:40";

            insert_request_as(
                &store,
                world.message_id,
                world.invocation_id,
                world.event_id,
                world.target,
                APPROVAL_REQUEST,
                "approval_presentation",
                TURN,
                0,
            )
            .expect("an approval-presentation request row");

            let update = |set: &str| {
                store.conn.execute(
                    &format!(
                        "UPDATE agent_message_provider_request_effects SET {set}
                         WHERE message_id=?1 AND provider_request_id=?2"
                    ),
                    params![world.message_id.to_string(), APPROVAL_REQUEST],
                )
            };

            // A decision may be recorded ONLY out of a durable pending state.
            // The row is still at `evidence_committed`.
            refused(
                update(&format!(
                    "decision_kind='approved', decision_digest='{}',
                     decision_authority='operator', decision_recorded_at='{TRIGGER_TS}'",
                    trigger_digest("6")
                )),
                "V81 approval identity, presentation, and decision are fill-once",
                "recording a decision without a durable pending state",
            );

            // ALLOWED: approval identity and presentation fill once, from null.
            store
                .conn
                .execute(
                    "INSERT INTO approvals (id,session_id,tool_name,tool_input,status,created_at)
                     VALUES (?1,?2,'Bash','{}','pending',?3)",
                    params![
                        world.permit_id.to_string(),
                        world.target.to_string(),
                        TRIGGER_TS
                    ],
                )
                .expect("seed an approval row");
            update(&format!(
                "approval_id='{}', approval_presented_at='{TRIGGER_TS}'",
                world.permit_id
            ))
            .expect("approval identity and presentation must fill once from null");

            // ... and never change or clear afterwards.
            let approval_rewrites: [(&str, String); 4] = [
                ("approval_id", format!("'{}'", Uuid::new_v4())),
                ("approval_id", "NULL".to_string()),
                (
                    "approval_presented_at",
                    "'2027-01-01T00:00:00.000000000Z'".to_string(),
                ),
                ("approval_presented_at", "NULL".to_string()),
            ];
            for (column, value) in approval_rewrites {
                refused(
                    update(&format!("{column}={value}")),
                    "V81 approval identity, presentation, and decision are fill-once",
                    &format!("rewriting filled approval column {column} to {value}"),
                );
            }

            // Drive to a durable pending decision, then record exactly one.
            update(&format!(
                "handler_phase='handler_authorized', handler_capability_id='{}',
                 handler_authorized_at='{TRIGGER_TS}'",
                Uuid::new_v4()
            ))
            .expect("authorize the approval handler");
            // The permit must name THIS request's ID; `issue_permit` is bound
            // to the scaffold's `REQUEST_ID`, so it is written out here.
            store
                .conn
                .execute(
                    "INSERT INTO agent_message_provider_effect_permits (
                         permit_id, message_id, attempt_number, turn_start_request_id,
                         provider_turn_id, provider_request_id, provider_request_kind,
                         provider_request_digest, delivery_model_invocation_id,
                         turn_gate_generation, permit_kind, request_phase, permit_state,
                         executor_boot_id, external_join_id, issued_at, updated_at)
                     VALUES (?1,?2,1,?3,?4,?5,?6,?7,?8,0,'approval_presentation',
                             'handler_started','issued',?9,?10,?11,?11)",
                    params![
                        Uuid::new_v4().to_string(),
                        world.message_id.to_string(),
                        TURN_START_ID,
                        TURN,
                        APPROVAL_REQUEST,
                        REQUEST_KIND,
                        request_digest(),
                        world.invocation_id.to_string(),
                        Uuid::new_v4().to_string(),
                        Uuid::new_v4().to_string(),
                        TRIGGER_TS,
                    ],
                )
                .expect("issue the approval handler's permit");
            update(&format!(
                "handler_phase='handler_started', handler_started_at='{TRIGGER_TS}'"
            ))
            .expect("start the approval handler");
            update(&format!(
                "handler_phase='handler_completed', handler_completed_at='{TRIGGER_TS}',
                 handler_outcome_digest='{}'",
                trigger_digest("7")
            ))
            .expect("complete the approval handler");
            update(&format!(
                "handler_phase='pending_decision', pending_decision_at='{TRIGGER_TS}'"
            ))
            .expect("a durable pending decision");

            // ALLOWED: exactly one decision, out of pending_decision.
            update(&format!(
                "decision_kind='approved', decision_digest='{}',
                 decision_authority='operator', decision_recorded_at='{TRIGGER_TS}',
                 handler_phase='decision_recorded'",
                trigger_digest("8")
            ))
            .expect("one decision recorded out of a durable pending state must be admitted");

            // ... and that decision is immutable.
            for (column, value) in [
                ("decision_kind", "'denied'"),
                ("decision_authority", "'someone_else'"),
                ("decision_recorded_at", "'2027-01-01T00:00:00.000000000Z'"),
                ("pending_decision_at", "'2027-01-01T00:00:00.000000000Z'"),
            ] {
                refused(
                    update(&format!("{column}={value}")),
                    "V81 approval identity, presentation, and decision are fill-once",
                    &format!("rewriting recorded decision column {column}"),
                );
            }
        }

        /// Trigger 21/32 — `agent_message_request_effects_v81_reply_forward`.
        #[test]
        fn p202_agent_message_request_effects_v81_reply_forward() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            // Reach a completed handler so the coupled tool-reply CHECK (a tool
            // reply requires its handler outcome digest) is satisfied and the
            // reply phase is the only thing under test.
            start_handler(&store, &world);
            update_request(
                &store,
                &world,
                &format!(
                    "handler_phase='handler_completed', handler_completed_at='{TRIGGER_TS}',
                     handler_outcome_digest='{}'",
                    trigger_digest("7")
                ),
            )
            .expect("complete the handler");

            // Issue the reply permit up front so `permit_coherence` is content
            // and `reply_forward` is the only layer able to object to a skip.
            issue_permit(
                &store,
                world.message_id,
                world.invocation_id,
                Uuid::new_v4(),
                "provider_reply",
                "reply_started",
            )
            .expect("issue the reply permit");

            refused(
                update_request(
                    &store,
                    &world,
                    &format!("reply_phase='reply_started', reply_started_at='{TRIGGER_TS}'"),
                ),
                "V81 provider request reply phase is forward-only",
                "skipping not_authorized -> reply_started",
            );
            refused(
                update_request(&store, &world, "reply_phase='reply_completed'"),
                "V81 provider request reply phase is forward-only",
                "skipping straight to reply_completed",
            );

            // ALLOWED: the exact forward chain.
            update_request(
                &store,
                &world,
                &format!(
                    "reply_phase='reply_authorized', reply_capability_id='{}',
                     reply_authorized_at='{TRIGGER_TS}'",
                    Uuid::new_v4()
                ),
            )
            .expect("not_authorized -> reply_authorized must be admitted");
            update_request(
                &store,
                &world,
                &format!("reply_phase='reply_started', reply_started_at='{TRIGGER_TS}'"),
            )
            .expect("reply_authorized -> reply_started over an issued permit must be admitted");

            // A started reply never resends.
            for backward in ["not_authorized", "reply_authorized"] {
                refused(
                    update_request(&store, &world, &format!("reply_phase='{backward}'")),
                    "V81 provider request reply phase is forward-only",
                    &format!("rewinding reply_started -> {backward}"),
                );
            }

            // reply_uncertain is a sink.
            update_request(
                &store,
                &world,
                &format!("reply_phase='reply_uncertain', reply_uncertain_at='{TRIGGER_TS}'"),
            )
            .expect("reply_started -> reply_uncertain must be admitted");
            refused(
                update_request(&store, &world, "reply_phase='reply_started'"),
                "V81 provider request reply phase is forward-only",
                "retrying a reply that already sealed uncertain",
            );
        }

        /// Trigger 22/32 — `agent_message_request_effects_v81_terminal_immutable`.
        ///
        /// Also records an accurate SCOPE observation about what this trigger
        /// does and does not own: the fence a cancellation CLAIMS is owned one
        /// layer down by the named CHECK `v81_terminal_fence_matches_request`,
        /// asserted by
        /// `p202_provider_terminal_fence_must_be_the_requests_own_invocation_and_turn`.
        #[test]
        fn p202_agent_message_request_effects_v81_terminal_immutable() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);
            start_handler(&store, &world);

            // Seal the request provider-terminal cancelled. The cancellation
            // edge is legal from every nonterminal handler/reply combination
            // and must PRESERVE those phases as evidence.
            update_request(
                &store,
                &world,
                &format!(
                    "request_disposition='provider_terminal_cancelled',
                     provider_terminal_invocation_id='{}',
                     provider_terminal_turn_id='{TURN}',
                     provider_terminal_authority='dispatcher',
                     provider_terminal_evidence_digest='{}',
                     provider_terminal_sealed_at='{TRIGGER_TS}'",
                    world.invocation_id,
                    trigger_digest("9")
                ),
            )
            .expect("live -> provider_terminal_cancelled must be admitted");

            // Nothing moves afterwards: no later handler or approval action,
            // decision, reply write, phase rewrite, or terminal-evidence edit.
            //
            // The two capability columns are deliberately NOT probed here even
            // though this trigger also lists them. They are guarded jointly by
            // `..._permit_coherence`, which is UNCONDITIONAL rather than
            // terminal-only, and SQLite leaves the firing order of two BEFORE
            // UPDATE triggers on one table undefined -- so a capability rewrite
            // cannot be attributed to this trigger. `permit_coherence` owns
            // that assertion; see
            // `p202_agent_message_request_effects_v81_permit_coherence`.
            let frozen: [(&str, String); 6] = [
                ("request_disposition", "'live'".to_string()),
                ("request_disposition", "'completed'".to_string()),
                ("handler_phase", "'handler_completed'".to_string()),
                ("reply_phase", "'reply_authorized'".to_string()),
                (
                    "provider_terminal_sealed_at",
                    "'2027-01-01T00:00:00.000000000Z'".to_string(),
                ),
                ("provider_terminal_authority", "'operator'".to_string()),
            ];
            for (column, value) in frozen {
                refused(
                    update_request(&store, &world, &format!("{column}={value}")),
                    "V81 terminal provider request custody is immutable",
                    &format!("mutating sealed terminal request column {column}"),
                );
            }

            // The prior phases really were preserved as evidence.
            let (handler, reply): (String, String) = store
                .conn
                .query_row(
                    "SELECT handler_phase,reply_phase FROM agent_message_provider_request_effects
                     WHERE message_id=?1 AND provider_request_id=?2",
                    params![world.message_id.to_string(), REQUEST_ID],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("read sealed row");
            assert_eq!(
                (handler.as_str(), reply.as_str()),
                ("handler_started", "not_authorized"),
                "cancellation must preserve the phases reached before it"
            );

            // ALLOWED: a column outside the frozen set still moves, so the
            // freeze is scoped rather than total.
            update_request(
                &store,
                &world,
                "updated_at='2026-08-04T00:00:00.000000000Z'",
            )
            .expect("a non-custody column must still move on a sealed request");
        }

        /// Trigger 23/32 — `agent_message_request_effects_v81_permit_coherence`.
        #[test]
        fn p202_agent_message_request_effects_v81_permit_coherence() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            // --- the started-effect half -----------------------------------
            // A request whose handler permit was never issued may not start.
            // Built as its own row so the scaffold's permit cannot satisfy it.
            const UNPERMITTED: &str = "n:50";
            insert_request_as(
                &store,
                world.message_id,
                world.invocation_id,
                world.event_id,
                world.target,
                UNPERMITTED,
                "tool_execution",
                TURN,
                0,
            )
            .expect("a second request row with no permit of its own");
            let bare = |set: &str| {
                store.conn.execute(
                    &format!(
                        "UPDATE agent_message_provider_request_effects SET {set}
                         WHERE message_id=?1 AND provider_request_id=?2"
                    ),
                    params![world.message_id.to_string(), UNPERMITTED],
                )
            };
            bare(&format!(
                "handler_phase='handler_authorized', handler_capability_id='{}',
                 handler_authorized_at='{TRIGGER_TS}'",
                Uuid::new_v4()
            ))
            .expect("authorization needs no permit");
            refused(
                bare(&format!(
                    "handler_phase='handler_started', handler_started_at='{TRIGGER_TS}'"
                )),
                "V81 started effect requires its issued permit and a fill-once capability",
                "starting a handler with no issued permit",
            );

            // The scaffold's row DOES have its permit, so the same edge is
            // admitted there. Without this the trigger could be refusing every
            // start and still pass above.
            start_handler(&store, &world);

            // --- the fill-once capability half ------------------------------
            for column in ["handler_capability_id", "reply_capability_id"] {
                // reply_capability_id is still null on the scaffold row, so
                // fill it once first to have something to rewrite.
                if column == "reply_capability_id" {
                    update_request(
                        &store,
                        &world,
                        &format!(
                            "reply_capability_id='{}', reply_authorized_at='{TRIGGER_TS}'",
                            Uuid::new_v4()
                        ),
                    )
                    .expect("a capability fills once from null");
                }
                for value in [format!("'{}'", Uuid::new_v4()), "NULL".to_string()] {
                    refused(
                        update_request(&store, &world, &format!("{column}={value}")),
                        "V81 started effect requires its issued permit and a fill-once capability",
                        &format!("rewriting consumed capability {column} to {value}"),
                    );
                }
            }
        }

        /// H21-P2-P202-001, **CLOSED** by the named CHECK
        /// `v81_terminal_fence_matches_request`.
        ///
        /// P2-02 requires `live -> provider_terminal_cancelled` to happen
        /// "under the matching invocation and genuine turn terminal fence".
        /// Before this constraint existed, `provider_terminal_invocation_id`
        /// and `provider_terminal_turn_id` occurred in the whole V81 source
        /// only in their column declarations, their shape checks
        /// (`sql_uuid` / bounded text), and the `sql_all_or_none` terminal
        /// group -- no trigger body referenced either column, and
        /// `agent_message_request_effects_v81_terminal_immutable` only freezes
        /// a row that is ALREADY non-live, so it never inspected the fence a
        /// cancellation claims. A raw writer could seal a request cancelled
        /// under a completely unrelated invocation and turn, which is exactly
        /// the terminal-fence forgery the requirement exists to prevent.
        ///
        /// The requirement is enforced one layer DOWN from where P2-02
        /// originally described it, and P2-02 has been amended to match: fence
        /// equality is a whole-row predicate over one row's own columns, so it
        /// belongs to the declarative layer per C-P2-17's rule that trigger
        /// enforcement is for "where cross-row truth is required".
        ///
        /// This test also records the layering on the INSERT door rather than
        /// claiming it: SQLite fires BEFORE INSERT triggers ahead of CHECK
        /// constraints, so `..._evidence_coherence` -- which admits only a
        /// `live` birth -- objects first, and the coupled `live`/all-or-none
        /// CHECKs independently forbid any INSERT from carrying a fence at all.
        /// Both are asserted by their own refusals below.
        #[test]
        fn p202_provider_terminal_fence_must_be_the_requests_own_invocation_and_turn() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            const FOREIGN_TURN: &str = "turn-belonging-to-nobody";
            let foreign_invocation = Uuid::new_v4();

            let seal = |invocation: &str, turn: &str| {
                update_request(
                    &store,
                    &world,
                    &format!(
                        "request_disposition='provider_terminal_cancelled',
                         provider_terminal_invocation_id='{invocation}',
                         provider_terminal_turn_id='{turn}',
                         provider_terminal_authority='dispatcher',
                         provider_terminal_evidence_digest='{}',
                         provider_terminal_sealed_at='{TRIGGER_TS}'",
                        trigger_digest("9")
                    ),
                )
            };

            // REFUSED on UPDATE. Every forged value below is well-SHAPED -- a
            // canonical UUID and a bounded turn string -- so the shape CHECKs
            // cannot catch them and only the equality constraint can. The
            // refusal is asserted by the CONSTRAINT NAME, so a sibling CHECK,
            // an FK, or a trigger objecting first fails the test instead of
            // silently satisfying it.
            for (invocation, turn, what) in [
                (
                    foreign_invocation.to_string(),
                    TURN,
                    "sealing under a foreign invocation",
                ),
                (
                    world.invocation_id.to_string(),
                    FOREIGN_TURN,
                    "sealing under a foreign turn",
                ),
                (
                    foreign_invocation.to_string(),
                    FOREIGN_TURN,
                    "sealing under a wholly foreign fence",
                ),
            ] {
                refused(
                    seal(&invocation, turn),
                    "CHECK constraint failed: v81_terminal_fence_matches_request",
                    what,
                );
            }

            // Nothing was written: the request is still live with a null fence.
            let (disposition, invocation, turn): (String, Option<String>, Option<String>) = store
                .conn
                .query_row(
                    "SELECT request_disposition,provider_terminal_invocation_id,
                            provider_terminal_turn_id
                     FROM agent_message_provider_request_effects
                     WHERE message_id=?1 AND provider_request_id=?2",
                    params![world.message_id.to_string(), REQUEST_ID],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("read the still-live row");
            assert_eq!(
                (disposition.as_str(), invocation, turn),
                ("live", None, None),
                "a refused seal must leave the request live with no fence at all"
            );

            // The OTHER door -- a row born already sealed under a foreign fence
            // -- is closed too, but NOT by this constraint, and the layering is
            // recorded rather than engineered around.
            //
            // SQLite fires BEFORE INSERT triggers ahead of CHECK constraints,
            // and `..._evidence_coherence` refuses ANY birth whose
            // `request_disposition` is not `live`. So it objects first, by
            // name, and the assertion below says so instead of being loosened
            // to accept whichever layer happened to win.
            refused(
                store.conn.execute(
                    &format!(
                        "INSERT INTO agent_message_provider_request_effects (
                             message_id, attempt_number, turn_start_request_id,
                             provider_turn_id, provider_request_id, provider_request_kind,
                             provider_request_digest, delivery_model_invocation_id,
                             turn_gate_generation, evidence_event_id,
                             evidence_event_session_id, evidence_event_sequence,
                             handler_kind, evidence_committed_at, created_at,
                             handler_phase, reply_phase, request_disposition,
                             provider_terminal_invocation_id, provider_terminal_turn_id,
                             provider_terminal_authority, provider_terminal_evidence_digest,
                             provider_terminal_sealed_at, updated_at)
                         VALUES (?1,1,'{TURN_START_ID}','{TURN}','n:71','{REQUEST_KIND}',?2,?3,0,
                                 ?4,?5,{EVENT_SEQUENCE},'tool_execution','{TRIGGER_TS}','{TRIGGER_TS}',
                                 'evidence_committed','not_authorized','provider_terminal_cancelled',
                                 '{foreign_invocation}','{FOREIGN_TURN}','dispatcher',?6,
                                 '{TRIGGER_TS}','{TRIGGER_TS}')"
                    ),
                    params![
                        world.message_id.to_string(),
                        request_digest(),
                        world.invocation_id.to_string(),
                        world.event_id,
                        world.target.to_string(),
                        trigger_digest("9"),
                    ],
                ),
                "V81 provider request evidence requires an acknowledged correlated attempt and an open gate",
                "inserting a row born sealed under a foreign fence",
            );

            // ...and that is not the only lock on that door: a row must be born
            // `live`, a `live` row's `provider_terminal_sealed_at` must be NULL,
            // and the terminal group is all-null or all-nonnull -- so no INSERT
            // can carry ANY fence, forged or genuine. Proven, not asserted from
            // reading the DDL: the same row born live but carrying a fence is
            // refused by those coupled CHECKs.
            let born_live_with_a_fence = store.conn.execute(
                &format!(
                    "INSERT INTO agent_message_provider_request_effects (
                         message_id, attempt_number, turn_start_request_id,
                         provider_turn_id, provider_request_id, provider_request_kind,
                         provider_request_digest, delivery_model_invocation_id,
                         turn_gate_generation, evidence_event_id,
                         evidence_event_session_id, evidence_event_sequence,
                         handler_kind, evidence_committed_at, created_at,
                         handler_phase, reply_phase, request_disposition,
                         provider_terminal_invocation_id, provider_terminal_turn_id,
                         provider_terminal_authority, provider_terminal_evidence_digest,
                         provider_terminal_sealed_at, updated_at)
                     VALUES (?1,1,'{TURN_START_ID}','{TURN}','n:72','{REQUEST_KIND}',?2,?3,0,
                             ?4,?5,{EVENT_SEQUENCE},'tool_execution','{TRIGGER_TS}','{TRIGGER_TS}',
                             'evidence_committed','not_authorized','live',
                             ?3,'{TURN}','dispatcher',?6,'{TRIGGER_TS}','{TRIGGER_TS}')"
                ),
                params![
                    world.message_id.to_string(),
                    request_digest(),
                    world.invocation_id.to_string(),
                    world.event_id,
                    world.target.to_string(),
                    trigger_digest("9"),
                ],
            );
            let error = born_live_with_a_fence
                .expect_err("no INSERT may carry a terminal fence at all")
                .to_string();
            assert!(
                error.contains("CHECK constraint failed"),
                "a live birth carrying a fence must be refused by a CHECK, got {error:?}"
            );

            // ALLOWED: the request's OWN invocation and genuine turn. Without
            // this the constraint could be refusing every seal and still pass
            // every assertion above.
            seal(&world.invocation_id.to_string(), TURN)
                .expect("live -> provider_terminal_cancelled under its own fence must be admitted");
            let (invocation, turn): (String, String) = store
                .conn
                .query_row(
                    "SELECT provider_terminal_invocation_id,provider_terminal_turn_id
                     FROM agent_message_provider_request_effects
                     WHERE message_id=?1 AND provider_request_id=?2",
                    params![world.message_id.to_string(), REQUEST_ID],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("read the sealed row");
            assert_eq!(
                (invocation.as_str(), turn.as_str()),
                (world.invocation_id.to_string().as_str(), TURN),
                "the stored fence must be the request's own invocation and turn"
            );

            // The all-null/all-nonnull half of the same requirement remains
            // enforced, by the `sql_all_or_none` group rather than this
            // constraint -- recorded at its real enforcement point.
            let partial = store.conn.execute(
                "UPDATE agent_message_provider_request_effects
                    SET provider_terminal_turn_id=NULL
                  WHERE message_id=?1 AND provider_request_id=?2",
                params![world.message_id.to_string(), REQUEST_ID],
            );
            let error = partial
                .expect_err("a partially-null terminal group must be refused")
                .to_string();
            assert!(
                error.contains("V81 terminal provider request custody is immutable")
                    || error.contains("CHECK constraint failed"),
                "unexpected refusal for a partial terminal group: {error:?}"
            );
        }

        // ==================================================================
        // Exact-turn gates: agent_message_provider_turn_gates (5 triggers)
        // ==================================================================

        fn update_gate(store: &Store, world: &World, set: &str) -> rusqlite::Result<usize> {
            store.conn.execute(
                &format!(
                    "UPDATE agent_message_provider_turn_gates SET {set}
                     WHERE message_id=?1 AND provider_turn_id=?2 AND gate_generation=0"
                ),
                params![world.message_id.to_string(), TURN],
            )
        }

        /// Move the scaffold's gate to `closing`, the only terminal admission
        /// edge, filling everything the coupled CHECKs demand.
        fn begin_closing(store: &Store, world: &World) {
            update_gate(
                store,
                world,
                &format!(
                    "gate_state='closing', terminal_authority='dispatcher',
                     terminal_evidence_digest='{}', closing_at='{TRIGGER_TS}',
                     updated_at='{TRIGGER_TS}'",
                    trigger_digest("5")
                ),
            )
            .expect("open -> closing must be admitted");
        }

        /// Trigger 24/32 — `agent_message_turn_gates_v81_no_delete`.
        #[test]
        fn p202_agent_message_turn_gates_v81_no_delete() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            for (sql, what) in [
                (
                    "DELETE FROM agent_message_provider_turn_gates WHERE message_id=?1",
                    "deleting one exact-turn gate",
                ),
                (
                    "DELETE FROM agent_message_provider_turn_gates WHERE ?1 IS NOT NULL",
                    "sweeping every gate",
                ),
            ] {
                refused(
                    store
                        .conn
                        .execute(sql, params![world.message_id.to_string()]),
                    "agent message provider turn gates are append-only",
                    what,
                );
            }
        }

        /// Trigger 25/32 — `agent_message_turn_gates_v81_identity_immutable`.
        #[test]
        fn p202_agent_message_turn_gates_v81_identity_immutable() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            let rewrites: [(&str, Box<dyn rusqlite::ToSql>); 6] = [
                ("message_id", Box::new(Uuid::new_v4().to_string())),
                ("attempt_number", Box::new(2i64)),
                (
                    "delivery_model_invocation_id",
                    Box::new(Uuid::new_v4().to_string()),
                ),
                ("provider_turn_id", Box::new("turn-elsewhere")),
                ("gate_generation", Box::new(1i64)),
                ("opened_at", Box::new("2027-01-01T00:00:00.000000000Z")),
            ];
            for (column, value) in rewrites {
                refused(
                    store.conn.execute(
                        &format!(
                            "UPDATE agent_message_provider_turn_gates SET {column}=?2
                             WHERE message_id=?1 AND gate_generation=0"
                        ),
                        params![world.message_id.to_string(), value],
                    ),
                    "V81 exact-turn gate identity is immutable",
                    &format!("rewriting immutable gate column {column}"),
                );
            }

            // ALLOWED: a mutable column still moves.
            update_gate(
                &store,
                &world,
                "updated_at='2026-08-04T00:00:00.000000000Z'",
            )
            .expect("a mutable gate column must still move");
        }

        /// Trigger 26/32 — `agent_message_turn_gates_v81_forward`.
        #[test]
        fn p202_agent_message_turn_gates_v81_forward() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            // Resolve the scaffold's permit first. Otherwise
            // `..._close_requires_settled_permits` would object to any close
            // and the refusal could not be attributed to THIS trigger.
            store
                .conn
                .execute(
                    "UPDATE agent_message_provider_effect_permits
                        SET permit_state='completed', completion_evidence_digest=?2,
                            settled_at=?3, updated_at=?3
                      WHERE permit_id=?1",
                    params![world.permit_id.to_string(), trigger_digest("c"), TRIGGER_TS],
                )
                .expect("settle the permit so the close guard is content");

            // open -> closed skips the terminal admission edge. Every coupled
            // CHECK is satisfied (counts are equal at zero and the terminal
            // group is filled) and no permit is unresolved, so this trigger is
            // the only objector.
            refused(
                update_gate(
                    &store,
                    &world,
                    &format!(
                        "gate_state='closed', terminal_authority='dispatcher',
                         terminal_evidence_digest='{}', closing_at='{TRIGGER_TS}',
                         closed_at='{TRIGGER_TS}', updated_at='{TRIGGER_TS}'",
                        trigger_digest("5")
                    ),
                ),
                "V81 exact-turn gate state is forward-only",
                "skipping open -> closed without closing",
            );

            // ALLOWED: open -> closing -> closed.
            begin_closing(&store, &world);
            update_gate(
                &store,
                &world,
                &format!("gate_state='closed', closed_at='{TRIGGER_TS}'"),
            )
            .expect("closing -> closed with no unresolved permit must be admitted");

            // A closed gate never reopens.
            for backward in ["open", "closing"] {
                refused(
                    update_gate(&store, &world, &format!("gate_state='{backward}'")),
                    "V81 exact-turn gate state is forward-only",
                    &format!("reopening closed -> {backward}"),
                );
            }
        }

        /// Trigger 27/32 — `agent_message_turn_gates_v81_admission_coherence`.
        ///
        /// The counters are transaction-derived, never caller-authored: they
        /// are monotonic, and they may advance ONLY while the gate is open, so
        /// `closing` blocks every later request insert, capability mint, and
        /// effect start.
        #[test]
        fn p202_agent_message_turn_gates_v81_admission_coherence() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            // ALLOWED first: while open, the counters advance.
            update_gate(
                &store,
                &world,
                "admission_sequence=1, issued_permit_count=1, updated_at='2026-08-04T00:00:00.000000000Z'",
            )
            .expect("counters must advance while the gate is open");
            update_gate(&store, &world, "settled_permit_count=1")
                .expect("settled may catch up to issued while open");

            // Monotonicity: no counter ever decreases.
            for column in [
                "admission_sequence",
                "issued_permit_count",
                "settled_permit_count",
            ] {
                refused(
                    update_gate(&store, &world, &format!("{column}=0")),
                    "V81 gate admission and permit counters are monotonic and open-only",
                    &format!("rewinding counter {column}"),
                );
            }

            // Once closing, admission and issuance are frozen.
            begin_closing(&store, &world);
            for set in ["admission_sequence=2", "issued_permit_count=2"] {
                refused(
                    update_gate(&store, &world, set),
                    "V81 gate admission and permit counters are monotonic and open-only",
                    &format!("advancing {set} after the gate began closing"),
                );
            }

            // The terminal authority is equally frozen once not open.
            refused(
                update_gate(&store, &world, "terminal_authority='operator'"),
                "V81 gate admission and permit counters are monotonic and open-only",
                "rewriting the terminal authority of a closing gate",
            );

            // ALLOWED even while closing: settlements still land, which is the
            // whole point of a drain phase.
            update_gate(&store, &world, "settled_permit_count=1")
                .expect("a settlement must still be recordable while closing");
        }

        /// Trigger 28/32 — `agent_message_turn_gates_v81_close_requires_settled_permits`.
        #[test]
        fn p202_agent_message_turn_gates_v81_close_requires_settled_permits() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            // The scaffold issued exactly one permit, still `issued`. Record it
            // on the gate's counters the way the production CAS would.
            update_gate(
                &store,
                &world,
                "issued_permit_count=1, admission_sequence=1",
            )
            .expect("record the issued permit");
            begin_closing(&store, &world);

            // Counts alone are not enough: even claiming settled==issued, the
            // trigger re-derives the truth from the permit rows themselves.
            refused(
                update_gate(
                    &store,
                    &world,
                    &format!(
                        "gate_state='closed', settled_permit_count=1, closed_at='{TRIGGER_TS}'"
                    ),
                ),
                "V81 gate cannot close while a started effect permit is unresolved",
                "closing over a permit still in `issued` while claiming the counts agree",
            );

            // An `uncertain_unjoined` permit is equally unresolved: an
            // unjoined uncertainty is exactly the state a restart must still
            // join, so it may not be closed over either.
            store
                .conn
                .execute(
                    "UPDATE agent_message_provider_effect_permits
                        SET permit_state='uncertain_unjoined',
                            uncertainty_evidence_digest=?2, settled_at=?3, updated_at=?3
                      WHERE permit_id=?1",
                    params![world.permit_id.to_string(), trigger_digest("a"), TRIGGER_TS],
                )
                .expect("issued -> uncertain_unjoined is a legal permit edge");
            refused(
                update_gate(
                    &store,
                    &world,
                    &format!(
                        "gate_state='closed', settled_permit_count=1, closed_at='{TRIGGER_TS}'"
                    ),
                ),
                "V81 gate cannot close while a started effect permit is unresolved",
                "closing over an `uncertain_unjoined` permit",
            );

            // ALLOWED: once the permit reaches a genuinely resolved state, the
            // gate closes.
            store
                .conn
                .execute(
                    "UPDATE agent_message_provider_effect_permits
                        SET permit_state='uncertain_joined', join_evidence_digest=?2,
                            updated_at=?3
                      WHERE permit_id=?1",
                    params![world.permit_id.to_string(), trigger_digest("b"), TRIGGER_TS],
                )
                .expect("uncertain_unjoined -> uncertain_joined is a legal permit edge");
            update_gate(
                &store,
                &world,
                &format!("gate_state='closed', settled_permit_count=1, closed_at='{TRIGGER_TS}'"),
            )
            .expect("a gate with every permit resolved must close");
        }

        // ==================================================================
        // Started-effect permits: agent_message_provider_effect_permits
        // (4 triggers)
        // ==================================================================

        fn update_permit(store: &Store, world: &World, set: &str) -> rusqlite::Result<usize> {
            store.conn.execute(
                &format!(
                    "UPDATE agent_message_provider_effect_permits SET {set} WHERE permit_id=?1"
                ),
                params![world.permit_id.to_string()],
            )
        }

        /// Trigger 29/32 — `agent_message_effect_permits_v81_no_delete`.
        #[test]
        fn p202_agent_message_effect_permits_v81_no_delete() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            for (sql, what) in [
                (
                    "DELETE FROM agent_message_provider_effect_permits WHERE permit_id=?1",
                    "deleting a started-effect permit",
                ),
                (
                    "DELETE FROM agent_message_provider_effect_permits WHERE ?1 IS NOT NULL",
                    "sweeping every permit",
                ),
            ] {
                refused(
                    store
                        .conn
                        .execute(sql, params![world.permit_id.to_string()]),
                    "agent message provider effect permits are append-only",
                    what,
                );
            }
        }

        /// Trigger 30/32 — `agent_message_effect_permits_v81_identity_immutable`.
        #[test]
        fn p202_agent_message_effect_permits_v81_identity_immutable() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            let rewrites: [(&str, Box<dyn rusqlite::ToSql>); 15] = [
                ("permit_id", Box::new(Uuid::new_v4().to_string())),
                ("message_id", Box::new(Uuid::new_v4().to_string())),
                ("attempt_number", Box::new(2i64)),
                ("turn_start_request_id", Box::new("n:97")),
                ("provider_turn_id", Box::new("turn-elsewhere")),
                ("provider_request_id", Box::new("n:96")),
                ("provider_request_kind", Box::new("other_call")),
                ("provider_request_digest", Box::new(trigger_digest("4"))),
                (
                    "delivery_model_invocation_id",
                    Box::new(Uuid::new_v4().to_string()),
                ),
                ("turn_gate_generation", Box::new(1i64)),
                ("permit_kind", Box::new("provider_reply")),
                ("request_phase", Box::new("reply_started")),
                ("executor_boot_id", Box::new(Uuid::new_v4().to_string())),
                // The two join handles a restart depends on.
                ("external_join_id", Box::new(Uuid::new_v4().to_string())),
                ("issued_at", Box::new("2027-01-01T00:00:00.000000000Z")),
            ];
            for (column, value) in rewrites {
                refused(
                    store.conn.execute(
                        &format!(
                            "UPDATE agent_message_provider_effect_permits SET {column}=?2
                             WHERE permit_id=?1"
                        ),
                        params![world.permit_id.to_string(), value],
                    ),
                    "V81 effect permit identity and join handles are immutable",
                    &format!("rewriting immutable permit column {column}"),
                );
            }

            // `operation_id` is fill-once rather than frozen: null until a
            // provider write claims one, immutable afterwards. Both halves are
            // asserted because `IS NOT NULL AND ... IS NOT ...` is exactly the
            // shape a naive `!=` guard would get wrong.
            update_permit(&store, &world, "permit_kind=permit_kind, operation_id=NULL")
                .expect("a null operation_id may stay null");

            // A tool-execution permit may not hold a writer operation at all
            // (coupled CHECK), so the fill-once half is asserted on a reply
            // permit, which is the only kind that may.
            let reply_permit = Uuid::new_v4();
            issue_permit(
                &store,
                world.message_id,
                world.invocation_id,
                reply_permit,
                "provider_reply",
                "reply_started",
            )
            .expect("issue a reply permit");
            let operation = Uuid::new_v4();
            store
                .conn
                .execute(
                    "UPDATE agent_message_provider_effect_permits SET operation_id=?2
                     WHERE permit_id=?1",
                    params![reply_permit.to_string(), operation.to_string()],
                )
                .expect("a writer operation fills once from null");
            for value in [Uuid::new_v4().to_string(), String::new()] {
                let target: Box<dyn rusqlite::ToSql> = if value.is_empty() {
                    Box::new(Option::<String>::None)
                } else {
                    Box::new(value.clone())
                };
                refused(
                    store.conn.execute(
                        "UPDATE agent_message_provider_effect_permits SET operation_id=?2
                         WHERE permit_id=?1",
                        params![reply_permit.to_string(), target],
                    ),
                    "V81 effect permit identity and join handles are immutable",
                    "rewriting or clearing a filled writer operation_id",
                );
            }
        }

        /// Trigger 31/32 — `agent_message_effect_permits_v81_forward`.
        ///
        /// Note on the trigger body: its `WHEN` reads
        /// `A AND B OR (C AND D)` with no parentheses around the first
        /// conjunction. That is NOT a defect — SQL binds `AND` tighter than
        /// `OR`, so it groups as `(A AND B) OR (C AND D)` exactly as intended.
        /// The runtime behaviour asserted below is what proves it.
        #[test]
        fn p202_agent_message_effect_permits_v81_forward() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            // ALLOWED: issued -> uncertain_unjoined.
            update_permit(
                &store,
                &world,
                &format!(
                    "permit_state='uncertain_unjoined', uncertainty_evidence_digest='{}',
                     settled_at='{TRIGGER_TS}'",
                    trigger_digest("a")
                ),
            )
            .expect("issued -> uncertain_unjoined must be admitted");

            // A settled permit never returns to `issued`. That invariant is
            // owned JOINTLY and is recorded here as the layering it actually
            // is: the coupled CHECK `(permit_state='issued')=(settled_at IS
            // NULL)` forces any resurrection to also clear `settled_at`, and
            // clearing `settled_at` is refused by
            // `..._evidence_coherence`'s fill-once guard. This trigger's own
            // enum map has no edge INTO `issued` either, but no single write
            // can reach it in isolation, so the observed refusal is asserted
            // for what it is rather than mislabelled.
            let resurrect = update_permit(&store, &world, "permit_state='issued', settled_at=NULL")
                .expect_err("a settled permit must never return to issued");
            assert!(
                resurrect
                    .to_string()
                    .contains("V81 effect permit settlement evidence is fill-once"),
                "unexpected refusal for a permit resurrection: {resurrect}"
            );

            // ALLOWED: the second disjunct — an unjoined uncertainty may
            // acquire exact join evidence. This is the branch that proves the
            // unparenthesised `OR` groups as intended.
            update_permit(
                &store,
                &world,
                &format!(
                    "permit_state='uncertain_joined', join_evidence_digest='{}'",
                    trigger_digest("b")
                ),
            )
            .expect("uncertain_unjoined -> uncertain_joined must be admitted");

            // Nothing moves out of a joined uncertainty.
            for onward in [
                "completed",
                "cancel_confirmed",
                "uncertain_unjoined",
                "issued",
            ] {
                refused(
                    update_permit(&store, &world, &format!("permit_state='{onward}'")),
                    "V81 effect permit state is forward-only and settles once",
                    &format!("moving uncertain_joined -> {onward}"),
                );
            }

            // A fresh permit takes the other first-disjunct branches.
            let second = Uuid::new_v4();
            issue_permit(
                &store,
                world.message_id,
                world.invocation_id,
                second,
                "provider_reply",
                "reply_started",
            )
            .expect("issue a second permit");
            store
                .conn
                .execute(
                    "UPDATE agent_message_provider_effect_permits
                        SET permit_state='completed', completion_evidence_digest=?2,
                            settled_at=?3
                      WHERE permit_id=?1",
                    params![second.to_string(), trigger_digest("c"), TRIGGER_TS],
                )
                .expect("issued -> completed must be admitted");
            refused(
                store.conn.execute(
                    "UPDATE agent_message_provider_effect_permits
                        SET permit_state='uncertain_unjoined' WHERE permit_id=?1",
                    params![second.to_string()],
                ),
                "V81 effect permit state is forward-only and settles once",
                "moving a completed permit to uncertainty",
            );
        }

        /// Trigger 32/32 — `agent_message_effect_permits_v81_evidence_coherence`.
        #[test]
        fn p202_agent_message_effect_permits_v81_evidence_coherence() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            // A reply permit, the only kind that may carry a writer receipt.
            let permit = Uuid::new_v4();
            issue_permit(
                &store,
                world.message_id,
                world.invocation_id,
                permit,
                "provider_reply",
                "reply_started",
            )
            .expect("issue a reply permit");
            let set = |sql: &str| {
                store.conn.execute(
                    &format!(
                        "UPDATE agent_message_provider_effect_permits SET {sql} WHERE permit_id=?1"
                    ),
                    params![permit.to_string()],
                )
            };

            // ALLOWED: every settlement-evidence field fills once from null.
            let operation = Uuid::new_v4();
            set(&format!(
                "operation_id='{operation}', writer_receipt_state='write_flushed',
                 writer_receipt_digest='{}', writer_receipt_at='{TRIGGER_TS}'",
                trigger_digest("d")
            ))
            .expect("a writer receipt fills once from null");
            set(&format!("cancel_requested_at='{TRIGGER_TS}'"))
                .expect("a cancellation request fills once from null");
            set(&format!(
                "permit_state='cancel_confirmed', cancel_confirmed_at='{TRIGGER_TS}',
                 cancel_evidence_digest='{}', settled_at='{TRIGGER_TS}'",
                trigger_digest("e")
            ))
            .expect("a confirmed cancellation settles the permit");

            // ... and never changes or clears afterwards. The clearing half is
            // the one a `!=` guard would miss, so both are probed.
            let frozen = [
                "writer_receipt_state",
                "writer_receipt_digest",
                "writer_receipt_at",
                "cancel_requested_at",
                "cancel_confirmed_at",
                "cancel_evidence_digest",
                "settled_at",
            ];
            for column in frozen {
                for value in ["NULL", "'2027-01-01T00:00:00.000000000Z'"] {
                    // Only feed a plausibly-typed value to each column.
                    let candidate = match (column, value) {
                        (_, "NULL") => "NULL".to_string(),
                        ("writer_receipt_state", _) => "'write_failed'".to_string(),
                        ("writer_receipt_digest" | "cancel_evidence_digest", _) => {
                            format!("'{}'", trigger_digest("f"))
                        }
                        _ => value.to_string(),
                    };
                    refused(
                        set(&format!("{column}={candidate}")),
                        "V81 effect permit settlement evidence is fill-once",
                        &format!("rewriting settled evidence {column} to {candidate}"),
                    );
                }
            }

            // completion and join digests are frozen on their own permits.
            let completed = Uuid::new_v4();
            issue_permit(
                &store,
                world.message_id,
                world.invocation_id,
                completed,
                "approval_presentation",
                "handler_started",
            )
            .expect("issue a third permit");
            store
                .conn
                .execute(
                    "UPDATE agent_message_provider_effect_permits
                        SET permit_state='completed', completion_evidence_digest=?2,
                            settled_at=?3
                      WHERE permit_id=?1",
                    params![completed.to_string(), trigger_digest("c"), TRIGGER_TS],
                )
                .expect("completion evidence fills once");
            for value in [format!("'{}'", trigger_digest("0")), "NULL".to_string()] {
                refused(
                    store.conn.execute(
                        &format!(
                            "UPDATE agent_message_provider_effect_permits
                                SET completion_evidence_digest={value} WHERE permit_id=?1"
                        ),
                        params![completed.to_string()],
                    ),
                    "V81 effect permit settlement evidence is fill-once",
                    &format!("rewriting completion evidence to {value}"),
                );
            }
        }

        // ==================================================================
        // P2-02 catalog evidence
        // ==================================================================

        /// P2-02 requires a reopen through the PRODUCTION `Store::open()`
        /// proving catalog, data, and `user_version` identity.
        ///
        /// The inherited
        /// `agent_coordination_v80_fresh_v79_failpoints_reopen_and_fingerprint`
        /// covers the catalog and version halves on an EMPTY store. This adds
        /// the data half over a fully populated V81 world: every row of all six
        /// Phase 2 tables must survive a real close/reopen byte-identically,
        /// with the catalog fingerprint and `user_version` unmoved and
        /// referential integrity still clean.
        #[test]
        fn p202_full_world_survives_production_reopen_with_identical_catalog_data_and_version() {
            let directory = tempfile::tempdir().expect("temp dir");
            let path = directory.path().join("v81-world.sqlite");

            /// Serialise every Phase 2 table in a deterministic order.
            fn snapshot(store: &Store) -> Vec<String> {
                let mut rows = Vec::new();
                for table in AGENT_MESSAGE_V81_TABLES {
                    let columns: Vec<String> = {
                        let mut stmt = store
                            .conn
                            .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
                            .expect("table_info");
                        let names = stmt
                            .query_map([], |row| row.get::<_, String>(0))
                            .expect("columns")
                            .collect::<rusqlite::Result<Vec<_>>>()
                            .expect("columns");
                        names
                    };
                    let projection = columns
                        .iter()
                        .map(|c| format!("quote({c})"))
                        .collect::<Vec<_>>()
                        .join("||'|'||");
                    let mut stmt = store
                        .conn
                        .prepare(&format!("SELECT {projection} FROM {table} ORDER BY 1"))
                        .expect("prepare projection");
                    let table_rows = stmt
                        .query_map([], |row| row.get::<_, String>(0))
                        .expect("project rows")
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .expect("project rows");
                    for row in table_rows {
                        rows.push(format!("{table}:{row}"));
                    }
                }
                rows
            }

            let (before, fingerprint_before, v82_fingerprint_before, version_before) = {
                let store = Store::open(&path).expect("production open");
                let world = scaffold(&store);
                // Exercise more of the world so the snapshot is not trivial.
                start_handler(&store, &world);
                seal_acknowledged(&store, world.message_id);
                (
                    snapshot(&store),
                    v81_schema_fingerprint(&store.conn).expect("fingerprint"),
                    accepted_v82_fingerprint_at_head(&store),
                    store
                        .conn
                        .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                        .expect("user_version"),
                )
            };

            assert_eq!(v82_fingerprint_before, AGENT_MESSAGE_V82_PINNED_FINGERPRINT);
            assert_eq!(
                version_before,
                i64::from(crate::store::LATEST_SCHEMA_VERSION)
            );
            assert!(
                before.len() >= 8,
                "the snapshot must be non-trivial, got {} rows",
                before.len()
            );

            // A real close/reopen through the production path. The migration
            // must NOT re-run and must NOT rewrite anything.
            let reopened = Store::open(&path).expect("production reopen");

            assert_eq!(
                v81_schema_fingerprint(&reopened.conn).expect("fingerprint"),
                fingerprint_before,
                "the catalog changed across a reopen"
            );
            assert_eq!(
                accepted_v82_fingerprint_at_head(&reopened),
                v82_fingerprint_before,
                "the accepted V82 mailbox catalog changed across a later-head reopen"
            );
            assert_eq!(
                reopened
                    .conn
                    .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
                    .expect("user_version"),
                version_before,
                "user_version moved across a reopen"
            );
            assert_eq!(
                snapshot(&reopened),
                before,
                "V81 row data was not preserved byte-identically across a reopen"
            );

            let mut stmt = reopened
                .conn
                .prepare("PRAGMA foreign_key_check")
                .expect("prepare foreign_key_check");
            let violations: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .expect("run foreign_key_check")
                .collect::<rusqlite::Result<Vec<_>>>()
                .expect("drain foreign_key_check");
            assert!(
                violations.is_empty(),
                "the reopened store has FK violations: {violations:?}"
            );
        }

        /// FINDING (H21-P2-P202-002), recorded as an executable characterisation
        /// rather than a fix.
        ///
        /// P2-02's evidence list asks that "counter changes that do not
        /// correspond to a unique permit insertion or first settlement" be
        /// rejected, and C-P2-17 states that the gate's issued/settled counts
        /// and admission sequence are "transaction-derived counters, never
        /// caller-authored".
        ///
        /// Half of that IS trigger-owned, and is asserted in
        /// `p202_agent_message_request_effects_v81_permit_coherence`: a
        /// `*_started` phase transition must have inserted its unique permit.
        /// The other half — that the COUNTER move matches a real permit row —
        /// is not enforced by any V81 trigger. `..._admission_coherence`
        /// enforces only monotonicity and open-only advancement, and the
        /// coupled CHECK enforces only `settled<=issued`. Nothing compares
        /// either counter against `agent_message_provider_effect_permits`.
        ///
        /// So at the schema layer the counters ARE caller-authored: a writer
        /// may inflate `issued_permit_count` arbitrarily. The practical blast
        /// radius is bounded — `..._close_requires_settled_permits` re-derives
        /// the unresolved-permit truth from the permit rows themselves, so an
        /// inflated count cannot let a gate close over live work; it can only
        /// wedge the gate shut. Making the counters genuinely transaction-
        /// derived is therefore a Store-layer obligation (P2-04 and later), not
        /// something the frozen V81 catalog delivers.
        ///
        /// Asserted here so the boundary is explicit rather than assumed.
        #[test]
        fn p202_finding_gate_counters_are_not_reconciled_against_permit_rows() {
            let store = Store::open_in_memory().unwrap();
            let world = scaffold(&store);

            let permits: i64 = store
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM agent_message_provider_effect_permits",
                    [],
                    |r| r.get(0),
                )
                .expect("count permits");
            assert_eq!(permits, 1, "the scaffold issued exactly one permit");

            // An `issued_permit_count` wildly divorced from reality is admitted.
            update_gate(
                &store,
                &world,
                "issued_permit_count=99, admission_sequence=99",
            )
            .expect("characterising the gap: an unbacked counter move is admitted today");

            // The bound that makes this survivable: the close guard re-derives
            // the truth from the permit rows, so the inflated count cannot be
            // used to close over the still-`issued` permit.
            begin_closing(&store, &world);
            refused(
                update_gate(
                    &store,
                    &world,
                    &format!(
                        "gate_state='closed', settled_permit_count=99, closed_at='{TRIGGER_TS}'"
                    ),
                ),
                "V81 gate cannot close while a started effect permit is unresolved",
                "closing a gate whose counters were inflated past its real permits",
            );
        }
    }

    // ---- C-P2-18 reserved-child failure settlement -------------------------

    /// Reserve one live spawn request and return `(owner, epic, child, request_id)`.
    ///
    /// Built through the real `reserve_agent_spawn_request` writer so the rows
    /// this settlement acts on are the same shape production produces.
    fn reserved_spawn(store: &Store, key: &str) -> (Uuid, Uuid, Uuid, Uuid) {
        let owner = Uuid::new_v4();
        let epic_id = Uuid::new_v4();
        let child_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();

        let mut owner_row = test_session(owner, std::path::PathBuf::from("/tmp"));
        owner_row.status = SessionStatus::Running;
        store.insert_session(&owner_row).expect("insert owner");
        let mut epic = test_session(epic_id, std::path::PathBuf::from("/tmp"));
        epic.session_kind = SessionKind::Epic;
        epic.lead_session_id = Some(owner);
        store.insert_session(&epic).expect("insert epic");

        let request = AgentSpawnChildRequestV1 {
            kind: SessionKind::Task,
            provider: None,
            model: None,
            effort: None,
            query: "work".into(),
            agent_role: None,
            topology_node: None,
            iteration: None,
            tags: None,
            idempotency_key: key.into(),
        };
        store
            .reserve_agent_spawn_request(
                owner,
                &crate::model_control::hash_request_fingerprint(&[
                    "agent-spawn-idempotency-v1",
                    &owner.to_string(),
                    &request.idempotency_key,
                ]),
                &crate::model_control::hash_request_fingerprint(&[
                    "agent-spawn-request-v1",
                    &owner.to_string(),
                    &serde_json::to_string(&request).expect("serialize request"),
                ]),
                &request,
                epic_id,
                request_id,
                child_id,
            )
            .expect("reserve spawn");
        (owner, epic_id, child_id, request_id)
    }

    fn message_state(store: &Store, message_id: Uuid) -> (String, i64) {
        store
            .conn
            .query_row(
                "SELECT state, state_version FROM agent_messages WHERE id=?1",
                params![message_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("read message state")
    }

    /// The headline C-P2-18 property: a permanently failed reservation leaves
    /// NO message queued behind it, and every settled row carries a real
    /// `spawn_settlement` transition rather than a silent aggregate rewrite.
    #[test]
    fn spawn_settlement_fails_queued_mail_and_leaves_nothing_behind_a_dead_reservation() {
        let store = Store::open_in_memory().expect("open V81 store");
        let (owner, _epic, child, request_id) = reserved_spawn(&store, "settle-queued");

        let first = store
            .accept_agent_message(owner, Some(request_id), &send_request(child, "m1", "one"))
            .expect("accept first")
            .receipt()
            .message_id;
        let second = store
            .accept_agent_message(owner, Some(request_id), &send_request(child, "m2", "two"))
            .expect("accept second")
            .receipt()
            .message_id;

        let authority = Uuid::new_v4();
        let outcome = store
            .settle_reserved_child_failure(request_id, "spawn_channel_closed", authority)
            .expect("settle reserved child failure");

        assert!(!outcome.already_failed);
        assert_eq!(outcome.child_session_id, child);
        assert_eq!(outcome.rows.len(), 2);
        for row in &outcome.rows {
            assert_eq!(row.disposition, SpawnSettlementDisposition::QueuedFailed);
        }

        // The reservation itself is durably failed.
        assert_eq!(
            store
                .get_agent_spawn_request(request_id)
                .expect("read spawn")
                .expect("spawn row")
                .state,
            AgentSpawnStateV1::Failed
        );

        // No mail survives queued behind a dead reservation.
        let still_queued: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM agent_messages
                  WHERE target_spawn_request_id=?1 AND state='queued'",
                params![request_id.to_string()],
                |row| row.get(0),
            )
            .expect("count queued");
        assert_eq!(still_queued, 0, "mail must not outlive its reservation");

        for message_id in [first, second] {
            let (state, version) = message_state(&store, message_id);
            assert_eq!(state, "failed");
            assert_eq!(version, 1, "exactly one settlement edge was authored");

            // The evidence edge exists, is attributed to spawn settlement, and
            // carries no attempt — nothing was ever delivered.
            let (authority_kind, authority_id, attempt): (String, String, Option<i64>) = store
                .conn
                .query_row(
                    "SELECT authority_kind, authority_id, attempt_number
                       FROM agent_message_state_transitions
                      WHERE message_id=?1 AND state_version=1",
                    params![message_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .expect("read settlement transition");
            assert_eq!(authority_kind, "spawn_settlement");
            assert_eq!(authority_id, authority.to_string());
            assert_eq!(attempt, None);
        }
    }

    /// Uncertainty erasure is the failure class this schema exists to prevent.
    ///
    /// A `claimed` attempt that never recorded admission CANNOT prove absence
    /// of effect, so settlement must NOT hand it a clean `failed`. It becomes
    /// `uncertain` with conservative effect-possible evidence and retained
    /// invocation custody.
    #[test]
    fn spawn_settlement_never_downgrades_an_unprovable_attempt_to_a_clean_failure() {
        let store = Store::open_in_memory().expect("open V81 store");
        let (owner, _epic, child, request_id) = reserved_spawn(&store, "settle-unprovable");

        let message_id = store
            .accept_agent_message(owner, Some(request_id), &send_request(child, "m1", "one"))
            .expect("accept")
            .receipt()
            .message_id;

        // Scaffold a claimed attempt with NO admission or effect evidence, the
        // exact shape a crash between claim and admission leaves behind.
        let invocation_id = Uuid::new_v4();
        store
            .conn
            .execute(
                "INSERT INTO model_invocations
                 (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,provider,model,backend,model_tier,effort,trigger_source,session_id,created_at)
                 VALUES (?1,'agent.deliver','orchestration','foreground','paid_capable','admitted','running','Claude','claude-sonnet-5','Claude','premium','high','test',?2,?3)",
                params![invocation_id.to_string(), owner.to_string(), TRIGGER_TS],
            )
            .expect("seed invocation");
        store
            .conn
            .execute(
                "INSERT INTO agent_message_delivery_attempts
                 (message_id,attempt_number,claim_token,delivery_boot_id,logical_root_session_id,
                  delivery_session_id,delivery_session_generation,delivery_model_invocation_id,
                  provider_kind,capability_kind,boundary_kind,claimed_at,claim_expires_at,
                  correlation_state,attempt_state,updated_at)
                 VALUES (?1,1,?2,?3,?4,?5,0,?6,'claude_cli','terminal_one_turn','model_invocation',
                         ?7,?7,'not_applicable','claimed',?7)",
                params![
                    message_id.to_string(),
                    Uuid::new_v4().to_string(),
                    Uuid::new_v4().to_string(),
                    owner.to_string(),
                    owner.to_string(),
                    invocation_id.to_string(),
                    TRIGGER_TS,
                ],
            )
            .expect("seed claimed attempt");
        store
            .conn
            .execute(
                "INSERT INTO agent_message_state_transitions
                 (message_id,state_version,from_state,to_state,attempt_number,
                  authority_kind,authority_id,evidence_digest,created_at)
                 VALUES (?1,1,'queued','claimed',1,'dispatcher',?2,?3,?4)",
                params![
                    message_id.to_string(),
                    Uuid::new_v4().to_string(),
                    trigger_digest("a"),
                    TRIGGER_TS,
                ],
            )
            .expect("seed claim transition");
        store
            .conn
            .execute(
                "UPDATE agent_messages
                    SET state='claimed', state_version=1, attempt_count=1,
                        current_attempt_number=1, updated_at=?2
                  WHERE id=?1",
                params![message_id.to_string(), TRIGGER_TS],
            )
            .expect("advance aggregate to claimed");

        let outcome = store
            .settle_reserved_child_failure(request_id, "spawn_channel_closed", Uuid::new_v4())
            .expect("settle reserved child failure");

        assert_eq!(outcome.rows.len(), 1);
        assert_eq!(
            outcome.rows[0].disposition,
            SpawnSettlementDisposition::EffectPossibleUncertain,
            "an unprovable attempt must never be reported as a clean failure"
        );

        let (state, version) = message_state(&store, message_id);
        assert_eq!(
            state, "uncertain",
            "settlement must not erase uncertainty by failing an unprovable attempt"
        );
        assert_eq!(version, 2);

        // Conservative evidence was filled, and invocation custody is retained.
        let (attempt_state, admission, effect, invocation): (
            String,
            Option<String>,
            Option<String>,
            String,
        ) = store
            .conn
            .query_row(
                "SELECT attempt_state, admission_classification, effect_classification,
                        delivery_model_invocation_id
                   FROM agent_message_delivery_attempts
                  WHERE message_id=?1 AND attempt_number=1",
                params![message_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("read attempt");
        assert_eq!(attempt_state, "effect_possible");
        assert_eq!(admission.as_deref(), Some("admitted_effect_possible"));
        assert_eq!(effect.as_deref(), Some("effect_possible"));
        assert_eq!(
            invocation,
            invocation_id.to_string(),
            "invocation custody must be retained, not cleared"
        );
    }

    /// Exact replay is read-only: a second settlement of an already-failed
    /// reservation settles nothing and rewrites no evidence.
    #[test]
    fn spawn_settlement_replay_is_read_only_and_settles_nothing_twice() {
        let store = Store::open_in_memory().expect("open V81 store");
        let (owner, _epic, child, request_id) = reserved_spawn(&store, "settle-replay");
        let message_id = store
            .accept_agent_message(owner, Some(request_id), &send_request(child, "m1", "one"))
            .expect("accept")
            .receipt()
            .message_id;

        store
            .settle_reserved_child_failure(request_id, "spawn_channel_closed", Uuid::new_v4())
            .expect("first settlement");
        let (_, version_after_first) = message_state(&store, message_id);

        let replay = store
            .settle_reserved_child_failure(request_id, "spawn_channel_closed", Uuid::new_v4())
            .expect("replay settlement");
        assert!(replay.already_failed);
        assert!(replay.rows.is_empty(), "replay must settle nothing");

        let (state, version_after_replay) = message_state(&store, message_id);
        assert_eq!(state, "failed");
        assert_eq!(
            version_after_replay, version_after_first,
            "replay must not author a second settlement edge"
        );
        let edges: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM agent_message_state_transitions
                  WHERE message_id=?1 AND authority_kind='spawn_settlement'",
                params![message_id.to_string()],
                |row| row.get(0),
            )
            .expect("count settlement edges");
        assert_eq!(edges, 1);
    }

    /// A launched child is a live session, not a reserved failure. This writer
    /// must never be the thing that tears one down.
    #[test]
    fn spawn_settlement_refuses_a_launched_reservation() {
        let store = Store::open_in_memory().expect("open V81 store");
        let (owner, epic, child, request_id) = reserved_spawn(&store, "settle-launched");

        let invocation_id = Uuid::new_v4();
        store
            .conn
            .execute(
                "INSERT INTO model_invocations
                 (id,purpose,invocation_kind,foreground,paid_risk,admission_status,status,provider,model,backend,model_tier,effort,trigger_source,session_id,created_at)
                 VALUES (?1,'agent.spawn','orchestration','foreground','paid_capable','admitted','running','Claude','claude-sonnet-5','Claude','premium','high','test',?2,?3)",
                params![invocation_id.to_string(), owner.to_string(), TRIGGER_TS],
            )
            .expect("seed invocation");
        let mut child_row = test_session(child, std::path::PathBuf::from("/tmp"));
        child_row.session_kind = SessionKind::Task;
        child_row.parent_id = Some(epic);
        let reserved = store.get_agent_spawn_request(request_id).unwrap().unwrap();
        child_row.agent_role = reserved.request.agent_role;
        child_row.epic_spawn_ordinal = Some(reserved.epic_spawn_ordinal);
        store
            .admit_agent_child_session(
                &child_row,
                invocation_id,
                request_id,
                owner,
                SessionCustodyBinding::Ordinary,
            )
            .expect("admit child");

        let refused =
            store.settle_reserved_child_failure(request_id, "spawn_channel_closed", Uuid::new_v4());
        assert!(
            refused.is_err(),
            "a launched reservation must never be settled as a reserved-child failure"
        );
        assert_eq!(
            store
                .get_agent_spawn_request(request_id)
                .expect("read spawn")
                .expect("spawn row")
                .state,
            AgentSpawnStateV1::Launched,
            "the refused settlement must leave the launched row untouched"
        );
    }
}

// ---------------------------------------------------------------------------
// V81 reserved-child-capable mailbox: aggregate, append-only attempts,
// transitions, provider-request effects, exact-turn gates, and started-effect
// permits (P2-02, C-P2-04, C-P2-09, C-P2-11, C-P2-14, C-P2-17).
// ---------------------------------------------------------------------------

/// Pinned semantic catalog digest for corrected V81's exact six tables, 18
/// named indexes, and 32 named triggers (C-P2-17).
///
/// The rejected-V81 digest
/// `sha256:ad872771c0293e03a9047dbc890c7442974e09baf2750a1d2a3a8a09cc560868`
/// is forbidden and must never be accepted here.
pub(crate) const AGENT_MESSAGE_V81_SCHEMA_FINGERPRINT: &str = AGENT_MESSAGE_V81_PINNED_FINGERPRINT;

/// Digest recomputed from the canonical inventory. Kept as a separate constant
/// so the migration, Store tests, and the implementation manifest can each
/// recompute and compare it independently.
///
/// Moved from `sha256:727fb99c…` when H21-P2-INT-REV-002 tightened two trigger
/// bodies (`agent_messages_v81_cas_coherence` gained the frozen terminal
/// pointer/disposition join and the transition attempt-number join;
/// `agent_message_transitions_v81_coherence` gained `'uncertain'`). The digest
/// absorbs each trigger's normalized SQL, so a body change necessarily moves
/// it. This is a legitimate move rather than drift: V81 is unshipped (no
/// integration ref carries it and the live database is still at
/// `user_version = 80`), the catalog inventory is unchanged at exactly 6
/// tables / 18 indexes / 32 triggers, and the change only ever REFUSES writes
/// the previous body admitted.
///
/// Moved again from `sha256:e72d316a…` when H21-P2-P202-001 added the named
/// CHECK `v81_terminal_fence_matches_request` to
/// `agent_message_provider_request_effects`, closing the unenforced
/// provider-terminal invocation/turn equality P2-02 requires. The digest
/// absorbs each table's normalized `sqlite_master.sql`, so a new table-level
/// constraint necessarily moves it. Legitimate on the same three grounds: V81
/// is still unshipped, the canonical inventory is UNCHANGED at exactly 6
/// tables / 18 indexes / **32** triggers (a CHECK is not a catalog object, so
/// no count moved), and the constraint only ever REFUSES writes the previous
/// catalog admitted.
/// Moved again from `sha256:9f038add…` when H21-P2-R5-001 added the durable
/// PRE-DISPATCH marker: the `dispatching` value in the `attempt_state` CHECK
/// enum, the `dispatching_at` column with its RFC3339 and two coherence CHECKs,
/// and two clauses in `agent_message_attempts_v81_evidence_monotonic` (the
/// `dispatching_at` fill-once guard and the `claimed -> dispatching ->
/// {admission_recorded,effect_possible,terminal}` edges). The digest absorbs
/// ordered `table_info` and normalized trigger SQL, so a new column and a
/// changed trigger body each necessarily move it.
///
/// **ATTRIBUTED BY EXPERIMENT, not by observation.** The five catalog-affecting
/// hunks above were reverted in place while every non-catalog change in this
/// slice (the `AttemptStateV1::Dispatching` variant, the widened
/// `attempt_state IN (…)` WHERE guards, the delivery-path marker write, and the
/// tests) was left applied; `v81_catalog_fingerprint_is_pinned_and_rejects_the_rejected_digest`
/// then passed at the OLD `sha256:9f038add…`, and the hunks were reinstated
/// from a hash-verified copy. That proves this slice's schema edits are the
/// ENTIRE delta and that nothing else drifted into the digest.
///
/// Legitimate on the same three grounds as the previous moves: V81 is still
/// unshipped (`rolling` carries `LATEST_SCHEMA_VERSION = 80` with no
/// `version < 81` block and the live database is still at `user_version = 80`),
/// the canonical inventory is UNCHANGED at exactly 6 tables / 18 indexes /
/// **32** triggers (a column and a CHECK are not catalog objects, and the
/// trigger was amended rather than added, so no count moved), and every new
/// constraint only ever REFUSES writes the previous catalog admitted — the
/// added transition edges are reachable solely from the new `dispatching`
/// state, which no pre-existing writer can produce.
///
/// Moved again from `sha256:ba81a43a…` by **P2-06a**, which made exactly THREE
/// catalog-affecting edits:
///   1. `restart_reconciler` added to the `authority_kind` CHECK enum on
///      `agent_message_state_transitions`, so startup crash recovery authors
///      its edges under its own authority instead of relabelling them
///      `dispatcher`;
///   2. `admission_recorded` REMOVED from the `attempt_state` CHECK enum on
///      `agent_message_delivery_attempts`; and
///   3. the two `admission_recorded` clauses removed from
///      `agent_message_attempts_v81_evidence_monotonic`, leaving
///      `claimed -> {dispatching,effect_possible,terminal}` and
///      `dispatching -> {effect_possible,terminal}`.
///
/// Edits 2 and 3 resolve review finding **R6 LOW-2**:
/// `AttemptStateV1::AdmissionRecorded` had NO writer anywhere — production or
/// test — verified by sweeping every `SET attempt_state=` site, all of which
/// write `claimed`, `dispatching`, `effect_possible`, or `terminal` (TX-C's
/// parameterised value maps only to the last two). It was decided here rather
/// than deferred because V81 is still unshipped, so the digest moves for free
/// exactly once, and because dead vocabulary in this enum actively misleads the
/// new recovery classifier into thinking a state exists between `dispatching`
/// and `effect_possible`. It was REMOVED rather than given a writer because
/// `record_agent_message_admission` fills admission and effect classification
/// in one statement; manufacturing a durable instant between them would open a
/// new crash window, the opposite of this phase's purpose.
///
/// **ATTRIBUTED BY EXPERIMENT, not by observation.** The three catalog hunks
/// above were reverted in place while every non-catalog change in this slice
/// stayed applied — `CRASHED_AGENT_MESSAGE_ATTEMPT_SQL`,
/// `list_crashed_agent_message_attempts_v1` (renamed to
/// `list_crashed_agent_message_attempts_page_v1` by P2-06b, which added the
/// continuation cursor; that rename is NOT a catalog edit and does not move
/// this digest),
/// `requeue_crashed_agent_message_attempt_v1`,
/// `mark_crashed_agent_message_attempt_uncertain_v1`, the narrowed `IN (…)` in
/// the acknowledgement seal, the `rsi-common` enum removal, and all three new
/// tests. `v81_catalog_fingerprint_is_pinned_and_rejects_the_rejected_digest`
/// then passed at the OLD `sha256:ba81a43a…`, and both files were restored from
/// sha256-verified copies (`c8ecae41…`, `782993d7…`). That proves these three
/// schema edits are the ENTIRE catalog delta and nothing else drifted in.
///
/// Legitimate on the same three grounds: V81 is still unshipped (`rolling`
/// carries `LATEST_SCHEMA_VERSION = 80` with no `version < 81` block and the
/// live database is still at `user_version = 80`, both re-verified in this
/// slice rather than remembered); the canonical inventory is UNCHANGED at
/// exactly 6 tables / 18 indexes / **32** triggers (CHECK enums are not catalog
/// objects and the trigger was amended, not added); and the net effect is
/// strictly narrowing — removing a value from a CHECK enum and removing edges
/// from a transition graph only ever REFUSE writes the previous catalog
/// admitted, while the one ADDED value (`restart_reconciler`) widens a column
/// no pre-existing writer populates.
pub(crate) const AGENT_MESSAGE_V81_PINNED_FINGERPRINT: &str =
    "sha256:517b10f62ac51c4229e7129235cce3673e8e89bc73fb6d02e0ae651641759bf3";

/// Schema version corrected Phase 2 exclusively owns (C-P2-02).
pub(crate) const AGENT_MESSAGE_V81_USER_VERSION: i64 = 81;

/// Semantic catalog fingerprint of the V82 mailbox catalog, at
/// `PRAGMA user_version = 82` (P2-06d).
///
/// **Why a SECOND pin rather than an edit to the V81 one.** V81 is SHIPPED: the
/// operator's database is durably at `user_version = 81`, so a rewritten
/// `version < 81` block could never re-run there and a fresh install would
/// silently diverge from it. V82 is therefore a NEW forward migration, and the
/// V81 pin above stays frozen exactly as shipped. The two pins are checked at
/// different instants of the same open: the V81 pin is the V82 block's
/// exact-source preflight (so it is still re-proved on every fresh open, and a
/// drift there aborts before the first DDL statement), and this pin is the head
/// of the catalog after V82 commits. This mirrors
/// `program_runs::D05_V78_SCHEMA_FINGERPRINT` / `D05_V79_SCHEMA_FINGERPRINT`:
/// ONE fingerprint function, one pinned constant per version.
///
/// **ATTRIBUTED BY EXPERIMENT, not by observation.** The move from
/// `sha256:517b10f6…` decomposes into exactly three measured contributions, each
/// isolated by reverting the others in place while every non-catalog change in
/// this slice stayed applied. See
/// `docs/reference/agent-message-v82-fingerprint-attribution.md` for the four
/// measured digests and the exact procedure. Note that the usual two-way
/// experiment ("revert the catalog hunks, confirm the OLD digest returns") is
/// NOT expressible for V82: `v81_schema_fingerprint` absorbs the LIVE
/// `PRAGMA user_version`, so the version bump alone moves the digest even with
/// zero catalog edits. Reporting a two-way result here would have been a lie;
/// the three-way decomposition is the honest form of the same proof.
///
/// The canonical inventory is UNCHANGED at exactly 6 tables / 18 indexes / 32
/// triggers — RECOUNTED against the live catalog by
/// `v82_catalog_inventory_is_unchanged_and_recounted`, not assumed. A
/// table-level CHECK is not a catalog object, and the CAS-coherence trigger was
/// amended and replaced under its own name rather than added.
///
/// Both amendments are strictly NARROWING: each only ever REFUSES a write the
/// V81 catalog admitted, and neither admits anything new.
pub(crate) const AGENT_MESSAGE_V82_PINNED_FINGERPRINT: &str =
    "sha256:1bd9560e1899f4cbaae987f447034cadb3e4bf682afd061df361e939523389d2";

/// Schema version P2-06d advances the head to.
pub(crate) const AGENT_MESSAGE_V82_USER_VERSION: i64 = 82;

/// Forbidden fingerprint of the rejected V81 source (C-P2-17).
pub(crate) const AGENT_MESSAGE_REJECTED_V81_SCHEMA_FINGERPRINT: &str =
    "sha256:ad872771c0293e03a9047dbc890c7442974e09baf2750a1d2a3a8a09cc560868";

/// Exact V81 table inventory, in fingerprint order.
pub(crate) const AGENT_MESSAGE_V81_TABLES: [&str; 6] = [
    "agent_message_delivery_attempts",
    "agent_message_provider_effect_permits",
    "agent_message_provider_request_effects",
    "agent_message_provider_turn_gates",
    "agent_message_state_transitions",
    "agent_messages",
];

/// Exact V81 index inventory (18 objects).
pub(crate) const AGENT_MESSAGE_V81_INDEXES: [&str; 18] = [
    "idx_agent_message_attempts_v81_ack_event",
    "idx_agent_message_attempts_v81_delivery_fence",
    "idx_agent_message_attempts_v81_lease",
    "idx_agent_message_attempts_v81_native_boundary",
    "idx_agent_message_effect_permits_v81_unresolved",
    "idx_agent_message_request_effects_v81_approval_id",
    "idx_agent_message_request_effects_v81_event",
    "idx_agent_message_request_effects_v81_handler_capability",
    "idx_agent_message_request_effects_v81_pending",
    "idx_agent_message_request_effects_v81_reply_capability",
    "idx_agent_message_request_effects_v81_request_identity",
    "idx_agent_message_transitions_v81_state",
    "idx_agent_message_turn_gates_v81_closing",
    "idx_agent_messages_v81_cleanup_custody",
    "idx_agent_messages_v81_expiry_due",
    "idx_agent_messages_v81_owner_pending",
    "idx_agent_messages_v81_reservation",
    "idx_agent_messages_v81_target_fifo",
];

/// Exact V81 trigger inventory (32 objects).
pub(crate) const AGENT_MESSAGE_V81_TRIGGERS: [&str; 32] = [
    "agent_message_attempts_v81_ack_event_coherence",
    "agent_message_attempts_v81_correlation_coherence",
    "agent_message_attempts_v81_evidence_monotonic",
    "agent_message_attempts_v81_identity_immutable",
    "agent_message_attempts_v81_no_delete",
    "agent_message_attempts_v81_terminal_immutable",
    "agent_message_effect_permits_v81_evidence_coherence",
    "agent_message_effect_permits_v81_forward",
    "agent_message_effect_permits_v81_identity_immutable",
    "agent_message_effect_permits_v81_no_delete",
    "agent_message_request_effects_v81_approval_coherence",
    "agent_message_request_effects_v81_evidence_coherence",
    "agent_message_request_effects_v81_handler_forward",
    "agent_message_request_effects_v81_identity_immutable",
    "agent_message_request_effects_v81_no_delete",
    "agent_message_request_effects_v81_permit_coherence",
    "agent_message_request_effects_v81_reply_forward",
    "agent_message_request_effects_v81_terminal_immutable",
    "agent_message_transitions_v81_coherence",
    "agent_message_transitions_v81_identity_immutable",
    "agent_message_transitions_v81_no_delete",
    "agent_message_turn_gates_v81_admission_coherence",
    "agent_message_turn_gates_v81_close_requires_settled_permits",
    "agent_message_turn_gates_v81_forward",
    "agent_message_turn_gates_v81_identity_immutable",
    "agent_message_turn_gates_v81_no_delete",
    "agent_messages_v81_cas_coherence",
    "agent_messages_v81_identity_immutable",
    "agent_messages_v81_no_delete",
    "agent_messages_v81_requeue_requires_no_effect",
    "agent_messages_v81_state_forward",
    "agent_messages_v81_target_custody",
];

/// V80 objects on `agent_messages` that the rebuild removes. The V80 spawn,
/// progress, and watch objects are deliberately NOT touched (C-P2-03).
const AGENT_MESSAGE_V80_REPLACED_OBJECTS: [(&str, &str); 6] = [
    ("TRIGGER", "agent_messages_no_delete"),
    ("TRIGGER", "agent_messages_identity_immutable"),
    ("TRIGGER", "agent_messages_v80_validate_insert"),
    ("TRIGGER", "agent_messages_v80_validate_update"),
    ("INDEX", "idx_agent_messages_owner_state"),
    ("INDEX", "idx_agent_messages_target_fifo"),
];

/// Canonical lowercase-UUID CHECK body over a bare column reference.
fn sql_uuid(column: &str, optional: bool) -> String {
    let guard = format!(
        "length({column})=36 AND {column}!='00000000-0000-0000-0000-000000000000' \
         AND {column}=lower({column}) \
         AND substr({column},9,1)='-' AND substr({column},14,1)='-' \
         AND substr({column},19,1)='-' AND substr({column},24,1)='-' \
         AND replace({column},'-','') NOT GLOB '*[^0-9a-f]*'"
    );
    if optional {
        format!("({column} IS NULL OR ({guard}))")
    } else {
        format!("({guard})")
    }
}

/// RFC3339 timestamp with exactly nine fractional digits.
fn sql_timestamp(column: &str, optional: bool) -> String {
    let guard = format!(
        "length({column})=30 \
         AND strftime('%Y-%m-%dT%H:%M:%S',{column})=substr({column},1,19) \
         AND substr({column},20,1)='.' \
         AND substr({column},21,9) NOT GLOB '*[^0-9]*' \
         AND substr({column},30,1)='Z'"
    );
    if optional {
        format!("({column} IS NULL OR ({guard}))")
    } else {
        format!("({guard})")
    }
}

/// `sha256:` plus exactly 64 lowercase hex characters.
fn sql_digest(column: &str, optional: bool) -> String {
    let guard = format!(
        "length({column})=71 AND substr({column},1,7)='sha256:' \
         AND substr({column},8) NOT GLOB '*[^0-9a-f]*'"
    );
    if optional {
        format!("({column} IS NULL OR ({guard}))")
    } else {
        format!("({guard})")
    }
}

/// Nonempty bounded text measured in raw bytes.
fn sql_bounded(column: &str, max_bytes: usize, optional: bool) -> String {
    let guard = format!("length(CAST({column} AS BLOB)) BETWEEN 1 AND {max_bytes}");
    if optional {
        format!("({column} IS NULL OR ({guard}))")
    } else {
        format!("({guard})")
    }
}

/// Closed enum membership over the exact persisted spellings.
fn sql_enum(column: &str, values: &[&str], optional: bool) -> String {
    let list = values
        .iter()
        .map(|value| format!("'{value}'"))
        .collect::<Vec<_>>()
        .join(",");
    if optional {
        format!("({column} IS NULL OR {column} IN ({list}))")
    } else {
        format!("({column} IN ({list}))")
    }
}

/// Canonical JSON-RPC ID, validated at the SQL layer by the deterministic
/// function registered in `Store::register_sql_functions` (C-P2-17).
fn sql_jsonrpc_id(column: &str, optional: bool) -> String {
    let guard = format!(
        "length(CAST({column} AS BLOB)) BETWEEN 1 AND {max} \
         AND rsi_jsonrpc_id_is_canonical({column})=1",
        max = AGENT_MESSAGE_MAX_CANONICAL_JSONRPC_ID_BYTES,
    );
    if optional {
        format!("({column} IS NULL OR ({guard}))")
    } else {
        format!("({guard})")
    }
}

/// All-null-or-all-nonnull coherence over a column group.
fn sql_all_or_none(columns: &[&str]) -> String {
    let nulls = columns
        .iter()
        .map(|column| format!("{column} IS NULL"))
        .collect::<Vec<_>>()
        .join(" AND ");
    let nonnulls = columns
        .iter()
        .map(|column| format!("{column} IS NOT NULL"))
        .collect::<Vec<_>>()
        .join(" AND ");
    format!("(({nulls}) OR ({nonnulls}))")
}

/// The delivery-attempt relation's DDL, shared by the V81 install and the V82
/// rebuild so the two can never drift apart.
///
/// `reconciler_backstop` appends the P2-06d structural CHECK. V81 passes
/// `false`, which reproduces the frozen V81 text token-for-token; that is not
/// an assumption but a re-proved fact, because the V82 migration's preflight
/// recomputes the pinned V81 catalog fingerprint on every fresh open and
/// aborts if a single token moved.
fn agent_message_delivery_attempts_ddl(reconciler_backstop: bool) -> String {
    // P2-06d / R7. Scoped to the reconciler authority ON PURPOSE. The
    // dispatcher legitimately seals a post-marker row proved-no-effect when it
    // holds the provider's own `RejectedBeforeEffect` answer, so a blanket
    // `dispatching_at IS NULL` rule would break that live path. The restart
    // reconciler can NEVER hold that answer -- it runs when no dispatcher is
    // alive -- so for it a durable `dispatching_at` stamp means the send window
    // had already opened and no-effect is unprovable.
    //
    // `dispatching_at` is the right witness because it is durable, fill-once,
    // and survives the row going terminal; `attempt_state` does not, since a
    // terminal row no longer says whether it passed the marker.
    //
    // R7 proved this backstop is load-bearing for the REQUEUE path: widening
    // only the requeue writer's guard to `IN ('claimed','dispatching')` was
    // caught by exactly one test and by no database object at all. The
    // `proved_no_effect_expired` member is deliberate belt-and-braces, NOT
    // load-bearing: review R9 established that the expiry path is already
    // refused by `agent_message_transitions_v81_coherence`.
    // NAMED, unlike the twenty CHECKs above it. `SQLite` reports an unnamed
    // table CHECK only as "CHECK constraint failed: <table>", which cannot say
    // WHICH constraint refused — so a test asserting that string would pass
    // just as happily if some other CHECK had fired, and the acceptance
    // evidence for this backstop would be vacuous. The name makes the refusal
    // attributable to this constraint and nothing else.
    let backstop = if reconciler_backstop {
        ",
            CONSTRAINT agent_message_attempts_v82_reconciler_no_effect_backstop
            CHECK(settlement_authority IS NOT 'restart_reconciler'
               OR terminal_disposition NOT IN
                    ('proved_no_effect_requeue','proved_no_effect_failed','proved_no_effect_expired')
               OR dispatching_at IS NULL)"
    } else {
        ""
    };
    format!(
        "CREATE TABLE agent_message_delivery_attempts (
            message_id TEXT NOT NULL REFERENCES agent_messages(id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            attempt_number INTEGER NOT NULL,
            claim_token TEXT NOT NULL UNIQUE,
            delivery_boot_id TEXT NOT NULL,
            logical_root_session_id TEXT NOT NULL,
            delivery_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            delivery_session_generation INTEGER NOT NULL,
            delivery_model_invocation_id TEXT NOT NULL REFERENCES model_invocations(id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            provider_kind TEXT NOT NULL,
            capability_kind TEXT NOT NULL,
            boundary_kind TEXT NOT NULL,
            claimed_at TEXT NOT NULL,
            claim_expires_at TEXT NOT NULL,
            boundary_value TEXT,
            admission_classification TEXT,
            effect_classification TEXT,
            provider_error_class TEXT,
            correlation_state TEXT NOT NULL,
            correlation_reason TEXT,
            correlation_source TEXT,
            correlated_at TEXT,
            evidence_suppression_class TEXT,
            evidence_sealed_at TEXT,
            attempt_state TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            acknowledged_event_id INTEGER REFERENCES conversation_events(id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            acknowledged_event_session_id TEXT REFERENCES sessions(id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            acknowledged_event_sequence INTEGER,
            terminal_disposition TEXT,
            settlement_authority TEXT,
            settlement_evidence_digest TEXT,
            dispatching_at TEXT,
            admission_recorded_at TEXT,
            effect_possible_at TEXT,
            acknowledged_at TEXT,
            settled_at TEXT,
            terminal_at TEXT,
            PRIMARY KEY (message_id,attempt_number),
            CHECK({msg} AND {token} AND {boot} AND {root} AND {session} AND {invocation}),
            CHECK({ack_session}),
            CHECK(attempt_number>0 AND delivery_session_generation>=0
              AND (acknowledged_event_sequence IS NULL OR acknowledged_event_sequence>=0)
              AND (acknowledged_event_id IS NULL OR acknowledged_event_id>=0)),
            CHECK({provider} AND {capability} AND {boundary} AND {attempt_state}
              AND {admission} AND {effect} AND {correlation} AND {disposition}),
            CHECK({boundary_value} AND {error_class} AND {corr_reason} AND {corr_source}
              AND {suppression} AND {authority}),
            CHECK({settlement_digest}),
            CHECK({claimed} AND {expires} AND {updated} AND {correlated}
              AND {sealed} AND {dispatching_at} AND {admission_at} AND {effect_at}
              AND {acked_at} AND {settled} AND {terminal_at}),
            -- The pre-dispatch marker and its stamp are one fact, so neither
            -- half can exist alone (H21-P2-R5-001 option (i)):
            --   * `dispatching` without a stamp would be a marker that cannot
            --     say WHEN the send window opened, and
            --   * a stamp on a still-`claimed` row would assert the row had
            --     passed the marker while its state still claims it had not —
            --     exactly the ambiguity the marker exists to remove.
            -- A claimed -> terminal pre-dispatch rejection legitimately has
            -- neither, so this is deliberately NOT the stronger rule that every
            -- non-claimed state carries a stamp.
            CHECK(attempt_state!='dispatching' OR dispatching_at IS NOT NULL),
            CHECK(dispatching_at IS NULL OR attempt_state!='claimed'),
            -- The acknowledgement triple is all-null or all-nonnull.
            CHECK({ack_triple}),
            -- A native-turn boundary keeps a null value until a genuine
            -- provider response supplies the turn ID; a model-invocation
            -- boundary always carries the durable invocation UUID.
            CHECK(boundary_kind!='model_invocation'
               OR boundary_value IS NULL
               OR boundary_value=delivery_model_invocation_id),
            -- Terminal attempts carry closed settlement evidence.
            CHECK(attempt_state!='terminal'
               OR (terminal_disposition IS NOT NULL AND settlement_authority IS NOT NULL
                   AND settlement_evidence_digest IS NOT NULL AND settled_at IS NOT NULL
                   AND terminal_at IS NOT NULL AND effect_classification IS NOT NULL)),
            CHECK(terminal_disposition IS NULL OR attempt_state='terminal'),
            -- An acknowledged attempt proves an acknowledging event.
            CHECK(terminal_disposition!='acknowledged'
               OR (acknowledged_event_id IS NOT NULL AND acknowledged_at IS NOT NULL
                   AND effect_classification='effect_acknowledged')),
            -- A proved-no-effect disposition requires the matching admission
            -- and effect evidence; lease expiry alone is never that proof.
            CHECK(terminal_disposition NOT IN
                    ('proved_no_effect_requeue','proved_no_effect_failed','proved_no_effect_expired')
               OR (admission_classification='rejected_before_effect'
                   AND effect_classification='proved_no_effect')),
            CHECK(terminal_disposition!='unsupported'
               OR admission_classification='unsupported'),
            -- Correlation custody is AppServer-only; every other provider is
            -- permanently not_applicable.
            CHECK(capability_kind='native_multi_turn'
               OR correlation_state='not_applicable'),
            CHECK(correlation_state!='correlated'
               OR (boundary_value IS NOT NULL AND correlated_at IS NOT NULL
                   AND correlation_source IS NOT NULL)),
            CHECK(correlation_state!='sealed_live_uncertain'
               OR (evidence_suppression_class IS NOT NULL AND evidence_sealed_at IS NOT NULL)){backstop}
        )",
        msg = sql_uuid("message_id", false),
        token = sql_uuid("claim_token", false),
        boot = sql_uuid("delivery_boot_id", false),
        root = sql_uuid("logical_root_session_id", false),
        session = sql_uuid("delivery_session_id", false),
        invocation = sql_uuid("delivery_model_invocation_id", false),
        ack_session = sql_uuid("acknowledged_event_session_id", true),
        provider = sql_enum(
            "provider_kind",
            &[
                "harness",
                "codex_app_server",
                "claude_cli",
                "codex_cli",
                "antigravity_cli",
                "local_openai_compatible",
            ],
            false,
        ),
        capability = sql_enum(
            "capability_kind",
            &["native_multi_turn", "terminal_one_turn"],
            false,
        ),
        boundary = sql_enum("boundary_kind", &["native_turn", "model_invocation"], false),
        attempt_state = sql_enum(
            "attempt_state",
            &[
                "claimed",
                "dispatching",
                "effect_possible",
                "terminal",
            ],
            false,
        ),
        admission = sql_enum(
            "admission_classification",
            &[
                "rejected_before_effect",
                "admitted_effect_possible",
                "unsupported",
            ],
            true,
        ),
        effect = sql_enum(
            "effect_classification",
            &["proved_no_effect", "effect_possible", "effect_acknowledged"],
            true,
        ),
        correlation = sql_enum(
            "correlation_state",
            &[
                "not_applicable",
                "correlation_pending",
                "correlated",
                "sealed_live_uncertain",
            ],
            false,
        ),
        disposition = sql_enum(
            "terminal_disposition",
            &[
                "proved_no_effect_requeue",
                "proved_no_effect_failed",
                "proved_no_effect_expired",
                "unsupported",
                "acknowledged",
                "settled_failed",
            ],
            true,
        ),
        boundary_value = sql_bounded("boundary_value", AGENT_MESSAGE_MAX_PROVIDER_TURN_BYTES, true),
        error_class = sql_bounded("provider_error_class", AGENT_MESSAGE_MAX_ERROR_CLASS_BYTES, true),
        corr_reason = sql_bounded("correlation_reason", AGENT_MESSAGE_MAX_ENUM_BYTES, true),
        corr_source = sql_bounded("correlation_source", AGENT_MESSAGE_MAX_ENUM_BYTES, true),
        suppression = sql_bounded("evidence_suppression_class", AGENT_MESSAGE_MAX_ENUM_BYTES, true),
        authority = sql_bounded("settlement_authority", AGENT_MESSAGE_MAX_ENUM_BYTES, true),
        settlement_digest = sql_digest("settlement_evidence_digest", true),
        claimed = sql_timestamp("claimed_at", false),
        expires = sql_timestamp("claim_expires_at", false),
        updated = sql_timestamp("updated_at", false),
        correlated = sql_timestamp("correlated_at", true),
        sealed = sql_timestamp("evidence_sealed_at", true),
        dispatching_at = sql_timestamp("dispatching_at", true),
        admission_at = sql_timestamp("admission_recorded_at", true),
        effect_at = sql_timestamp("effect_possible_at", true),
        acked_at = sql_timestamp("acknowledged_at", true),
        settled = sql_timestamp("settled_at", true),
        terminal_at = sql_timestamp("terminal_at", true),
        ack_triple = sql_all_or_none(&[
            "acknowledged_event_id",
            "acknowledged_event_session_id",
            "acknowledged_event_sequence",
        ]),
    )
}

/// Create the six V81 mailbox relations with their closed inline CHECK
/// constraints (P2-02).
fn install_v81_tables(tx: &Transaction<'_>) -> Result<()> {
    // ---- aggregate -------------------------------------------------------
    let messages = format!(
        "CREATE TABLE agent_messages (
            id TEXT PRIMARY KEY,
            owner_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            target_session_id TEXT NOT NULL,
            target_spawn_request_id TEXT REFERENCES agent_spawn_requests(spawn_request_id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            idempotency_digest TEXT NOT NULL,
            request_fingerprint TEXT NOT NULL,
            payload_digest TEXT NOT NULL,
            payload TEXT NOT NULL,
            created_at TEXT NOT NULL,
            expires_at TEXT,
            state TEXT NOT NULL,
            state_version INTEGER NOT NULL,
            attempt_count INTEGER NOT NULL,
            current_attempt_number INTEGER,
            safe_error_class TEXT,
            updated_at TEXT NOT NULL,
            acknowledged_at TEXT,
            uncertain_at TEXT,
            failed_at TEXT,
            expired_at TEXT,
            UNIQUE(owner_session_id,idempotency_digest),
            FOREIGN KEY (id,current_attempt_number)
              REFERENCES agent_message_delivery_attempts(message_id,attempt_number)
              ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            CHECK({id} AND {owner} AND {target} AND {reservation}),
            CHECK({idem} AND {fingerprint} AND {payload_digest}),
            CHECK({payload}),
            CHECK({created} AND {updated} AND {expires}
              AND {acked} AND {uncertain} AND {failed} AND {expired}),
            CHECK({state}),
            CHECK({error_class}),
            CHECK(state_version>=0 AND attempt_count>=0),
            -- The pointer moves only when the next claim inserts attempt_count+1.
            CHECK((attempt_count=0 AND current_attempt_number IS NULL)
               OR (attempt_count>0 AND current_attempt_number=attempt_count)),
            -- Aggregate lifecycle timestamps match the aggregate state exactly.
            CHECK((state!='acknowledged' OR acknowledged_at IS NOT NULL)
              AND (state!='uncertain' OR uncertain_at IS NOT NULL)
              AND (state!='failed' OR (failed_at IS NOT NULL AND safe_error_class IS NOT NULL))
              AND (state!='expired' OR expired_at IS NOT NULL))
        )",
        id = sql_uuid("id", false),
        owner = sql_uuid("owner_session_id", false),
        target = sql_uuid("target_session_id", false),
        reservation = sql_uuid("target_spawn_request_id", true),
        idem = sql_digest("idempotency_digest", false),
        fingerprint = sql_digest("request_fingerprint", false),
        payload_digest = sql_digest("payload_digest", false),
        payload = sql_bounded("payload", AGENT_MESSAGE_MAX_PAYLOAD_BYTES, false),
        created = sql_timestamp("created_at", false),
        updated = sql_timestamp("updated_at", false),
        expires = sql_timestamp("expires_at", true),
        acked = sql_timestamp("acknowledged_at", true),
        uncertain = sql_timestamp("uncertain_at", true),
        failed = sql_timestamp("failed_at", true),
        expired = sql_timestamp("expired_at", true),
        state = sql_enum(
            "state",
            &[
                "queued",
                "claimed",
                "injected",
                "acknowledged",
                "uncertain",
                "failed",
                "expired",
            ],
            false,
        ),
        error_class = sql_bounded("safe_error_class", AGENT_MESSAGE_MAX_ERROR_CLASS_BYTES, true),
    );

    // ---- append-only delivery attempts -----------------------------------
    let attempts = agent_message_delivery_attempts_ddl(false);

    // ---- append-only aggregate state transitions -------------------------
    let transitions = format!(
        "CREATE TABLE agent_message_state_transitions (
            message_id TEXT NOT NULL REFERENCES agent_messages(id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            state_version INTEGER NOT NULL,
            from_state TEXT NOT NULL,
            to_state TEXT NOT NULL,
            attempt_number INTEGER,
            authority_kind TEXT NOT NULL,
            authority_id TEXT NOT NULL,
            evidence_digest TEXT NOT NULL,
            created_at TEXT NOT NULL,
            PRIMARY KEY (message_id,state_version),
            FOREIGN KEY (message_id,attempt_number)
              REFERENCES agent_message_delivery_attempts(message_id,attempt_number)
              ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            CHECK({msg}),
            CHECK(state_version>=0 AND (attempt_number IS NULL OR attempt_number>0)),
            CHECK({from} AND {to} AND {authority}),
            CHECK({authority_id} AND {digest} AND {created}),
            -- Version 0 is always the acceptance/migration edge and carries no
            -- attempt, because no delivery can precede acceptance.
            CHECK(state_version>0 OR (from_state='none' AND to_state='queued'
                  AND attempt_number IS NULL
                  AND authority_kind IN ('acceptance','migration'))),
            CHECK(state_version=0 OR from_state!='none')
        )",
        msg = sql_uuid("message_id", false),
        from = sql_enum(
            "from_state",
            &[
                "none",
                "queued",
                "claimed",
                "injected",
                "acknowledged",
                "uncertain",
                "failed",
                "expired",
            ],
            false,
        ),
        to = sql_enum(
            "to_state",
            &[
                "queued",
                "claimed",
                "injected",
                "acknowledged",
                "uncertain",
                "failed",
                "expired",
            ],
            false,
        ),
        authority = sql_enum(
            "authority_kind",
            &[
                "acceptance",
                "migration",
                "dispatcher",
                "spawn_settlement",
                "expiry_reconciler",
                "acknowledgement_store",
                "operator_settlement",
                // P2-06a: startup crash recovery. It is its OWN authority on
                // purpose — relabelling a restart-reconciler edge as
                // `dispatcher` would hide, in the one ledger built to make
                // custody legible, that no dispatcher was running when the
                // edge was authored.
                "restart_reconciler",
            ],
            false,
        ),
        authority_id = sql_bounded("authority_id", AGENT_MESSAGE_MAX_CAPABILITY_ID_BYTES, false),
        digest = sql_digest("evidence_digest", false),
        created = sql_timestamp("created_at", false),
    );

    tx.execute_batch(&messages)?;
    tx.execute_batch(&attempts)?;
    tx.execute_batch(&transitions)?;
    install_v81_effect_tables(tx)?;
    Ok(())
}

/// Create the three exact-turn effect-custody relations: the append-only
/// provider-request ledger, the exact-turn gates, and the started-effect
/// permits (P2-02, C-P2-11, C-P2-14).
fn install_v81_effect_tables(tx: &Transaction<'_>) -> Result<()> {
    // ---- exact-turn gates (created first: requests/permits reference it) --
    let gates = format!(
        "CREATE TABLE agent_message_provider_turn_gates (
            message_id TEXT NOT NULL,
            attempt_number INTEGER NOT NULL,
            delivery_model_invocation_id TEXT NOT NULL,
            provider_turn_id TEXT NOT NULL,
            gate_generation INTEGER NOT NULL,
            gate_state TEXT NOT NULL,
            admission_sequence INTEGER NOT NULL,
            issued_permit_count INTEGER NOT NULL,
            settled_permit_count INTEGER NOT NULL,
            opened_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            terminal_authority TEXT,
            terminal_evidence_digest TEXT,
            closing_at TEXT,
            closed_at TEXT,
            PRIMARY KEY (message_id,attempt_number,delivery_model_invocation_id,provider_turn_id,gate_generation),
            FOREIGN KEY (message_id,attempt_number)
              REFERENCES agent_message_delivery_attempts(message_id,attempt_number)
              ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            CHECK({msg} AND {invocation}),
            CHECK({turn} AND {authority} AND {digest}),
            CHECK(attempt_number>0 AND gate_generation>=0 AND admission_sequence>=0
              AND issued_permit_count>=0 AND settled_permit_count>=0),
            CHECK({state}),
            CHECK({opened} AND {updated} AND {closing} AND {closed}),
            -- Counts are monotonic and settled never exceeds issued.
            CHECK(settled_permit_count<=issued_permit_count),
            -- Terminal fields are all-null while open and fill once at closing.
            CHECK(gate_state!='open'
               OR (terminal_authority IS NULL AND terminal_evidence_digest IS NULL
                   AND closing_at IS NULL AND closed_at IS NULL)),
            CHECK(gate_state='open'
               OR (terminal_authority IS NOT NULL AND terminal_evidence_digest IS NOT NULL
                   AND closing_at IS NOT NULL)),
            -- Closed requires every issued permit settled.
            CHECK(gate_state!='closed'
               OR (closed_at IS NOT NULL AND settled_permit_count=issued_permit_count)),
            CHECK(gate_state='closed' OR closed_at IS NULL)
        )",
        msg = sql_uuid("message_id", false),
        invocation = sql_uuid("delivery_model_invocation_id", false),
        turn = sql_bounded("provider_turn_id", AGENT_MESSAGE_MAX_PROVIDER_TURN_BYTES, false),
        authority = sql_bounded("terminal_authority", AGENT_MESSAGE_MAX_ENUM_BYTES, true),
        digest = sql_digest("terminal_evidence_digest", true),
        state = sql_enum("gate_state", &["open", "closing", "closed"], false),
        opened = sql_timestamp("opened_at", false),
        updated = sql_timestamp("updated_at", false),
        closing = sql_timestamp("closing_at", true),
        closed = sql_timestamp("closed_at", true),
    );

    // ---- append-only provider-request ledger -----------------------------
    let requests = format!(
        "CREATE TABLE agent_message_provider_request_effects (
            message_id TEXT NOT NULL,
            attempt_number INTEGER NOT NULL,
            turn_start_request_id TEXT NOT NULL,
            provider_turn_id TEXT NOT NULL,
            provider_request_id TEXT NOT NULL,
            provider_request_kind TEXT NOT NULL,
            provider_request_digest TEXT NOT NULL,
            delivery_model_invocation_id TEXT NOT NULL,
            turn_gate_generation INTEGER NOT NULL,
            evidence_event_id INTEGER NOT NULL REFERENCES conversation_events(id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            evidence_event_session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            evidence_event_sequence INTEGER NOT NULL,
            handler_kind TEXT NOT NULL,
            evidence_committed_at TEXT NOT NULL,
            created_at TEXT NOT NULL,
            handler_phase TEXT NOT NULL,
            handler_capability_id TEXT UNIQUE,
            handler_authorized_at TEXT,
            handler_started_at TEXT,
            handler_completed_at TEXT,
            handler_outcome_digest TEXT,
            pending_decision_at TEXT,
            approval_id TEXT UNIQUE REFERENCES approvals(id) ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            approval_presented_at TEXT,
            decision_kind TEXT,
            decision_digest TEXT,
            decision_authority TEXT,
            decision_recorded_at TEXT,
            handler_uncertain_at TEXT,
            reply_phase TEXT NOT NULL,
            reply_kind TEXT,
            reply_digest TEXT,
            reply_capability_id TEXT UNIQUE,
            reply_authorized_at TEXT,
            reply_started_at TEXT,
            reply_completed_at TEXT,
            reply_uncertain_at TEXT,
            request_disposition TEXT NOT NULL,
            provider_terminal_invocation_id TEXT,
            provider_terminal_turn_id TEXT,
            provider_terminal_authority TEXT,
            provider_terminal_evidence_digest TEXT,
            provider_terminal_sealed_at TEXT,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (message_id,attempt_number,turn_start_request_id,provider_turn_id,
                         provider_request_id,provider_request_kind,provider_request_digest),
            FOREIGN KEY (message_id,attempt_number)
              REFERENCES agent_message_delivery_attempts(message_id,attempt_number)
              ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            FOREIGN KEY (message_id,attempt_number,delivery_model_invocation_id,
                         provider_turn_id,turn_gate_generation)
              REFERENCES agent_message_provider_turn_gates(message_id,attempt_number,
                         delivery_model_invocation_id,provider_turn_id,gate_generation)
              ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            CHECK({msg} AND {invocation} AND {evidence_session}),
            -- Both JSON-RPC IDs use the canonical domain-tagged encoding so a
            -- numeric and a string ID can never alias.
            CHECK({turn_start_id} AND {request_id}),
            CHECK({turn} AND {terminal_turn}),
            CHECK({request_kind} AND {request_digest}),
            CHECK(attempt_number>0 AND turn_gate_generation>=0
              AND evidence_event_id>=0 AND evidence_event_sequence>=0),
            CHECK({handler_kind} AND {handler_phase} AND {reply_phase} AND {disposition}),
            CHECK({decision_kind} AND {reply_kind}),
            CHECK({handler_cap} AND {reply_cap} AND {approval} AND {terminal_invocation}),
            CHECK({outcome_digest} AND {decision_digest} AND {reply_digest}
              AND {terminal_digest}),
            CHECK({decision_authority} AND {terminal_authority}),
            CHECK({evidence_at} AND {created} AND {updated}
              AND {h_auth} AND {h_start} AND {h_done} AND {pending} AND {presented}
              AND {decided} AND {h_uncertain}
              AND {r_auth} AND {r_start} AND {r_done} AND {r_uncertain}
              AND {sealed}),
            -- Capability identity and its timestamp fill atomically.
            CHECK((handler_capability_id IS NULL)=(handler_authorized_at IS NULL)),
            CHECK((reply_capability_id IS NULL)=(reply_authorized_at IS NULL)),
            -- An approval row is the only handler kind that may hold approval
            -- identity, a pending decision, or a recorded decision.
            CHECK(handler_kind='approval_presentation'
               OR (approval_id IS NULL AND approval_presented_at IS NULL
                   AND pending_decision_at IS NULL AND decision_kind IS NULL
                   AND decision_digest IS NULL AND decision_authority IS NULL
                   AND decision_recorded_at IS NULL
                   AND handler_phase NOT IN ('pending_decision','decision_recorded'))),
            -- A recorded decision is immutable and complete.
            CHECK((decision_kind IS NULL)=(decision_recorded_at IS NULL)),
            CHECK(decision_kind IS NULL
               OR (decision_digest IS NOT NULL AND decision_authority IS NOT NULL)),
            -- Terminal request columns are all-null or all-nonnull.
            CHECK({terminal_group}),
            -- ...and the fence a cancellation claims is THIS request's own
            -- durable model invocation and genuine provider turn, so a raw
            -- writer cannot seal a cancellation under an unrelated turn
            -- (H21-P2-P202-001).
            --
            -- CHECK-owned rather than trigger-owned, deliberately: this is a
            -- whole-row predicate over one row's OWN columns, and C-P2-17
            -- reserves trigger enforcement for where cross-row truth is
            -- required. It is also unconditional -- no WHEN clause to scope
            -- wrongly -- and applies to INSERT as well as UPDATE, though on
            -- INSERT the forgery is already unreachable: `..._evidence_
            -- coherence` admits only a `live` birth and a `live` row's
            -- terminal group must be all-null.
            -- The turn's GENUINENESS is inherited, not re-derived: this row's
            -- (message,attempt,invocation,turn,generation) composite foreign
            -- key already binds `provider_turn_id` to a real gate row, so
            -- equality with it transfers that proof to the fence for free.
            --
            -- Named so its refusal is attributable: SQLite reports the
            -- constraint NAME in its abort text for a named constraint and
            -- only the table name for an anonymous one, and an unattributable
            -- refusal cannot be told apart from a sibling CHECK objecting
            -- first.
            CONSTRAINT v81_terminal_fence_matches_request
              CHECK(provider_terminal_invocation_id IS NULL
                 OR (provider_terminal_invocation_id=delivery_model_invocation_id
                     AND provider_terminal_turn_id=provider_turn_id)),
            CHECK(request_disposition!='provider_terminal_cancelled'
               OR provider_terminal_sealed_at IS NOT NULL),
            CHECK(request_disposition!='live' OR provider_terminal_sealed_at IS NULL),
            -- A completed request seals atomically with reply_completed.
            CHECK(request_disposition!='completed' OR reply_phase='reply_completed'),
            -- A tool reply requires its completed handler outcome digest.
            CHECK(reply_phase='not_authorized' OR handler_kind!='tool_execution'
               OR handler_outcome_digest IS NOT NULL),
            -- An approval reply requires exactly one immutable decision.
            CHECK(reply_phase='not_authorized' OR handler_kind!='approval_presentation'
               OR decision_kind IS NOT NULL)
        )",
        msg = sql_uuid("message_id", false),
        invocation = sql_uuid("delivery_model_invocation_id", false),
        evidence_session = sql_uuid("evidence_event_session_id", false),
        terminal_invocation = sql_uuid("provider_terminal_invocation_id", true),
        turn_start_id = sql_jsonrpc_id("turn_start_request_id", false),
        request_id = sql_jsonrpc_id("provider_request_id", false),
        turn = sql_bounded("provider_turn_id", AGENT_MESSAGE_MAX_PROVIDER_TURN_BYTES, false),
        terminal_turn = sql_bounded(
            "provider_terminal_turn_id",
            AGENT_MESSAGE_MAX_PROVIDER_TURN_BYTES,
            true
        ),
        request_kind = sql_bounded("provider_request_kind", AGENT_MESSAGE_MAX_METHOD_BYTES, false),
        request_digest = sql_digest("provider_request_digest", false),
        handler_kind = sql_enum(
            "handler_kind",
            &["tool_execution", "approval_presentation"],
            false,
        ),
        handler_phase = sql_enum(
            "handler_phase",
            &[
                "evidence_committed",
                "handler_authorized",
                "handler_started",
                "handler_completed",
                "pending_decision",
                "decision_recorded",
                "handler_uncertain",
            ],
            false,
        ),
        reply_phase = sql_enum(
            "reply_phase",
            &[
                "not_authorized",
                "reply_authorized",
                "reply_started",
                "reply_completed",
                "reply_uncertain",
            ],
            false,
        ),
        disposition = sql_enum(
            "request_disposition",
            &["live", "completed", "provider_terminal_cancelled"],
            false,
        ),
        decision_kind = sql_enum(
            "decision_kind",
            &["approved", "denied", "cancelled"],
            true,
        ),
        reply_kind = sql_bounded("reply_kind", AGENT_MESSAGE_MAX_ENUM_BYTES, true),
        handler_cap = sql_bounded(
            "handler_capability_id",
            AGENT_MESSAGE_MAX_CAPABILITY_ID_BYTES,
            true
        ),
        reply_cap = sql_bounded(
            "reply_capability_id",
            AGENT_MESSAGE_MAX_CAPABILITY_ID_BYTES,
            true
        ),
        approval = sql_uuid("approval_id", true),
        outcome_digest = sql_digest("handler_outcome_digest", true),
        decision_digest = sql_digest("decision_digest", true),
        reply_digest = sql_digest("reply_digest", true),
        terminal_digest = sql_digest("provider_terminal_evidence_digest", true),
        decision_authority = sql_bounded(
            "decision_authority",
            AGENT_MESSAGE_MAX_ENUM_BYTES,
            true
        ),
        terminal_authority = sql_bounded(
            "provider_terminal_authority",
            AGENT_MESSAGE_MAX_ENUM_BYTES,
            true
        ),
        evidence_at = sql_timestamp("evidence_committed_at", false),
        created = sql_timestamp("created_at", false),
        updated = sql_timestamp("updated_at", false),
        h_auth = sql_timestamp("handler_authorized_at", true),
        h_start = sql_timestamp("handler_started_at", true),
        h_done = sql_timestamp("handler_completed_at", true),
        pending = sql_timestamp("pending_decision_at", true),
        presented = sql_timestamp("approval_presented_at", true),
        decided = sql_timestamp("decision_recorded_at", true),
        h_uncertain = sql_timestamp("handler_uncertain_at", true),
        r_auth = sql_timestamp("reply_authorized_at", true),
        r_start = sql_timestamp("reply_started_at", true),
        r_done = sql_timestamp("reply_completed_at", true),
        r_uncertain = sql_timestamp("reply_uncertain_at", true),
        sealed = sql_timestamp("provider_terminal_sealed_at", true),
        terminal_group = sql_all_or_none(&[
            "provider_terminal_invocation_id",
            "provider_terminal_turn_id",
            "provider_terminal_authority",
            "provider_terminal_evidence_digest",
            "provider_terminal_sealed_at",
        ]),
    );

    // ---- append-only started-effect permits ------------------------------
    let permits = format!(
        "CREATE TABLE agent_message_provider_effect_permits (
            permit_id TEXT PRIMARY KEY,
            message_id TEXT NOT NULL,
            attempt_number INTEGER NOT NULL,
            turn_start_request_id TEXT NOT NULL,
            provider_turn_id TEXT NOT NULL,
            provider_request_id TEXT NOT NULL,
            provider_request_kind TEXT NOT NULL,
            provider_request_digest TEXT NOT NULL,
            delivery_model_invocation_id TEXT NOT NULL,
            turn_gate_generation INTEGER NOT NULL,
            permit_kind TEXT NOT NULL,
            request_phase TEXT NOT NULL,
            permit_state TEXT NOT NULL,
            executor_boot_id TEXT NOT NULL,
            external_join_id TEXT NOT NULL,
            issued_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            operation_id TEXT,
            writer_receipt_state TEXT,
            writer_receipt_digest TEXT,
            writer_receipt_at TEXT,
            completion_evidence_digest TEXT,
            uncertainty_evidence_digest TEXT,
            join_evidence_digest TEXT,
            cancel_requested_at TEXT,
            cancel_confirmed_at TEXT,
            cancel_evidence_digest TEXT,
            settled_at TEXT,
            UNIQUE(message_id,attempt_number,turn_start_request_id,provider_turn_id,
                   provider_request_id,provider_request_kind,provider_request_digest,permit_kind),
            FOREIGN KEY (message_id,attempt_number,turn_start_request_id,provider_turn_id,
                         provider_request_id,provider_request_kind,provider_request_digest)
              REFERENCES agent_message_provider_request_effects(message_id,attempt_number,
                         turn_start_request_id,provider_turn_id,provider_request_id,
                         provider_request_kind,provider_request_digest)
              ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            FOREIGN KEY (message_id,attempt_number,delivery_model_invocation_id,
                         provider_turn_id,turn_gate_generation)
              REFERENCES agent_message_provider_turn_gates(message_id,attempt_number,
                         delivery_model_invocation_id,provider_turn_id,gate_generation)
              ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED,
            CHECK({permit} AND {msg} AND {invocation} AND {boot} AND {join}),
            CHECK({turn_start_id} AND {request_id}),
            CHECK({turn} AND {request_kind} AND {request_digest}),
            CHECK(attempt_number>0 AND turn_gate_generation>=0),
            CHECK({kind} AND {phase} AND {state} AND {receipt_state}),
            CHECK({operation}),
            CHECK({completion} AND {uncertainty} AND {join_digest} AND {cancel_digest}
              AND {receipt_digest}),
            CHECK({issued} AND {updated} AND {receipt_at} AND {cancel_req}
              AND {cancel_conf} AND {settled}),
            -- A settled permit records its settlement time exactly once.
            CHECK((permit_state='issued')=(settled_at IS NULL)),
            -- Evidence matches the exact terminal state.
            CHECK(permit_state!='completed' OR completion_evidence_digest IS NOT NULL),
            CHECK(permit_state NOT IN ('uncertain_unjoined','uncertain_joined')
               OR uncertainty_evidence_digest IS NOT NULL),
            CHECK(permit_state!='uncertain_joined' OR join_evidence_digest IS NOT NULL),
            -- A cancellation REQUEST alone never settles a permit; only a
            -- confirmed cancellation with immutable evidence does.
            CHECK(permit_state!='cancel_confirmed'
               OR (cancel_confirmed_at IS NOT NULL AND cancel_evidence_digest IS NOT NULL)),
            CHECK(cancel_confirmed_at IS NULL OR cancel_requested_at IS NOT NULL),
            -- A provider write carries writer operation identity; an
            -- in-process effect never does.
            CHECK(permit_kind='provider_reply' OR operation_id IS NULL),
            CHECK((writer_receipt_state IS NULL)=(writer_receipt_at IS NULL)),
            CHECK(writer_receipt_state IS NULL OR operation_id IS NOT NULL),
            -- The permit's request phase matches its kind.
            CHECK((permit_kind='provider_reply')=(request_phase='reply_started'))
        )",
        permit = sql_uuid("permit_id", false),
        msg = sql_uuid("message_id", false),
        invocation = sql_uuid("delivery_model_invocation_id", false),
        boot = sql_uuid("executor_boot_id", false),
        join = sql_uuid("external_join_id", false),
        operation = sql_uuid("operation_id", true),
        turn_start_id = sql_jsonrpc_id("turn_start_request_id", false),
        request_id = sql_jsonrpc_id("provider_request_id", false),
        turn = sql_bounded(
            "provider_turn_id",
            AGENT_MESSAGE_MAX_PROVIDER_TURN_BYTES,
            false
        ),
        request_kind = sql_bounded(
            "provider_request_kind",
            AGENT_MESSAGE_MAX_METHOD_BYTES,
            false
        ),
        request_digest = sql_digest("provider_request_digest", false),
        kind = sql_enum(
            "permit_kind",
            &["tool_execution", "approval_presentation", "provider_reply"],
            false,
        ),
        phase = sql_enum(
            "request_phase",
            &["handler_started", "reply_started"],
            false,
        ),
        state = sql_enum(
            "permit_state",
            &[
                "issued",
                "completed",
                "uncertain_unjoined",
                "uncertain_joined",
                "cancel_confirmed",
            ],
            false,
        ),
        receipt_state = sql_enum(
            "writer_receipt_state",
            &[
                "rejected_before_enqueue",
                "write_flushed",
                "write_failed",
                "writer_closed",
            ],
            true,
        ),
        completion = sql_digest("completion_evidence_digest", true),
        uncertainty = sql_digest("uncertainty_evidence_digest", true),
        join_digest = sql_digest("join_evidence_digest", true),
        cancel_digest = sql_digest("cancel_evidence_digest", true),
        receipt_digest = sql_digest("writer_receipt_digest", true),
        issued = sql_timestamp("issued_at", false),
        updated = sql_timestamp("updated_at", false),
        receipt_at = sql_timestamp("writer_receipt_at", true),
        cancel_req = sql_timestamp("cancel_requested_at", true),
        cancel_conf = sql_timestamp("cancel_confirmed_at", true),
        settled = sql_timestamp("settled_at", true),
    );

    tx.execute_batch(&gates)?;
    tx.execute_batch(&requests)?;
    tx.execute_batch(&permits)?;
    Ok(())
}

/// Create the exact 18 bounded-work indexes frozen by P2-02.
fn install_v81_indexes(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(
        "CREATE INDEX idx_agent_messages_v81_owner_pending
           ON agent_messages(owner_session_id,state,created_at,id);
         CREATE INDEX idx_agent_messages_v81_target_fifo
           ON agent_messages(target_session_id,state,created_at,id)
           WHERE state IN ('queued','claimed');
         CREATE INDEX idx_agent_messages_v81_reservation
           ON agent_messages(target_spawn_request_id,state,id);
         CREATE INDEX idx_agent_messages_v81_expiry_due
           ON agent_messages(state,expires_at,id)
           WHERE state IN ('queued','claimed');
         CREATE INDEX idx_agent_messages_v81_cleanup_custody
           ON agent_messages(target_session_id,state,updated_at,id)
           WHERE state IN ('queued','claimed','injected','uncertain');

         CREATE INDEX idx_agent_message_attempts_v81_lease
           ON agent_message_delivery_attempts(attempt_state,claim_expires_at,message_id,attempt_number)
           WHERE attempt_state!='terminal';
         CREATE INDEX idx_agent_message_attempts_v81_delivery_fence
           ON agent_message_delivery_attempts(delivery_boot_id,delivery_session_id,
               delivery_session_generation,delivery_model_invocation_id,attempt_state,
               message_id,attempt_number);
         CREATE INDEX idx_agent_message_attempts_v81_native_boundary
           ON agent_message_delivery_attempts(provider_kind,boundary_kind,boundary_value)
           WHERE boundary_value IS NOT NULL;
         CREATE UNIQUE INDEX idx_agent_message_attempts_v81_ack_event
           ON agent_message_delivery_attempts(acknowledged_event_id)
           WHERE acknowledged_event_id IS NOT NULL;

         CREATE INDEX idx_agent_message_transitions_v81_state
           ON agent_message_state_transitions(to_state,created_at,message_id,state_version);

         CREATE UNIQUE INDEX idx_agent_message_request_effects_v81_handler_capability
           ON agent_message_provider_request_effects(handler_capability_id)
           WHERE handler_capability_id IS NOT NULL;
         CREATE UNIQUE INDEX idx_agent_message_request_effects_v81_reply_capability
           ON agent_message_provider_request_effects(reply_capability_id)
           WHERE reply_capability_id IS NOT NULL;
         CREATE UNIQUE INDEX idx_agent_message_request_effects_v81_approval_id
           ON agent_message_provider_request_effects(approval_id)
           WHERE approval_id IS NOT NULL;
         CREATE UNIQUE INDEX idx_agent_message_request_effects_v81_request_identity
           ON agent_message_provider_request_effects(message_id,attempt_number,
               turn_start_request_id,provider_turn_id,provider_request_id);
         CREATE INDEX idx_agent_message_request_effects_v81_pending
           ON agent_message_provider_request_effects(handler_phase,updated_at,message_id,attempt_number)
           WHERE handler_phase IN ('pending_decision','handler_started','decision_recorded');
         CREATE INDEX idx_agent_message_request_effects_v81_event
           ON agent_message_provider_request_effects(evidence_event_id);

         CREATE INDEX idx_agent_message_turn_gates_v81_closing
           ON agent_message_provider_turn_gates(gate_state,updated_at,
               delivery_model_invocation_id,provider_turn_id);

         CREATE INDEX idx_agent_message_effect_permits_v81_unresolved
           ON agent_message_provider_effect_permits(permit_state,updated_at,
               delivery_model_invocation_id,provider_turn_id,executor_boot_id,
               external_join_id,operation_id,permit_id);",
    )?;
    Ok(())
}

/// The aggregate CAS-coherence trigger's DDL, shared by the V81 install and the
/// V82 replacement so the two can never drift apart.
///
/// `sealed_live_uncertain_guard` closes P2-06d GAP 1. V81 passes `false`, which
/// reproduces the frozen V81 text token-for-token; the V82 migration's V81
/// fingerprint preflight re-proves that on every fresh open.
fn agent_messages_v81_cas_coherence_ddl(sealed_live_uncertain_guard: bool) -> String {
    // P2-06d GAP 1. The NEGATIVE formulation is load-bearing and was verified
    // by R7: `correlation_state` is permanently `not_applicable` for every
    // provider except AppServer, so the POSITIVE form
    // (`correlation_state='correlation_pending'`) would refuse acknowledgement
    // for EVERY non-AppServer provider. `!='sealed_live_uncertain'` is a no-op
    // for `not_applicable` and `correlated` rows and refuses only the one
    // custody state that can never legitimately be acknowledged.
    //
    // It sits inside the `NEW.state!='acknowledged' OR EXISTS(...)` conjunct,
    // which is short-circuited for every other target state, so this narrows
    // exactly the `-> acknowledged` edges and nothing else.
    let seal_guard = if sealed_live_uncertain_guard {
        "
                                AND a.correlation_state!='sealed_live_uncertain'"
    } else {
        ""
    };
    format!(
        "-- Every state_version change needs exactly one matching immutable
         -- transition row, already inserted in this same transaction.
         CREATE TRIGGER agent_messages_v81_cas_coherence
           BEFORE UPDATE ON agent_messages
           WHEN NOT (
             -- Same-state replay is read-only and must not bump the version.
             (OLD.state=NEW.state AND OLD.state_version=NEW.state_version)
             OR (
               NEW.state_version=OLD.state_version+1
               -- The required transition row is joined on its attempt pointer as
               -- well as its identity: an attempt-linked edge must name the
               -- EXACT current attempt, never merely some attempt of this
               -- message. Null-attempt terminal edges (queued expiry, queued
               -- launch failure) are admitted here and constrained by the
               -- frozen terminal pointer/disposition rule below.
               AND EXISTS(SELECT 1 FROM agent_message_state_transitions t
                          WHERE t.message_id=NEW.id AND t.state_version=NEW.state_version
                            AND t.from_state=OLD.state AND t.to_state=NEW.state
                            AND (NEW.state NOT IN ('claimed','injected','uncertain','acknowledged')
                                 OR t.attempt_number IS NEW.current_attempt_number)
                            AND (NEW.state NOT IN ('failed','expired')
                                 OR t.attempt_number IS NULL
                                 OR t.attempt_number IS NEW.current_attempt_number))
               AND NEW.attempt_count>=OLD.attempt_count
               -- Active and attempt-linked terminal states join the exact
               -- current attempt; queued/terminal historical pointers may
               -- reference only a sealed no-effect attempt.
               AND (NEW.state NOT IN ('claimed','injected','uncertain','acknowledged')
                    OR EXISTS(SELECT 1 FROM agent_message_delivery_attempts a
                              WHERE a.message_id=NEW.id
                                AND a.attempt_number=NEW.current_attempt_number))
               AND (NEW.state!='acknowledged'
                    OR EXISTS(SELECT 1 FROM agent_message_delivery_attempts a
                              WHERE a.message_id=NEW.id
                                AND a.attempt_number=NEW.current_attempt_number
                                AND a.terminal_disposition='acknowledged'{seal_guard}))
               AND (NEW.state NOT IN ('injected','uncertain')
                    OR EXISTS(SELECT 1 FROM agent_message_delivery_attempts a
                              WHERE a.message_id=NEW.id
                                AND a.attempt_number=NEW.current_attempt_number
                                AND a.effect_classification='effect_possible'))
               AND (NEW.state!='queued' OR NEW.current_attempt_number IS NULL
                    OR EXISTS(SELECT 1 FROM agent_message_delivery_attempts a
                              WHERE a.message_id=NEW.id
                                AND a.attempt_number=NEW.current_attempt_number
                                AND a.terminal_disposition='proved_no_effect_requeue'))
               -- Frozen terminal pointer/disposition join. Without this a
               -- `claimed` message whose current attempt is still
               -- `effect_possible` could be driven to `failed`/`expired` with no
               -- sealed disposition anywhere: uncertainty erasure, which is the
               -- exact failure class this schema exists to prevent.
               AND (NEW.state NOT IN ('failed','expired')
                    OR EXISTS(
                         SELECT 1 FROM agent_message_state_transitions t
                         WHERE t.message_id=NEW.id AND t.state_version=NEW.state_version
                           AND t.from_state=OLD.state AND t.to_state=NEW.state
                           AND (
                             -- Null-attempt queued expiry / queued launch
                             -- failure: the aggregate pointer is null or is the
                             -- retained historical sealed-requeue attempt.
                             (t.attempt_number IS NULL
                              AND (NEW.current_attempt_number IS NULL
                                   OR EXISTS(SELECT 1 FROM agent_message_delivery_attempts a
                                             WHERE a.message_id=NEW.id
                                               AND a.attempt_number=NEW.current_attempt_number
                                               AND a.attempt_state='terminal'
                                               AND a.terminal_disposition='proved_no_effect_requeue')))
                             OR
                             -- Attempt-linked terminal: the exact current
                             -- attempt must already be sealed with a
                             -- disposition its target state authorizes.
                             (t.attempt_number IS NOT NULL
                              AND t.attempt_number=NEW.current_attempt_number
                              AND EXISTS(SELECT 1 FROM agent_message_delivery_attempts a
                                         WHERE a.message_id=NEW.id
                                           AND a.attempt_number=NEW.current_attempt_number
                                           AND a.attempt_state='terminal'
                                           AND ((NEW.state='expired'
                                                 AND a.terminal_disposition='proved_no_effect_expired')
                                             OR (NEW.state='failed'
                                                 AND a.terminal_disposition IN
                                                     ('proved_no_effect_failed','unsupported','settled_failed')))))
                           )))
             )
           )
           BEGIN SELECT RAISE(ABORT,'V81 agent message CAS coherence violated'); END;",
    )
}

/// Create the exact 32 named triggers frozen by P2-02.
///
/// A trigger never clears or overwrites earlier attempt, transition, request,
/// phase, capability, decision, or evidence custody.
fn install_v81_triggers(tx: &Transaction<'_>) -> Result<()> {
    // ---- aggregate (6) ---------------------------------------------------
    tx.execute_batch(
        "CREATE TRIGGER agent_messages_v81_no_delete
           BEFORE DELETE ON agent_messages
           BEGIN SELECT RAISE(ABORT,'agent message history cannot be deleted'); END;

         -- Dual insert truth: the logical target already exists as a Session,
         -- OR the reservation identifies the same owner, same child, and a
         -- nonfailed live spawn request. Acceptance always starts at queued,
         -- version 0, zero attempts.
         CREATE TRIGGER agent_messages_v81_target_custody
           BEFORE INSERT ON agent_messages
           WHEN NOT (
             NEW.state='queued' AND NEW.state_version=0 AND NEW.attempt_count=0
             AND NEW.current_attempt_number IS NULL
             AND NEW.acknowledged_at IS NULL AND NEW.uncertain_at IS NULL
             AND NEW.failed_at IS NULL AND NEW.expired_at IS NULL
             AND (
               (NEW.target_spawn_request_id IS NULL
                AND EXISTS(SELECT 1 FROM sessions WHERE id=NEW.target_session_id))
               OR
               (NEW.target_spawn_request_id IS NOT NULL
                AND EXISTS(SELECT 1 FROM agent_spawn_requests
                           WHERE spawn_request_id=NEW.target_spawn_request_id
                             AND owner_session_id=NEW.owner_session_id
                             AND child_session_id=NEW.target_session_id
                             AND state IN ('reserved','queued','launching','launched')))
             )
           )
           BEGIN SELECT RAISE(ABORT,'V81 agent message target custody is not exactly one of Session or live reservation'); END;

         CREATE TRIGGER agent_messages_v81_identity_immutable
           BEFORE UPDATE ON agent_messages
           WHEN OLD.id!=NEW.id OR OLD.owner_session_id!=NEW.owner_session_id
             OR OLD.target_session_id!=NEW.target_session_id
             OR OLD.idempotency_digest!=NEW.idempotency_digest
             OR OLD.request_fingerprint!=NEW.request_fingerprint
             OR OLD.payload_digest!=NEW.payload_digest OR OLD.payload!=NEW.payload
             OR OLD.created_at!=NEW.created_at
             OR OLD.target_spawn_request_id IS NOT NEW.target_spawn_request_id
             OR OLD.expires_at IS NOT NEW.expires_at
           BEGIN SELECT RAISE(ABORT,'V81 agent message acceptance identity and expiry are immutable'); END;

         -- Forward-only aggregate state machine; terminal states are frozen.
         CREATE TRIGGER agent_messages_v81_state_forward
           BEFORE UPDATE ON agent_messages
           WHEN OLD.state!=NEW.state AND NOT (
             (OLD.state='queued'  AND NEW.state IN ('claimed','failed','expired'))
          OR (OLD.state='claimed' AND NEW.state IN ('queued','injected','acknowledged','uncertain','failed','expired'))
          OR (OLD.state='injected' AND NEW.state IN ('acknowledged','uncertain'))
          OR (OLD.state='uncertain' AND NEW.state IN ('acknowledged','failed'))
           )
           BEGIN SELECT RAISE(ABORT,'V81 agent message state transition is not a legal forward edge'); END;
",
    )?;

    tx.execute_batch(&agent_messages_v81_cas_coherence_ddl(false))?;

    tx.execute_batch(
        "
         -- claimed->queued requires the current attempt to be durably sealed
         -- proved-no-effect in this same transaction. Lease expiry alone is
         -- never that proof.
         CREATE TRIGGER agent_messages_v81_requeue_requires_no_effect
           BEFORE UPDATE ON agent_messages
           WHEN OLD.state='claimed' AND NEW.state='queued'
             AND NOT EXISTS(SELECT 1 FROM agent_message_delivery_attempts a
                            WHERE a.message_id=NEW.id
                              AND a.attempt_number=OLD.current_attempt_number
                              AND a.attempt_state='terminal'
                              AND a.terminal_disposition='proved_no_effect_requeue'
                              AND a.effect_classification='proved_no_effect')
           BEGIN SELECT RAISE(ABORT,'V81 agent message requeue requires a sealed proved-no-effect attempt'); END;",
    )?;

    // ---- delivery attempts (6) ------------------------------------------
    tx.execute_batch(
        "CREATE TRIGGER agent_message_attempts_v81_no_delete
           BEFORE DELETE ON agent_message_delivery_attempts
           BEGIN SELECT RAISE(ABORT,'agent message delivery attempts are append-only'); END;

         CREATE TRIGGER agent_message_attempts_v81_identity_immutable
           BEFORE UPDATE ON agent_message_delivery_attempts
           WHEN OLD.message_id!=NEW.message_id OR OLD.attempt_number!=NEW.attempt_number
             OR OLD.claim_token!=NEW.claim_token OR OLD.delivery_boot_id!=NEW.delivery_boot_id
             OR OLD.logical_root_session_id!=NEW.logical_root_session_id
             OR OLD.delivery_session_id!=NEW.delivery_session_id
             OR OLD.delivery_session_generation!=NEW.delivery_session_generation
             OR OLD.delivery_model_invocation_id!=NEW.delivery_model_invocation_id
             OR OLD.provider_kind!=NEW.provider_kind OR OLD.capability_kind!=NEW.capability_kind
             OR OLD.boundary_kind!=NEW.boundary_kind OR OLD.claimed_at!=NEW.claimed_at
             OR OLD.claim_expires_at!=NEW.claim_expires_at
           BEGIN SELECT RAISE(ABORT,'V81 delivery attempt claim identity is immutable'); END;

         -- Evidence fills monotonically: a nonnull evidence scalar never
         -- changes or clears, and the attempt state only moves forward.
         CREATE TRIGGER agent_message_attempts_v81_evidence_monotonic
           BEFORE UPDATE ON agent_message_delivery_attempts
           WHEN (OLD.boundary_value IS NOT NULL AND OLD.boundary_value IS NOT NEW.boundary_value)
             OR (OLD.admission_classification IS NOT NULL
                 AND OLD.admission_classification IS NOT NEW.admission_classification)
             OR (OLD.provider_error_class IS NOT NULL
                 AND OLD.provider_error_class IS NOT NEW.provider_error_class)
             OR (OLD.dispatching_at IS NOT NULL
                 AND OLD.dispatching_at IS NOT NEW.dispatching_at)
             OR (OLD.admission_recorded_at IS NOT NULL
                 AND OLD.admission_recorded_at IS NOT NEW.admission_recorded_at)
             OR (OLD.effect_possible_at IS NOT NULL
                 AND OLD.effect_possible_at IS NOT NEW.effect_possible_at)
             OR (OLD.acknowledged_at IS NOT NULL
                 AND OLD.acknowledged_at IS NOT NEW.acknowledged_at)
             OR (OLD.effect_classification IS NOT NULL
                 AND OLD.effect_classification IS NOT NEW.effect_classification
                 -- effect_possible may only strengthen to acknowledged.
                 AND NOT (OLD.effect_classification='effect_possible'
                          AND NEW.effect_classification='effect_acknowledged'))
             -- Forward-only transition graph. The dispatching marker
             -- (H21-P2-R5-001) is inserted strictly between claimed and
             -- everything downstream: it is reachable ONLY from claimed and
             -- leads only forward, so adding it creates no cycle and no
             -- backward edge.
             --
             -- claimed keeps its original outward edges on purpose. The
             -- pre-dispatch rejection exits (invocation-fence mismatch, turn
             -- bind failure, unsupported provider) seal straight from claimed
             -- to terminal having never approached the provider, and the
             -- spawn-settlement conservative fill moves a never-dispatched
             -- attempt to effect_possible when effect cannot be excluded.
             -- Both are live paths and both are the SAFE direction.
             --
             -- Deliberately NOT done: forbidding claimed -> effect_possible to
             -- make the marker un-bypassable. It would break that
             -- spawn-settlement fill, and it would buy nothing real. This
             -- trigger governs row transitions, not whether a provider was
             -- called, so it cannot enforce the no-send-while-claimed property
             -- at all. That guarantee is an ORDERING property of the delivery
             -- path (the marker commits before the send, and the send is
             -- refused if the marker cannot be made durable) and is pinned
             -- where it is actually observable, at the dispatch instant, by
             -- a_claim_commits_before_any_provider_dispatch. Forbidding the
             -- edge would look structural while leaving the real invariant
             -- exactly where it already is.
             OR NOT (
               OLD.attempt_state=NEW.attempt_state
               OR (OLD.attempt_state='claimed'
                   AND NEW.attempt_state IN
                       ('dispatching','effect_possible','terminal'))
               OR (OLD.attempt_state='dispatching'
                   AND NEW.attempt_state IN ('effect_possible','terminal'))
               OR (OLD.attempt_state='effect_possible' AND NEW.attempt_state='terminal')
             )
           BEGIN SELECT RAISE(ABORT,'V81 delivery attempt evidence is fill-once and forward-only'); END;

         -- Correlation is forward-only and sealing is irreversible.
         CREATE TRIGGER agent_message_attempts_v81_correlation_coherence
           BEFORE UPDATE ON agent_message_delivery_attempts
           WHEN OLD.correlation_state!=NEW.correlation_state AND NOT (
             (OLD.correlation_state='correlation_pending'
              AND NEW.correlation_state IN ('correlated','sealed_live_uncertain'))
           )
           BEGIN SELECT RAISE(ABORT,'V81 correlation custody is forward-only and sealing is irreversible'); END;

         -- The acknowledgement event must resolve to the same Session and
         -- sequence it claims.
         CREATE TRIGGER agent_message_attempts_v81_ack_event_coherence
           BEFORE UPDATE ON agent_message_delivery_attempts
           WHEN NEW.acknowledged_event_id IS NOT NULL
             AND NOT EXISTS(SELECT 1 FROM conversation_events e
                            WHERE e.id=NEW.acknowledged_event_id
                              AND e.session_id=NEW.acknowledged_event_session_id
                              AND e.sequence=NEW.acknowledged_event_sequence)
           BEGIN SELECT RAISE(ABORT,'V81 acknowledgement event triple does not resolve to one exact event'); END;

         -- Once terminal, every settlement field is frozen.
         CREATE TRIGGER agent_message_attempts_v81_terminal_immutable
           BEFORE UPDATE ON agent_message_delivery_attempts
           WHEN OLD.attempt_state='terminal' AND (
                OLD.terminal_disposition IS NOT NEW.terminal_disposition
             OR OLD.settlement_authority IS NOT NEW.settlement_authority
             OR OLD.settlement_evidence_digest IS NOT NEW.settlement_evidence_digest
             OR OLD.settled_at IS NOT NEW.settled_at OR OLD.terminal_at IS NOT NEW.terminal_at
             OR OLD.attempt_state!=NEW.attempt_state
             OR OLD.effect_classification IS NOT NEW.effect_classification
             OR OLD.acknowledged_event_id IS NOT NEW.acknowledged_event_id)
           BEGIN SELECT RAISE(ABORT,'V81 terminal delivery attempt evidence is immutable'); END;",
    )?;

    // ---- transitions (3) -------------------------------------------------
    tx.execute_batch(
        "CREATE TRIGGER agent_message_transitions_v81_no_delete
           BEFORE DELETE ON agent_message_state_transitions
           BEGIN SELECT RAISE(ABORT,'agent message state transitions are append-only'); END;

         CREATE TRIGGER agent_message_transitions_v81_identity_immutable
           BEFORE UPDATE ON agent_message_state_transitions
           BEGIN SELECT RAISE(ABORT,'agent message state transitions are immutable'); END;

         -- Attempt linkage matches the frozen C-P2-09 rule: queued expiry and
         -- queued launch failure carry a null attempt; claimed expiry links
         -- the exact attempt already sealed proved_no_effect_expired.
         CREATE TRIGGER agent_message_transitions_v81_coherence
           BEFORE INSERT ON agent_message_state_transitions
           WHEN NOT (
             (NEW.attempt_number IS NULL OR EXISTS(
                SELECT 1 FROM agent_message_delivery_attempts a
                WHERE a.message_id=NEW.message_id AND a.attempt_number=NEW.attempt_number))
             AND (NEW.from_state!='queued' OR NEW.to_state!='expired'
                  OR (NEW.attempt_number IS NULL AND NEW.authority_kind='expiry_reconciler'))
             AND (NEW.from_state!='claimed' OR NEW.to_state!='expired'
                  OR EXISTS(SELECT 1 FROM agent_message_delivery_attempts a
                            WHERE a.message_id=NEW.message_id
                              AND a.attempt_number=NEW.attempt_number
                              AND a.terminal_disposition='proved_no_effect_expired'))
             AND (NEW.from_state!='queued' OR NEW.to_state!='failed'
                  OR NEW.attempt_number IS NULL)
             -- Claim, admission, uncertainty, acknowledgement, and settlement
             -- edges out of an active state always name their exact attempt.
             -- `uncertain` belongs here for the same reason `injected` does:
             -- both point at an effect-possible attempt, and an unnamed attempt
             -- on an uncertainty edge would erase the very custody the state
             -- exists to record.
             AND (NEW.to_state NOT IN ('claimed','injected','uncertain','acknowledged')
                  OR NEW.attempt_number IS NOT NULL)
           )
           BEGIN SELECT RAISE(ABORT,'V81 transition attempt linkage violates terminal coherence'); END;",
    )?;

    install_v81_effect_triggers(tx)
}

/// Create the 17 effect-custody triggers: 8 on the provider-request ledger,
/// 5 on the exact-turn gates, and 4 on the started-effect permits.
fn install_v81_effect_triggers(tx: &Transaction<'_>) -> Result<()> {
    // ---- provider-request ledger (8) ------------------------------------
    tx.execute_batch(
        "CREATE TRIGGER agent_message_request_effects_v81_no_delete
           BEFORE DELETE ON agent_message_provider_request_effects
           BEGIN SELECT RAISE(ABORT,'agent message provider request effects are append-only'); END;

         CREATE TRIGGER agent_message_request_effects_v81_identity_immutable
           BEFORE UPDATE ON agent_message_provider_request_effects
           WHEN OLD.message_id!=NEW.message_id OR OLD.attempt_number!=NEW.attempt_number
             OR OLD.turn_start_request_id!=NEW.turn_start_request_id
             OR OLD.provider_turn_id!=NEW.provider_turn_id
             OR OLD.provider_request_id!=NEW.provider_request_id
             OR OLD.provider_request_kind!=NEW.provider_request_kind
             OR OLD.provider_request_digest!=NEW.provider_request_digest
             OR OLD.delivery_model_invocation_id!=NEW.delivery_model_invocation_id
             OR OLD.turn_gate_generation!=NEW.turn_gate_generation
             OR OLD.evidence_event_id!=NEW.evidence_event_id
             OR OLD.evidence_event_session_id!=NEW.evidence_event_session_id
             OR OLD.evidence_event_sequence!=NEW.evidence_event_sequence
             OR OLD.handler_kind!=NEW.handler_kind
             OR OLD.evidence_committed_at!=NEW.evidence_committed_at
             OR OLD.created_at!=NEW.created_at
           BEGIN SELECT RAISE(ABORT,'V81 provider request identity and evidence custody are immutable'); END;

         -- The evidence triple resolves to one exact event, the request turn
         -- equals the attempt boundary value, and the attempt must already be
         -- correlated and durably acknowledged before evidence exists.
         CREATE TRIGGER agent_message_request_effects_v81_evidence_coherence
           BEFORE INSERT ON agent_message_provider_request_effects
           WHEN NOT (
             EXISTS(SELECT 1 FROM conversation_events e
                    WHERE e.id=NEW.evidence_event_id
                      AND e.session_id=NEW.evidence_event_session_id
                      AND e.sequence=NEW.evidence_event_sequence)
             AND EXISTS(SELECT 1 FROM agent_message_delivery_attempts a
                        WHERE a.message_id=NEW.message_id
                          AND a.attempt_number=NEW.attempt_number
                          AND a.delivery_model_invocation_id=NEW.delivery_model_invocation_id
                          AND a.boundary_value=NEW.provider_turn_id
                          AND a.correlation_state='correlated'
                          AND a.effect_classification='effect_acknowledged')
             -- No request row may exist without the same open gate generation.
             AND EXISTS(SELECT 1 FROM agent_message_provider_turn_gates g
                        WHERE g.message_id=NEW.message_id
                          AND g.attempt_number=NEW.attempt_number
                          AND g.delivery_model_invocation_id=NEW.delivery_model_invocation_id
                          AND g.provider_turn_id=NEW.provider_turn_id
                          AND g.gate_generation=NEW.turn_gate_generation
                          AND g.gate_state='open')
             AND NEW.handler_phase='evidence_committed'
             AND NEW.reply_phase='not_authorized'
             AND NEW.request_disposition='live'
           )
           BEGIN SELECT RAISE(ABORT,'V81 provider request evidence requires an acknowledged correlated attempt and an open gate'); END;

         -- Handler progression is frozen; a started phase never rewinds.
         CREATE TRIGGER agent_message_request_effects_v81_handler_forward
           BEFORE UPDATE ON agent_message_provider_request_effects
           WHEN OLD.handler_phase!=NEW.handler_phase AND NOT (
             (OLD.handler_phase='evidence_committed' AND NEW.handler_phase='handler_authorized')
          OR (OLD.handler_phase='handler_authorized' AND NEW.handler_phase='handler_started')
          OR (OLD.handler_phase='handler_started'
              AND NEW.handler_phase IN ('handler_completed','handler_uncertain'))
          OR (OLD.handler_phase='handler_completed' AND NEW.handler_phase='pending_decision')
          OR (OLD.handler_phase='pending_decision' AND NEW.handler_phase='decision_recorded')
           )
           BEGIN SELECT RAISE(ABORT,'V81 provider request handler phase is forward-only'); END;

         -- Approval custody: identity, presentation, and decision each fill
         -- exactly once and never clear.
         CREATE TRIGGER agent_message_request_effects_v81_approval_coherence
           BEFORE UPDATE ON agent_message_provider_request_effects
           WHEN (OLD.approval_id IS NOT NULL AND OLD.approval_id IS NOT NEW.approval_id)
             OR (OLD.approval_presented_at IS NOT NULL
                 AND OLD.approval_presented_at IS NOT NEW.approval_presented_at)
             OR (OLD.decision_kind IS NOT NULL AND OLD.decision_kind IS NOT NEW.decision_kind)
             OR (OLD.decision_digest IS NOT NULL AND OLD.decision_digest IS NOT NEW.decision_digest)
             OR (OLD.decision_authority IS NOT NULL
                 AND OLD.decision_authority IS NOT NEW.decision_authority)
             OR (OLD.decision_recorded_at IS NOT NULL
                 AND OLD.decision_recorded_at IS NOT NEW.decision_recorded_at)
             OR (OLD.pending_decision_at IS NOT NULL
                 AND OLD.pending_decision_at IS NOT NEW.pending_decision_at)
             -- A decision may be recorded only from a durable pending state.
             OR (NEW.decision_kind IS NOT NULL AND OLD.decision_kind IS NULL
                 AND OLD.handler_phase!='pending_decision')
           BEGIN SELECT RAISE(ABORT,'V81 approval identity, presentation, and decision are fill-once'); END;

         -- Reply progression is frozen; a started reply never resends.
         CREATE TRIGGER agent_message_request_effects_v81_reply_forward
           BEFORE UPDATE ON agent_message_provider_request_effects
           WHEN OLD.reply_phase!=NEW.reply_phase AND NOT (
             (OLD.reply_phase='not_authorized' AND NEW.reply_phase='reply_authorized')
          OR (OLD.reply_phase='reply_authorized' AND NEW.reply_phase='reply_started')
          OR (OLD.reply_phase='reply_started'
              AND NEW.reply_phase IN ('reply_completed','reply_uncertain'))
           )
           BEGIN SELECT RAISE(ABORT,'V81 provider request reply phase is forward-only'); END;

         -- A terminal request seals exactly once, preserves every prior phase
         -- and evidence value, and mints/executes/presents/replies nothing.
         CREATE TRIGGER agent_message_request_effects_v81_terminal_immutable
           BEFORE UPDATE ON agent_message_provider_request_effects
           WHEN OLD.request_disposition!='live' AND (
                OLD.request_disposition!=NEW.request_disposition
             OR OLD.handler_phase!=NEW.handler_phase OR OLD.reply_phase!=NEW.reply_phase
             OR OLD.handler_capability_id IS NOT NEW.handler_capability_id
             OR OLD.reply_capability_id IS NOT NEW.reply_capability_id
             OR OLD.decision_kind IS NOT NEW.decision_kind
             OR OLD.approval_id IS NOT NEW.approval_id
             OR OLD.provider_terminal_sealed_at IS NOT NEW.provider_terminal_sealed_at
             OR OLD.provider_terminal_authority IS NOT NEW.provider_terminal_authority
             OR OLD.provider_terminal_evidence_digest IS NOT NEW.provider_terminal_evidence_digest)
           BEGIN SELECT RAISE(ABORT,'V81 terminal provider request custody is immutable'); END;

         -- Every *_started transition must have inserted its unique permit,
         -- and a capability is consumed exactly once and never cleared.
         CREATE TRIGGER agent_message_request_effects_v81_permit_coherence
           BEFORE UPDATE ON agent_message_provider_request_effects
           WHEN (OLD.handler_capability_id IS NOT NULL
                 AND OLD.handler_capability_id IS NOT NEW.handler_capability_id)
             OR (OLD.reply_capability_id IS NOT NULL
                 AND OLD.reply_capability_id IS NOT NEW.reply_capability_id)
             OR (NEW.handler_phase='handler_started' AND OLD.handler_phase!='handler_started'
                 AND NOT EXISTS(SELECT 1 FROM agent_message_provider_effect_permits p
                                WHERE p.message_id=NEW.message_id
                                  AND p.attempt_number=NEW.attempt_number
                                  AND p.turn_start_request_id=NEW.turn_start_request_id
                                  AND p.provider_turn_id=NEW.provider_turn_id
                                  AND p.provider_request_id=NEW.provider_request_id
                                  AND p.provider_request_kind=NEW.provider_request_kind
                                  AND p.provider_request_digest=NEW.provider_request_digest
                                  AND p.request_phase='handler_started'))
             OR (NEW.reply_phase='reply_started' AND OLD.reply_phase!='reply_started'
                 AND NOT EXISTS(SELECT 1 FROM agent_message_provider_effect_permits p
                                WHERE p.message_id=NEW.message_id
                                  AND p.attempt_number=NEW.attempt_number
                                  AND p.turn_start_request_id=NEW.turn_start_request_id
                                  AND p.provider_turn_id=NEW.provider_turn_id
                                  AND p.provider_request_id=NEW.provider_request_id
                                  AND p.provider_request_kind=NEW.provider_request_kind
                                  AND p.provider_request_digest=NEW.provider_request_digest
                                  AND p.permit_kind='provider_reply'))
           BEGIN SELECT RAISE(ABORT,'V81 started effect requires its issued permit and a fill-once capability'); END;",
    )?;

    // ---- exact-turn gates (5) -------------------------------------------
    tx.execute_batch(
        "CREATE TRIGGER agent_message_turn_gates_v81_no_delete
           BEFORE DELETE ON agent_message_provider_turn_gates
           BEGIN SELECT RAISE(ABORT,'agent message provider turn gates are append-only'); END;

         CREATE TRIGGER agent_message_turn_gates_v81_identity_immutable
           BEFORE UPDATE ON agent_message_provider_turn_gates
           WHEN OLD.message_id!=NEW.message_id OR OLD.attempt_number!=NEW.attempt_number
             OR OLD.delivery_model_invocation_id!=NEW.delivery_model_invocation_id
             OR OLD.provider_turn_id!=NEW.provider_turn_id
             OR OLD.gate_generation!=NEW.gate_generation OR OLD.opened_at!=NEW.opened_at
           BEGIN SELECT RAISE(ABORT,'V81 exact-turn gate identity is immutable'); END;

         -- open -> closing -> closed only.
         CREATE TRIGGER agent_message_turn_gates_v81_forward
           BEFORE UPDATE ON agent_message_provider_turn_gates
           WHEN OLD.gate_state!=NEW.gate_state AND NOT (
             (OLD.gate_state='open' AND NEW.gate_state='closing')
          OR (OLD.gate_state='closing' AND NEW.gate_state='closed')
           )
           BEGIN SELECT RAISE(ABORT,'V81 exact-turn gate state is forward-only'); END;

         -- Counters are transaction-derived and monotonic; the admission
         -- sequence advances only while the gate is open, so closing blocks
         -- every later request insert, capability mint, and effect start.
         CREATE TRIGGER agent_message_turn_gates_v81_admission_coherence
           BEFORE UPDATE ON agent_message_provider_turn_gates
           WHEN NEW.admission_sequence<OLD.admission_sequence
             OR NEW.issued_permit_count<OLD.issued_permit_count
             OR NEW.settled_permit_count<OLD.settled_permit_count
             OR (NEW.admission_sequence!=OLD.admission_sequence AND OLD.gate_state!='open')
             OR (NEW.issued_permit_count!=OLD.issued_permit_count AND OLD.gate_state!='open')
             OR (OLD.gate_state!='open' AND NEW.terminal_authority IS NOT OLD.terminal_authority)
           BEGIN SELECT RAISE(ABORT,'V81 gate admission and permit counters are monotonic and open-only'); END;

         -- closing -> closed requires every issued permit durably resolved.
         CREATE TRIGGER agent_message_turn_gates_v81_close_requires_settled_permits
           BEFORE UPDATE ON agent_message_provider_turn_gates
           WHEN NEW.gate_state='closed' AND OLD.gate_state!='closed' AND (
             NEW.settled_permit_count!=NEW.issued_permit_count
             OR EXISTS(SELECT 1 FROM agent_message_provider_effect_permits p
                       WHERE p.message_id=NEW.message_id
                         AND p.attempt_number=NEW.attempt_number
                         AND p.delivery_model_invocation_id=NEW.delivery_model_invocation_id
                         AND p.provider_turn_id=NEW.provider_turn_id
                         AND p.turn_gate_generation=NEW.gate_generation
                         AND p.permit_state IN ('issued','uncertain_unjoined')))
           BEGIN SELECT RAISE(ABORT,'V81 gate cannot close while a started effect permit is unresolved'); END;",
    )?;

    // ---- started-effect permits (4) -------------------------------------
    tx.execute_batch(
        "CREATE TRIGGER agent_message_effect_permits_v81_no_delete
           BEFORE DELETE ON agent_message_provider_effect_permits
           BEGIN SELECT RAISE(ABORT,'agent message provider effect permits are append-only'); END;

         CREATE TRIGGER agent_message_effect_permits_v81_identity_immutable
           BEFORE UPDATE ON agent_message_provider_effect_permits
           WHEN OLD.permit_id!=NEW.permit_id OR OLD.message_id!=NEW.message_id
             OR OLD.attempt_number!=NEW.attempt_number
             OR OLD.turn_start_request_id!=NEW.turn_start_request_id
             OR OLD.provider_turn_id!=NEW.provider_turn_id
             OR OLD.provider_request_id!=NEW.provider_request_id
             OR OLD.provider_request_kind!=NEW.provider_request_kind
             OR OLD.provider_request_digest!=NEW.provider_request_digest
             OR OLD.delivery_model_invocation_id!=NEW.delivery_model_invocation_id
             OR OLD.turn_gate_generation!=NEW.turn_gate_generation
             OR OLD.permit_kind!=NEW.permit_kind OR OLD.request_phase!=NEW.request_phase
             OR OLD.executor_boot_id!=NEW.executor_boot_id
             -- external_join_id and the writer operation_id are immutable once
             -- issued: they are the only handles a restart can join on.
             OR OLD.external_join_id!=NEW.external_join_id
             OR (OLD.operation_id IS NOT NULL AND OLD.operation_id IS NOT NEW.operation_id)
             OR OLD.issued_at!=NEW.issued_at
           BEGIN SELECT RAISE(ABORT,'V81 effect permit identity and join handles are immutable'); END;

         -- A permit settles exactly once and never returns to issued.
         CREATE TRIGGER agent_message_effect_permits_v81_forward
           BEFORE UPDATE ON agent_message_provider_effect_permits
           WHEN OLD.permit_state!=NEW.permit_state AND NOT (
             OLD.permit_state='issued'
             AND NEW.permit_state IN ('completed','uncertain_unjoined','uncertain_joined','cancel_confirmed')
             -- An unjoined uncertainty may later acquire exact join or
             -- cancel-confirmation evidence; nothing else moves.
             OR (OLD.permit_state='uncertain_unjoined'
                 AND NEW.permit_state IN ('uncertain_joined','cancel_confirmed','completed'))
           )
           BEGIN SELECT RAISE(ABORT,'V81 effect permit state is forward-only and settles once'); END;

         -- Settlement evidence and writer receipts are fill-once.
         CREATE TRIGGER agent_message_effect_permits_v81_evidence_coherence
           BEFORE UPDATE ON agent_message_provider_effect_permits
           WHEN (OLD.completion_evidence_digest IS NOT NULL
                 AND OLD.completion_evidence_digest IS NOT NEW.completion_evidence_digest)
             OR (OLD.join_evidence_digest IS NOT NULL
                 AND OLD.join_evidence_digest IS NOT NEW.join_evidence_digest)
             OR (OLD.cancel_evidence_digest IS NOT NULL
                 AND OLD.cancel_evidence_digest IS NOT NEW.cancel_evidence_digest)
             OR (OLD.cancel_confirmed_at IS NOT NULL
                 AND OLD.cancel_confirmed_at IS NOT NEW.cancel_confirmed_at)
             OR (OLD.cancel_requested_at IS NOT NULL
                 AND OLD.cancel_requested_at IS NOT NEW.cancel_requested_at)
             OR (OLD.writer_receipt_state IS NOT NULL
                 AND OLD.writer_receipt_state IS NOT NEW.writer_receipt_state)
             OR (OLD.writer_receipt_digest IS NOT NULL
                 AND OLD.writer_receipt_digest IS NOT NEW.writer_receipt_digest)
             OR (OLD.writer_receipt_at IS NOT NULL
                 AND OLD.writer_receipt_at IS NOT NEW.writer_receipt_at)
             OR (OLD.settled_at IS NOT NULL AND OLD.settled_at IS NOT NEW.settled_at)
           BEGIN SELECT RAISE(ABORT,'V81 effect permit settlement evidence is fill-once'); END;",
    )?;

    Ok(())
}

/// Rebuild the mailbox domain from accepted V80 into corrected V81 (P2-02).
///
/// Only the mailbox changes: V80's accepted spawn, progress, and watch objects
/// are never edited (C-P2-03). Every compatible legacy row is copied exactly
/// once; any nonqueued, malformed, orphaned, or otherwise unrepresentable row
/// aborts the WHOLE migration rather than being dropped, guessed, or partially
/// copied.
pub(crate) fn install_v81_schema(tx: &Transaction<'_>) -> Result<()> {
    for (kind, name) in AGENT_MESSAGE_V80_REPLACED_OBJECTS {
        tx.execute_batch(&format!("DROP {kind} {name};"))?;
    }
    tx.execute_batch("ALTER TABLE agent_messages RENAME TO agent_messages_v80_legacy;")?;

    install_v81_tables(tx)?;
    install_v81_indexes(tx)?;
    install_v81_triggers(tx)?;
    copy_v80_messages_into_v81(tx)?;

    tx.execute_batch("DROP TABLE agent_messages_v80_legacy;")?;
    Ok(())
}

/// Scratch relation the V82 attempts rebuild parks rows in. Named so it can
/// never collide with a catalog object the fingerprint inventories.
const AGENT_MESSAGE_V82_REBUILD_SCRATCH: &str =
    "agent_message_delivery_attempts_v82_rebuild_scratch";

/// Amend the shipped V81 mailbox catalog into V82 (P2-06d).
///
/// Exactly two objects change and nothing else:
///   1. `agent_message_delivery_attempts` gains the R7 structural backstop
///      CHECK. `SQLite` has no `ALTER TABLE ... ADD CONSTRAINT`, so the relation
///      must be rebuilt.
///   2. `agent_messages_v81_cas_coherence` gains the GAP 1
///      `sealed_live_uncertain` guard. A trigger CAN be replaced in place, so
///      that half needs no rebuild.
///
/// The rebuild REPLAYS the attempts relation's own indexes and triggers from
/// the database's catalog rather than restating them, in catalog order, so this
/// migration cannot silently drift from the V81 definitions it is not amending.
/// `rebuild_d05_v78_table` uses the same discipline for the same reason.
///
/// **CALLER CONTRACT: `PRAGMA foreign_keys` MUST be OFF.** Four relations hold
/// `ON DELETE RESTRICT` foreign keys INTO this table — `agent_messages`,
/// `agent_message_state_transitions`, `agent_message_provider_turn_gates`, and
/// `agent_message_provider_request_effects` — and a RESTRICT action fires
/// IMMEDIATELY even when its constraint is `DEFERRABLE INITIALLY DEFERRED`. The
/// implicit `DELETE FROM` that `DROP TABLE` performs under `foreign_keys=ON`
/// would therefore abort against any live attempt row. This is precisely why
/// `rebuild_d05_v78_table`'s safety argument ("neither rebuilt table is the
/// target of a foreign key") does NOT transfer here, and why this follows the
/// V65 `PRAGMA foreign_keys=OFF` precedent instead. `PRAGMA foreign_key_check`
/// is an explicit sweep, unaffected by the enforcement pragma, so the caller's
/// post-rebuild referential proof still holds.
pub(crate) fn install_v82_amendments(tx: &Transaction<'_>) -> Result<()> {
    // Refuse legibly BEFORE the first destructive statement rather than letting
    // a live row fail the new CHECK mid-reinsert. A row that trips this is a
    // real custody violation already durable on disk: the restart reconciler
    // sealed a post-marker attempt proved-no-effect, which it can never have
    // had the evidence to do. Aborting is the only correct answer -- such a row
    // must never be dropped, rewritten, or guessed at.
    let violating: i64 = tx.query_row(
        "SELECT count(*) FROM agent_message_delivery_attempts
          WHERE settlement_authority='restart_reconciler'
            AND terminal_disposition IN
                ('proved_no_effect_requeue','proved_no_effect_failed','proved_no_effect_expired')
            AND dispatching_at IS NOT NULL",
        [],
        |row| row.get(0),
    )?;
    if violating != 0 {
        return Err(DaemonError::Store(format!(
            "V82 migration found {violating} delivery attempts the restart reconciler sealed \
             proved-no-effect after the pre-dispatch marker; refusing to drop, rewrite, or guess"
        )));
    }

    let scratch = AGENT_MESSAGE_V82_REBUILD_SCRATCH;
    let aux_ddl = {
        let mut catalog = tx.prepare(
            "SELECT sql FROM sqlite_master
             WHERE tbl_name = 'agent_message_delivery_attempts'
               AND type IN ('index','trigger') AND sql IS NOT NULL
             ORDER BY rowid",
        )?;
        catalog
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let before: i64 = tx.query_row(
        "SELECT count(*) FROM agent_message_delivery_attempts",
        [],
        |row| row.get(0),
    )?;

    // `DROP TABLE` fires no triggers, so the append-only BEFORE DELETE guard is
    // not an obstacle; the FK enforcement pragma is, which is the caller's job.
    tx.execute_batch(&format!(
        "DROP TABLE IF EXISTS {scratch};
         CREATE TABLE {scratch} AS SELECT * FROM agent_message_delivery_attempts;
         DROP TABLE agent_message_delivery_attempts;"
    ))?;
    tx.execute_batch(&agent_message_delivery_attempts_ddl(true))?;
    for statement in aux_ddl {
        tx.execute_batch(&format!("{statement};"))?;
    }
    // The amendment adds a table-level CHECK, never a column, so column order is
    // unchanged and the positional insert is exact.
    //
    // R10 L-3: the invariant this ordering rests on is STRONGER than "no BEFORE
    // INSERT trigger". The auxiliary DDL above is replayed BEFORE the rows are
    // copied back, so this relation must carry no INSERT trigger of ANY timing —
    // no BEFORE, and no AFTER either. An AFTER INSERT trigger would fire once per
    // restored row during this copy-back and author spurious evidence, and that
    // failure would be SILENT rather than a constraint abort. It holds today (the
    // relation has no INSERT trigger at all); if one is ever added, copy the rows
    // back BEFORE replaying the triggers, or suspend it across the copy.
    tx.execute_batch(&format!(
        "INSERT INTO agent_message_delivery_attempts SELECT * FROM {scratch};
         DROP TABLE {scratch};"
    ))?;

    let after: i64 = tx.query_row(
        "SELECT count(*) FROM agent_message_delivery_attempts",
        [],
        |row| row.get(0),
    )?;
    if before != after {
        return Err(DaemonError::Store(format!(
            "V82 rebuild carried {before} delivery attempts in but {after} out"
        )));
    }

    // Replacing a trigger APPENDS it to the catalog, and `SQLite` fires the
    // triggers of one table in REVERSE creation order. A naive DROP + CREATE
    // therefore promotes the amended trigger ahead of every sibling created
    // after it, silently changing WHICH refusal a caller observes for a write
    // that several triggers reject — a real behavior change even though the
    // write stays refused either way.
    //
    // So capture the siblings that follow it, drop them, recreate the amended
    // trigger in its original slot, and replay them in catalog order. Their SQL
    // is replayed from the database rather than restated, for the same
    // no-drift reason as the index/trigger replay above.
    let trailing = {
        let mut catalog = tx.prepare(
            "SELECT name, sql FROM sqlite_master
             WHERE type='trigger' AND tbl_name='agent_messages' AND sql IS NOT NULL
               AND rowid > (SELECT rowid FROM sqlite_master
                            WHERE type='trigger' AND name='agent_messages_v81_cas_coherence')
             ORDER BY rowid",
        )?;
        catalog
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    for (name, _) in &trailing {
        tx.execute_batch(&format!("DROP TRIGGER {name};"))?;
    }
    tx.execute_batch("DROP TRIGGER agent_messages_v81_cas_coherence;")?;
    tx.execute_batch(&agent_messages_v81_cas_coherence_ddl(true))?;
    for (_, sql) in &trailing {
        tx.execute_batch(&format!("{sql};"))?;
    }
    Ok(())
}

/// Copy every compatible V80 mailbox row exactly once.
///
/// Because accepted V80 exposed no writer, compatibility is deliberately
/// narrow: the row must be `queued`, and its Session custody, digests,
/// timestamps, and payload must pass BOTH schemas. Anything else aborts.
fn copy_v80_messages_into_v81(tx: &Transaction<'_>) -> Result<()> {
    let unrepresentable: i64 = tx.query_row(
        "SELECT count(*) FROM agent_messages_v80_legacy l
         WHERE l.state!='queued'
            OR NOT EXISTS(SELECT 1 FROM sessions s WHERE s.id=l.target_session_id)
            OR NOT EXISTS(SELECT 1 FROM sessions s WHERE s.id=l.owner_session_id)",
        [],
        |row| row.get(0),
    )?;
    if unrepresentable != 0 {
        return Err(DaemonError::Store(format!(
            "V81 migration found {unrepresentable} unrepresentable legacy agent_messages rows; \
             refusing to drop, guess, or partially copy"
        )));
    }

    // Ordinary Session custody: accepted V80 had no reservation column, so
    // every copied row enters V81 with target_spawn_request_id NULL, version
    // and attempt zero, a null current attempt, and null terminal fields.
    let copied = tx.execute(
        "INSERT INTO agent_messages (
             id, owner_session_id, target_session_id, target_spawn_request_id,
             idempotency_digest, request_fingerprint, payload_digest, payload,
             created_at, expires_at, state, state_version, attempt_count,
             current_attempt_number, safe_error_class, updated_at,
             acknowledged_at, uncertain_at, failed_at, expired_at)
         SELECT id, owner_session_id, target_session_id, NULL,
                idempotency_digest, request_fingerprint, payload_digest, payload,
                created_at, expires_at, 'queued', 0, 0,
                NULL, NULL, updated_at,
                NULL, NULL, NULL, NULL
         FROM agent_messages_v80_legacy
         ORDER BY id",
        [],
    )?;

    // Exactly one migration-authored version-0 none->queued transition per
    // copied row. No attempt is ever invented.
    let transitions = tx.execute(
        "INSERT INTO agent_message_state_transitions (
             message_id, state_version, from_state, to_state, attempt_number,
             authority_kind, authority_id, evidence_digest, created_at)
         SELECT id, 0, 'none', 'queued', NULL,
                'migration', 'v81-migration',
                payload_digest, created_at
         FROM agent_messages
         ORDER BY id",
        [],
    )?;

    if copied != transitions {
        return Err(DaemonError::Store(format!(
            "V81 migration copied {copied} messages but authored {transitions} transitions"
        )));
    }
    Ok(())
}

/// Compute the corrected-V81 semantic catalog fingerprint over the exact six
/// tables, 18 indexes, and 32 triggers (C-P2-17).
///
/// The input contains ordered `table_info`, foreign keys, index definitions
/// including predicates, trigger definitions including normalized SQL, the
/// expected counts, and `user_version`. Wildcard discovery and count-only
/// acceptance are forbidden.
pub(crate) fn v81_schema_fingerprint(connection: &rusqlite::Connection) -> Result<String> {
    let mut hasher = Sha256::new();

    let mut absorb = |hasher: &mut Sha256, value: &str| {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    };

    // Expected counts are part of the digest input, not a separate check.
    absorb(&mut hasher, "v81");
    for count in [
        AGENT_MESSAGE_V81_TABLES.len(),
        AGENT_MESSAGE_V81_INDEXES.len(),
        AGENT_MESSAGE_V81_TRIGGERS.len(),
    ] {
        absorb(&mut hasher, &count.to_string());
    }

    // Ordered catalog SQL for every named object, matched exactly by name.
    let mut names: Vec<(&str, &str)> = Vec::new();
    for name in AGENT_MESSAGE_V81_TABLES {
        names.push(("table", name));
    }
    for name in AGENT_MESSAGE_V81_INDEXES {
        names.push(("index", name));
    }
    for name in AGENT_MESSAGE_V81_TRIGGERS {
        names.push(("trigger", name));
    }

    for (expected_kind, name) in names {
        let mut stmt = connection.prepare(
            "SELECT type,name,tbl_name,COALESCE(sql,'') FROM sqlite_master WHERE name=?1",
        )?;
        let row = stmt
            .query_row([name], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .optional()?;
        let Some((kind, actual_name, table, sql)) = row else {
            return Err(DaemonError::Store(format!(
                "V81 catalog is missing required {expected_kind} {name}"
            )));
        };
        if kind != expected_kind {
            return Err(DaemonError::Store(format!(
                "V81 catalog object {name} is a {kind}, expected {expected_kind}"
            )));
        }
        for value in [
            kind.as_str(),
            actual_name.as_str(),
            table.as_str(),
            normalize_catalog_sql(&sql).as_str(),
        ] {
            absorb(&mut hasher, value);
        }
    }

    // Ordered column and foreign-key shape for every table.
    for table in AGENT_MESSAGE_V81_TABLES {
        let mut stmt = connection.prepare(&format!(
            "SELECT cid,name,type,\"notnull\",COALESCE(dflt_value,''),pk
             FROM pragma_table_info('{table}') ORDER BY cid"
        ))?;
        let columns = stmt
            .query_map([], |row| {
                Ok(format!(
                    "{}|{}|{}|{}|{}|{}",
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        absorb(&mut hasher, table);
        absorb(&mut hasher, &columns.len().to_string());
        for column in &columns {
            absorb(&mut hasher, column);
        }

        let mut fk_stmt = connection.prepare(&format!(
            "SELECT id,seq,\"table\",\"from\",\"to\",on_update,on_delete,match
             FROM pragma_foreign_key_list('{table}') ORDER BY id,seq"
        ))?;
        let keys = fk_stmt
            .query_map([], |row| {
                Ok(format!(
                    "{}|{}|{}|{}|{}|{}|{}|{}",
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        absorb(&mut hasher, &keys.len().to_string());
        for key in &keys {
            absorb(&mut hasher, key);
        }
    }

    // The copied-data contract and the LIVE user version.
    //
    // Absorbing `PRAGMA user_version` rather than the pinned constant is what
    // lets the digest detect a catalog/version mismatch on reopen, which plan
    // `:2251-2254` requires of it ("...and user version"). Absorbing the
    // constant instead would contribute ZERO runtime verification: it can
    // never disagree with itself.
    //
    // This is safe only because the migration computes this fingerprint AFTER
    // its `PRAGMA user_version` write (see the V81 block in `store/mod.rs`),
    // so the migrating connection and every later reopen absorb the same value
    // for one identical catalog.
    let user_version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    absorb(&mut hasher, &user_version.to_string());
    absorb(
        &mut hasher,
        "copy:queued-only;attempts:0;transitions:v0-migration",
    );

    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}
