//! MCP server definition and credential forms.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::App;
use crate::modalkit_types::LcAction;
use crate::types::OverlayState;
use rsi_common::mcp::{McpServerDefinition, McpServerSummary};

const FIELD_COUNT: usize = 6;

fn split_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

pub(crate) fn open_mcp_server_form(app: &mut App, editing: Option<&McpServerSummary>) {
    let (id, command, args, secret_env_names, working_dir, enabled, editing_index) = match editing {
        Some(summary) => (
            summary.definition.id.clone(),
            summary.definition.command.clone(),
            summary.definition.args.join(","),
            summary.definition.secret_env_names.join(","),
            summary.definition.working_dir.clone().unwrap_or_default(),
            summary.definition.enabled,
            None,
        ),
        None => (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            false,
            None,
        ),
    };
    app.overlay = OverlayState::McpServerForm {
        focused_field: 0,
        id,
        command,
        args,
        secret_env_names,
        working_dir,
        enabled,
        editing: editing_index,
    };
}

pub(super) fn handle_mcp_server_form_key(app: &mut App, key: KeyEvent) {
    let focused_field = match &app.overlay {
        OverlayState::McpServerForm { focused_field, .. } => *focused_field,
        _ => return,
    };
    // Standard editing: a real cursor and selection in the focused text field.
    if app.edit_field(key, |overlay| match overlay {
        OverlayState::McpServerForm {
            id,
            command,
            args,
            secret_env_names,
            working_dir,
            ..
        } => match focused_field {
            0 => Some(id),
            1 => Some(command),
            2 => Some(args),
            3 => Some(secret_env_names),
            4 => Some(working_dir),
            _ => None,
        },
        _ => None,
    }) != crate::field_edit::FieldKey::Ignored
    {
        return;
    }
    match key.code {
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => cycle_field(app, false),
        KeyCode::Tab => cycle_field(app, true),
        KeyCode::Enter => submit_mcp_server_form(app),
        KeyCode::Esc => app.overlay = OverlayState::None,
        KeyCode::Char(' ') if focused_field == 5 => toggle_enabled(app),
        KeyCode::Char(character) => {
            if let OverlayState::McpServerForm {
                focused_field,
                id,
                command,
                args,
                secret_env_names,
                working_dir,
                ..
            } = &mut app.overlay
            {
                match focused_field {
                    0 => id.push(character),
                    1 => command.push(character),
                    2 => args.push(character),
                    3 => secret_env_names.push(character),
                    4 => working_dir.push(character),
                    _ => {}
                }
            }
        }
        KeyCode::Backspace => {
            if let OverlayState::McpServerForm {
                focused_field,
                id,
                command,
                args,
                secret_env_names,
                working_dir,
                ..
            } = &mut app.overlay
            {
                match focused_field {
                    0 => id.pop(),
                    1 => command.pop(),
                    2 => args.pop(),
                    3 => secret_env_names.pop(),
                    4 => working_dir.pop(),
                    _ => None,
                };
            }
        }
        _ => {}
    }
}

fn cycle_field(app: &mut App, forward: bool) {
    if let OverlayState::McpServerForm { focused_field, .. } = &mut app.overlay {
        *focused_field = if forward {
            (*focused_field + 1) % FIELD_COUNT
        } else {
            (*focused_field + FIELD_COUNT - 1) % FIELD_COUNT
        };
    }
}

fn toggle_enabled(app: &mut App) {
    if let OverlayState::McpServerForm { enabled, .. } = &mut app.overlay {
        *enabled = !*enabled;
    }
}

fn submit_mcp_server_form(app: &mut App) {
    let (id, command, args, secret_env_names, working_dir, enabled) = match &app.overlay {
        OverlayState::McpServerForm {
            id,
            command,
            args,
            secret_env_names,
            working_dir,
            enabled,
            ..
        } => (
            id.clone(),
            command.clone(),
            args.clone(),
            secret_env_names.clone(),
            working_dir.clone(),
            *enabled,
        ),
        _ => return,
    };
    let server = McpServerDefinition {
        id,
        command,
        args: split_list(&args),
        secret_env_names: split_list(&secret_env_names),
        working_dir: (!working_dir.is_empty()).then_some(working_dir),
        enabled,
    };
    if let Err(error) = rsi_common::mcp::validate_mcp_server_definition(&server) {
        app.notify_error(error);
        return;
    }
    app.pending_lc_actions
        .push(LcAction::UpsertMcpServer { server });
    app.overlay = OverlayState::None;
}

pub(crate) fn open_mcp_server_secret_form(app: &mut App, id: String, rotate: bool) {
    app.overlay = OverlayState::McpServerSecretForm {
        id,
        rotate,
        secret: crate::types::McpSecretString::default(),
    };
}

pub(super) fn handle_mcp_server_secret_form_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => submit_mcp_server_secret_form(app),
        KeyCode::Esc => {
            if let OverlayState::McpServerSecretForm { secret, .. } = &mut app.overlay {
                *secret = crate::types::McpSecretString::default();
            }
            app.overlay = OverlayState::None;
        }
        KeyCode::Char(character) => {
            if let OverlayState::McpServerSecretForm { secret, .. } = &mut app.overlay {
                secret.push(character);
            }
        }
        KeyCode::Backspace => {
            if let OverlayState::McpServerSecretForm { secret, .. } = &mut app.overlay {
                secret.pop();
            }
        }
        _ => {}
    }
}

fn submit_mcp_server_secret_form(app: &mut App) {
    let (id, rotate, secret) = match &mut app.overlay {
        OverlayState::McpServerSecretForm { id, rotate, secret } => {
            (id.clone(), *rotate, secret.take())
        }
        _ => return,
    };
    if secret.expose().trim().is_empty() {
        app.notify_error("Credential cannot be empty");
        return;
    }
    app.pending_lc_actions.push(if rotate {
        LcAction::RotateMcpServerSecret { id, secret }
    } else {
        LcAction::SetMcpServerSecret { id, secret }
    });
    app.overlay = OverlayState::None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_secret(app: &mut App, value: &str) {
        for character in value.chars() {
            handle_mcp_server_secret_form_key(app, key(KeyCode::Char(character)));
        }
    }

    #[test]
    fn mcp_secret_submit_moves_secret_and_clears_overlay() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        open_mcp_server_secret_form(&mut app, "docs".to_string(), true);
        type_secret(&mut app, "mcp-secret-canary");
        handle_mcp_server_secret_form_key(&mut app, key(KeyCode::Enter));

        assert!(matches!(app.overlay, OverlayState::None));
        assert!(matches!(
            app.pending_lc_actions.last(),
            Some(LcAction::RotateMcpServerSecret { id, secret })
                if id == "docs" && secret.expose() == "mcp-secret-canary"
        ));
    }

    #[test]
    fn mcp_secret_debug_is_redacted() {
        let secret = crate::types::McpSecretString::new("mcp-secret-canary".to_string());
        let formatted = format!("{secret:?}");

        assert!(formatted.contains("<redacted>"));
        assert!(!formatted.contains("mcp-secret-canary"));
    }

    #[test]
    fn mcp_secret_cancel_scrubs_and_clears_overlay() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        open_mcp_server_secret_form(&mut app, "docs".to_string(), false);
        type_secret(&mut app, "mcp-secret-canary");
        handle_mcp_server_secret_form_key(&mut app, key(KeyCode::Esc));

        assert!(matches!(app.overlay, OverlayState::None));
        assert!(app.pending_lc_actions.is_empty());
    }

    #[test]
    fn mcp_definition_form_shows_daemon_validation_error_without_input_echo() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        open_mcp_server_form(&mut app, None);
        handle_mcp_server_form_key(&mut app, key(KeyCode::Char('I')));
        handle_mcp_server_form_key(&mut app, key(KeyCode::Tab));
        handle_mcp_server_form_key(&mut app, key(KeyCode::Enter));

        let latest_error = app.notifications.iter().next_back();
        assert!(matches!(
            latest_error,
            Some(notification)
                if notification.kind == crate::types::NotificationKind::OperationFailed
                    && notification.message
                        == "mcp server id must be canonical lowercase ascii"
        ));
        assert!(
            latest_error
                .map(|notification| !notification.message.contains('I'))
                .unwrap_or(false)
        );
    }
}
