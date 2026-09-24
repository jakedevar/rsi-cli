//! Poll controller: daemon connection state and poll phase management.

/// Tracks the current step of a simplified poll cycle.
/// ENHANCED: Eliminated ListSessions, ListProjects, ListLabels phases.
/// Navigation data is now handled by event-driven effects and manual refresh.
/// Each variant represents one RPC round-trip worth of work,
/// allowing render frames to fire between steps instead of
/// blocking the entire event loop for all RPC calls at once.
pub enum PollPhase {
    /// Reconnect to daemon if disconnected.
    Connect,
    /// Fetch conversation events for actively running sessions only.
    /// No longer fetches navigation data (sessions/projects/labels).
    FetchConversations { ids: Vec<uuid::Uuid>, index: usize },
}

/// Connection and capability state for the daemon poll loop.
///
/// Owns the two boolean fields that gate all poll work. Extracted from `App`
/// so poll logic has a narrowly-scoped owner that can be tested independently.
#[derive(Debug)]
pub struct PollController {
    /// Whether the daemon socket is currently connected.
    pub connected: bool,
    /// Whether the essential startup/reconnect handshake is currently running.
    /// A connected socket alone does not imply configuration readiness.
    pub bootstrap_in_flight: bool,
    /// True only after this TUI process applies a successful GetDaemonConfig.
    pub authoritative_config_ready: bool,
    /// True only after this TUI process applies a successful ListSessions snapshot.
    pub sessions_authoritative: bool,
    /// Whether the daemon supports the batched `GetConversationsSince` RPC.
    /// Negotiated via `GetDaemonCapabilities` on connect; defaults to `true`
    /// so older daemons without the RPC still work through fallback.
    pub batch_fetch_supported: bool,
    /// Whether the daemon supports `Subscribe` RPC for push notifications.
    pub push_supported: bool,
    /// Whether the daemon supports memory search RPC methods.
    pub memory_search_supported: bool,
    /// Whether the daemon supports sandbox isolation (Phase 2+).
    /// Gates the `s` toggle in the prompt overlay and the sandbox indicator in session rows.
    pub sandbox_supported: bool,
}

impl Default for PollController {
    fn default() -> Self {
        Self {
            connected: false,
            bootstrap_in_flight: false,
            authoritative_config_ready: false,
            sessions_authoritative: false,
            batch_fetch_supported: true,
            push_supported: false,
            memory_search_supported: false,
            sandbox_supported: false,
        }
    }
}
