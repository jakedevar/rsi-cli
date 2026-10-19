//! Transparent cold reads (#2384, slice S2; plan §4.1, §4.2).
//!
//! A cold session keeps a live tail in `conversation_events` and its bulky rows in
//! an archive chunk. [`Store::merge_cold_events`] is the one place that puts them
//! back together: every transcript reader (`load_events_since`,
//! `read_session_events_page`, the program-gate window) goes through it, so no
//! reader re-implements the merge. A hot session costs one primary-key lookup on
//! `cold_archive_sessions`.
//!
//! A chunk that cannot be read is a typed error ([`unavailable_error`]), never a
//! shorter transcript: the tail alone is not the transcript.

use super::candidates::{EVENT_COLUMNS, PROVENANCE_COLUMNS};
use super::format::{decode_rows, decompress_checked, parse_columns_json};
use super::writer::{EVENTS_TABLE_NAME, PROVENANCE_TABLE_NAME, open_archive_readonly, read_chunk};
use super::{ColdError, ColdResult};
use crate::error::{DaemonError, Result};
use crate::store::Store;
use rsi_common::types::{ConversationEvent, EventType};
use rusqlite::OptionalExtension;
use rusqlite::types::Value;
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// The typed code of a cold session whose archive cannot be read.
pub const UNAVAILABLE_CODE: &str = "cold_archive_unavailable";

/// Bytes of decoded cold transcripts kept in memory (plan §4.2).
const EVENT_CACHE_BUDGET: usize = 64 * 1024 * 1024;

/// `cold_archive_unavailable{file, code}`: the typed refusal of a cold read whose
/// archive file or chunk is missing, unreadable or fails its checksum. It names
/// the file and the stable reason; the transcript is never served short.
#[must_use]
pub fn unavailable_error(file: &str, error: &ColdError) -> DaemonError {
    tracing::warn!(
        file,
        reason = error.code,
        detail = %error.detail,
        "cold archive unavailable on read"
    );
    // A reader may sit inside a caller's transaction: the notice is raised later
    // by the cold-archive loop (plan §6.4).
    super::notices::queue_notice(
        super::notices::UNAVAILABLE_NOTICE,
        &format!("{file}: {}: {}", error.code, error.detail),
    );
    DaemonError::StructuredRpc {
        rpc_code: -32603,
        message: format!("{UNAVAILABLE_CODE}: {} ({file})", error.code),
        data: serde_json::json!({
            "code": UNAVAILABLE_CODE,
            "file": file,
            "reason": error.code,
            "detail": error.detail,
        }),
    }
}

/// True when `error` is the typed [`unavailable_error`].
#[must_use]
pub fn is_unavailable_error(error: &DaemonError) -> bool {
    matches!(
        error,
        DaemonError::StructuredRpc { data, .. }
            if data.get("code").and_then(serde_json::Value::as_str) == Some(UNAVAILABLE_CODE)
    )
}

/// Everything needed to find and check a cold session's chunks: the index row's
/// identity fields and the archive file's path. A restore fences on all of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColdRef {
    pub session_id: String,
    pub generation: i64,
    pub file_id: i64,
    pub path: PathBuf,
    pub events_chunk_id: i64,
    pub provenance_chunk_id: Option<i64>,
    pub events_sha256: String,
    pub provenance_sha256: Option<String>,
    pub archived_event_count: i64,
    pub archived_provenance_count: i64,
    pub archived_content_len: i64,
    pub raw_bytes: i64,
    pub compressed_bytes: i64,
}

/// Where a session's rows are (`GetColdSessionStorage`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ColdSessionStorage {
    /// `live` (never moved, or only staged), `cold` (rows moved to the archive)
    /// or `restored` (moved once and restored).
    pub tier: &'static str,
    pub generation: Option<i64>,
    pub archived_at: Option<String>,
    pub restored_at: Option<String>,
    pub file: Option<String>,
    pub archived_events: i64,
    pub raw_bytes: i64,
    pub compressed_bytes: i64,
}

impl Store {
    /// The cold reference of a session whose rows are in the archive
    /// (`state = 'cold'`); `None` for a hot session.
    pub fn cold_ref(&self, session_id: &str) -> ColdResult<Option<ColdRef>> {
        Ok(self
            .conn
            .query_row(
                "SELECT s.session_id, s.generation, s.file_id, f.path, s.events_chunk_id,
                        s.provenance_chunk_id, s.events_sha256, s.provenance_sha256,
                        s.archived_event_count, s.archived_provenance_count,
                        s.archived_content_len, s.raw_bytes, s.compressed_bytes
                 FROM cold_archive_sessions s
                 JOIN cold_archive_files f ON f.file_id = s.file_id
                 WHERE s.session_id = ?1 AND s.state = 'cold'",
                [session_id],
                |row| {
                    Ok(ColdRef {
                        session_id: row.get(0)?,
                        generation: row.get(1)?,
                        file_id: row.get(2)?,
                        path: PathBuf::from(row.get::<_, String>(3)?),
                        events_chunk_id: row.get(4)?,
                        provenance_chunk_id: row.get(5)?,
                        events_sha256: row.get(6)?,
                        provenance_sha256: row.get(7)?,
                        archived_event_count: row.get(8)?,
                        archived_provenance_count: row.get(9)?,
                        archived_content_len: row.get(10)?,
                        raw_bytes: row.get(11)?,
                        compressed_bytes: row.get(12)?,
                    })
                },
            )
            .optional()?)
    }

    /// Where `session_id`'s rows live, for the operator and the TUI badge.
    pub fn cold_session_storage(&self, session_id: &str) -> ColdResult<ColdSessionStorage> {
        let Some(row) = self.cold_session_row(session_id)? else {
            return Ok(ColdSessionStorage::live());
        };
        let tier = match row.state.as_str() {
            "cold" | "deleting" => "cold",
            "restored" | "restoring" => "restored",
            _ => "live",
        };
        if tier == "live" {
            return Ok(ColdSessionStorage::live());
        }
        let file: Option<String> = self
            .conn
            .query_row(
                "SELECT path FROM cold_archive_files WHERE file_id = ?1",
                [row.file_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(ColdSessionStorage {
            tier,
            generation: Some(row.generation),
            archived_at: row.archived_at,
            restored_at: row.restored_at,
            file,
            archived_events: row.archived_event_count,
            raw_bytes: row.raw_bytes,
            compressed_bytes: row.compressed_bytes,
        })
    }

    /// Puts a cold session's archived rows back into `live` (its tail), in
    /// `(sequence, id)` order, keeping only rows after `since_sequence`. A hot
    /// session returns `live` untouched. An archive that cannot be read is
    /// [`unavailable_error`], never the tail alone.
    ///
    /// The one merge point of every transcript reader (plan §4.1, class
    /// *merge*).
    pub(crate) fn merge_cold_events(
        &self,
        session_id: Uuid,
        since_sequence: Option<i32>,
        mut live: Vec<ConversationEvent>,
    ) -> Result<Vec<ConversationEvent>> {
        let Some(cold) = self.cold_ref(&session_id.to_string())? else {
            return Ok(live);
        };
        let archived = cold_decoded_events(&cold)?;
        // A restore can commit between the caller's live read and this lookup;
        // an id present in both is the same row.
        let live_ids: HashSet<i64> = live.iter().map(|event| event.id).collect();
        live.extend(
            archived
                .iter()
                .filter(|event| {
                    since_sequence.is_none_or(|since| event.sequence > since)
                        && !live_ids.contains(&event.id)
                })
                .cloned(),
        );
        live.sort_by_key(|event| (event.sequence, event.id));
        Ok(live)
    }

    /// One `AgentReadSessionEvents` page of a cold session, equal to what the
    /// same query returned before the move. `None` for a hot session (the caller
    /// runs its SQL page).
    pub(crate) fn cold_events_page(
        &self,
        session_id: Uuid,
        after_sequence: Option<i32>,
        limit: u32,
        event_types: Option<&[EventType]>,
    ) -> Result<Option<(Vec<ConversationEvent>, bool, bool)>> {
        if self.cold_ref(&session_id.to_string())?.is_none() {
            return Ok(None);
        }
        let all = self.load_events_since(session_id, None)?;
        Ok(Some(page_of_merged(
            all,
            after_sequence,
            limit,
            event_types,
        )))
    }

    /// The assistant-role window the program gate reads after `last_user`
    /// (sequence order, at most `limit` rows, each content clipped to
    /// `max_chars` characters and paired with its full byte length), for a cold
    /// session; `None` for a hot one. The same rows the gate's SQL returns for
    /// the unmoved transcript, including `ToolUse` rows (role `Assistant`).
    pub(crate) fn cold_assistant_window(
        &self,
        session_id: Uuid,
        last_user: i64,
        limit: usize,
        max_chars: usize,
    ) -> Result<Option<Vec<(String, usize)>>> {
        if self.cold_ref(&session_id.to_string())?.is_none() {
            return Ok(None);
        }
        let all = self.load_events_since(session_id, None)?;
        Ok(Some(
            all.into_iter()
                .filter(|event| {
                    event.role == Some(rsi_common::types::Role::Assistant)
                        && i64::from(event.sequence) > last_user
                })
                .take(limit)
                .map(|event| {
                    let bytes = event.content.len();
                    let clipped: String = event.content.chars().take(max_chars).collect();
                    (clipped, bytes)
                })
                .collect(),
        ))
    }
}

impl ColdSessionStorage {
    const fn live() -> Self {
        Self {
            tier: "live",
            generation: None,
            archived_at: None,
            restored_at: None,
            file: None,
            archived_events: 0,
            raw_bytes: 0,
            compressed_bytes: 0,
        }
    }
}

/// The `read_session_events_page` result over a merged, ascending transcript:
/// the same paging and probe-row rules as its SQL (forward page after
/// `after_sequence`, else the newest `limit` matching events).
fn page_of_merged(
    all: Vec<ConversationEvent>,
    after_sequence: Option<i32>,
    limit: u32,
    event_types: Option<&[EventType]>,
) -> (Vec<ConversationEvent>, bool, bool) {
    let limit = limit as usize;
    let wanted = |event: &ConversationEvent| match event_types {
        Some(types) if !types.is_empty() => types.contains(&event.event_type),
        _ => true,
    };
    let matching: Vec<ConversationEvent> = all.into_iter().filter(wanted).collect();
    if let Some(after) = after_sequence {
        let has_earlier = matching.iter().any(|event| event.sequence <= after);
        let mut rows: Vec<ConversationEvent> = matching
            .into_iter()
            .filter(|event| event.sequence > after)
            .take(limit + 1)
            .collect();
        let has_more = rows.len() > limit;
        if has_more {
            rows.pop();
        }
        (rows, has_more, has_earlier)
    } else {
        let skip = matching.len().saturating_sub(limit + 1);
        let mut rows: Vec<ConversationEvent> = matching.into_iter().skip(skip).collect();
        let has_earlier = rows.len() > limit;
        if has_earlier {
            rows.remove(0);
        }
        (rows, false, has_earlier)
    }
}

// ---------------------------------------------------------------------------
// Chunk decoding (no Store: callable outside the store mutex)
// ---------------------------------------------------------------------------

/// What a chunk read must find.
pub(super) struct ChunkSpec<'a> {
    pub chunk_id: i64,
    pub session_id: &'a str,
    pub generation: i64,
    pub table: &'static str,
    pub sha256: &'a str,
    pub row_count: i64,
    /// The table's columns the reader understands, in canonical order.
    pub canonical: &'static [&'static str],
}

/// Reads, checks and decodes one chunk into rows of the canonical column order.
/// Every failure is a [`ColdError`] with a stable code: a missing file or chunk,
/// an identity, checksum, length or count mismatch, or a column set this build
/// does not understand (`cold_archive_schema_drift`).
pub(super) fn read_decoded_chunk(path: &Path, spec: &ChunkSpec<'_>) -> ColdResult<Vec<Vec<Value>>> {
    if !path.is_file() {
        return Err(ColdError::new(
            "cold_archive_file_missing",
            format!("{} is not a file", path.display()),
        ));
    }
    let conn = open_archive_readonly(path)?;
    let record = read_chunk(&conn, spec.chunk_id)?.ok_or_else(|| {
        ColdError::new(
            "cold_archive_chunk_missing",
            format!("chunk {} is not in {}", spec.chunk_id, path.display()),
        )
    })?;
    if record.session_id != spec.session_id
        || record.generation != spec.generation
        || record.table_name != spec.table
    {
        return Err(ColdError::new(
            "cold_archive_verify_identity",
            format!(
                "chunk {} is {}/{}/{}, the index expects {}/{}/{}",
                spec.chunk_id,
                record.session_id,
                record.generation,
                record.table_name,
                spec.session_id,
                spec.generation,
                spec.table
            ),
        ));
    }
    if record.codec != "zstd" {
        return Err(ColdError::new(
            "cold_archive_verify_codec",
            format!("chunk {} uses codec {}", spec.chunk_id, record.codec),
        ));
    }
    if record.raw_sha256 != spec.sha256 {
        return Err(ColdError::new(
            "cold_archive_verify_sha_mismatch",
            format!(
                "chunk {} records sha256 {}, the index expects {}",
                spec.chunk_id, record.raw_sha256, spec.sha256
            ),
        ));
    }
    let raw = decompress_checked(&record.payload, record.raw_bytes, &record.raw_sha256)?;
    let columns = parse_columns_json(&record.columns_json)?;
    let names: Vec<&str> = columns.iter().map(|(name, _)| name.as_str()).collect();
    let positions: Vec<usize> = spec
        .canonical
        .iter()
        .map(|wanted| names.iter().position(|name| name == wanted))
        .collect::<Option<Vec<usize>>>()
        .filter(|positions| positions.len() == names.len())
        .ok_or_else(|| {
            ColdError::new(
                "cold_archive_schema_drift",
                format!(
                    "chunk {} has columns {names:?}; this build restores exactly {:?}",
                    spec.chunk_id, spec.canonical
                ),
            )
        })?;
    let rows = decode_rows(&raw, columns.len())?;
    let count = i64::try_from(rows.len()).unwrap_or(i64::MAX);
    if count != record.row_count || count != spec.row_count {
        return Err(ColdError::new(
            "cold_archive_verify_count",
            format!(
                "chunk {} decodes to {count} rows, the manifest records {} and the index {}",
                spec.chunk_id, record.row_count, spec.row_count
            ),
        ));
    }
    Ok(rows
        .into_iter()
        .map(|mut row| {
            positions
                .iter()
                .map(|position| std::mem::replace(&mut row[*position], Value::Null))
                .collect()
        })
        .collect())
}

/// The decoded events chunk of a cold session (canonical `EVENT_COLUMNS` order).
pub(super) fn read_events_chunk(cold: &ColdRef) -> ColdResult<Vec<Vec<Value>>> {
    read_decoded_chunk(
        &cold.path,
        &ChunkSpec {
            chunk_id: cold.events_chunk_id,
            session_id: &cold.session_id,
            generation: cold.generation,
            table: EVENTS_TABLE_NAME,
            sha256: &cold.events_sha256,
            row_count: cold.archived_event_count,
            canonical: &EVENT_COLUMNS,
        },
    )
}

/// The decoded provenance chunk of a cold session, if it has one.
pub(super) fn read_provenance_chunk(cold: &ColdRef) -> ColdResult<Vec<Vec<Value>>> {
    match (&cold.provenance_chunk_id, &cold.provenance_sha256) {
        (Some(chunk_id), Some(sha256)) => read_decoded_chunk(
            &cold.path,
            &ChunkSpec {
                chunk_id: *chunk_id,
                session_id: &cold.session_id,
                generation: cold.generation,
                table: PROVENANCE_TABLE_NAME,
                sha256,
                row_count: cold.archived_provenance_count,
                canonical: &PROVENANCE_COLUMNS,
            },
        ),
        (None, None) if cold.archived_provenance_count == 0 => Ok(Vec::new()),
        _ => Err(ColdError::new(
            "cold_archive_verify_identity",
            "the index row's provenance chunk, digest and count disagree",
        )),
    }
}

// ---------------------------------------------------------------------------
// Decoded-transcript cache (content-addressed by the chunk's digest)
// ---------------------------------------------------------------------------

struct CachedTranscript {
    session_id: String,
    sha256: String,
    bytes: usize,
    events: Arc<Vec<ConversationEvent>>,
}

struct EventCache {
    entries: std::collections::VecDeque<CachedTranscript>,
    bytes: usize,
}

static EVENT_CACHE: Mutex<EventCache> = Mutex::new(EventCache {
    entries: std::collections::VecDeque::new(),
    bytes: 0,
});

fn cache_lookup(session_id: &str, sha256: &str) -> Option<Arc<Vec<ConversationEvent>>> {
    let mut cache = EVENT_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let at = cache
        .entries
        .iter()
        .position(|entry| entry.session_id == session_id && entry.sha256 == sha256)?;
    let entry = cache.entries.remove(at)?;
    let events = Arc::clone(&entry.events);
    cache.entries.push_back(entry);
    Some(events)
}

fn cache_insert(
    session_id: &str,
    sha256: &str,
    bytes: usize,
    events: &Arc<Vec<ConversationEvent>>,
) {
    if bytes > EVENT_CACHE_BUDGET {
        return;
    }
    let mut cache = EVENT_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache
        .entries
        .retain(|entry| !(entry.session_id == session_id && entry.sha256 == sha256));
    cache.bytes = cache.entries.iter().map(|entry| entry.bytes).sum();
    while cache.bytes + bytes > EVENT_CACHE_BUDGET {
        let Some(old) = cache.entries.pop_front() else {
            break;
        };
        cache.bytes = cache.bytes.saturating_sub(old.bytes);
    }
    cache.bytes += bytes;
    cache.entries.push_back(CachedTranscript {
        session_id: session_id.to_string(),
        sha256: sha256.to_string(),
        bytes,
        events: Arc::clone(events),
    });
}

/// Drops every cached transcript of `session_id` (after a restore its rows are
/// live again).
pub(super) fn cache_forget(session_id: &str) {
    let mut cache = EVENT_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    cache.entries.retain(|entry| entry.session_id != session_id);
    cache.bytes = cache.entries.iter().map(|entry| entry.bytes).sum();
}

/// The archived events of a cold session as `ConversationEvent`s, verified
/// against the index row's digest on every decompression. Cached by digest.
fn cold_decoded_events(cold: &ColdRef) -> Result<Arc<Vec<ConversationEvent>>> {
    if let Some(events) = cache_lookup(&cold.session_id, &cold.events_sha256) {
        return Ok(events);
    }
    let file = cold.path.to_string_lossy().into_owned();
    let rows = read_events_chunk(cold).map_err(|error| unavailable_error(&file, &error))?;
    let events = rows
        .iter()
        .map(|row| Store::event_from_cold_values(row))
        .collect::<Result<Vec<_>>>()
        .map_err(|error| {
            unavailable_error(
                &file,
                &ColdError::new("cold_archive_verify_decode", error.to_string()),
            )
        })?;
    let events = Arc::new(events);
    cache_insert(
        &cold.session_id,
        &cold.events_sha256,
        usize::try_from(cold.raw_bytes).unwrap_or(usize::MAX),
        &events,
    );
    Ok(events)
}
