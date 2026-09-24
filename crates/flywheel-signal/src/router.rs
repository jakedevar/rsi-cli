//! Command router and dispatcher.
//!
//! Parses inbound Signal text with the `!` command prefix and routes plain
//! text to mapped sessions. v1 surface is intentionally narrower than the
//! iMessage bridge: `!new`, `!stop`, `!status`, `!help`, plus plain-text
//! routing. UUID-targeted commands (`!interrupt <uuid>`, `!approve`, etc.)
//! are deferred to a follow-up ticket.

use crate::config::SignalConfig;
use crate::daemon_client::{self, DaemonClient};
use crate::signal_cli::InboundMessage;
use crate::state::BridgeState;
use rsi_common::types::{Session, SessionStatus};
use uuid::Uuid;

/// Action to take after parsing an inbound message.
pub enum RouterAction {
    /// Start a new session with the given query.
    LaunchSession {
        query: String,
        project_id: Option<Uuid>,
    },
    /// Continue the active session for this chat with a plain-text follow-up.
    #[allow(dead_code)]
    ContinueSession { session_id: Uuid, query: String },
    /// Interrupt the active session for this chat (`!stop`).
    InterruptSession { session_id: Uuid },
    /// Report status of the active session for this chat (`!status`).
    GetStatus { session_id: Option<Uuid> },
    /// Reply with the help text (`!help` / unknown `!cmd`).
    SendHelp,
    /// Plain text with an active session — route to it; with no active session — launch.
    RouteToMapped { text: String },
}

/// Parse an inbound message into a router action.
pub fn route(msg: &InboundMessage, _config: &SignalConfig, state: &BridgeState) -> RouterAction {
    let text = msg.text.trim();

    if let Some(rest) = text.strip_prefix('!') {
        let parts: Vec<&str> = rest.splitn(2, char::is_whitespace).collect();
        let cmd = parts[0].to_lowercase();
        let args = parts.get(1).map(|s| s.trim()).unwrap_or("");

        match cmd.as_str() {
            "new" | "n" => {
                if args.is_empty() {
                    return RouterAction::SendHelp;
                }
                RouterAction::LaunchSession {
                    query: args.to_string(),
                    project_id: None,
                }
            }
            // `!s` is shorthand for `!stop` (not `!status`) — disambiguation per plan.
            "stop" | "s" => match state.get_session_for_sender(&msg.sender) {
                Some(sid) => RouterAction::InterruptSession { session_id: sid },
                None => RouterAction::SendHelp,
            },
            "status" => RouterAction::GetStatus {
                session_id: state.get_session_for_sender(&msg.sender),
            },
            "help" | "h" | "?" => RouterAction::SendHelp,
            _ => RouterAction::SendHelp,
        }
    } else {
        RouterAction::RouteToMapped {
            text: text.to_string(),
        }
    }
}

/// Format a list of sessions for Signal display (utility, unused by v1 router but
/// carried forward for parity with the iMessage bridge / future `!list`).
#[allow(dead_code)]
pub fn format_session_list(sessions: &[Session]) -> String {
    if sessions.is_empty() {
        return "No active sessions.".to_string();
    }

    let mut lines = Vec::new();
    let active: Vec<&Session> = sessions
        .iter()
        .filter(|s| !s.status.is_terminal())
        .collect();
    let recent_terminal: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.status.is_terminal())
        .take(5)
        .collect();

    if !active.is_empty() {
        lines.push("Active:".to_string());
        for s in &active {
            lines.push(format_session_line(s));
        }
    }

    if !recent_terminal.is_empty() {
        if !active.is_empty() {
            lines.push(String::new());
        }
        lines.push("Recent:".to_string());
        for s in &recent_terminal {
            lines.push(format_session_line(s));
        }
    }

    lines.join("\n")
}

fn format_session_line(s: &Session) -> String {
    let status_icon = match s.status {
        SessionStatus::Starting => "...",
        SessionStatus::Running => ">>>",
        SessionStatus::WaitingApproval => "???",
        SessionStatus::Completed => "OK",
        SessionStatus::Failed => "ERR",
        SessionStatus::Interrupted => "INT",
        SessionStatus::Archived => "ARC",
        SessionStatus::Deleted => "DEL",
        _ => "?",
    };

    let title = s
        .title
        .as_deref()
        .unwrap_or_else(|| truncate_query(&s.query, 40));
    let model = s.model.as_deref().unwrap_or("?");

    format!(
        "[{}] {} ({}) - {}",
        status_icon,
        title,
        model,
        &s.id.to_string()[..8]
    )
}

fn truncate_query(q: &str, max_len: usize) -> &str {
    if q.len() <= max_len {
        q
    } else {
        let mut end = max_len;
        while !q.is_char_boundary(end) {
            end -= 1;
        }
        &q[..end]
    }
}

/// Handle a `!status` command. If a `session_name` is provided, resolve it;
/// otherwise summarize active sessions.
pub async fn handle_status(
    client: &mut DaemonClient,
    session_name: Option<&str>,
) -> daemon_client::Result<String> {
    let sessions = client.list_sessions().await?;

    if let Some(name) = session_name {
        if let Some(session) = resolve_session_by_name(&sessions, name) {
            let events = client
                .get_conversation(session.id, None)
                .await
                .unwrap_or_default();
            let last_events: Vec<_> = events.iter().rev().take(3).collect();

            let mut lines = vec![format!(
                "Session: {} ({})",
                session.title.as_deref().unwrap_or(&session.query),
                session.id
            )];
            lines.push(format!(
                "Status: {:?} | Model: {} | Turns: {}",
                session.status,
                session.model.as_deref().unwrap_or("?"),
                session.num_turns.unwrap_or(0)
            ));

            if !last_events.is_empty() {
                lines.push("Last events:".to_string());
                for event in last_events.iter().rev() {
                    let preview: String = event.content.chars().take(100).collect();
                    lines.push(format!(
                        "  [{}] {}",
                        event
                            .role
                            .map(|r| format!("{:?}", r))
                            .unwrap_or_else(|| "?".to_string()),
                        preview
                    ));
                }
            }

            Ok(lines.join("\n"))
        } else {
            Ok(format!("No session matching '{}'", name))
        }
    } else {
        let active: Vec<&Session> = sessions
            .iter()
            .filter(|s| !s.status.is_terminal())
            .collect();
        if active.is_empty() {
            Ok("No active sessions.".to_string())
        } else {
            let lines: Vec<String> = active.iter().map(|s| format_session_line(s)).collect();
            Ok(lines.join("\n"))
        }
    }
}

/// Resolve a session by name/ID prefix. UUID prefix match preferred, then title prefix,
/// then title substring match.
fn resolve_session_by_name<'a>(sessions: &'a [Session], name: &str) -> Option<&'a Session> {
    if let Some(session) = sessions.iter().find(|s| s.id.to_string().starts_with(name)) {
        return Some(session);
    }

    if let Some(session) = sessions.iter().find(|s| {
        s.title
            .as_deref()
            .is_some_and(|t| t.to_lowercase().starts_with(&name.to_lowercase()))
    }) {
        return Some(session);
    }

    sessions.iter().find(|s| {
        s.title
            .as_deref()
            .is_some_and(|t| t.to_lowercase().contains(&name.to_lowercase()))
    })
}

/// Generate help text for Signal commands.
pub fn help_text() -> String {
    [
        "RSI Signal Commands:",
        "",
        "!new <query>  – Launch a new session",
        "!stop         – Interrupt the active session for this chat",
        "!status       – Show the active session's status",
        "!help         – Show this help",
        "",
        "Plain text routes to your active session (launches one if none is active).",
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_msg(text: &str) -> InboundMessage {
        InboundMessage {
            timestamp_ms: 1713542400000,
            sender: "+15551234567".to_string(),
            text: text.to_string(),
            is_sync: false,
        }
    }

    fn default_config() -> SignalConfig {
        SignalConfig {
            account: "+15559999999".to_string(),
            ..Default::default()
        }
    }

    fn make_state_with_active(sender: &str, sid: Uuid) -> BridgeState {
        let mut s = BridgeState::new_test(PathBuf::from("/tmp/test-sig-router.json"));
        s.map_sender_to_session(sender, sid);
        s
    }

    #[test]
    fn test_route_new_with_query() {
        let msg = make_msg("!new hello world");
        let state = BridgeState::new_test(PathBuf::from("/tmp/t"));
        match route(&msg, &default_config(), &state) {
            RouterAction::LaunchSession { query, project_id } => {
                assert_eq!(query, "hello world");
                assert!(project_id.is_none());
            }
            _ => panic!("Expected LaunchSession"),
        }
    }

    #[test]
    fn test_route_new_short_alias() {
        let msg = make_msg("!n do something");
        let state = BridgeState::new_test(PathBuf::from("/tmp/t"));
        match route(&msg, &default_config(), &state) {
            RouterAction::LaunchSession { query, .. } => {
                assert_eq!(query, "do something");
            }
            _ => panic!("Expected LaunchSession"),
        }
    }

    #[test]
    fn test_route_new_without_args_yields_help() {
        let msg = make_msg("!new");
        let state = BridgeState::new_test(PathBuf::from("/tmp/t"));
        assert!(matches!(
            route(&msg, &default_config(), &state),
            RouterAction::SendHelp
        ));
    }

    #[test]
    fn test_route_stop_with_active() {
        let sid = Uuid::new_v4();
        let state = make_state_with_active("+15551234567", sid);
        let msg = make_msg("!stop");
        match route(&msg, &default_config(), &state) {
            RouterAction::InterruptSession { session_id } => assert_eq!(session_id, sid),
            _ => panic!("Expected InterruptSession"),
        }
    }

    #[test]
    fn test_route_stop_short_alias_with_active() {
        let sid = Uuid::new_v4();
        let state = make_state_with_active("+15551234567", sid);
        let msg = make_msg("!s");
        match route(&msg, &default_config(), &state) {
            RouterAction::InterruptSession { session_id } => assert_eq!(session_id, sid),
            _ => panic!("Expected InterruptSession"),
        }
    }

    #[test]
    fn test_route_stop_no_active_yields_help() {
        let state = BridgeState::new_test(PathBuf::from("/tmp/t"));
        let msg = make_msg("!stop");
        assert!(matches!(
            route(&msg, &default_config(), &state),
            RouterAction::SendHelp
        ));
    }

    #[test]
    fn test_route_status_no_active() {
        let state = BridgeState::new_test(PathBuf::from("/tmp/t"));
        let msg = make_msg("!status");
        match route(&msg, &default_config(), &state) {
            RouterAction::GetStatus { session_id } => assert!(session_id.is_none()),
            _ => panic!("Expected GetStatus"),
        }
    }

    #[test]
    fn test_route_status_with_active() {
        let sid = Uuid::new_v4();
        let state = make_state_with_active("+15551234567", sid);
        let msg = make_msg("!status");
        match route(&msg, &default_config(), &state) {
            RouterAction::GetStatus { session_id } => assert_eq!(session_id, Some(sid)),
            _ => panic!("Expected GetStatus"),
        }
    }

    #[test]
    fn test_route_help_variants() {
        let state = BridgeState::new_test(PathBuf::from("/tmp/t"));
        for cmd in ["!help", "!h", "!?"] {
            let msg = make_msg(cmd);
            assert!(matches!(
                route(&msg, &default_config(), &state),
                RouterAction::SendHelp
            ));
        }
    }

    #[test]
    fn test_route_unknown_command_yields_help() {
        let state = BridgeState::new_test(PathBuf::from("/tmp/t"));
        let msg = make_msg("!xyz whatever");
        assert!(matches!(
            route(&msg, &default_config(), &state),
            RouterAction::SendHelp
        ));
    }

    #[test]
    fn test_route_plain_text() {
        let state = BridgeState::new_test(PathBuf::from("/tmp/t"));
        let msg = make_msg("just a regular message");
        match route(&msg, &default_config(), &state) {
            RouterAction::RouteToMapped { text } => {
                assert_eq!(text, "just a regular message")
            }
            _ => panic!("Expected RouteToMapped"),
        }
    }

    #[test]
    fn test_help_text_mentions_commands() {
        let help = help_text();
        assert!(help.contains("!new"));
        assert!(help.contains("!stop"));
        assert!(help.contains("!status"));
        assert!(help.contains("!help"));
    }

    #[test]
    fn test_format_session_list_empty() {
        assert_eq!(format_session_list(&[]), "No active sessions.");
    }

    #[test]
    fn test_truncate_query_does_not_split_utf8_codepoint() {
        // 38 ASCII bytes + a 4-byte emoji means byte index 40 (max_len)
        // lands squarely inside the emoji's codepoint, not on a boundary.
        let query = format!("{}😀 more text to pad out the string", "a".repeat(38));
        assert!(!query.is_char_boundary(40));

        let truncated = truncate_query(&query, 40);

        assert!(truncated.len() <= 40);
        assert!(query.is_char_boundary(truncated.len()));
        assert!(query.starts_with(truncated));
    }
}
