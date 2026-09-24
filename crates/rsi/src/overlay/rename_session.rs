//! Rename session overlay input handling.

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent};

/// Handle keys in the session rename overlay.
pub(super) async fn handle_rename_session_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            if let OverlayState::RenameSession { session_id, title } = &app.overlay {
                let sid = *session_id;
                let t = title.trim().to_string();
                app.overlay = OverlayState::None;
                if !t.is_empty() {
                    // Persist via RPC and update local state immediately
                    app.client.update_session_title(sid, &t).await.ok();
                    if let Some(state) = app.sessions.get_mut(&sid) {
                        state.session.title = Some(t);
                    }
                    app.notify("Title updated");
                }
            }
        }
        KeyCode::Esc => {
            app.overlay = OverlayState::None;
        }
        KeyCode::Char(c) => {
            if let OverlayState::RenameSession { title, .. } = &mut app.overlay {
                title.push(c);
            }
        }
        KeyCode::Backspace => {
            if let OverlayState::RenameSession { title, .. } = &mut app.overlay {
                title.pop();
            }
        }
        _ => {}
    }
}
