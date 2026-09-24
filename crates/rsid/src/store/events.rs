//! Conversation event persistence operations.

use super::Store;
use super::row_mappers::{
    event_type_to_str, parse_timestamp, role_to_str, str_to_event_type, str_to_role,
};
use crate::error::{DaemonError, Result};
use chrono::{DateTime, Utc};
use rsi_common::closure_kernel::ConversationEventProvenanceV1;
use rsi_common::types::{ConversationEvent, EventType, Role};
use rusqlite::{Transaction, TransactionBehavior, params};
use uuid::Uuid;

impl Store {
    /// Insert a conversation event. Returns the assigned auto-increment ID.
    pub fn insert_event(&self, event: &ConversationEvent) -> Result<i64> {
        self.insert_event_inner(event, None)
    }

    /// Atomically insert one event and its immutable typed producer
    /// provenance. There is no state where a Closure-visible provider event
    /// exists without its discriminator.
    pub fn insert_event_with_provenance(
        &self,
        event: &ConversationEvent,
        provenance: &ConversationEventProvenanceV1,
    ) -> Result<i64> {
        self.insert_event_inner(event, Some(provenance))
    }

    fn insert_event_inner(
        &self,
        event: &ConversationEvent,
        provenance: Option<&ConversationEventProvenanceV1>,
    ) -> Result<i64> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let event_id = Self::insert_event_in_transaction(&tx, event, provenance)?;
        // A legacy/un-normalized AskUserQuestion event must not leave a prior
        // same-text publication answerable. Only the dedicated producer path
        // can bind a pending snapshot to this newly inserted event.
        if event.event_type == EventType::ToolUse
            && event.tool_name.as_deref() == Some("AskUserQuestion")
        {
            tx.execute(
                "UPDATE pending_question_publications SET state='unresolved',
                 conversation_event_id=NULL,event_sequence=NULL,tool_use_id=NULL,
                 model_invocation_id=NULL WHERE session_id=?1",
                [event.session_id.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(event_id)
    }

    /// Shared event/provenance writer for transactions with additional producer
    /// state. The caller owns commit; there is never a nested transaction.
    pub(super) fn insert_event_in_transaction(
        tx: &Transaction<'_>,
        event: &ConversationEvent,
        provenance: Option<&ConversationEventProvenanceV1>,
    ) -> Result<i64> {
        let tool_input_json = event
            .tool_input
            .as_ref()
            .map(|v| serde_json::to_string(v))
            .transpose()
            .map_err(|e| DaemonError::Store(format!("failed to serialize tool_input: {}", e)))?;
        let metadata_json = event
            .metadata
            .as_ref()
            .map(|v| serde_json::to_string(v))
            .transpose()
            .map_err(|e| DaemonError::Store(format!("failed to serialize metadata: {}", e)))?;

        if let Some(provenance) = provenance {
            let invocation_session: String = tx
                .query_row(
                    "SELECT session_id FROM model_invocations WHERE id=?1",
                    [provenance.model_invocation_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|error| {
                    DaemonError::Store(format!(
                        "event provenance model invocation is unavailable: {error}"
                    ))
                })?;
            if invocation_session != event.session_id.to_string() {
                return Err(DaemonError::Store(
                    "event provenance model invocation belongs to another session".into(),
                ));
            }
            if matches!(
                provenance.producer_kind,
                rsi_common::closure_kernel::ConversationEventProducerKindV1::ProviderAssistantOutput
                    | rsi_common::closure_kernel::ConversationEventProducerKindV1::DaemonProviderDiagnostic
            ) && !(event.event_type == EventType::Message && event.role == Some(Role::Assistant))
            {
                return Err(DaemonError::Store(
                    "assistant/diagnostic provenance requires a Message/Assistant event".into(),
                ));
            }
        }

        tx.execute(
            "INSERT INTO conversation_events (session_id, sequence, event_type, role, content, tool_name, tool_input, created_at, tool_use_id, metadata)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                event.session_id.to_string(),
                event.sequence,
                event_type_to_str(event.event_type),
                event.role.map(role_to_str),
                event.content,
                event.tool_name,
                tool_input_json,
                event.created_at.to_rfc3339(),
                event.tool_use_id.as_deref(),
                metadata_json,
            ],
        )?;

        let event_id = tx.last_insert_rowid();
        if let Some(provenance) = provenance {
            tx.execute(
                "INSERT INTO conversation_event_provenance
                 (conversation_event_id,producer_kind,model_invocation_id,provider_event_type,created_at)
                 VALUES (?1,?2,?3,?4,?5)",
                params![
                    event_id,
                    provenance.producer_kind.as_str(),
                    provenance.model_invocation_id.to_string(),
                    provenance.provider_event_type,
                    event.created_at.to_rfc3339(),
                ],
            )?;
        }
        Ok(event_id)
    }

    /// Load events for a session with sequence > since_sequence, ordered by sequence.
    /// If `since_sequence` is None, loads all events.
    pub fn load_events_since(
        &self,
        session_id: Uuid,
        since_sequence: Option<i32>,
    ) -> Result<Vec<ConversationEvent>> {
        let (sql, param_seq) = match since_sequence {
            Some(seq) => (
                "SELECT id, session_id, sequence, event_type, role, content, tool_name, tool_input, created_at, tool_use_id, metadata
                 FROM conversation_events WHERE session_id = ?1 AND sequence > ?2 ORDER BY sequence ASC",
                Some(seq),
            ),
            None => (
                "SELECT id, session_id, sequence, event_type, role, content, tool_name, tool_input, created_at, tool_use_id, metadata
                 FROM conversation_events WHERE session_id = ?1 ORDER BY sequence ASC",
                None,
            ),
        };
        let mut stmt = self.conn.prepare(sql)?;

        let rows = if let Some(seq) = param_seq {
            stmt.query_map(params![session_id.to_string(), seq], Self::map_event_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![session_id.to_string()], Self::map_event_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?
        };

        rows.into_iter().map(Self::convert_event_row).collect()
    }

    /// Load all events for a session, ordered by sequence.
    pub fn load_events(&self, session_id: Uuid) -> Result<Vec<ConversationEvent>> {
        self.load_events_since(session_id, None)
    }

    /// Cheap change-detection summary for a session's event stream.
    ///
    /// Returns `"<count>:<max_sequence>:<total_content_len>"`, or `None` when
    /// the session has no events. Callers that would otherwise `load_events`
    /// purely to detect change (memory sync) can compare this against a stored
    /// value and skip materializing the transcript entirely.
    ///
    /// The three dimensions cover the ways an event stream can move:
    /// appends change the count and max sequence, and in-place content rewrites
    /// (context offload — see `update_event_content`) change the total length
    /// without touching either of the others.
    pub fn event_watermark(&self, session_id: Uuid) -> Result<Option<String>> {
        let (count, max_seq, total_len) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(MAX(sequence), -1), COALESCE(SUM(LENGTH(content)), 0)
             FROM conversation_events WHERE session_id = ?1",
            params![session_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?;

        if count == 0 {
            return Ok(None);
        }

        Ok(Some(format!("{}:{}:{}", count, max_seq, total_len)))
    }

    /// Instant of the most recent PROVIDER-AUTHORED event for a session, or
    /// `None` if the session has never produced one.
    ///
    /// This is the evidence that a turn actually *ran*, as distinct from having
    /// merely been spawned. A resume whose turn dies before the model emits
    /// anything writes only `System` rows plus the injected prompt, so the
    /// predicate deliberately excludes both:
    ///
    /// - `System` — transport/init noise, emitted even by a turn that produced
    ///   nothing at all.
    /// - `Message` with `role = User` — includes daemon-injected prompts such as
    ///   a terminal-watch delivery, which would otherwise confirm itself.
    ///
    /// `role = 'Assistant'` covers assistant text and `ToolUse` (both carry that
    /// role); `Thinking` carries no role and is included explicitly.
    ///
    /// Ordering is by `sequence` (per-session monotonic), NOT by comparing
    /// `created_at` strings. Stored RFC3339 values carry variable subsecond
    /// precision and legacy rows can even carry a non-UTC offset, so
    /// lexicographic comparison is not a valid ordering over this column. The
    /// caller compares the returned instant in Rust.
    pub fn last_provider_output_at(&self, session_id: Uuid) -> Result<Option<DateTime<Utc>>> {
        let mut stmt = self.conn.prepare(
            "SELECT created_at FROM conversation_events
             WHERE session_id = ?1
               AND (role = 'Assistant' OR event_type = 'Thinking')
             ORDER BY sequence DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![session_id.to_string()])?;
        match rows.next()? {
            Some(row) => {
                let raw: String = row.get(0)?;
                let parsed = parse_timestamp(&raw).map_err(|e| {
                    DaemonError::Store(format!(
                        "unparseable conversation_events.created_at {raw:?}: {e}"
                    ))
                })?;
                Ok(Some(parsed))
            }
            None => Ok(None),
        }
    }

    fn map_event_row(
        row: &rusqlite::Row,
    ) -> rusqlite::Result<(
        i64,
        String,
        i32,
        String,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
    )> {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
            row.get(7)?,
            row.get(8)?,
            row.get(9)?,
            row.get(10)?,
        ))
    }

    fn convert_event_row(
        row: (
            i64,
            String,
            i32,
            String,
            Option<String>,
            String,
            Option<String>,
            Option<String>,
            String,
            Option<String>,
            Option<String>,
        ),
    ) -> Result<ConversationEvent> {
        let (
            id,
            session_id_str,
            sequence,
            event_type_str,
            role_str,
            content,
            tool_name,
            tool_input_str,
            created_at_str,
            tool_use_id,
            metadata_str,
        ) = row;
        let session_id = Uuid::parse_str(&session_id_str)
            .map_err(|e| DaemonError::Store(format!("Invalid UUID: {}", e)))?;
        let event_type = str_to_event_type(&event_type_str)?;
        let role = role_str.as_deref().map(str_to_role).transpose()?;
        let tool_input = tool_input_str
            .as_deref()
            .map(serde_json::from_str::<serde_json::Value>)
            .transpose()
            .map_err(|e| DaemonError::Store(format!("Invalid tool_input JSON: {}", e)))?
            .map(Box::new);
        let metadata = metadata_str
            .as_deref()
            .map(serde_json::from_str::<serde_json::Value>)
            .transpose()
            .map_err(|e| DaemonError::Store(format!("Invalid metadata JSON: {}", e)))?
            .map(Box::new);
        let created_at = parse_timestamp(&created_at_str).map_err(DaemonError::Store)?;

        Ok(ConversationEvent {
            id,
            session_id,
            sequence,
            event_type,
            role,
            content,
            tool_name,
            tool_input,
            created_at,
            // NOTE: `offload_id` is not persisted by this table today; it is
            // reconstructed elsewhere. Out of scope for V81.
            offload_id: None,
            tool_use_id,
            metadata,
        })
    }

    /// Update the content of a conversation event (for compression/offload).
    pub fn update_event_content(&self, event_id: i64, new_content: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE conversation_events SET content = ?1 WHERE id = ?2",
            params![new_content, event_id],
        )?;
        Ok(())
    }
}
