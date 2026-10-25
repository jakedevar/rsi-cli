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
#[cfg(test)]
pub(crate) const PROVISIONAL_VERSION: i32 = 142;

/// #86: see [`Store::paste_image_reference_texts`].
const PASTE_IMAGE_REFERENCE_TEXTS_SQL: &str = "SELECT m.content FROM operator_messages m \
     JOIN sessions s ON s.id=m.session_id \
     WHERE instr(m.content,'paste')>0 \
       AND (m.state IN ('queued','dispatching','effect_possible','uncertain') \
            OR (m.state='delivered' AND s.status IN ('Starting','Running','WaitingApproval'))) \
     UNION ALL \
     SELECT message FROM scheduled_jobs WHERE enabled=1 AND instr(message,'paste')>0 \
     LIMIT ?1";

/// #86: one chunk of the "ever referenced" scan over every operator message
/// (any state, any session; the edited and the original text).
/// See [`Store::paste_reference_scan_chunk`].
const PASTE_REFERENCE_MESSAGES_SQL: &str = "SELECT rowid, \
     CASE WHEN instr(content || char(10) || original_content, '.png')>0 \
            OR instr(content || char(10) || original_content, '.jpg')>0 \
            OR instr(content || char(10) || original_content, '.gif')>0 \
            OR instr(content || char(10) || original_content, '.webp')>0 \
          THEN content || char(10) || original_content END \
     FROM operator_messages WHERE rowid>?1 ORDER BY rowid LIMIT ?2";

/// #86: the same over the live rows of `conversation_events`, every event
/// type (`Message`, `ToolUse`, `ToolResult`, `System`, ...): the text a row
/// carries is its content, its tool input and its metadata. The rows of a
/// session cold storage moved out of this table are read from the archive
/// ([`PasteReferenceSource::ArchivedConversationEvents`]).
const PASTE_REFERENCE_EVENTS_SQL: &str = "SELECT id, \
     CASE WHEN instr(t,'.png')>0 OR instr(t,'.jpg')>0 OR instr(t,'.gif')>0 \
               OR instr(t,'.webp')>0 \
          THEN t END \
     FROM (SELECT id, content || char(10) || COALESCE(tool_input,'') || char(10) \
                      || COALESCE(metadata,'') AS t \
           FROM conversation_events WHERE id>?1 ORDER BY id LIMIT ?2) \
     ORDER BY id";

/// #86: the same over enabled scheduled jobs.
const PASTE_REFERENCE_SCHEDULED_SQL: &str = "SELECT rowid, \
     CASE WHEN enabled=1 \
           AND (instr(message,'.png')>0 OR instr(message,'.jpg')>0 \
                OR instr(message,'.gif')>0 OR instr(message,'.webp')>0) \
          THEN message END \
     FROM scheduled_jobs WHERE rowid>?1 ORDER BY rowid LIMIT ?2";

/// #86 (hub #3322 finding 2): the operator messages written or edited since
/// the marks of a scan ([`Store::paste_reference_late_texts`]): a row after the
/// rowid mark, or one whose `updated_at` (canonical RFC3339 nanos, so text
/// order is time order) is at or after the newest one the scan saw.
const PASTE_REFERENCE_LATE_MESSAGES_SQL: &str = "SELECT rowid, \
     CASE WHEN instr(content || char(10) || original_content, '.png')>0 \
            OR instr(content || char(10) || original_content, '.jpg')>0 \
            OR instr(content || char(10) || original_content, '.gif')>0 \
            OR instr(content || char(10) || original_content, '.webp')>0 \
          THEN content || char(10) || original_content END \
     FROM operator_messages WHERE rowid>?1 AND (rowid>?3 OR updated_at>=?4) \
     ORDER BY rowid LIMIT ?2";

/// #86 (hub #3322 finding 2): every enabled scheduled job, whatever its
/// `updated_at` (the column's text format predates the canonical check, so the
/// late check re-reads them all; the enabled set is small).
const PASTE_REFERENCE_LATE_SCHEDULED_SQL: &str = "SELECT rowid, \
     CASE WHEN instr(message,'.png')>0 OR instr(message,'.jpg')>0 \
               OR instr(message,'.gif')>0 OR instr(message,'.webp')>0 \
          THEN message END \
     FROM scheduled_jobs WHERE enabled=1 AND rowid>?1 ORDER BY rowid LIMIT ?2";

/// #178: the unfinished part of one chunk of the paste reference scan: an
/// archived session too large to read under the store lock. Run it with the
/// store released ([`PasteReferenceStream::run`]); the caller's cold
/// fingerprint fence rejects a scan a move or restore raced meanwhile.
#[derive(Debug)]
pub struct PasteReferenceStream(super::cold_archive::ColdArchiveStream);

impl PasteReferenceStream {
    /// Streams the session's archived rows, passing `visit` each text that may
    /// name an image. An error means the stream is unverified: discard what
    /// `visit` collected and remove nothing.
    pub fn run(self, visit: &mut dyn FnMut(&str)) -> Result<()> {
        self.0.run(&mut |event| visit_archived_event(event, visit))
    }
}

/// One chunk of the scan: the cursor of the next chunk (`None` once the source
/// is done) and, for an oversized archived session, the stream still to run.
#[derive(Debug)]
pub struct PasteReferenceStep {
    pub next: Option<i64>,
    pub stream: Option<PasteReferenceStream>,
}

fn visit_archived_event(event: &ConversationEvent, visit: &mut dyn FnMut(&str)) {
    let text = paste_reference_event_text(event);
    if rsi_common::input_assets::PASTE_IMAGE_MIME_ALLOWLIST
        .iter()
        .any(|(_, ext)| text.contains(&format!(".{ext}")))
    {
        visit(&text);
    }
}

/// #86 (hub #3322 finding 2): where every source of the "ever referenced"
/// scan stood when the scan started ([`Store::paste_reference_marks`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasteReferenceMarks {
    operator_message_rowid: i64,
    operator_message_updated_at: String,
    event_id: i64,
    cold: String,
}

/// Rows the late check reads per statement; it holds the store throughout.
const LATE_CHUNK_ROWS: i64 = 1_000;

/// #86: the tables the "ever referenced" paste image scan reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteReferenceSource {
    /// Every operator message, sent or not, of any session.
    OperatorMessages,
    /// Every live conversation event of any session, of every type.
    ConversationEvents,
    /// The archived rows of every session whose rows are in cold storage, or
    /// being moved to or restored from it.
    ArchivedConversationEvents,
    /// Every enabled scheduled job's message.
    EnabledScheduledJobs,
}

impl PasteReferenceSource {
    pub const ALL: [Self; 4] = [
        Self::OperatorMessages,
        Self::ConversationEvents,
        Self::ArchivedConversationEvents,
        Self::EnabledScheduledJobs,
    ];
}

/// #86: the text an archived conversation event carries, as
/// `PASTE_REFERENCE_EVENTS_SQL` reads a live row: content, tool input and
/// metadata, one per line.
fn paste_reference_event_text(event: &ConversationEvent) -> String {
    let json = |value: &Option<Box<serde_json::Value>>| {
        value
            .as_deref()
            .map(serde_json::Value::to_string)
            .unwrap_or_default()
    };
    format!(
        "{}\n{}\n{}",
        event.content,
        json(&event.tool_input),
        json(&event.metadata)
    )
}

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

/// The refusal for a prompt addressed to a rotated predecessor: it names the
/// live successor the prompt belongs to (#3978 defect a).
pub fn rotation_successor_refusal(predecessor: Uuid, successor: Uuid) -> DaemonError {
    DaemonError::InvalidParam(format!(
        "session_rotation_successor_current: session {predecessor} was rotated; send the prompt to its live successor {successor}"
    ))
}

impl Store {
    /// The live rotation tip of `session_id` when that session is an Archived
    /// rotated predecessor (its published lineage leads elsewhere); `None` for
    /// every other session. An Archived predecessor never runs another turn, so
    /// an operator prompt addressed to it belongs to the tip (#3978).
    pub fn archived_rotation_tip(&self, session_id: Uuid) -> Result<Option<Uuid>> {
        let archived = self
            .get_session(session_id)?
            .is_some_and(|session| session.status == rsi_common::types::SessionStatus::Archived);
        if !archived {
            return Ok(None);
        }
        Ok(self
            .published_lineage_tip(session_id)?
            .filter(|tip| *tip != session_id))
    }

    /// Refuse (naming the live successor) a prompt addressed to an Archived
    /// rotated predecessor; `Ok` for every other session (#3978).
    pub fn refuse_rotated_predecessor_prompt(&self, session_id: Uuid) -> Result<()> {
        match self.archived_rotation_tip(session_id)? {
            Some(tip) => Err(rotation_successor_refusal(session_id, tip)),
            None => Ok(()),
        }
    }

    /// Re-address every still-queued operator message of an Archived rotated
    /// predecessor to its live rotation tip. A message queued while the session
    /// ran, then stranded when the rotation archived it, is never dispatched
    /// (the dispatcher serves only terminal sessions), so without this the
    /// operator prompt is silently dropped (#3978). Returns the rows moved.
    pub fn redirect_rotated_operator_messages(&mut self) -> Result<usize> {
        let stranded: Vec<String> = self
            .conn
            .prepare(
                "SELECT DISTINCT m.session_id FROM operator_messages m JOIN sessions s ON s.id=m.session_id
                 WHERE m.state='queued' AND s.status='Archived'",
            )?
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut moved = 0;
        for raw in stranded {
            let predecessor =
                Uuid::parse_str(&raw).map_err(|error| DaemonError::Store(error.to_string()))?;
            let Some(tip) = self.archived_rotation_tip(predecessor)? else {
                continue;
            };
            moved += self.conn.execute(
                "UPDATE OR IGNORE operator_messages SET session_id=?1,updated_at=?2
                 WHERE session_id=?3 AND state='queued'",
                params![tip.to_string(), now(), predecessor.to_string()],
            )?;
        }
        Ok(moved)
    }

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
        if status == "Archived"
            && let Some(tip) = self.archived_rotation_tip(session_id)?
        {
            return Err(rotation_successor_refusal(session_id, tip));
        }
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

    /// #86: the texts that may still make a provider read a pasted image:
    /// operator messages not yet surely delivered (`queued`, `dispatching`,
    /// `effect_possible`, `uncertain`), delivered ones of a session that is
    /// starting, running or waiting on an approval, and enabled scheduled-job
    /// messages, each only when it mentions `paste`. At most `limit` texts;
    /// the flag is true when more exist (the caller must then keep every
    /// image). Read only.
    pub fn paste_image_reference_texts(&self, limit: usize) -> Result<(Vec<String>, bool)> {
        let mut statement = self.conn.prepare(PASTE_IMAGE_REFERENCE_TEXTS_SQL)?;
        let bound = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
        let mut texts = statement
            .query_map([bound], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let truncated = texts.len() > limit;
        texts.truncate(limit);
        Ok((texts, truncated))
    }

    /// #86: one chunk of the scan for pasted image names any message,
    /// conversation or enabled scheduled job ever held: the next `rows` rows of
    /// `source` after the cursor `after` (a rowid; start at 0), in rowid
    /// order. `visit` gets each text that may name an image (it holds an image
    /// extension); other rows are walked but not copied, so a chunk always
    /// covers exactly `rows` rows and a caller can release the store between
    /// chunks. A chunk of [`PasteReferenceSource::ArchivedConversationEvents`]
    /// is every archived row of one session instead. Returns the cursor of the
    /// next chunk, or `None` once the source is done. An archive larger than
    /// `PASTE_REFERENCE_ARCHIVE_MAX_RAW_BYTES` is streamed with bounded memory
    /// (#178; a caller that must not hold the store for it uses
    /// [`Store::paste_reference_scan_step`]); an archive that cannot be read, one whose record disagrees with
    /// its index row about the bound (hub #3322, checked before it is
    /// decompressed), or a single archived row over the bound is an error
    /// naming the session or file, never a shorter scan. Read only.
    ///
    /// Cold storage can move or restore a session while a caller walks the
    /// sources; a caller compares [`Store::paste_reference_cold_fingerprint`]
    /// before and after the walk and trusts it only when they are equal.
    pub fn paste_reference_scan_chunk(
        &self,
        source: PasteReferenceSource,
        after: i64,
        rows: usize,
        visit: &mut dyn FnMut(&str),
    ) -> Result<Option<i64>> {
        let step = self.paste_reference_scan_step(source, after, rows, visit)?;
        if let Some(stream) = step.stream {
            stream.run(visit)?;
        }
        Ok(step.next)
    }

    /// [`Store::paste_reference_scan_chunk`] without the unbounded part (#178):
    /// an archived session over the bound is returned as a
    /// [`PasteReferenceStream`] for the caller to run after it released the
    /// store, so a large or slow archive never holds the store lock. Every
    /// other chunk is read here, bounded by `rows` or the bound.
    pub fn paste_reference_scan_step(
        &self,
        source: PasteReferenceSource,
        after: i64,
        rows: usize,
        visit: &mut dyn FnMut(&str),
    ) -> Result<PasteReferenceStep> {
        let rows = rows.max(1);
        let limit = i64::try_from(rows).unwrap_or(i64::MAX);
        let mut statement = match source {
            PasteReferenceSource::OperatorMessages => {
                self.conn.prepare(PASTE_REFERENCE_MESSAGES_SQL)?
            }
            PasteReferenceSource::ConversationEvents => {
                self.conn.prepare(PASTE_REFERENCE_EVENTS_SQL)?
            }
            PasteReferenceSource::ArchivedConversationEvents => {
                let bound = super::cold_archive::paste_reference_archive_bound();
                let session = self.cold_reference_scan_session(after, bound, &mut |event| {
                    visit_archived_event(event, visit);
                })?;
                return Ok(match session {
                    Some((cursor, stream)) => PasteReferenceStep {
                        next: Some(cursor),
                        stream: stream.map(PasteReferenceStream),
                    },
                    None => PasteReferenceStep {
                        next: None,
                        stream: None,
                    },
                });
            }
            PasteReferenceSource::EnabledScheduledJobs => {
                self.conn.prepare(PASTE_REFERENCE_SCHEDULED_SQL)?
            }
        };
        let mut query = statement.query(params![after, limit])?;
        let mut last = after;
        let mut seen = 0usize;
        while let Some(row) = query.next()? {
            last = row.get::<_, i64>(0)?;
            seen += 1;
            if let Some(text) = row.get::<_, Option<String>>(1)? {
                visit(&text);
            }
        }
        Ok(PasteReferenceStep {
            next: (seen == rows).then_some(last),
            stream: None,
        })
    }

    /// #86 (hub #3322 finding 2): the high-water mark of every source of the
    /// "ever referenced" scan, taken before the scan reads anything: the newest
    /// operator message rowid and `updated_at`, the newest conversation event
    /// id and the cold-storage fingerprint. Read only.
    pub fn paste_reference_marks(&self) -> Result<PasteReferenceMarks> {
        let (operator_message_rowid, operator_message_updated_at) = self.conn.query_row(
            "SELECT COALESCE(MAX(rowid), 0), COALESCE(MAX(updated_at), '') \
             FROM operator_messages",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )?;
        let event_id = self.conn.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM conversation_events",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(PasteReferenceMarks {
            operator_message_rowid,
            operator_message_updated_at,
            event_id,
            cold: self.paste_reference_cold_fingerprint()?,
        })
    }

    /// #86 (hub #3322 finding 2): the texts written after `marks` that may
    /// name a pasted image, each passed to `visit`: operator messages added or
    /// edited since, conversation events added since (any type) and every
    /// enabled scheduled job. A caller holds the store from this call until it
    /// has removed the images, so no reference can be committed in between.
    /// An error, never a shorter read, when cold storage moved or restored a
    /// session since the marks, more than `max_rows` rows are late, or
    /// `deadline` passes. Read only.
    pub fn paste_reference_late_texts(
        &self,
        marks: &PasteReferenceMarks,
        max_rows: usize,
        deadline: std::time::Instant,
        visit: &mut dyn FnMut(&str),
    ) -> Result<()> {
        if self.paste_reference_cold_fingerprint()? != marks.cold {
            return Err(DaemonError::Store(
                "cold storage moved or restored a session since the reference scan".into(),
            ));
        }
        let mut rows = 0usize;
        for (sql, start) in [
            (PASTE_REFERENCE_LATE_MESSAGES_SQL, 0),
            (PASTE_REFERENCE_EVENTS_SQL, marks.event_id),
            (PASTE_REFERENCE_LATE_SCHEDULED_SQL, 0),
        ] {
            let mut statement = self.conn.prepare(sql)?;
            let mut after = start;
            loop {
                if std::time::Instant::now() >= deadline {
                    return Err(DaemonError::Store(
                        "the late reference check outlasted its budget".into(),
                    ));
                }
                let mut query = if sql == PASTE_REFERENCE_LATE_MESSAGES_SQL {
                    statement.query(params![
                        after,
                        LATE_CHUNK_ROWS,
                        marks.operator_message_rowid,
                        marks.operator_message_updated_at
                    ])?
                } else {
                    statement.query(params![after, LATE_CHUNK_ROWS])?
                };
                let mut seen = 0i64;
                while let Some(row) = query.next()? {
                    after = row.get::<_, i64>(0)?;
                    seen += 1;
                    rows += 1;
                    if rows > max_rows {
                        return Err(DaemonError::Store(format!(
                            "more than {max_rows} rows were written during the reference scan"
                        )));
                    }
                    if let Some(text) = row.get::<_, Option<String>>(1)? {
                        visit(&text);
                    }
                }
                if seen < LATE_CHUNK_ROWS {
                    break;
                }
            }
        }
        Ok(())
    }

    /// The newest `limit` operator messages of a session with their delivery
    /// state, newest first and without their text (#2962, the phone's
    /// delivery view). Read only.
    pub fn recent_operator_message_states(
        &self,
        session_id: Uuid,
        limit: u32,
    ) -> Result<Vec<rsi_common::remote_manager_v2::OperatorMessageStateV2>> {
        let mut statement = self.conn.prepare(concat!(
            "SELECT id,session_id,content,state,created_at,updated_at,delivered_at,original_content,",
            "idempotency_key FROM operator_messages ",
            "WHERE session_id=?1 ORDER BY created_at DESC,id DESC LIMIT ?2"
        ))?;
        let messages = statement
            .query_map(params![session_id.to_string(), i64::from(limit)], |r| {
                Ok((row(r)?, r.get::<_, String>(8)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(messages
            .into_iter()
            .map(
                |(message, key)| rsi_common::remote_manager_v2::OperatorMessageStateV2 {
                    message_id: message.id,
                    state: message.state,
                    created_at: message.created_at,
                    updated_at: message.updated_at,
                    delivered_at: message.delivered_at,
                    text_len: u32::try_from(message.original_content.len()).unwrap_or(u32::MAX),
                    idempotency_key: key
                        .strip_prefix(rsi_common::remote_manager_v2::IDEMPOTENCY_PREFIX)
                        .and_then(|raw| Uuid::parse_str(raw).ok()),
                },
            )
            .collect())
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

    /// #86: only texts a provider may still read name a pasted image.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn paste_image_reference_texts_are_pending_live_or_scheduled_messages() {
        let mut store = Store::open_in_memory().expect("open store");
        let mut live = make_test_session();
        live.status = SessionStatus::Running;
        store.insert_session(&live).expect("insert live session");
        let mut done = make_test_session();
        done.status = SessionStatus::Running;
        store.insert_session(&done).expect("insert done session");

        let queued = store
            .queue_operator_message(live.id, "see @/h/.rsi/paste/rsid/queued.png", "k1")
            .expect("queue");
        let withdrawn = store
            .queue_operator_message(live.id, "@/h/.rsi/paste/rsid/withdrawn.png", "k2")
            .expect("queue");
        store
            .withdraw_operator_message(withdrawn.id)
            .expect("withdraw");
        store
            .queue_operator_message(live.id, "no image here", "k3")
            .expect("queue");
        let finished = store
            .queue_operator_message(done.id, "@/h/.rsi/paste/rsid/finished.png", "k4")
            .expect("queue");
        let claimed = store
            .claim_operator_message(done.id)
            .expect("claim")
            .expect("row");
        assert_eq!(claimed.id, finished.id);
        store
            .mark_operator_message_effect_possible(finished.id)
            .expect("effect");
        store
            .settle_operator_message(finished.id, true)
            .expect("deliver");
        store
            .update_session_status(done.id, SessionStatus::Completed)
            .expect("complete");
        for (id, message, enabled) in [
            ("job-on", "@/h/.rsi/paste/rsid/scheduled.png", 1),
            ("job-off", "@/h/.rsi/paste/rsid/disabled.png", 0),
        ] {
            store
                .conn
                .execute(
                    "INSERT INTO scheduled_jobs (id,name,message,schedule_json,next_fire_at,
                         enabled,created_at,updated_at)
                     VALUES (?1,?1,?2,'{}','2026-01-01T00:00:00.000000000Z',?3,
                         '2026-01-01T00:00:00.000000000Z','2026-01-01T00:00:00.000000000Z')",
                    params![id, message, enabled],
                )
                .expect("insert scheduled job");
        }

        let (mut texts, truncated) = store.paste_image_reference_texts(100).expect("read");
        texts.sort();
        assert!(!truncated);
        assert_eq!(
            texts,
            [
                "@/h/.rsi/paste/rsid/scheduled.png".to_string(),
                queued.content.clone(),
            ]
        );

        let (texts, truncated) = store.paste_image_reference_texts(1).expect("read");
        assert_eq!(texts.len(), 1);
        assert!(truncated);

        // A delivered message still counts while its session is running.
        store
            .update_session_status(done.id, SessionStatus::Running)
            .expect("resume");
        let (texts, _) = store.paste_image_reference_texts(100).expect("read");
        assert!(texts.contains(&finished.content));
    }

    /// #86: the "ever referenced" scan reads every operator message (any
    /// state, both texts), every Message event and enabled scheduled jobs
    /// only, in chunks of exactly `rows` rows.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn paste_reference_scan_reads_every_message_every_event_type_and_enabled_jobs() {
        let mut store = Store::open_in_memory().expect("open store");
        let mut done = make_test_session();
        done.status = SessionStatus::Running;
        store.insert_session(&done).expect("insert session");

        let withdrawn = store
            .queue_operator_message(done.id, "unsent @/h/withdrawn.png", "k1")
            .expect("queue");
        store
            .withdraw_operator_message(withdrawn.id)
            .expect("withdraw");
        let edited = store
            .queue_operator_message(done.id, "see @/h/original.webp", "k2")
            .expect("queue");
        store
            .conn
            .execute(
                "UPDATE operator_messages SET content='edited, no image' WHERE id=?1",
                params![edited.id.to_string()],
            )
            .expect("edit");
        store
            .queue_operator_message(done.id, "nothing to see", "k3")
            .expect("queue");
        store
            .update_session_status(done.id, SessionStatus::Completed)
            .expect("complete");
        for (sequence, event_type, content, tool_input) in [
            (1, EventType::Message, "user sent @/h/event.jpg", None),
            (2, EventType::ToolResult, "listing /h/tool.gif", None),
            (3, EventType::Message, "plain words", None),
            (
                4,
                EventType::ToolUse,
                "",
                Some(serde_json::json!({"file_path": "/h/input.webp"})),
            ),
            (5, EventType::System, "attached /h/system.png", None),
            (6, EventType::ToolResult, "no image here", None),
        ] {
            store
                .insert_event(&ConversationEvent {
                    id: 0,
                    session_id: done.id,
                    sequence,
                    event_type,
                    role: Some(Role::User),
                    content: content.to_string(),
                    tool_name: tool_input.as_ref().map(|_| "Read".to_string()),
                    tool_input: tool_input.map(Box::new),
                    created_at: Utc::now(),
                    offload_id: None,
                    tool_use_id: None,
                    metadata: None,
                })
                .expect("insert event");
        }
        for (id, message, enabled) in [
            ("job-on", "@/h/scheduled.png", 1),
            ("job-off", "@/h/disabled.png", 0),
        ] {
            store
                .conn
                .execute(
                    "INSERT INTO scheduled_jobs (id,name,message,schedule_json,next_fire_at,
                         enabled,created_at,updated_at)
                     VALUES (?1,?1,?2,'{}','2026-01-01T00:00:00.000000000Z',?3,
                         '2026-01-01T00:00:00.000000000Z','2026-01-01T00:00:00.000000000Z')",
                    params![id, message, enabled],
                )
                .expect("insert scheduled job");
        }

        let scan = |source, rows| {
            let mut texts = Vec::new();
            let mut chunks = 0;
            let mut after = 0;
            loop {
                chunks += 1;
                let next = store
                    .paste_reference_scan_chunk(source, after, rows, &mut |text: &str| {
                        texts.push(text.to_string());
                    })
                    .expect("scan");
                match next {
                    Some(cursor) => after = cursor,
                    None => break,
                }
            }
            (texts, chunks)
        };
        let (messages, chunks) = scan(PasteReferenceSource::OperatorMessages, 1);
        assert_eq!(
            messages,
            [
                "unsent @/h/withdrawn.png\nunsent @/h/withdrawn.png".to_string(),
                "edited, no image\nsee @/h/original.webp".to_string(),
            ]
        );
        assert_eq!(chunks, 4, "three rows in chunks of one, then the end");
        // Every event type that carries text counts: a name only a tool
        // result, a tool input or a system row holds is a reference.
        let (events, _) = scan(PasteReferenceSource::ConversationEvents, 2);
        assert_eq!(
            events,
            [
                "user sent @/h/event.jpg\n\n".to_string(),
                "listing /h/tool.gif\n\n".to_string(),
                "\n{\"file_path\":\"/h/input.webp\"}\n".to_string(),
                "attached /h/system.png\n\n".to_string(),
            ]
        );
        // No session is in cold storage: the archived source is empty.
        let (archived, chunks) = scan(PasteReferenceSource::ArchivedConversationEvents, 2);
        assert!(archived.is_empty());
        assert_eq!(chunks, 1);
        let (jobs, _) = scan(PasteReferenceSource::EnabledScheduledJobs, 10);
        assert_eq!(jobs, ["@/h/scheduled.png".to_string()]);
    }

    /// #86: every scan statement looks for every extension an upload stores.
    /// #86 (hub #3322 finding 2): the late check reads only what was written
    /// after the marks (new and edited operator messages, new events of any
    /// type) plus every enabled scheduled job, and fails closed on a cold move
    /// or restore since the marks, on its row bound and on its deadline.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn paste_reference_late_texts_read_rows_written_after_the_marks() {
        let mut store = Store::open_in_memory().expect("open store");
        let mut session = make_test_session();
        session.status = SessionStatus::Running;
        store.insert_session(&session).expect("insert session");
        let event = |sequence, event_type, content: &str| ConversationEvent {
            id: 0,
            session_id: session.id,
            sequence,
            event_type,
            role: Some(Role::Assistant),
            content: content.to_string(),
            tool_name: None,
            tool_input: None,
            created_at: Utc::now(),
            offload_id: None,
            tool_use_id: None,
            metadata: None,
        };
        let job = |store: &Store, id: &str, message: &str, enabled: i64| {
            store
                .conn
                .execute(
                    "INSERT INTO scheduled_jobs (id,name,message,schedule_json,next_fire_at,
                         enabled,created_at,updated_at)
                     VALUES (?1,?1,?2,'{}','2026-01-01T00:00:00.000000000Z',?3,
                         '2026-01-01T00:00:00.000000000Z','2026-01-01T00:00:00.000000000Z')",
                    params![id, message, enabled],
                )
                .expect("insert scheduled job");
        };
        store
            .queue_operator_message(session.id, "seen @/h/old.png", "k1")
            .expect("queue");
        let to_edit = store
            .queue_operator_message(session.id, "plain words", "k2")
            .expect("queue");
        store
            .insert_event(&event(1, EventType::ToolResult, "seen /h/old-event.png"))
            .expect("insert event");
        job(&store, "job-on", "@/h/job.png", 1);
        job(&store, "job-off", "@/h/off.png", 0);

        let marks = store.paste_reference_marks().expect("marks");
        let late = |store: &Store, max_rows, budget| {
            let mut texts = Vec::new();
            store
                .paste_reference_late_texts(
                    &marks,
                    max_rows,
                    std::time::Instant::now() + budget,
                    &mut |text: &str| texts.push(text.to_string()),
                )
                .map(|()| texts.join("|"))
        };
        let minute = std::time::Duration::from_secs(60);
        assert_eq!(
            late(&store, 100, minute).expect("late"),
            "@/h/job.png",
            "only enabled jobs before anything was written"
        );

        store
            .queue_operator_message(session.id, "new @/h/new.png", "k3")
            .expect("queue");
        store
            .edit_operator_message(to_edit.id, "edited @/h/edited.jpg")
            .expect("edit");
        store
            .insert_event(&event(2, EventType::ToolResult, "late /h/late.gif"))
            .expect("insert event");
        job(&store, "job-late", "@/h/late-job.webp", 1);
        let texts = late(&store, 100, minute).expect("late");
        for name in [
            "new.png",
            "edited.jpg",
            "late.gif",
            "job.png",
            "late-job.webp",
        ] {
            assert!(texts.contains(name), "{name} in {texts}");
        }
        for name in ["old.png", "old-event.png", "off.png"] {
            assert!(!texts.contains(name), "{name} in {texts}");
        }

        assert!(late(&store, 2, minute).is_err(), "row bound");
        assert!(
            late(&store, 100, std::time::Duration::ZERO).is_err(),
            "deadline"
        );
        let mut archived = make_test_session();
        archived.status = SessionStatus::Archived;
        store.insert_session(&archived).expect("insert session");
        store
            .cold_index_row_for_test(archived.id, 1)
            .expect("index row");
        let moved = late(&store, 100, minute).expect_err("cold move since the marks");
        assert!(moved.to_string().contains("cold storage"), "{moved}");
    }

    /// #178 (was #86, hub #3322 finding 3): an archive over the scan's size
    /// bound is no longer refused from its index row alone: the scan streams
    /// it, so an archive that cannot be read still fails the scan closed
    /// (`cold_archive_unavailable`, naming its file), never a shorter scan.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn paste_reference_scan_streams_an_archive_over_its_bound_and_fails_closed_when_unreadable() {
        let store = Store::open_in_memory().expect("open store");
        let mut archived = make_test_session();
        archived.status = SessionStatus::Archived;
        store.insert_session(&archived).expect("insert session");
        store
            .cold_index_row_for_test(
                archived.id,
                super::super::cold_archive::PASTE_REFERENCE_ARCHIVE_MAX_RAW_BYTES + 1,
            )
            .expect("index row");
        let mut visited = 0;
        let refused = store
            .paste_reference_scan_chunk(
                PasteReferenceSource::ArchivedConversationEvents,
                0,
                1,
                &mut |_: &str| visited += 1,
            )
            .expect_err("no archive at the indexed path");
        let text = refused.to_string();
        assert!(
            text.contains(super::super::cold_archive::UNAVAILABLE_CODE),
            "{text}"
        );
        assert!(text.contains("cold_archive_file_missing"), "{text}");
        assert!(text.contains(&archived.id.to_string()), "{text}");
        assert_eq!(visited, 0);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn paste_reference_scans_filter_on_every_stored_extension() {
        for sql in [
            PASTE_REFERENCE_MESSAGES_SQL,
            PASTE_REFERENCE_EVENTS_SQL,
            PASTE_REFERENCE_SCHEDULED_SQL,
        ] {
            for (_, ext) in rsi_common::input_assets::PASTE_IMAGE_MIME_ALLOWLIST {
                assert!(sql.contains(&format!("'.{ext}')>0")), "{ext} in {sql}");
            }
        }
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn operator_queue_edit_withdraw_and_restart_preserve_fifo() {
        let directory = tempfile::tempdir().expect("test directory");
        let path = directory.path().join("operator-messages.db");
        let mut store = Store::open_test_fixture(&path).expect("open store");
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
        let mut store = Store::open_test_fixture(&path).expect("open store");
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
        let mut store = Store::open_test_fixture(&path).expect("open store");
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

    /// #3978 (a): an operator prompt for a session that rotates is never lost.
    /// A new prompt to the Archived predecessor is refused naming the live
    /// successor; a message already queued on it is re-addressed to the
    /// successor, where the dispatcher serves it.
    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-03"))]
    #[test]
    fn prompt_to_a_rotated_predecessor_reaches_or_names_its_live_successor() {
        let mut store = Store::open_in_memory().expect("open store");
        let mut predecessor = make_test_session();
        predecessor.status = SessionStatus::Running;
        store
            .insert_session(&predecessor)
            .expect("insert predecessor");
        let queued = store
            .queue_operator_message(predecessor.id, "composed before the rotation", "k-before")
            .expect("queue while the predecessor runs");

        let mut successor = make_test_session();
        successor.status = SessionStatus::Completed;
        successor.continued_from = Some(predecessor.id);
        store.insert_session(&successor).expect("insert successor");
        store
            .update_session_status(predecessor.id, SessionStatus::Archived)
            .expect("rotation archives the predecessor");

        let refusal = store
            .queue_operator_message(predecessor.id, "composed after the rotation", "k-after")
            .expect_err("a new prompt to the rotated predecessor is refused");
        let text = refusal.to_string();
        assert!(
            text.contains("session_rotation_successor_current"),
            "{text}"
        );
        assert!(text.contains(&successor.id.to_string()), "{text}");
        assert!(
            store
                .refuse_rotated_predecessor_prompt(predecessor.id)
                .expect_err("ContinueSession guard")
                .to_string()
                .contains(&successor.id.to_string())
        );
        store
            .refuse_rotated_predecessor_prompt(successor.id)
            .expect("the live successor itself is not refused");
        assert_eq!(
            store
                .next_terminal_operator_message_session()
                .expect("next"),
            None,
            "the stranded message is not dispatchable on the Archived predecessor"
        );

        assert_eq!(
            store
                .redirect_rotated_operator_messages()
                .expect("redirect"),
            1
        );
        assert_eq!(
            store
                .redirect_rotated_operator_messages()
                .expect("idempotent"),
            0
        );
        assert!(
            store
                .list_operator_messages(predecessor.id)
                .expect("predecessor rows")
                .is_empty()
        );
        let moved = store
            .list_operator_messages(successor.id)
            .expect("successor rows");
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].id, queued.id);
        assert_eq!(moved[0].content, "composed before the rotation");
        assert_eq!(moved[0].state, "queued");
        assert_eq!(
            store
                .next_terminal_operator_message_session()
                .expect("next"),
            Some(successor.id)
        );
    }
}
