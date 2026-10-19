//! P6, the move (#2384, slice S4; plan §3.3).
//!
//! The only production code that deletes `conversation_events` and
//! `conversation_event_provenance` rows of an archived session. A session is
//! moved only when every one of these holds, checked in this order:
//!
//! 1. **Verified in this pass (GM condition 1).** The caller hands in a
//!    [`VerifiedChunks`] proof that only the pass's P5 read-back builds: the
//!    chunks were read back through a separate read-only connection, decompressed
//!    and their `SHA-256` recomputed in this same pass, and their recorded column
//!    lists compared with the live tables' (the digest covers the values only). A
//!    session an earlier pass staged is re-verified first, columns included. The
//!    proof must still equal the index row.
//! 2. **A fresh backup exists (GM condition 2)**, derived by the pass again
//!    before every transaction, not once per pass ([`super::backup`]).
//! 3. **The spawn guard is free.** The pass try-acquires each session's guard
//!    before the store mutex and never waits on it: a resume, restore or
//!    unarchive in flight skips the session (`guard_busy`).
//! 4. **Inside the transaction:** the session is still `Archived`/`Deleted`, no
//!    hold applies, every row is provably older than the cutoff, and the live
//!    movable rows hash to exactly the archived digest (else `changed`, and the
//!    session must be verified again before a later pass may move it).
//!
//! Then, in the same `IMMEDIATE` transaction: `staged` -> `deleting`, delete the
//! provenance of the movable rows (Guard 2 admits it only in `deleting`), delete
//! the movable rows, require both counts to equal the archived counts, and
//! `deleting` -> `cold`. Any error rolls the whole batch back; the pass then
//! retries the batch one session at a time to find the culprit.

use super::candidates::{AGE_UNPROVEN_CODE, HoldContext, prove_age, session_hold};
use super::index::ColdSessionRow;
use super::pass::build_session_in;
use super::reader::cache_forget;
use super::{ColdError, ColdResult, failpoint};
use crate::store::Store;
use crate::store_support::spawn_single_flight::SpawnGuard;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

/// The session's spawn guard was held (a resume, restore or unarchive in flight).
pub const GUARD_BUSY_CODE: &str = "guard_busy";
/// The live rows, or the index row, changed since the chunks were verified.
pub const CHANGED_CODE: &str = "changed";
/// The index row is not `staged` and verified any more.
pub const STATE_BUSY_CODE: &str = "state_busy";
/// The session left `Archived`/`Deleted`.
pub const STATUS_CHANGED_CODE: &str = "status_changed";
/// A delete transaction was not admitted at its boundary (the host load, the
/// wall budget or a cancel changed while it waited for the store or SQLite's
/// write lock); nothing was deleted.
pub const NOT_ADMITTED_CODE: &str = "cold_move_not_admitted";

/// What P5 proved in this pass about one session's chunks. Built only by the
/// pass right after it read the chunks back and checked their digests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedChunks {
    pub session_id: String,
    pub generation: i64,
    pub file_id: i64,
    pub events_chunk_id: i64,
    pub provenance_chunk_id: Option<i64>,
    pub events_sha256: String,
    pub provenance_sha256: Option<String>,
    pub archived_event_count: i64,
    pub archived_provenance_count: i64,
    pub raw_bytes: i64,
}

impl VerifiedChunks {
    /// The proof for an index row whose chunks were just read back equal to it.
    pub(super) fn of_row(row: &ColdSessionRow) -> Self {
        Self {
            session_id: row.session_id.clone(),
            generation: row.generation,
            file_id: row.file_id,
            events_chunk_id: row.events_chunk_id,
            provenance_chunk_id: row.provenance_chunk_id,
            events_sha256: row.events_sha256.clone(),
            provenance_sha256: row.provenance_sha256.clone(),
            archived_event_count: row.archived_event_count,
            archived_provenance_count: row.archived_provenance_count,
            raw_bytes: row.raw_bytes,
        }
    }

    fn matches(&self, row: &ColdSessionRow) -> bool {
        self.generation == row.generation
            && self.file_id == row.file_id
            && self.events_chunk_id == row.events_chunk_id
            && self.provenance_chunk_id == row.provenance_chunk_id
            && self.events_sha256 == row.events_sha256
            && self.provenance_sha256 == row.provenance_sha256
            && self.archived_event_count == row.archived_event_count
            && self.archived_provenance_count == row.archived_provenance_count
    }
}

/// How P6 ended for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveVerdict {
    /// The rows left the live tables.
    Moved { events: i64, provenance: i64 },
    /// The session stays live, for this reason (a hold code or one of the
    /// `*_CODE` constants above).
    Skipped(&'static str),
}

/// The clock of one move transaction.
#[derive(Debug, Clone, Copy)]
pub struct MoveContext {
    pub now: DateTime<Utc>,
    /// Every row of a moved session is at or before this instant.
    pub cutoff: DateTime<Utc>,
}

const MARK_DELETING_SQL: &str =
    "UPDATE cold_archive_sessions SET state = 'deleting', updated_at = ?2
     WHERE session_id = ?1 AND state = 'staged'";
const MARK_COLD_SQL: &str = "UPDATE cold_archive_sessions
     SET state = 'cold', archived_at = ?2, updated_at = ?2
     WHERE session_id = ?1 AND state = 'deleting'";
/// A session whose live rows no longer match its chunks must be verified (and
/// rebuilt) again before any later pass may move it.
const CLEAR_VERIFIED_SQL: &str =
    "UPDATE cold_archive_sessions SET verified_at = NULL, updated_at = ?2
     WHERE session_id = ?1 AND state = 'staged'";
const SESSION_STATUS_SQL: &str = "SELECT status FROM sessions WHERE id = ?1";

/// The provenance of the movable rows of `?1` (the filter of
/// `candidates::MOVABLE_EVENTS_SQL`, repeated verbatim; compared by
/// `the_movable_statements_share_one_filter`).
pub const DELETE_MOVABLE_PROVENANCE_SQL: &str = "DELETE FROM conversation_event_provenance
 WHERE conversation_event_id IN (SELECT e.id FROM conversation_events e
 WHERE e.session_id = ?1
  AND e.event_type IN ('ToolUse','ToolResult')
  AND e.id <> (SELECT id FROM conversation_events WHERE session_id = ?1
               ORDER BY sequence DESC, id DESC LIMIT 1)
  AND e.id IS NOT (SELECT id FROM conversation_events WHERE session_id = ?1
               AND (role = 'Assistant' OR event_type = 'Thinking')
               ORDER BY sequence DESC, id DESC LIMIT 1)
  AND e.id <> (SELECT id FROM conversation_events WHERE session_id = ?1
               ORDER BY created_at DESC, sequence DESC, id DESC LIMIT 1)
  AND e.id IS NOT (SELECT id FROM conversation_events WHERE session_id = ?1
               AND event_type <> 'System'
               ORDER BY created_at DESC, sequence DESC, id DESC LIMIT 1)
  AND NOT EXISTS (SELECT 1 FROM agent_message_delivery_attempts x WHERE x.acknowledged_event_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM agent_message_provider_request_effects x WHERE x.evidence_event_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM closure_output_validations x WHERE x.conversation_event_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM closure_evidence x WHERE x.review_handoff_event_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM pending_question_publications x WHERE x.conversation_event_id = e.id))";

/// The movable rows of `?1` (same filter).
pub const DELETE_MOVABLE_EVENTS_SQL: &str = "DELETE FROM conversation_events
 WHERE id IN (SELECT e.id FROM conversation_events e
 WHERE e.session_id = ?1
  AND e.event_type IN ('ToolUse','ToolResult')
  AND e.id <> (SELECT id FROM conversation_events WHERE session_id = ?1
               ORDER BY sequence DESC, id DESC LIMIT 1)
  AND e.id IS NOT (SELECT id FROM conversation_events WHERE session_id = ?1
               AND (role = 'Assistant' OR event_type = 'Thinking')
               ORDER BY sequence DESC, id DESC LIMIT 1)
  AND e.id <> (SELECT id FROM conversation_events WHERE session_id = ?1
               ORDER BY created_at DESC, sequence DESC, id DESC LIMIT 1)
  AND e.id IS NOT (SELECT id FROM conversation_events WHERE session_id = ?1
               AND event_type <> 'System'
               ORDER BY created_at DESC, sequence DESC, id DESC LIMIT 1)
  AND NOT EXISTS (SELECT 1 FROM agent_message_delivery_attempts x WHERE x.acknowledged_event_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM agent_message_provider_request_effects x WHERE x.evidence_event_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM closure_output_validations x WHERE x.conversation_event_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM closure_evidence x WHERE x.review_handoff_event_id = e.id)
  AND NOT EXISTS (SELECT 1 FROM pending_question_publications x WHERE x.conversation_event_id = e.id))";

fn now_string(now: DateTime<Utc>) -> String {
    now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
}

fn count(changed: usize) -> i64 {
    i64::try_from(changed).unwrap_or(i64::MAX)
}

impl Store {
    /// P6 for one batch, in one `IMMEDIATE` transaction. Each entry pairs the
    /// pass's [`VerifiedChunks`] proof with the session's spawn guard, which the
    /// caller already holds (try-acquired before the store mutex). Returns one
    /// verdict per entry, in order. An error rolls the whole batch back and
    /// leaves every session `staged`.
    ///
    /// # Errors
    /// A guard for another session, a count mismatch on delete, or a
    /// persistence error.
    pub fn cold_move_batch(
        &self,
        batch: &[(VerifiedChunks, &SpawnGuard)],
        context: MoveContext,
    ) -> ColdResult<Vec<MoveVerdict>> {
        self.cold_move_batch_admitted(batch, context, &|| Ok(()))
    }

    /// [`Self::cold_move_batch`] with an admission check at the transaction
    /// boundary: `admit` runs once the `IMMEDIATE` transaction holds `SQLite`'s
    /// write lock (so after any wait for it) and before the first statement that
    /// reads or deletes a row. It runs while the caller holds the store lock, so
    /// it must not block or sleep. Its error rolls the transaction back, nothing
    /// is deleted, and the error is returned.
    ///
    /// # Errors
    /// `admit`'s error, a guard for another session, a count mismatch on delete,
    /// or a persistence error.
    pub fn cold_move_batch_admitted(
        &self,
        batch: &[(VerifiedChunks, &SpawnGuard)],
        context: MoveContext,
        admit: &dyn Fn() -> ColdResult<()>,
    ) -> ColdResult<Vec<MoveVerdict>> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        admit()?;
        let mut verdicts = Vec::with_capacity(batch.len());
        for (proof, guard) in batch {
            if guard.session_id().to_string() != proof.session_id {
                return Err(ColdError::new(
                    "cold_move_guard_mismatch",
                    format!(
                        "the spawn guard held is for {}, the move is for {}",
                        guard.session_id(),
                        proof.session_id
                    ),
                ));
            }
            verdicts.push(self.cold_move_one(&tx, proof, context)?);
        }
        failpoint::hit("move_before_commit")?;
        tx.commit()?;
        for ((proof, _), verdict) in batch.iter().zip(&verdicts) {
            if matches!(verdict, MoveVerdict::Moved { .. }) {
                cache_forget(&proof.session_id);
            }
        }
        Ok(verdicts)
    }

    fn cold_move_one(
        &self,
        tx: &Connection,
        proof: &VerifiedChunks,
        context: MoveContext,
    ) -> ColdResult<MoveVerdict> {
        let sid = proof.session_id.as_str();
        failpoint::hit_session(sid)?;
        let Some(row) = self.cold_session_row(sid)? else {
            return Ok(MoveVerdict::Skipped(STATE_BUSY_CODE));
        };
        if row.state != "staged" || row.verified_at.is_none() || row.last_error_code.is_some() {
            return Ok(MoveVerdict::Skipped(STATE_BUSY_CODE));
        }
        if !proof.matches(&row) {
            return Ok(MoveVerdict::Skipped(CHANGED_CODE));
        }
        let status: Option<String> = tx
            .query_row(SESSION_STATUS_SQL, [sid], |r| r.get(0))
            .optional()?;
        if !matches!(status.as_deref(), Some("Archived" | "Deleted")) {
            return Ok(MoveVerdict::Skipped(STATUS_CHANGED_CODE));
        }
        if let Some(code) = session_hold(tx, sid, HoldContext { now: context.now })? {
            return Ok(MoveVerdict::Skipped(code));
        }
        if !prove_age(tx, sid, context.cutoff)? {
            return Ok(MoveVerdict::Skipped(AGE_UNPROVEN_CODE));
        }
        // The live rows must still be exactly what was archived.
        let built = build_session_in(tx, sid)?;
        if built.events_sha != row.events_sha256
            || built.provenance_sha != row.provenance_sha256
            || built.events.row_count != row.archived_event_count
            || built.provenance.as_ref().map_or(0, |p| p.row_count) != row.archived_provenance_count
        {
            tx.execute(CLEAR_VERIFIED_SQL, params![sid, now_string(context.now)])?;
            return Ok(MoveVerdict::Skipped(CHANGED_CODE));
        }

        let now = now_string(context.now);
        if tx.execute(MARK_DELETING_SQL, params![sid, now])? != 1 {
            return Ok(MoveVerdict::Skipped(STATE_BUSY_CODE));
        }
        let provenance = count(tx.execute(DELETE_MOVABLE_PROVENANCE_SQL, [sid])?);
        let events = count(tx.execute(DELETE_MOVABLE_EVENTS_SQL, [sid])?);
        if events != row.archived_event_count || provenance != row.archived_provenance_count {
            return Err(ColdError::new(
                "cold_move_count_mismatch",
                format!(
                    "{sid}: deleted {events} events and {provenance} provenance rows, \
                     archived {} and {}",
                    row.archived_event_count, row.archived_provenance_count
                ),
            ));
        }
        if tx.execute(MARK_COLD_SQL, params![sid, now])? != 1 {
            return Err(ColdError::new(
                "cold_move_not_staged",
                format!("{sid} left the deleting state"),
            ));
        }
        Ok(MoveVerdict::Moved { events, provenance })
    }
}
