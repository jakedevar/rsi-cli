//! Diagnostics overlay input handling.

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent};

/// Handle key events in the diagnostics overlay.
/// Read-only — q/Esc closes it.
pub(super) fn handle_diagnostics_key(app: &mut App, key: KeyEvent) {
    if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
        app.overlay = OverlayState::None;
    }
}
