//! Recent completions focus-mode input handling.
//!
//! `gr` focuses the recent completions section in the right sidebar.
//! Keys are intercepted via the overlay mechanism but rendering happens
//! in the gutter window (not a centered popup).

use crate::app::App;
use crate::types::{InputMode, OverlayState};
use crossterm::event::{KeyCode, KeyEvent};
use rsi_common::types::SessionStatus;

use super::list;

/// Handle keys while the recent completions section is focused.
pub(super) fn handle_recent_completions_key(app: &mut App, key: KeyEvent) {
    let ids = recent_completion_ids(app);
    let len = ids.len();

    // Navigation (j/k/g/G) only when list is non-empty
    if len > 0 {
        if let OverlayState::RecentCompletions { selected_index } = &mut app.overlay {
            if list::handle_list_nav_key(selected_index, len, &key) {
                return;
            }
        }
    }

    match key.code {
        KeyCode::Enter if len > 0 => {
            let session_id =
                if let OverlayState::RecentCompletions { selected_index } = &app.overlay {
                    ids.get(*selected_index).copied()
                } else {
                    None
                };

            if let Some(sid) = session_id {
                app.overlay = OverlayState::None;
                app.open_session_in_current_pane(sid);
            }
        }
        // i — return to session detail input bar in insert mode
        KeyCode::Char('i') => {
            app.overlay = OverlayState::None;
            app.input_mode = InputMode::Input;
        }
        // Esc — return to normal mode
        KeyCode::Esc | KeyCode::Char('q') => {
            app.overlay = OverlayState::None;
        }
        _ => {}
    }
}

/// Get the session IDs of recent completions in display order (newest first).
fn recent_completion_ids(app: &App) -> Vec<uuid::Uuid> {
    let mut recent: Vec<&rsi_common::types::Session> = app
        .sessions
        .values()
        .filter(|s| {
            matches!(
                s.session.status,
                SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Interrupted
            )
        })
        .map(|s| &s.session)
        .collect();

    recent.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    recent.truncate(6);
    recent.iter().map(|s| s.id).collect()
}
