//! RSI Remote API v2 daemon wire (#2962): the manager seats of exposed
//! projects, a manager seat's conversation and one message write.
//!
//! These are operator-local methods on the daemon socket, like the V1 Remote
//! reads and the answer write. They stay out of `AGENT_VERBS`, `READ_VERBS`,
//! native tools and the agent CLI catalog: only the `rsi-remote` gateway
//! calls them, after it has authorized the tailnet device, the browser or
//! native-client session, the project scope and the `allow_messages` switch.
//! The daemon still re-proves that the named session is the current manager
//! seat of the named project (or the portfolio seat covering it).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// `RemoteListManagerSeatsV2` (read).
pub const METHOD_LIST_SEATS: &str = "RemoteListManagerSeatsV2";
/// `RemoteGetManagerConversationV2` (read).
pub const METHOD_CONVERSATION: &str = "RemoteGetManagerConversationV2";
/// `RemoteSendManagerMessageV2` (write, `QueueOperatorMessage` semantics).
pub const METHOD_SEND: &str = "RemoteSendManagerMessageV2";

/// Longest message text, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 4096;
/// Newest-first conversation items per page.
pub const CONVERSATION_PAGE_LIMIT: u32 = 25;
/// Longest conversation item text, in bytes; longer text is cut at a char
/// boundary and flagged `truncated`.
pub const CONVERSATION_TEXT_BYTES: usize = 4096;
/// Recent operator messages (with their delivery state) on a first page.
pub const RECENT_OPERATOR_MESSAGES: u32 = 10;
/// Prefix that namespaces a gateway idempotency key in the operator message
/// table, so it can never collide with a key the TUI chose.
pub const IDEMPOTENCY_PREFIX: &str = "remote-v2:";

/// Which seat a session holds for a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatRoleV2 {
    /// The project's appointed (current) manager seat.
    ProjectManager,
    /// The active portfolio ("global") node seat covering the project; when
    /// several nodes cover it, the root-most live one.
    GlobalManager,
}

/// One manager seat as the phone shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerSeatV2 {
    pub session_id: Uuid,
    pub role: SeatRoleV2,
    /// The session status, serde name (`Running`, `Completed`, ...).
    pub status: String,
    pub provider: String,
    pub model: Option<String>,
    /// RFC3339 with nanoseconds: the session row's last update.
    pub last_activity_at: String,
    /// Waiting decisions in the project's manager ledger. `null` for the
    /// global seat, which has no per-project ledger of its own.
    pub pending_decisions: Option<u32>,
    /// True when the decision count stopped at its scan cap.
    pub pending_decisions_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectSeatsV2 {
    pub project_id: Uuid,
    pub name: String,
    pub project_manager: Option<ManagerSeatV2>,
    pub global_manager: Option<ManagerSeatV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListManagerSeatsRequestV2 {
    pub project_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListManagerSeatsResponseV2 {
    /// In `project_id` order; a configured id with no project row is absent.
    pub projects: Vec<ProjectSeatsV2>,
}

/// Where an older page starts: strictly before this event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationAnchorV2 {
    pub sequence: i32,
    pub id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationRequestV2 {
    pub project_id: Uuid,
    pub seat_session_id: Uuid,
    #[serde(default)]
    pub before: Option<ConversationAnchorV2>,
    pub limit: u32,
}

/// Who wrote a conversation item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerV2 {
    /// The operator, delivered through `QueueOperatorMessage`.
    Operator,
    /// The manager's own assistant message.
    Manager,
    /// A daemon-written user-role message (wake prompts, agent mail, notices).
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationItemV2 {
    /// Decimal string: the event id.
    pub id: String,
    pub sequence: i32,
    pub speaker: SpeakerV2,
    pub text: String,
    pub truncated: bool,
    pub created_at: String,
    /// The operator message this item delivered, for an operator item.
    pub operator_message_id: Option<Uuid>,
}

/// The delivery state of one message the operator queued for the seat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorMessageStateV2 {
    pub message_id: Uuid,
    /// `queued`, `dispatching`, `effect_possible`, `delivered`, `uncertain`
    /// or `withdrawn`. Only `delivered` means the manager has seen it.
    pub state: String,
    pub created_at: String,
    pub updated_at: String,
    pub delivered_at: Option<String>,
    /// Length of the message text in bytes (the text is never echoed here).
    pub text_len: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationResponseV2 {
    pub seat: ManagerSeatV2,
    /// Newest first.
    pub items: Vec<ConversationItemV2>,
    /// `<sequence>:<id>` to pass as `?before=` for the next older page.
    pub next_before: Option<String>,
    pub has_more: bool,
    /// Recent operator messages with their delivery state, newest first.
    /// Only on the first page (no `before`); empty on older pages.
    pub operator_messages: Vec<OperatorMessageStateV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendMessageRequestV2 {
    pub project_id: Uuid,
    pub seat_session_id: Uuid,
    pub text: String,
    pub idempotency_key: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendMessageReceiptV2 {
    pub message_id: Uuid,
    pub seat_session_id: Uuid,
    pub state: String,
    pub created_at: String,
    /// True when this idempotency key had already queued the message.
    pub deduplicated: bool,
}

/// Why the daemon refused a v2 call; the closed set the gateway maps to a
/// status. The daemon error message contains exactly one of these codes.
pub const REFUSAL_SEAT_OUT_OF_SCOPE: &str = "seat_out_of_scope";
pub const REFUSAL_INVALID_MESSAGE: &str = "invalid_message";
pub const REFUSAL_IDEMPOTENCY_CONFLICT: &str = "idempotency_conflict";
pub const REFUSAL_SEAT_NOT_LIVE: &str = "seat_not_live";

/// `text` is 1..=[`MAX_MESSAGE_BYTES`] bytes, not blank, without NUL.
#[must_use]
pub fn valid_message_text(text: &str) -> bool {
    !text.trim().is_empty() && text.len() <= MAX_MESSAGE_BYTES && !text.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_text_grammar() {
        assert!(valid_message_text("ship it"));
        assert!(valid_message_text(&"x".repeat(MAX_MESSAGE_BYTES)));
        assert!(!valid_message_text(&"x".repeat(MAX_MESSAGE_BYTES + 1)));
        assert!(!valid_message_text(""));
        assert!(!valid_message_text("  \n\t"));
        assert!(!valid_message_text("a\u{0}b"));
    }

    #[test]
    fn requests_deny_unknown_fields() {
        assert!(
            serde_json::from_str::<SendMessageRequestV2>(
                r#"{"project_id":"550e8400-e29b-41d4-a716-446655440000","seat_session_id":"550e8400-e29b-41d4-a716-446655440001","text":"hi","idempotency_key":"550e8400-e29b-41d4-a716-446655440002","extra":1}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<ListManagerSeatsRequestV2>(r#"{"project_ids":[],"x":1}"#)
                .is_err()
        );
    }
}
