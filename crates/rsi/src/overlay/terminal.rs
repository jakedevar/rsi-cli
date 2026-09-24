//! Key handler for the embedded terminal overlay.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::App;
use crate::types::OverlayState;

/// Handle keys in the terminal overlay.
pub(crate) fn handle_terminal_key(app: &mut App, key: KeyEvent) {
    if !matches!(app.overlay, OverlayState::Terminal) {
        return;
    }

    // Ctrl+\ → close overlay
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('\\') {
        app.overlay = OverlayState::None;
        return;
    }

    // All other keys → pass through to PTY (including Esc)
    let bytes = crate::terminal::crossterm_key_to_bytes(key);
    if !bytes.is_empty() {
        if let Some(term) = &mut app.terminal {
            let _ = term.write_input(&bytes);
        }
    }
}
