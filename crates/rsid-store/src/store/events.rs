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
        self.note_committed_provider_event(event, event_id);
        Ok(event_id)
    }

    pub(super) fn note_committed_provider_event(&self, event: &ConversationEvent, event_id: i64) {
        // #1343: telemetry has its own transaction after the event is durable;
        // even a database error that rolls it back cannot undo ingestion.
        if let Err(error) = super::friction::note_repeated_tool_error(&self.conn, event, event_id) {
            tracing::warn!(%error, "tool error friction not recorded");
        }
        // #46: an assistant event answers the mail handed to this session.
        // Best effort and after the commit: the event itself is already durable.
        if let Err(error) = self.acknowledge_injected_agent_mail_for_event_v1(event, event_id) {
            tracing::warn!(
                target: "agent_coordination",
                session_id = %event.session_id,
                error = %error,
                "assistant event persisted but delivered mail could not be acknowledged"
            );
        }
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
        // #2373: a ToolResult body over the operator's cap is stored as head +
        // marker + tail, the cut recorded in metadata. Storage only: the caller
        // keeps `event` whole, so nothing live (provider, TUI, friction) sees
        // the cut.
        let truncated = stored_tool_result_body(event);
        let metadata_json = match truncated.as_ref() {
            Some(truncated) => Some(
                serde_json::to_string(&metadata_with_truncation(
                    event.metadata.as_deref(),
                    truncated,
                ))
                .map_err(|e| DaemonError::Store(format!("failed to serialize metadata: {}", e)))?,
            ),
            None => event
                .metadata
                .as_ref()
                .map(|v| serde_json::to_string(v))
                .transpose()
                .map_err(|e| DaemonError::Store(format!("failed to serialize metadata: {}", e)))?,
        };
        let content: &str = truncated
            .as_ref()
            .map_or(event.content.as_str(), |truncated| {
                truncated.content.as_str()
            });

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
                content,
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

        let live = rows
            .into_iter()
            .map(Self::convert_event_row)
            .collect::<Result<Vec<_>>>()?;
        // #2384: a cold session's archived rows are merged back here, in the one
        // place every transcript reader goes through.
        self.merge_cold_events(session_id, since_sequence, live)
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
        // #2384: the count and the content length are logical aggregates, live
        // rows plus the rows archived for a cold session, so the watermark does
        // not change when a session moves or is restored (see
        // `cold_archive::LOGICAL_EVENT_COUNT_EXPR`; the maximum sequence is exact
        // because the newest row always stays live).
        let (count, max_seq, total_len) = self.conn.query_row(
            "SELECT COUNT(*) + COALESCE((SELECT archived_event_count FROM cold_archive_sessions
                                          WHERE session_id = ?1 AND state = 'cold'), 0),
                    COALESCE(MAX(sequence), -1),
                    COALESCE(SUM(LENGTH(content)), 0) + COALESCE((SELECT archived_content_len
                                          FROM cold_archive_sessions
                                          WHERE session_id = ?1 AND state = 'cold'), 0)
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

    /// A decoded cold-archive row (`cold_archive::EVENT_COLUMNS` order) as an
    /// event: the same conversion a live row gets in `load_events_since`.
    pub(super) fn event_from_cold_values(
        values: &[rusqlite::types::Value],
    ) -> Result<ConversationEvent> {
        use rusqlite::types::Value;
        let bad = |index: usize, what: &str| {
            DaemonError::Store(format!(
                "cold archive event row column {index} is not {what}: {:?}",
                values.get(index)
            ))
        };
        let text = |index: usize| match values.get(index) {
            Some(Value::Text(text)) => Ok(text.clone()),
            _ => Err(bad(index, "text")),
        };
        let optional_text = |index: usize| match values.get(index) {
            Some(Value::Null) => Ok(None),
            Some(Value::Text(text)) => Ok(Some(text.clone())),
            _ => Err(bad(index, "text or NULL")),
        };
        let id = match values.first() {
            Some(Value::Integer(id)) => *id,
            _ => return Err(bad(0, "an integer")),
        };
        let sequence = match values.get(2) {
            Some(Value::Integer(sequence)) => {
                i32::try_from(*sequence).map_err(|_| bad(2, "an i32 sequence"))?
            }
            _ => return Err(bad(2, "an integer")),
        };
        Self::convert_event_row((
            id,
            text(1)?,
            sequence,
            text(3)?,
            optional_text(4)?,
            text(5)?,
            optional_text(6)?,
            optional_text(7)?,
            text(8)?,
            optional_text(9)?,
            optional_text(10)?,
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

    /// `AgentReadSessionEvents` (#1041): one page of a session's events.
    ///
    /// With `after_sequence` the page is forward (sequence greater, ascending);
    /// without it the page is the newest `limit` matching events, ascending.
    /// Returns `(events, has_more, has_earlier)` relative to the page. Read
    /// only.
    pub fn read_session_events_page(
        &self,
        session_id: Uuid,
        after_sequence: Option<i32>,
        limit: u32,
        event_types: Option<&[EventType]>,
    ) -> Result<(Vec<ConversationEvent>, bool, bool)> {
        // #2384: a cold session pages over the merged (archive + live tail)
        // transcript with the same rules as the SQL below.
        if let Some(page) = self.cold_events_page(session_id, after_sequence, limit, event_types)? {
            return Ok(page);
        }
        // The type filter is one bound JSON array (NULL = no filter), so every
        // statement below is a static string the protected-DML audit can prove
        // never writes: no SQL text is assembled from event-type names.
        let type_filter: Option<String> = match event_types {
            Some(types) if !types.is_empty() => {
                let names: Vec<&str> = types.iter().map(|t| event_type_to_str(*t)).collect();
                Some(serde_json::to_string(&names).map_err(|e| DaemonError::Store(e.to_string()))?)
            }
            _ => None,
        };
        let fetch = i64::from(limit) + 1;
        let sid = session_id.to_string();
        let (mut rows, has_more, has_earlier) = if let Some(after) = after_sequence {
            let mut stmt = self.conn.prepare(concat!(
                "SELECT id, session_id, sequence, event_type, role, content, tool_name, tool_input, created_at, tool_use_id, metadata FROM conversation_events WHERE session_id = ?1 AND sequence > ?2 ",
                "AND (?4 IS NULL OR event_type IN (SELECT value FROM json_each(?4))) ORDER BY sequence ASC LIMIT ?3"
            ))?;
            let rows = stmt
                .query_map(params![sid, after, fetch, type_filter], Self::map_event_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let earlier: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM conversation_events WHERE session_id = ?1 AND sequence <= ?2 AND (?3 IS NULL OR event_type IN (SELECT value FROM json_each(?3))))",
                params![sid, after, type_filter],
                |row| row.get(0),
            )?;
            let more = rows.len() as i64 > i64::from(limit);
            (rows, more, earlier)
        } else {
            let mut stmt = self.conn.prepare(concat!(
                "SELECT id, session_id, sequence, event_type, role, content, tool_name, tool_input, created_at, tool_use_id, metadata FROM conversation_events WHERE session_id = ?1 ",
                "AND (?3 IS NULL OR event_type IN (SELECT value FROM json_each(?3))) ORDER BY sequence DESC LIMIT ?2"
            ))?;
            let mut rows = stmt
                .query_map(params![sid, fetch, type_filter], Self::map_event_row)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let earlier = rows.len() as i64 > i64::from(limit);
            rows.reverse();
            (rows, false, earlier)
        };
        // Drop the probe row: the oldest one for a tail, the newest otherwise.
        if rows.len() as i64 > i64::from(limit) {
            if after_sequence.is_some() {
                rows.pop();
            } else {
                rows.remove(0);
            }
        }
        let events = rows
            .into_iter()
            .map(Self::convert_event_row)
            .collect::<Result<Vec<_>>>()?;
        Ok((events, has_more, has_earlier))
    }

    /// The newest assistant `Message` event of a session, if any. Provider
    /// diagnostics (marked `metadata.provider_diagnostic`) are skipped: they
    /// are not the agent's own output (#1089).
    pub fn last_assistant_message_event(
        &self,
        session_id: Uuid,
    ) -> Result<Option<ConversationEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, sequence, event_type, role, content, tool_name, tool_input, created_at, tool_use_id, metadata
             FROM conversation_events
             WHERE session_id = ?1 AND event_type = 'Message' AND role = 'Assistant'
               AND (metadata IS NULL
                    OR NOT json_valid(metadata)
                    OR COALESCE(json_extract(metadata, '$.provider_diagnostic'), 0) != 1)
             ORDER BY sequence DESC LIMIT 1",
        )?;
        let row = stmt
            .query_map(params![session_id.to_string()], Self::map_event_row)?
            .next()
            .transpose()?;
        row.map(Self::convert_event_row).transpose()
    }

    /// The rotation tip of `origin`'s `continued_from` lineage.
    pub fn session_lineage_tip(&self, origin: Uuid) -> Result<Uuid> {
        let tx = self.conn.unchecked_transaction()?;
        super::agent_coordination::resolve_lineage_tip(&tx, origin)
    }

    /// Update the content of a conversation event (for compression/offload).
    ///
    /// The row must be live (#2384): an id that matches no row, such as one
    /// whose session moved to cold storage, is an error, not a silent no-op.
    pub fn update_event_content(&self, event_id: i64, new_content: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE conversation_events SET content = ?1 WHERE id = ?2",
            params![new_content, event_id],
        )?;
        if changed != 1 {
            return Err(DaemonError::Store(format!(
                "update_event_content: event {event_id} is not a live conversation event"
            )));
        }
        Ok(())
    }
}

/// #2373: the capped stored form of a `ToolResult` body. `None` when the event
/// is not a tool result, the operator turned the cap off, or the body fits (the
/// row is then written byte-identical).
fn stored_tool_result_body(
    event: &ConversationEvent,
) -> Option<rsid_core::tool_result_cap::StoredBody> {
    if event.event_type != EventType::ToolResult {
        return None;
    }
    let cap = rsid_core::tool_result_cap::current_cap_bytes()?;
    rsid_core::tool_result_cap::cap_body(&event.content, cap)
}

/// The row's metadata with the cut recorded under
/// [`rsid_core::tool_result_cap::TRUNCATION_METADATA_KEY`]. Existing keys
/// (`is_error`, ...) are kept; a metadata value that is not an object stays as
/// it is, and the marker inside the body still names the cut.
fn metadata_with_truncation(
    existing: Option<&serde_json::Value>,
    truncated: &rsid_core::tool_result_cap::StoredBody,
) -> serde_json::Value {
    let mut map = match existing {
        Some(serde_json::Value::Object(map)) => map.clone(),
        None | Some(serde_json::Value::Null) => serde_json::Map::new(),
        Some(other) => return other.clone(),
    };
    map.insert(
        rsid_core::tool_result_cap::TRUNCATION_METADATA_KEY.to_string(),
        truncated.metadata(),
    );
    serde_json::Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_session;
    use rsid_core::tool_result_cap;
    use serde_json::json;
    use std::path::PathBuf;

    fn tool_result(
        session_id: Uuid,
        sequence: i32,
        content: String,
        metadata: Option<serde_json::Value>,
    ) -> ConversationEvent {
        ConversationEvent {
            id: 0,
            offload_id: None,
            session_id,
            sequence,
            event_type: EventType::ToolResult,
            role: None,
            content,
            tool_name: Some("Bash".into()),
            tool_input: None,
            created_at: Utc::now(),
            tool_use_id: Some(format!("tool-{sequence}")),
            metadata: metadata.map(Box::new),
        }
    }

    fn store_with_session() -> (Store, Uuid) {
        let store = Store::open_in_memory().unwrap();
        let id = Uuid::new_v4();
        store
            .insert_session(&test_session(id, PathBuf::from("/tmp/events")))
            .unwrap();
        (store, id)
    }

    fn stored(store: &Store, session: Uuid, sequence: i32) -> ConversationEvent {
        store
            .load_events(session)
            .unwrap()
            .into_iter()
            .find(|event| event.sequence == sequence)
            .expect("stored event")
    }

    /// 200 KB of numbered lines: the first and last lines are recognisable.
    fn big_body() -> String {
        (0..8000)
            .map(|line| format!("line {line:05} of the cargo output\n"))
            .collect()
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn an_over_cap_tool_result_is_stored_as_head_marker_tail() {
        let _guard = tool_result_cap::test_serial_guard();
        tool_result_cap::set_cap_kib(tool_result_cap::TOOL_RESULT_STORAGE_CAP_KIB_DEFAULT);
        let cap = 32 * 1024;
        let (store, session) = store_with_session();
        let body = big_body();
        assert!(body.len() > 6 * cap);
        let event = tool_result(session, 1, body.clone(), Some(json!({"is_error": true})));
        store.insert_event(&event).unwrap();

        let row = stored(&store, session, 1);
        assert!(row.content.len() <= cap, "{}", row.content.len());
        assert!(row.content.starts_with("line 00000 of the cargo output\n"));
        assert!(row.content.ends_with("line 07999 of the cargo output\n"));
        assert!(
            row.content
                .contains(&format!("of {} bytes omitted", body.len())),
            "the marker names the original byte count"
        );
        let truncation = row
            .metadata
            .as_deref()
            .and_then(|metadata| metadata.get(tool_result_cap::TRUNCATION_METADATA_KEY))
            .cloned()
            .expect("the cut is recorded in metadata");
        let head = usize::try_from(truncation["head_bytes"].as_u64().unwrap()).unwrap();
        let tail = usize::try_from(truncation["tail_bytes"].as_u64().unwrap()).unwrap();
        assert_eq!(truncation["original_bytes"], body.len());
        assert_eq!(truncation["cap_bytes"], cap);
        assert!(body.starts_with(&row.content[..head]));
        assert!(body.ends_with(&row.content[row.content.len() - tail..]));
        assert_eq!(
            row.metadata.as_deref().and_then(|m| m.get("is_error")),
            Some(&json!(true)),
            "existing metadata keys survive"
        );
        // Storage only: the event the caller holds is the whole body.
        assert_eq!(event.content, body);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn an_over_cap_tool_result_without_metadata_still_records_the_cut() {
        let _guard = tool_result_cap::test_serial_guard();
        tool_result_cap::set_cap_kib(tool_result_cap::TOOL_RESULT_STORAGE_CAP_KIB_DEFAULT);
        let (store, session) = store_with_session();
        let body = big_body();
        store
            .insert_event(&tool_result(session, 1, body.clone(), None))
            .unwrap();
        let row = stored(&store, session, 1);
        let metadata = row.metadata.expect("cut recorded");
        assert_eq!(
            metadata[tool_result_cap::TRUNCATION_METADATA_KEY]["original_bytes"],
            body.len()
        );
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn a_tool_result_within_the_cap_and_other_event_types_are_stored_unchanged() {
        let _guard = tool_result_cap::test_serial_guard();
        tool_result_cap::set_cap_kib(tool_result_cap::TOOL_RESULT_STORAGE_CAP_KIB_DEFAULT);
        let (store, session) = store_with_session();
        let small = "ok\n".repeat(10_000);
        assert!(small.len() < 32 * 1024);
        let exact = "y".repeat(32 * 1024);
        store
            .insert_event(&tool_result(
                session,
                1,
                small.clone(),
                Some(json!({"is_error": false})),
            ))
            .unwrap();
        store
            .insert_event(&tool_result(session, 2, exact.clone(), None))
            .unwrap();
        let mut message = tool_result(session, 3, big_body(), None);
        message.event_type = EventType::Message;
        message.role = Some(Role::Assistant);
        message.tool_name = None;
        message.tool_use_id = None;
        store.insert_event(&message).unwrap();

        let small_row = stored(&store, session, 1);
        assert_eq!(small_row.content, small);
        assert_eq!(
            small_row.metadata.as_deref(),
            Some(&json!({"is_error": false}))
        );
        let exact_row = stored(&store, session, 2);
        assert_eq!(exact_row.content, exact);
        assert_eq!(exact_row.metadata, None);
        assert_eq!(stored(&store, session, 3).content, message.content);
    }

    #[cfg(any(not(feature = "test-shard-mode"), feature = "test-shard-store-01"))]
    #[test]
    fn turning_the_cap_off_stores_tool_results_whole() {
        let _guard = tool_result_cap::test_serial_guard();
        tool_result_cap::set_cap_kib(0);
        let (store, session) = store_with_session();
        let body = big_body();
        store
            .insert_event(&tool_result(session, 1, body.clone(), None))
            .unwrap();
        let row = stored(&store, session, 1);
        tool_result_cap::set_cap_kib(tool_result_cap::TOOL_RESULT_STORAGE_CAP_KIB_DEFAULT);
        assert_eq!(row.content, body);
        assert_eq!(row.metadata, None);
    }
}
