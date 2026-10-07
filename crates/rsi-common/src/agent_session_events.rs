//! `AgentReadSessionEvents` (#1041): a typed, scoped, byte-bounded read of one
//! session's conversation events, so a manager or lead reads its workers'
//! results through the daemon instead of raw SQLite.
//!
//! The caller identity is never a parameter: the daemon binds it from the
//! authenticated session token.

use crate::types::{EventType, Role, SessionStatus};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Events returned when `limit` is omitted.
pub const AGENT_READ_EVENTS_DEFAULT_LIMIT: u32 = 20;
/// Hard ceiling for `limit`.
pub const AGENT_READ_EVENTS_MAX_LIMIT: u32 = 100;
/// Page byte budget when `max_bytes` is omitted.
pub const AGENT_READ_EVENTS_DEFAULT_MAX_BYTES: u32 = 32 * 1024;
/// Hard ceiling for `max_bytes`; larger requests are clamped to it.
pub const AGENT_READ_EVENTS_MAX_BYTES_CEILING: u32 = 256 * 1024;
/// Floor for `max_bytes` (a page always carries at least one event).
pub const AGENT_READ_EVENTS_MIN_BYTES: u32 = 1024;
/// Per-event cap, in characters, for content, tool input and metadata.
pub const AGENT_READ_EVENT_FIELD_MAX_CHARS: usize = 2_000;
/// Stable refusal returned for an unknown OR out-of-scope target, so a
/// refusal never reveals whether the session exists.
pub const AGENT_READ_EVENTS_SCOPE_DENIED: &str = "agent_verb_scope_denied";

/// Request for `AgentReadSessionEvents`.
///
/// With `after_sequence` the page is forward (events with a greater
/// sequence, ascending). Without it the page is the tail: the newest `limit`
/// events, ascending.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentReadSessionEventsRequestV1 {
    pub session_id: Uuid,
    #[serde(default)]
    pub after_sequence: Option<i32>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub event_types: Option<Vec<EventType>>,
    #[serde(default)]
    pub max_bytes: Option<u32>,
    /// Return the final assistant message up to the larger full-message cap.
    #[serde(default)]
    pub final_message_full: bool,
}

impl AgentReadSessionEventsRequestV1 {
    /// Validate the structural bounds.
    ///
    /// # Errors
    /// Returns a stable code for the first invalid field.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.session_id.is_nil() {
            return Err("agent_read_events_invalid_session_id");
        }
        if self.after_sequence.is_some_and(|s| s < 0) {
            return Err("agent_read_events_invalid_after_sequence");
        }
        if self
            .limit
            .is_some_and(|l| l == 0 || l > AGENT_READ_EVENTS_MAX_LIMIT)
        {
            return Err("agent_read_events_invalid_limit");
        }
        if self
            .max_bytes
            .is_some_and(|b| b < AGENT_READ_EVENTS_MIN_BYTES)
        {
            return Err("agent_read_events_invalid_max_bytes");
        }
        if self.event_types.as_ref().is_some_and(|t| t.len() > 8) {
            return Err("agent_read_events_invalid_event_types");
        }
        Ok(())
    }

    #[must_use]
    pub fn effective_limit(&self) -> u32 {
        self.limit.unwrap_or(AGENT_READ_EVENTS_DEFAULT_LIMIT)
    }

    #[must_use]
    pub fn effective_max_bytes(&self) -> u32 {
        self.max_bytes
            .unwrap_or(AGENT_READ_EVENTS_DEFAULT_MAX_BYTES)
            .min(AGENT_READ_EVENTS_MAX_BYTES_CEILING)
    }
}

/// Character cap on `final_message.content` (larger than the per-event cap,
/// since the final message is the worker's result).
pub const FINAL_MESSAGE_MAX_CHARS: usize = 8_000;
/// Character cap for an explicitly requested full final message.
pub const FINAL_MESSAGE_FULL_MAX_CHARS: usize = 32 * 1024;

/// One bounded event projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentSessionEventV1 {
    pub sequence: i32,
    pub event_type: EventType,
    pub role: Option<Role>,
    pub tool_name: Option<String>,
    pub tool_use_id: Option<String>,
    pub content: String,
    /// True when `content`, `tool_input` or `metadata` was clipped.
    pub truncated: bool,
    pub tool_input: Option<serde_json::Value>,
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    /// True for a daemon-recorded provider diagnostic (provider stderr/stdout
    /// or a provider-event error), which is not the agent's own output. It
    /// never counts as the session's `final_message` (#1089).
    #[serde(default)]
    pub provider_diagnostic: bool,
}

/// Metadata key marking a daemon-recorded provider diagnostic event.
pub const PROVIDER_DIAGNOSTIC_METADATA_KEY: &str = "provider_diagnostic";

/// Metadata for a provider diagnostic event: the marker, the provider output
/// `source` (`stderr`, `stdout`, `codex_event`, ...), and whether it ended the
/// turn as a terminal provider error.
#[must_use]
pub fn provider_diagnostic_metadata(source: &str, terminal: bool) -> serde_json::Value {
    serde_json::json!({
        PROVIDER_DIAGNOSTIC_METADATA_KEY: true,
        "source": source,
        "terminal": terminal,
    })
}

/// Whether event metadata carries the provider-diagnostic marker.
#[must_use]
pub fn is_provider_diagnostic_metadata(metadata: Option<&serde_json::Value>) -> bool {
    metadata
        .and_then(|value| value.get(PROVIDER_DIAGNOSTIC_METADATA_KEY))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// The last assistant message of the tip session: the agent's own last
/// assistant `Message`, never a provider diagnostic. The cap on `content` is
/// [`FINAL_MESSAGE_MAX_CHARS`]; `truncated` is set when it was clipped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentFinalMessageV1 {
    pub sequence: i32,
    pub content: String,
    pub truncated: bool,
    pub created_at: DateTime<Utc>,
}

/// Result of `AgentReadSessionEvents`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentReadSessionEventsResultV1 {
    pub session_id: Uuid,
    /// The rotation tip of the target's lineage (equals `session_id` when
    /// the session was never rotated). `events` are read from `session_id`;
    /// `status`, `terminal_reason` and `final_message` describe the tip.
    pub tip_session_id: Uuid,
    pub status: SessionStatus,
    /// The tip's recorded `stop_reason`, when it has ended.
    pub terminal_reason: Option<String>,
    pub events: Vec<AgentSessionEventV1>,
    /// Sequence to pass as `after_sequence` for the next forward page.
    pub next_after_sequence: Option<i32>,
    /// Matching events exist after the last returned one.
    pub has_more: bool,
    /// Matching events exist before the first returned one.
    pub has_earlier: bool,
    /// Something was clipped: a field, or the page by `max_bytes`.
    pub truncated: bool,
    pub final_message: Option<AgentFinalMessageV1>,
}

/// Clip `text` to at most `max_chars` characters on a char boundary.
/// Returns the clipped text and whether anything was removed.
#[must_use]
pub fn clip_chars(text: &str, max_chars: usize) -> (String, bool) {
    match text.char_indices().nth(max_chars) {
        Some((byte_index, _)) => (text[..byte_index].to_string(), true),
        None => (text.to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_rejects_caller_identity_and_bad_bounds() {
        let id = "5d73c05d-1040-49f7-92ab-0123456789ab";
        assert!(
            serde_json::from_value::<AgentReadSessionEventsRequestV1>(serde_json::json!({
                "session_id": id, "caller_session_id": id
            }))
            .is_err()
        );
        let ok: AgentReadSessionEventsRequestV1 =
            serde_json::from_value(serde_json::json!({"session_id": id})).unwrap();
        assert_eq!(ok.validate(), Ok(()));
        assert!(!ok.final_message_full);
        assert_eq!(ok.effective_limit(), AGENT_READ_EVENTS_DEFAULT_LIMIT);
        let mut bad = ok.clone();
        bad.limit = Some(0);
        assert!(bad.validate().is_err());
        bad.limit = Some(AGENT_READ_EVENTS_MAX_LIMIT + 1);
        assert!(bad.validate().is_err());
        bad.limit = None;
        bad.max_bytes = Some(10);
        assert!(bad.validate().is_err());
        bad.max_bytes = Some(u32::MAX);
        assert_eq!(bad.validate(), Ok(()));
        assert_eq!(
            bad.effective_max_bytes(),
            AGENT_READ_EVENTS_MAX_BYTES_CEILING
        );
    }

    #[test]
    fn clip_chars_respects_char_boundaries() {
        assert_eq!(clip_chars("héllo", 2), ("hé".to_string(), true));
        assert_eq!(clip_chars("hi", 2), ("hi".to_string(), false));
    }
}
