//! Prompt preview overlay input handling.

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Open the prompt preview overlay for the currently selected session.
pub fn open_prompt_preview(app: &mut App) {
    if app.selected_session_id().is_none() {
        return;
    }
    app.overlay = OverlayState::PromptPreview { scroll_offset: 0 };
}

/// Handle keys in the prompt preview overlay.
pub(super) fn handle_prompt_preview_key(app: &mut App, key: KeyEvent) {
    match key.code {
        // Navigate session list (popup updates because content is derived at render time)
        KeyCode::Char('j') | KeyCode::Down => {
            app.nav_down();
            // Reset scroll on navigation since we're viewing a different session's query
            if let OverlayState::PromptPreview { scroll_offset } = &mut app.overlay {
                *scroll_offset = 0;
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.nav_up();
            if let OverlayState::PromptPreview { scroll_offset } = &mut app.overlay {
                *scroll_offset = 0;
            }
        }
        // G = jump to bottom of session list
        KeyCode::Char('G') => {
            app.jump_to_bottom();
            if let OverlayState::PromptPreview { scroll_offset } = &mut app.overlay {
                *scroll_offset = 0;
            }
        }
        // Scroll within popup: Ctrl+d / Ctrl+u
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let OverlayState::PromptPreview { scroll_offset } = &mut app.overlay {
                *scroll_offset = scroll_offset.saturating_add(10);
            }
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let OverlayState::PromptPreview { scroll_offset } = &mut app.overlay {
                *scroll_offset = scroll_offset.saturating_sub(10);
            }
        }
        // Enter = close overlay and enter session detail
        KeyCode::Enter => {
            let session_id = app.selected_session_id();
            app.overlay = OverlayState::None;
            if let Some(id) = session_id {
                app.open_session_in_current_pane(id);
            }
        }
        // Close overlay
        KeyCode::Char('p') | KeyCode::Esc | KeyCode::Char('q') => {
            app.overlay = OverlayState::None;
        }
        _ => {} // Swallow all other keys
    }
}
