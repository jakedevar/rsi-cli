//! Signal/iMessage bridge connection form input handling.

use crate::app::App;
use crate::settings::{
    MessageBridgeKind, MessageBridgeSettings, load_message_bridge_config,
    write_message_bridge_config,
};
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub fn open_message_bridge_form(app: &mut App, bridge: MessageBridgeKind) {
    let settings = match load_message_bridge_config(bridge) {
        Ok(Some(settings)) => settings,
        Ok(None) => app.settings.message_bridges.get(bridge).clone(),
        Err(e) => {
            app.notify(format!("Using saved settings; {}", e));
            app.settings.message_bridges.get(bridge).clone()
        }
    };
    app.overlay = OverlayState::MessageBridgeForm {
        bridge,
        focused_field: 0,
        enabled: settings.enabled,
        account: settings.account,
        allow_from: settings.allow_from,
        working_dir: settings.working_dir,
    };
}

pub(super) fn handle_message_bridge_form_key(app: &mut App, key: KeyEvent) {
    let (bridge, focused_field) = match &app.overlay {
        OverlayState::MessageBridgeForm {
            bridge,
            focused_field,
            ..
        } => (*bridge, *focused_field),
        _ => return,
    };

    let field_count = field_count(bridge);
    match key.code {
        KeyCode::BackTab => focus_prev(app, field_count),
        KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => focus_prev(app, field_count),
        KeyCode::Tab => focus_next(app, field_count),
        KeyCode::Esc => app.overlay = OverlayState::None,
        KeyCode::Char(' ') if focused_field == 0 => toggle_enabled(app),
        KeyCode::Enter => submit_message_bridge_form(app),
        KeyCode::Backspace => edit_focused_text(app, bridge, focused_field, TextEdit::Backspace),
        KeyCode::Char(c) => edit_focused_text(app, bridge, focused_field, TextEdit::Push(c)),
        _ => {}
    }
}

fn field_count(bridge: MessageBridgeKind) -> usize {
    match bridge {
        MessageBridgeKind::Signal => 4,
        MessageBridgeKind::Imessage => 3,
    }
}

fn focus_prev(app: &mut App, field_count: usize) {
    if let OverlayState::MessageBridgeForm { focused_field, .. } = &mut app.overlay {
        *focused_field = (*focused_field + field_count - 1) % field_count;
    }
}

fn focus_next(app: &mut App, field_count: usize) {
    if let OverlayState::MessageBridgeForm { focused_field, .. } = &mut app.overlay {
        *focused_field = (*focused_field + 1) % field_count;
    }
}

fn toggle_enabled(app: &mut App) {
    if let OverlayState::MessageBridgeForm { enabled, .. } = &mut app.overlay {
        *enabled = !*enabled;
    }
}

enum TextEdit {
    Push(char),
    Backspace,
}

fn edit_focused_text(
    app: &mut App,
    bridge: MessageBridgeKind,
    focused_field: usize,
    edit: TextEdit,
) {
    let OverlayState::MessageBridgeForm {
        account,
        allow_from,
        working_dir,
        ..
    } = &mut app.overlay
    else {
        return;
    };

    let target = match (bridge, focused_field) {
        (MessageBridgeKind::Signal, 1) => Some(account),
        (MessageBridgeKind::Signal, 2) | (MessageBridgeKind::Imessage, 1) => Some(allow_from),
        (MessageBridgeKind::Signal, 3) | (MessageBridgeKind::Imessage, 2) => Some(working_dir),
        _ => None,
    };

    if let Some(text) = target {
        match edit {
            TextEdit::Push(c) => text.push(c),
            TextEdit::Backspace => {
                text.pop();
            }
        }
    }
}

fn submit_message_bridge_form(app: &mut App) {
    let (bridge, settings) = match &app.overlay {
        OverlayState::MessageBridgeForm {
            bridge,
            enabled,
            account,
            allow_from,
            working_dir,
            ..
        } => (
            *bridge,
            MessageBridgeSettings {
                enabled: *enabled,
                account: account.trim().to_string(),
                allow_from: allow_from.trim().to_string(),
                working_dir: working_dir.trim().to_string(),
            },
        ),
        _ => return,
    };

    if bridge == MessageBridgeKind::Signal && settings.account.is_empty() {
        app.notify("Signal account required");
        return;
    }
    if settings.working_dir.is_empty() {
        app.notify("Working directory required");
        return;
    }

    *app.settings.message_bridges.get_mut(bridge) = settings.clone();
    match write_message_bridge_config(bridge, &settings) {
        Ok(path) => {
            app.overlay = OverlayState::None;
            app.notify_success(format!(
                "Saved {} config: {}",
                bridge.label(),
                path.display()
            ));
        }
        Err(e) => app.notify(format!("Failed to save {} config: {}", bridge.label(), e)),
    }
}
