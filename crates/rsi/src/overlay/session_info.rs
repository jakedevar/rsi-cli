//! Session info panel overlay input handling.
//!
//! Consolidated, read-only view of session metadata (id, provider/model,
//! working dir, project, hierarchy, rating, label, tags, context usage,
//! description) reachable via `F3` — replaces the dead `detail_header_line`
//! `"views: chat v"` stub. `R`/`G` jump into the existing (RPC-wired but
//! otherwise quarantined) Rating/Label-picker overlays, mirroring
//! `rating.rs`'s own shape.

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent};

/// Open the session info panel for the currently focused/selected session.
pub fn open_session_info_panel(app: &mut App) {
    let session_id = match app.selected_session_id() {
        Some(id) => id,
        None => {
            app.notify("No session selected");
            return;
        }
    };
    app.overlay = OverlayState::SessionInfoPanel { session_id };
    app.mark_dirty();
}

/// Handle keys in the session info panel.
///
/// `R` opens the Rating overlay, `G` opens the Label picker (both replace the
/// info panel — single active-overlay model, matching every other overlay).
/// `Esc`/`q` closes back to normal session-detail view.
pub(super) async fn handle_session_info_panel_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Char('R') => {
            super::rating::open_rating_overlay(app);
        }
        KeyCode::Char('G') => {
            super::label_picker::open_label_picker(app);
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            app.overlay = OverlayState::None;
            app.mark_dirty();
        }
        _ => {}
    }
}
