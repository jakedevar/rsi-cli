//! A one-session entry to the production move (P6) for tests (#2384).
//!
//! Slice S2 introduced this seam to move a session before the production move
//! existed; since slice S4 it delegates to [`Store::cold_move_batch`], so every
//! read, restore and invariance test also exercises the production delete. It
//! is compiled only under `cfg(any(test, feature = "test-seam"))`, which no
//! release build compiles (`scripts/check-release-seam.sh`).
//!
//! Like the pass it try-acquires the session's spawn guard (a busy guard skips
//! the session), reads the staged chunks back and checks their digests (the
//! in-pass proof of GM condition 1), then runs P6. It does not check for a
//! fresh backup: that gate belongs to the pass (`run_move_pass`).

use super::mover::{CHANGED_CODE, MoveContext, MoveVerdict, VerifiedChunks};
use super::reader::{ColdRef, read_events_chunk, read_provenance_chunk};
use super::{ColdError, ColdResult};
use crate::store::Store;
use crate::store_support::spawn_single_flight::try_acquire_spawn_guard;
use std::path::PathBuf;
use uuid::Uuid;

/// What a move moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveReceipt {
    pub events: i64,
    pub provenance: i64,
}

/// How a test-seam move ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveOutcome {
    Moved(MoveReceipt),
    /// The session's spawn guard was held (a resume, restore or unarchive in
    /// flight): the session is skipped, as `held:guard_busy`.
    GuardBusy,
    /// The live rows no longer match the staged digest: left `staged`.
    Changed,
}

impl Store {
    /// Moves one staged session's rows into cold state through the production
    /// P6 (see the module documentation). The session must be `staged` with a
    /// verified chunk.
    pub fn cold_move_session_for_test(&self, session_id: Uuid) -> ColdResult<MoveOutcome> {
        let Some(guard) = try_acquire_spawn_guard(session_id) else {
            return Ok(MoveOutcome::GuardBusy);
        };
        let sid = session_id.to_string();
        let Some(row) = self.cold_session_row(&sid)? else {
            return Err(ColdError::new(
                "cold_move_not_staged",
                format!("{sid} has no cold-archive index row"),
            ));
        };
        if row.state != "staged" || row.events_chunk_id <= 0 || row.last_error_code.is_some() {
            return Err(ColdError::new(
                "cold_move_not_staged",
                format!("{sid} is {} (chunk {})", row.state, row.events_chunk_id),
            ));
        }
        let path: String = self.conn.query_row(
            "SELECT path FROM cold_archive_files WHERE file_id = ?1",
            [row.file_id],
            |r| r.get(0),
        )?;
        // Read the chunk back (GM condition 1): a chunk that does not decode and
        // match its digest never lets a row go.
        let staged = ColdRef {
            session_id: sid.clone(),
            generation: row.generation,
            file_id: row.file_id,
            path: PathBuf::from(path),
            events_chunk_id: row.events_chunk_id,
            provenance_chunk_id: row.provenance_chunk_id,
            events_sha256: row.events_sha256.clone(),
            provenance_sha256: row.provenance_sha256.clone(),
            archived_event_count: row.archived_event_count,
            archived_provenance_count: row.archived_provenance_count,
            archived_content_len: row.archived_content_len,
            raw_bytes: row.raw_bytes,
            compressed_bytes: row.compressed_bytes,
        };
        read_events_chunk(&staged)?;
        read_provenance_chunk(&staged)?;

        let now = chrono::Utc::now();
        let verdicts = self.cold_move_batch(
            &[(VerifiedChunks::of_row(&row), &guard)],
            MoveContext { now, cutoff: now },
        )?;
        match verdicts.first() {
            Some(MoveVerdict::Moved { events, provenance }) => {
                Ok(MoveOutcome::Moved(MoveReceipt {
                    events: *events,
                    provenance: *provenance,
                }))
            }
            Some(MoveVerdict::Skipped(code)) if *code == CHANGED_CODE => Ok(MoveOutcome::Changed),
            Some(MoveVerdict::Skipped(code)) => Err(ColdError::new(
                "cold_move_not_staged",
                format!("{sid} was not moved: {code}"),
            )),
            None => Err(ColdError::new("cold_move_not_staged", "no verdict")),
        }
    }
}
