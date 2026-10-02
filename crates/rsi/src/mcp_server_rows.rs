//! Settings row formatting for operator-configured MCP servers.

use crate::app::App;
use rsi_common::mcp::{McpCredentialState, McpServerSummary};

fn state_label(state: &McpCredentialState) -> &'static str {
    match state {
        McpCredentialState::Vault => "vault",
        McpCredentialState::Cleared => "cleared",
        McpCredentialState::Absent => "absent",
    }
}

pub(crate) fn format_server(summary: &McpServerSummary) -> String {
    let definition = &summary.definition;
    [
        format!("enabled:{}", if definition.enabled { "on" } else { "off" }),
        format!("command:{}", definition.command),
        format!("args:{}", definition.args.len()),
        format!("secrets:{}", definition.secret_env_names.join(",")),
        format!("state:{}", state_label(&summary.credential.state)),
        format!(
            "fp:{}",
            summary.credential.fingerprint.as_deref().unwrap_or("-")
        ),
        format!("generation:{}", summary.credential.generation),
    ]
    .join("  ")
}

pub(crate) fn mcp_server_rows(app: &App) -> Vec<(String, String)> {
    match &app.cached_mcp_servers {
        Some(list) => list
            .servers
            .iter()
            .map(|summary| (summary.definition.id.clone(), format_server(summary)))
            .collect(),
        None => vec![("MCP servers".to_string(), "loading…".to_string())],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::app_test_helpers::with_session_list;
    use chrono::TimeZone;

    #[test]
    fn mcp_row_renders_metadata_without_secret_bytes() {
        let mut app = with_session_list(0);
        app.cached_mcp_servers = Some(rsi_common::mcp::ListMcpServersResult {
            servers: vec![rsi_common::mcp::McpServerSummary {
                definition: rsi_common::mcp::McpServerDefinition {
                    id: "docs".to_string(),
                    command: "/usr/bin/mcp".to_string(),
                    args: vec!["serve".to_string(), "--verbose".to_string()],
                    secret_env_names: vec!["MCP_DOCS_TOKEN".to_string()],
                    working_dir: None,
                    enabled: false,
                },
                credential: rsi_common::mcp::McpCredentialMetadata {
                    id: "docs".to_string(),
                    state: rsi_common::mcp::McpCredentialState::Vault,
                    fingerprint: Some("abc123".to_string()),
                    set_at: Some(chrono::Utc.timestamp_opt(0, 0).unwrap()),
                    rotated_from_fingerprint: None,
                    cleared_at: None,
                    generation: 7,
                },
            }],
        });

        let rows = mcp_server_rows(&app);
        assert_eq!(rows[0].0, "docs");
        assert!(rows[0].1.contains("enabled:off"));
        assert!(rows[0].1.contains("command:/usr/bin/mcp"));
        assert!(rows[0].1.contains("args:2"));
        assert!(rows[0].1.contains("secrets:MCP_DOCS_TOKEN"));
        assert!(rows[0].1.contains("state:vault"));
        assert!(rows[0].1.contains("fp:abc123"));
        assert!(rows[0].1.contains("generation:7"));
    }
}
