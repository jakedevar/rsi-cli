//! Restore of a cold session (#2384, slice S2; plan §4.3).
//!
//! Restore puts a cold session's archived rows back into `conversation_events`
//! and `conversation_event_provenance` with their original ids, then flips the
//! index row `cold` -> `restored`. It is one store protocol in three steps so a
//! caller can decode the chunks outside the store mutex:
//!
//! 1. [`Store::cold_restore_plan`] (short, under the store): snapshot the
//!    [`ColdRef`] of a `cold` session, or learn it is already hot.
//! 2. [`decode_cold_session`] (no store): read, verify and decode the chunks.
//! 3. [`Store::cold_restore_commit`] (one `IMMEDIATE` transaction): a
//!    compare-and-set fence on the whole [`ColdRef`], then plain `INSERT`s.
//!    A fence that matches nothing is `AlreadyHot` (another caller restored it)
//!    or `Stale` (the row moved to another generation or chunk: re-plan).
//!
//! The session's spawn guard serializes a restore with a resume, an unarchive
//! and the archiver's move. Every entry point takes the guard as proof and never
//! acquires it, so a caller that already holds it cannot self-deadlock.
//! [`Store::restore_cold_session`] runs the three steps with a bounded retry.

use super::candidates::TOTAL_EVENTS_SQL;
use super::reader::{ColdRef, cache_forget, read_events_chunk, read_provenance_chunk};
use super::{ColdError, ColdResult};
use crate::store::Store;
use crate::store_support::spawn_single_flight::SpawnGuard;
use rusqlite::types::Value;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use uuid::Uuid;

/// Attempts before a restore gives up with `cold_restore_contended`.
pub const RESTORE_ATTEMPTS: u32 = 3;

/// The result of a restore, as `RestoreColdSession` reports it. Both outcomes
/// are success.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RestoreReceipt {
    /// `restored` or `already_hot`.
    pub outcome: &'static str,
    pub generation: i64,
    /// Event rows inserted (0 for `already_hot`).
    pub events: i64,
    /// Provenance rows inserted (0 for `already_hot`).
    pub provenance: i64,
}

impl RestoreReceipt {
    /// The receipt of a session whose rows are already live.
    #[must_use]
    pub const fn already_hot(generation: i64) -> Self {
        Self {
            outcome: "already_hot",
            generation,
            events: 0,
            provenance: 0,
        }
    }
}

/// A cold session's rows, decoded and checked against the index row.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedColdSession {
    /// `conversation_events` rows in `EVENT_COLUMNS` order.
    pub events: Vec<Vec<Value>>,
    /// `conversation_event_provenance` rows in `PROVENANCE_COLUMNS` order.
    pub provenance: Vec<Vec<Value>>,
}

/// What a commit did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColdRestoreCommit {
    /// The rows are live again; the index row is `restored`.
    Restored(RestoreReceipt),
    /// The session was not `cold` any more (another caller restored it): nothing
    /// was inserted.
    AlreadyHot { generation: i64 },
    /// The session is still `cold` but at another generation or chunk than the
    /// decoded one: drop the decode and plan again.
    Stale,
}

/// Reads, verifies and decodes a cold session's chunks (steps 2 of the protocol).
/// Needs no store, so a daemon calls it outside the store mutex.
pub fn decode_cold_session(cold: &ColdRef) -> ColdResult<DecodedColdSession> {
    let events = read_events_chunk(cold)?;
    let provenance = read_provenance_chunk(cold)?;
    Ok(DecodedColdSession { events, provenance })
}

fn require_guard(guard: &SpawnGuard, session_id: Uuid) -> ColdResult<()> {
    if guard.session_id() == session_id {
        Ok(())
    } else {
        Err(ColdError::new(
            "cold_restore_guard_mismatch",
            format!(
                "the spawn guard held is for {}, the restore is for {session_id}",
                guard.session_id()
            ),
        ))
    }
}

fn now_string() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

/// The compare-and-set fence: the row must still be the one that was decoded.
const FENCE_SQL: &str = "UPDATE cold_archive_sessions
     SET state = 'restoring', updated_at = ?2
     WHERE session_id = ?1 AND state = 'cold' AND generation = ?3 AND file_id = ?4
       AND events_chunk_id = ?5 AND provenance_chunk_id IS ?6
       AND events_sha256 = ?7 AND provenance_sha256 IS ?8";

/// Plain `INSERT`s with the original ids: never `OR IGNORE`, never `REPLACE`. A
/// primary-key collision is a contradiction, not a race.
const INSERT_EVENT_SQL: &str = "INSERT INTO conversation_events
     (id, session_id, sequence, event_type, role, content, tool_name, tool_input,
      created_at, tool_use_id, metadata)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)";
const INSERT_PROVENANCE_SQL: &str = "INSERT INTO conversation_event_provenance
     (conversation_event_id, producer_kind, model_invocation_id, provider_event_type, created_at)
     VALUES (?1, ?2, ?3, ?4, ?5)";

const SETTLE_SQL: &str = "UPDATE cold_archive_sessions
     SET state = 'restored', restored_at = ?2, updated_at = ?2, last_error_code = NULL
     WHERE session_id = ?1 AND state = 'restoring'";

/// Maps an `INSERT` failure: a primary-key or unique collision is the typed
/// `cold_restore_conflict`; anything else is `cold_restore_failed`.
fn insert_error(error: &rusqlite::Error, row: &[Value]) -> ColdError {
    let id = match row.first() {
        Some(Value::Integer(id)) => id.to_string(),
        _ => "?".to_string(),
    };
    if let rusqlite::Error::SqliteFailure(failure, _) = error
        && (failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
            || failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE)
    {
        return ColdError::new(
            "cold_restore_conflict",
            format!("event id {id} already exists in the live table"),
        );
    }
    ColdError::new(
        "cold_restore_failed",
        format!("restoring a row (id {id}) failed: {error}"),
    )
}

impl Store {
    /// Step 1: the reference of `session_id` if its rows are in the archive.
    /// `None` means the session is already hot. Takes the spawn guard as proof
    /// that the caller serializes with every other resume path.
    pub fn cold_restore_plan(
        &self,
        guard: &SpawnGuard,
        session_id: Uuid,
    ) -> ColdResult<Option<ColdRef>> {
        require_guard(guard, session_id)?;
        self.cold_ref(&session_id.to_string())
    }

    /// The `already_hot` receipt of a session that is not `cold`: its index row's
    /// generation, or 0 when it never had one.
    pub fn cold_already_hot_receipt(&self, session_id: Uuid) -> ColdResult<RestoreReceipt> {
        let generation = self
            .cold_session_row(&session_id.to_string())?
            .map_or(0, |row| row.generation);
        Ok(RestoreReceipt::already_hot(generation))
    }

    /// Step 3: fence on `cold` (the whole reference), insert the decoded rows with
    /// their original ids, and flip the index row to `restored`, in one
    /// `IMMEDIATE` transaction. A failure rolls back and the row stays `cold`.
    pub fn cold_restore_commit(
        &self,
        guard: &SpawnGuard,
        cold: &ColdRef,
        decoded: &DecodedColdSession,
    ) -> ColdResult<ColdRestoreCommit> {
        let session_uuid = Uuid::parse_str(&cold.session_id).map_err(|error| {
            ColdError::new("cold_restore_failed", format!("bad session id: {error}"))
        })?;
        require_guard(guard, session_uuid)?;
        let session_id = cold.session_id.as_str();
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let now = now_string();
        let fenced = tx.execute(
            FENCE_SQL,
            params![
                session_id,
                now,
                cold.generation,
                cold.file_id,
                cold.events_chunk_id,
                cold.provenance_chunk_id,
                cold.events_sha256,
                cold.provenance_sha256,
            ],
        )?;
        if fenced == 0 {
            let current: Option<(String, i64)> = tx
                .query_row(
                    "SELECT state, generation FROM cold_archive_sessions WHERE session_id = ?1",
                    [session_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            drop(tx);
            return Ok(match current {
                Some((state, _)) if state == "cold" => ColdRestoreCommit::Stale,
                Some((_, generation)) => ColdRestoreCommit::AlreadyHot { generation },
                None => ColdRestoreCommit::AlreadyHot {
                    generation: cold.generation,
                },
            });
        }

        let live_before: i64 = tx.query_row(TOTAL_EVENTS_SQL, [session_id], |row| row.get(0))?;
        let mut inserted_events = 0_i64;
        {
            let mut insert = tx.prepare(INSERT_EVENT_SQL)?;
            for row in &decoded.events {
                insert
                    .execute(rusqlite::params_from_iter(row.iter()))
                    .map_err(|error| insert_error(&error, row))?;
                inserted_events += 1;
            }
        }
        let mut inserted_provenance = 0_i64;
        {
            let mut insert = tx.prepare(INSERT_PROVENANCE_SQL)?;
            for row in &decoded.provenance {
                insert
                    .execute(rusqlite::params_from_iter(row.iter()))
                    .map_err(|error| insert_error(&error, row))?;
                inserted_provenance += 1;
            }
        }
        if inserted_events != cold.archived_event_count
            || inserted_provenance != cold.archived_provenance_count
        {
            return Err(ColdError::new(
                "cold_restore_count_mismatch",
                format!(
                    "inserted {inserted_events} events and {inserted_provenance} provenance rows, \
                     the index records {} and {}",
                    cold.archived_event_count, cold.archived_provenance_count
                ),
            ));
        }
        // The logical transcript is unchanged: live rows now equal the live rows
        // before plus the archived ones.
        let live_after: i64 = tx.query_row(TOTAL_EVENTS_SQL, [session_id], |row| row.get(0))?;
        if live_after != live_before + cold.archived_event_count {
            return Err(ColdError::new(
                "cold_restore_count_mismatch",
                format!(
                    "the session holds {live_after} live events after the restore, expected {}",
                    live_before + cold.archived_event_count
                ),
            ));
        }
        let settled = tx.execute(SETTLE_SQL, params![session_id, now_string()])?;
        if settled != 1 {
            return Err(ColdError::new(
                "cold_restore_failed",
                "the index row left the restoring state inside its own transaction",
            ));
        }
        tx.commit()?;
        cache_forget(session_id);
        Ok(ColdRestoreCommit::Restored(RestoreReceipt {
            outcome: "restored",
            generation: cold.generation,
            events: inserted_events,
            provenance: inserted_provenance,
        }))
    }

    /// The whole protocol with the store held throughout: plan, decode, commit,
    /// retrying a `Stale` fence up to [`RESTORE_ATTEMPTS`] times
    /// (`cold_restore_contended` after that). A daemon that must not hold the
    /// store while it decodes runs the three steps itself.
    pub fn restore_cold_session(
        &self,
        guard: &SpawnGuard,
        session_id: Uuid,
    ) -> ColdResult<RestoreReceipt> {
        for _ in 0..RESTORE_ATTEMPTS {
            let Some(cold) = self.cold_restore_plan(guard, session_id)? else {
                return self.cold_already_hot_receipt(session_id);
            };
            let decoded = decode_cold_session(&cold)?;
            match self.cold_restore_commit(guard, &cold, &decoded)? {
                ColdRestoreCommit::Restored(receipt) => return Ok(receipt),
                ColdRestoreCommit::AlreadyHot { generation } => {
                    return Ok(RestoreReceipt::already_hot(generation));
                }
                ColdRestoreCommit::Stale => {}
            }
        }
        Err(ColdError::new(
            "cold_restore_contended",
            format!("{session_id} changed under {RESTORE_ATTEMPTS} restore attempts"),
        ))
    }
}
