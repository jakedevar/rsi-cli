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

/// What a conversation item is. Old clients that ignore the field see every
/// item as a `message`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKindV2 {
    /// A user- or assistant-role message (the only kind before #3797).
    #[default]
    Message,
    /// The manager started a tool call; `tool_use_id` is its call id.
    ToolUse,
    /// The result of the tool call with the same `tool_use_id`.
    ToolResult,
}

impl ItemKindV2 {
    fn is_message(&self) -> bool {
        *self == Self::Message
    }
}

/// Longest tool name or call id in a conversation item, in bytes. Longer
/// values are omitted rather than cut, so a call id is always exact.
pub const CONVERSATION_TOOL_FIELD_BYTES: usize = 256;

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
    /// `message`, `tool_use` or `tool_result` (#3797). Absent in older
    /// responses, which carried messages only.
    #[serde(default, skip_serializing_if = "ItemKindV2::is_message")]
    pub kind: ItemKindV2,
    /// The tool of a tool item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    /// The provider call id of a tool item: a `tool_use` and its `tool_result`
    /// share one. Absent when the stored id is missing or over
    /// [`CONVERSATION_TOOL_FIELD_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_id: Option<String>,
    /// The `idempotency_key` the phone sent with the operator message this
    /// item delivered. Absent for a message queued elsewhere (the TUI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<Uuid>,
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
    /// The `idempotency_key` the phone sent with this message, so its outbox
    /// can mark the entry delivered. Absent for a message queued elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<Uuid>,
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

/// `RemoteGetDashboardV2` (read, hub #3287): the satellites the hub polls and
/// its running agents per provider and effort. Labels and counts only.
pub const METHOD_DASHBOARD: &str = "RemoteGetDashboardV2";
/// Most satellites one dashboard lists.
pub const DASHBOARD_MAX_SATELLITES: usize = 32;
/// Most satellite peer rows one dashboard reads before it orders them by
/// label: the registry's own peer limit, so the order is exact.
pub const DASHBOARD_SATELLITE_SOURCE_ROWS: usize = 128;
/// Most running agents one dashboard counts. The read stops after one more
/// row; past the cap it reports `agents_incomplete` instead of a full count.
pub const DASHBOARD_MAX_RUNNING_AGENTS: usize = 1024;
/// Most `(provider, effort)` groups one dashboard lists.
pub const DASHBOARD_MAX_AGENT_GROUPS: usize = 32;
/// Longest satellite label, provider or effort string, in bytes.
pub const DASHBOARD_LABEL_BYTES: usize = 128;
/// Longest build SHA (a git commit id is 40 or 64 hex characters).
pub const DASHBOARD_BUILD_SHA_BYTES: usize = 64;

/// The dashboard request has no parameters: it is not project-scoped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardRequestV2 {}

/// One satellite the hub polls (enabled, read-enabled, paired).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardSatelliteV2 {
    /// The operator's peer label; no host, address or link detail.
    pub label: String,
    /// The hub's latest poll observed the satellite healthy.
    pub reachable: bool,
    /// The daemon build the satellite last reported, `null` until the hub
    /// has received a report since it started.
    pub build_sha: Option<String>,
    /// RFC3339 with nanoseconds: the hub's last good poll, `null` if never.
    pub last_seen_at: Option<String>,
    /// Running plus waiting-approval sessions the satellite last reported,
    /// `null` until the hub has received a report since it started.
    pub running_agents: Option<u32>,
}

/// Running agents of one provider at one effort.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardAgentGroupV2 {
    /// The session provider (`Claude`, `Codex`, ...).
    pub provider: String,
    /// The session effort, `null` when none is set.
    pub effort: Option<String>,
    pub running: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardResponseV2 {
    /// RFC3339 with nanoseconds: when the daemon read the dashboard.
    pub observed_at: String,
    /// In label order.
    pub satellites: Vec<DashboardSatelliteV2>,
    /// True when more satellites exist than [`DASHBOARD_MAX_SATELLITES`].
    pub satellites_truncated: bool,
    /// Largest group first, then by provider and effort.
    pub agents: Vec<DashboardAgentGroupV2>,
    /// Every running agent the read counted, including groups cut by the
    /// group cap. Exactly [`DASHBOARD_MAX_RUNNING_AGENTS`] when
    /// `agents_incomplete` is true.
    pub agents_total: u32,
    /// True when more groups exist than [`DASHBOARD_MAX_AGENT_GROUPS`].
    pub agents_truncated: bool,
    /// True when more running agents exist than
    /// [`DASHBOARD_MAX_RUNNING_AGENTS`]: the read stopped at the cap, so
    /// `agents_total` and every group count are lower bounds ("more than N").
    pub agents_incomplete: bool,
}

/// `RemoteGetProvidersV2` (read, hub #3796): the usage dashboard of the RSI
/// phone app. One bounded row per provider with its plan windows, overage
/// posture and 1h/24h spend. Labels and numbers only: no account, key, URL,
/// host path or raw provider error text.
pub const METHOD_PROVIDERS: &str = "RemoteGetProvidersV2";
/// Most provider rows one response lists.
pub const PROVIDERS_MAX: usize = 16;
/// Most plan windows one provider lists.
pub const PROVIDER_WINDOWS_MAX: usize = 8;
/// Most per-model spend rows one provider lists (largest 24h cost first).
pub const PROVIDER_MODELS_MAX: usize = 8;
/// Longest label (provider, window key, status, model, reason) in bytes.
pub const PROVIDER_LABEL_BYTES: usize = 64;
/// Most rate-limit rows one read examines. The table is keyed by
/// `(provider, window_key)`, so a larger table is not a legitimate state; the
/// read stops one row past this and reports `windows_incomplete`.
pub const PROVIDERS_WINDOW_SOURCE_ROWS: usize = 256;

/// The providers request has no parameters: it is not project-scoped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersRequestV2 {}

/// One plan window of a provider (for Claude `five_hour`, `seven_day`, ...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderWindowV2 {
    /// The provider's own window key.
    pub key: String,
    /// Fraction of the window consumed, finite, `0.0..=1.0`.
    pub utilization: f64,
    /// RFC3339 with nanoseconds: when the window resets, `null` if the
    /// provider reported no reset time.
    pub resets_at: Option<String>,
    /// The same instant as the provider's unix epoch seconds.
    pub resets_at_epoch: Option<i64>,
}

/// Spend of one provider over the last hour and 24 hours.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSpendV2 {
    pub cost_1h_usd: f64,
    pub cost_24h_usd: f64,
}

/// Spend of one model of a provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderModelSpendV2 {
    pub model: String,
    pub cost_1h_usd: f64,
    pub cost_24h_usd: f64,
}

/// One provider. Absent data is explicit: a `null` block carries a `*_reason`
/// code beside it, never a guessed value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderRowV2 {
    /// The session provider (`Claude`, `Codex`, ...).
    pub provider: String,
    /// The provider's own verdict for its last request (`allowed`, ...).
    pub status: Option<String>,
    /// The window the provider currently considers binding.
    pub binding_window: Option<String>,
    pub overage_status: Option<String>,
    pub is_using_overage: bool,
    /// RFC3339 with nanoseconds: when the hub last observed the windows,
    /// `null` when it never has.
    pub observed_at: Option<String>,
    /// Set when the latest refresh failed, so the windows are the last known
    /// values (a stable code such as `probe_timeout`).
    pub stale_reason: Option<String>,
    /// In key order, at most [`PROVIDER_WINDOWS_MAX`].
    pub windows: Vec<ProviderWindowV2>,
    /// Why `windows` is empty: `no_observation`.
    pub windows_reason: Option<String>,
    /// True when more windows exist than [`PROVIDER_WINDOWS_MAX`].
    pub windows_truncated: bool,
    /// `null` when the spend is not known, with `spend_reason`.
    pub spend: Option<ProviderSpendV2>,
    /// Why `spend` is `null`: `warming`, `stale`, `unavailable` or `truncated`.
    pub spend_reason: Option<String>,
    /// Largest 24h cost first, at most [`PROVIDER_MODELS_MAX`].
    pub models: Vec<ProviderModelSpendV2>,
    /// True when more models exist than [`PROVIDER_MODELS_MAX`].
    pub models_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProvidersResponseV2 {
    /// RFC3339 with nanoseconds: when the daemon read the providers.
    pub observed_at: String,
    /// Canonical provider order, at most [`PROVIDERS_MAX`]; every provider
    /// appears, including one with no data.
    pub providers: Vec<ProviderRowV2>,
    pub providers_truncated: bool,
    /// True when the rate-limit read hit [`PROVIDERS_WINDOW_SOURCE_ROWS`]: some
    /// windows are missing.
    pub windows_incomplete: bool,
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

#[cfg(test)]
mod tool_item_tests {
    use super::*;

    /// A response from before #3797 (messages only, no new fields) still
    /// parses, and a message item serializes exactly as before.
    #[test]
    fn a_pre_tool_item_parses_as_a_message_and_serializes_unchanged() {
        let old = serde_json::json!({
            "id": "904", "sequence": 4, "speaker": "manager", "text": "hi",
            "truncated": false, "created_at": "2026-10-17T09:24:00.000000000Z",
            "operator_message_id": null,
        });
        let item: ConversationItemV2 = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(item.kind, ItemKindV2::Message);
        assert_eq!(item.tool_use_id, None);
        assert_eq!(serde_json::to_value(&item).unwrap(), old);

        let tool = ConversationItemV2 {
            kind: ItemKindV2::ToolResult,
            tool_name: Some("Bash".into()),
            tool_use_id: Some("toolu_01".into()),
            ..item
        };
        let wire = serde_json::to_value(&tool).unwrap();
        assert_eq!(wire["kind"], "tool_result");
        assert_eq!(wire["tool_use_id"], "toolu_01");
    }
}
