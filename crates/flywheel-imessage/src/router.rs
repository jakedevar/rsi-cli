//! Command router and dispatcher.
//!
//! Parses incoming message text against slash-command patterns
//! and routes plain text to mapped sessions.

use crate::chatdb::InboundMessage;
use crate::config::ImessageConfig;
use crate::daemon_client::{self, DaemonClient};
use crate::state::BridgeState;
use rsi_common::types::{Session, SessionStatus};
use uuid::Uuid;

/// Action to take after parsing an inbound message.
pub enum RouterAction {
    /// Launch a new session with the given query.
    LaunchSession {
        query: String,
        project_id: Option<Uuid>,
    },
    /// Continue an existing session.
    ContinueSession { session_id: Uuid, query: String },
    /// Answer a pending approval question.
    AnswerQuestion { session_id: Uuid, response: String },
    /// Interrupt a running session.
    InterruptSession { session_id: Uuid },
    /// List all sessions.
    ListSessions,
    /// Get status of a specific session or all active sessions.
    GetStatus { session_name: Option<String> },
    /// Send help text.
    SendHelp,
    /// Route plain text to the mapped session for this chat.
    RouteToMapped { text: String },
}

/// Parse an inbound message into a router action.
pub fn route(msg: &InboundMessage, _config: &ImessageConfig, _state: &BridgeState) -> RouterAction {
    let text = msg.text.trim();

    // Slash commands
    if let Some(rest) = text.strip_prefix('/') {
        let parts: Vec<&str> = rest.splitn(2, char::is_whitespace).collect();
        let cmd = parts[0].to_lowercase();
        let args = parts.get(1).map(|s| s.trim()).unwrap_or("");

        match cmd.as_str() {
            "new" | "n" => {
                if args.is_empty() {
                    return RouterAction::SendHelp;
                }
                // Check for -p project flag
                if let Some(rest) = args.strip_prefix("-p ") {
                    let project_parts: Vec<&str> = rest.splitn(2, char::is_whitespace).collect();
                    if project_parts.len() == 2 {
                        // project_parts[0] is project name — resolved later via RPC
                        return RouterAction::LaunchSession {
                            query: project_parts[1].to_string(),
                            project_id: None, // resolved at dispatch time
                        };
                    }
                }
                RouterAction::LaunchSession {
                    query: args.to_string(),
                    project_id: None,
                }
            }
            "list" | "ls" | "l" => RouterAction::ListSessions,
            "status" | "s" => {
                if args.is_empty() {
                    RouterAction::GetStatus { session_name: None }
                } else {
                    RouterAction::GetStatus {
                        session_name: Some(args.to_string()),
                    }
                }
            }
            "continue" | "c" => {
                let continue_parts: Vec<&str> = args.splitn(2, char::is_whitespace).collect();
                if continue_parts.len() < 2 {
                    return RouterAction::SendHelp;
                }
                // Try to parse as UUID first
                if let Ok(session_id) = Uuid::parse_str(continue_parts[0]) {
                    return RouterAction::ContinueSession {
                        session_id,
                        query: continue_parts[1].to_string(),
                    };
                }
                // Otherwise treat as session name — resolved later
                RouterAction::SendHelp
            }
            "interrupt" | "i" | "stop" => {
                if args.is_empty() {
                    return RouterAction::SendHelp;
                }
                if let Ok(session_id) = Uuid::parse_str(args) {
                    return RouterAction::InterruptSession { session_id };
                }
                RouterAction::SendHelp
            }
            "approve" | "a" => {
                let approve_parts: Vec<&str> = args.splitn(2, char::is_whitespace).collect();
                if approve_parts.is_empty() || approve_parts[0].is_empty() {
                    return RouterAction::SendHelp;
                }
                if let Ok(session_id) = Uuid::parse_str(approve_parts[0]) {
                    let response = approve_parts.get(1).unwrap_or(&"yes").to_string();
                    return RouterAction::AnswerQuestion {
                        session_id,
                        response,
                    };
                }
                // Try short prefix matching
                RouterAction::SendHelp
            }
            "help" | "h" | "?" => RouterAction::SendHelp,
            _ => RouterAction::SendHelp,
        }
    } else {
        // Plain text — route to mapped session
        RouterAction::RouteToMapped {
            text: text.to_string(),
        }
    }
}

/// Format a list of sessions for iMessage display.
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

/// Handle a /status command.
pub async fn handle_status(
    client: &mut DaemonClient,
    session_name: Option<&str>,
) -> daemon_client::Result<String> {
    let sessions = client.list_sessions().await?;

    if let Some(name) = session_name {
        // Try to find a matching session
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
        // Show summary of all active sessions
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

/// Resolve a session by name/ID prefix. Exact prefix match preferred, then substring.
fn resolve_session_by_name<'a>(sessions: &'a [Session], name: &str) -> Option<&'a Session> {
    // Try UUID prefix match first
    if let Some(session) = sessions.iter().find(|s| s.id.to_string().starts_with(name)) {
        return Some(session);
    }

    // Try exact title prefix match
    if let Some(session) = sessions.iter().find(|s| {
        s.title
            .as_deref()
            .is_some_and(|t| t.to_lowercase().starts_with(&name.to_lowercase()))
    }) {
        return Some(session);
    }

    // Try substring match
    sessions.iter().find(|s| {
        s.title
            .as_deref()
            .is_some_and(|t| t.to_lowercase().contains(&name.to_lowercase()))
    })
}

/// Generate help text for iMessage commands.
pub fn help_text() -> String {
    [
        "RSI iMessage Commands:",
        "",
        "/new <query> - Launch a new session",
        "/list - List sessions",
        "/status [name] - Session status",
        "/continue <id> <msg> - Continue session",
        "/interrupt <id> - Interrupt session",
        "/approve <id> [msg] - Approve pending action",
        "/help - Show this help",
        "",
        "Plain text goes to your current session.",
    ]
    .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chatdb::InboundMessage;
    use chrono::Utc;

    fn make_msg(text: &str) -> InboundMessage {
        InboundMessage {
            rowid: 1,
            text: text.to_string(),
            sender: "+15551234567".to_string(),
            chat_id: Some(1),
            chat_identifier: Some("chat1".to_string()),
            is_group: false,
            group_name: None,
            timestamp: Utc::now(),
        }
    }

    fn default_config() -> ImessageConfig {
        ImessageConfig::default()
    }

    fn make_state() -> BridgeState {
        BridgeState::new_test(std::path::PathBuf::from("/tmp/test"))
    }

    #[test]
    fn test_route_new_session() {
        let msg = make_msg("/new hello world");
        let config = default_config();
        let state = make_state();
        match route(&msg, &config, &state) {
            RouterAction::LaunchSession { query, .. } => assert_eq!(query, "hello world"),
            _ => panic!("Expected LaunchSession"),
        }
    }

    #[test]
    fn test_route_list() {
        let msg = make_msg("/list");
        let config = default_config();
        let state = make_state();
        matches!(route(&msg, &config, &state), RouterAction::ListSessions);
    }

    #[test]
    fn test_route_help() {
        let msg = make_msg("/help");
        let config = default_config();
        let state = make_state();
        matches!(route(&msg, &config, &state), RouterAction::SendHelp);
    }

    #[test]
    fn test_route_plain_text() {
        let msg = make_msg("just a regular message");
        let config = default_config();
        let state = make_state();
        match route(&msg, &config, &state) {
            RouterAction::RouteToMapped { text } => assert_eq!(text, "just a regular message"),
            _ => panic!("Expected RouteToMapped"),
        }
    }

    #[test]
    fn test_route_status_no_args() {
        let msg = make_msg("/status");
        let config = default_config();
        let state = make_state();
        match route(&msg, &config, &state) {
            RouterAction::GetStatus { session_name } => assert!(session_name.is_none()),
            _ => panic!("Expected GetStatus"),
        }
    }

    #[test]
    fn test_route_status_with_name() {
        let msg = make_msg("/status my-session");
        let config = default_config();
        let state = make_state();
        match route(&msg, &config, &state) {
            RouterAction::GetStatus { session_name } => {
                assert_eq!(session_name, Some("my-session".to_string()))
            }
            _ => panic!("Expected GetStatus"),
        }
    }

    #[test]
    fn test_format_session_list_empty() {
        assert_eq!(format_session_list(&[]), "No active sessions.");
    }

    #[test]
    fn test_help_text_not_empty() {
        let help = help_text();
        assert!(help.contains("/new"));
        assert!(help.contains("/list"));
        assert!(help.contains("/help"));
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
