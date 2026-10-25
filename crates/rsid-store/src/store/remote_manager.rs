//! Store reads behind the RSI Remote API v2 manager routes (#2962): which
//! sessions are the manager seats of a project, and a seat's conversation
//! newest-first. Read only; the daemon (`remote_read/manager_v2.rs`) owns
//! admission, project authorization and the message write.

use super::Store;
use crate::error::Result;
use rsi_common::remote_manager_v2::{IDEMPOTENCY_PREFIX, ItemKindV2, SeatRoleV2};
use rusqlite::params;
use uuid::Uuid;

/// One conversation row of a manager seat, already classified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConversationRow {
    pub id: i64,
    pub sequence: i32,
    /// True for an assistant message (the manager's own words).
    pub assistant: bool,
    /// True for a user-role message the operator queued and the daemon
    /// delivered (`metadata.source == "operator"`).
    pub operator: bool,
    pub operator_message_id: Option<Uuid>,
    /// The phone-chosen idempotency key of that operator message, when the
    /// message came through the Remote API.
    pub idempotency_key: Option<Uuid>,
    /// `Message`, `ToolUse` or `ToolResult`.
    pub kind: rsi_common::remote_manager_v2::ItemKindV2,
    pub tool_name: Option<String>,
    /// The provider call id, only when it fits the wire bound.
    pub tool_use_id: Option<String>,
    /// At most [`CONTENT_CHARS`] characters of the message.
    pub content: String,
    /// True when the stored message was longer than [`CONTENT_CHARS`].
    pub content_cut: bool,
    pub created_at: String,
}

/// Characters of a message the read loads: a seat message may be megabytes,
/// the phone shows a bounded prefix.
pub const CONTENT_CHARS: usize = 4096;

fn live(status: rsi_common::types::SessionStatus) -> bool {
    !matches!(
        status,
        rsi_common::types::SessionStatus::Archived | rsi_common::types::SessionStatus::Deleted
    )
}

impl Store {
    fn session_is_live(&self, id: Uuid) -> Result<bool> {
        Ok(self
            .get_session(id)?
            .is_some_and(|session| live(session.status)))
    }

    /// The live project-manager seat of `project`: the appointment's current
    /// session when its grant is unrevoked and the session is not archived.
    pub fn remote_project_manager_seat(&self, project: Uuid) -> Result<Option<Uuid>> {
        self.on_call_project_manager_seat(project)
    }

    /// The portfolio ("global") seat covering `project`: the root-most active
    /// node whose current seat session is live. Revoked nodes never count.
    pub fn remote_global_manager_seat(&self, project: Uuid) -> Result<Option<Uuid>> {
        for coverage in self.portfolio_chain_for_project(project)? {
            let Some(node) = self.get_portfolio_node(coverage.node_id)? else {
                continue;
            };
            if node.state != "active" {
                continue;
            }
            let seat = node.grant.seat_session_id;
            if self.session_is_live(seat)? {
                return Ok(Some(seat));
            }
        }
        Ok(None)
    }

    /// The role `seat` holds for `project` right now, or `None` when it is
    /// neither the project's current manager seat nor the covering global
    /// seat. A rotated-out predecessor is `None`: the phone re-lists seats.
    pub fn remote_manager_seat_role(
        &self,
        project: Uuid,
        seat: Uuid,
    ) -> Result<Option<SeatRoleV2>> {
        if self.remote_project_manager_seat(project)? == Some(seat) {
            return Ok(Some(SeatRoleV2::ProjectManager));
        }
        if self.remote_global_manager_seat(project)? == Some(seat) {
            return Ok(Some(SeatRoleV2::GlobalManager));
        }
        Ok(None)
    }

    /// Up to `limit` conversation items of `session`, newest first,
    /// strictly before `before` (`(sequence, event id)`) when given, plus
    /// whether older ones remain. User-role and assistant-role `Message`
    /// events plus `ToolUse` and `ToolResult` events (#3797; the tool input
    /// column is never read). Provider diagnostics are not the manager's
    /// words and are skipped. The session must be neither archived nor
    /// deleted, so it is never in cold storage.
    pub fn remote_manager_events_before(
        &self,
        session: Uuid,
        before: Option<(i32, i64)>,
        limit: u32,
    ) -> Result<(Vec<RemoteConversationRow>, bool)> {
        let fetch = i64::from(limit) + 1;
        let (anchor_sequence, anchor_id) = match before {
            Some((sequence, id)) => (Some(i64::from(sequence)), id),
            None => (None, 0),
        };
        let mut statement = self.conn.prepare(concat!(
            "SELECT e.id, e.sequence, e.role, substr(e.content,1,4096), e.created_at, e.metadata, ",
            "length(e.content)>4096, e.event_type, ",
            "CASE WHEN length(CAST(e.tool_name AS BLOB))<=256 THEN e.tool_name END, ",
            "CASE WHEN length(CAST(e.tool_use_id AS BLOB))<=256 THEN e.tool_use_id END, ",
            "(SELECT m.idempotency_key FROM operator_messages m ",
            "WHERE m.session_id=e.session_id AND m.id=CASE WHEN json_valid(e.metadata) ",
            "THEN json_extract(e.metadata,'$.operator_message_id') END) ",
            "FROM conversation_events e JOIN sessions s ON s.id=e.session_id ",
            "WHERE e.session_id=?1 AND s.status NOT IN ('Archived','Deleted') ",
            "AND ((e.event_type='Message' AND e.role IN ('User','Assistant')) ",
            "OR e.event_type IN ('ToolUse','ToolResult')) ",
            "AND (e.metadata IS NULL OR NOT json_valid(e.metadata) ",
            "OR COALESCE(json_extract(e.metadata,'$.provider_diagnostic'),0)!=1) ",
            "AND (?2 IS NULL OR e.sequence<?2 OR (e.sequence=?2 AND e.id<?3)) ",
            "ORDER BY e.sequence DESC, e.id DESC LIMIT ?4"
        ))?;
        let mut rows = statement
            .query_map(
                params![session.to_string(), anchor_sequence, anchor_id, fetch],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i32>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, bool>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, Option<String>>(8)?,
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, Option<String>>(10)?,
                    ))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let has_more = rows.len() as i64 > i64::from(limit);
        rows.truncate(limit as usize);
        let rows = rows
            .into_iter()
            .map(
                |(
                    id,
                    sequence,
                    role,
                    content,
                    created_at,
                    metadata,
                    content_cut,
                    event_type,
                    tool_name,
                    tool_use_id,
                    idempotency_key,
                )| {
                    let kind = match event_type.as_str() {
                        "ToolUse" => ItemKindV2::ToolUse,
                        "ToolResult" => ItemKindV2::ToolResult,
                        _ => ItemKindV2::Message,
                    };
                    let message = kind == ItemKindV2::Message;
                    let role = role.unwrap_or_default();
                    let metadata = metadata
                        .as_deref()
                        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
                    let operator = message
                        && role == "User"
                        && metadata
                            .as_ref()
                            .and_then(|value| value.get("source"))
                            .and_then(serde_json::Value::as_str)
                            == Some(rsi_common::types::OPERATOR_EVENT_SOURCE);
                    let operator_message_id = metadata
                        .as_ref()
                        .and_then(|value| value.get("operator_message_id"))
                        .and_then(serde_json::Value::as_str)
                        .and_then(|raw| Uuid::parse_str(raw).ok())
                        .filter(|_| operator);
                    let idempotency_key = idempotency_key
                        .as_deref()
                        .and_then(|key| key.strip_prefix(IDEMPOTENCY_PREFIX))
                        .and_then(|raw| Uuid::parse_str(raw).ok())
                        .filter(|_| operator_message_id.is_some());
                    RemoteConversationRow {
                        id,
                        sequence,
                        assistant: message && role == "Assistant",
                        operator,
                        operator_message_id,
                        idempotency_key,
                        kind,
                        tool_name: tool_name.filter(|name| !name.is_empty() && !message),
                        tool_use_id: tool_use_id.filter(|id| !id.is_empty() && !message),
                        content,
                        content_cut,
                        created_at,
                    }
                },
            )
            .collect();
        Ok((rows, has_more))
    }
}
