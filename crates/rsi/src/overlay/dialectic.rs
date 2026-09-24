//! Dialectic query interface overlay -- natural-language Q&A against accumulated knowledge.

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent};

/// Open the dialectic query overlay, optionally with a pre-filled query.
pub fn open_dialectic(app: &mut App, initial_query: Option<String>) {
    let project_id = app.active_project_id();
    app.overlay = OverlayState::Dialectic {
        messages: Vec::new(),
        sources: Vec::new(),
        input: initial_query.unwrap_or_default(),
        in_flight: false,
        scroll_offset: 0,
        sources_expanded: false,
        project_id,
    };
}

/// Handle keys in the dialectic overlay.
pub(super) async fn handle_dialectic_key(app: &mut App, key: KeyEvent) {
    let is_in_flight = matches!(
        &app.overlay,
        OverlayState::Dialectic {
            in_flight: true,
            ..
        }
    );

    if is_in_flight {
        if key.code == KeyCode::Esc {
            app.overlay = OverlayState::None;
        }
        return;
    }

    match key.code {
        KeyCode::Esc => {
            if let OverlayState::Dialectic { input, .. } = &app.overlay {
                if input.is_empty() {
                    app.overlay = OverlayState::None;
                } else if let OverlayState::Dialectic { input, .. } = &mut app.overlay {
                    input.clear();
                    app.mark_dirty();
                }
            }
        }
        KeyCode::Char('q') => {
            if let OverlayState::Dialectic { input, .. } = &app.overlay {
                if input.is_empty() {
                    app.overlay = OverlayState::None;
                    return;
                }
            }
            if let OverlayState::Dialectic { input, .. } = &mut app.overlay {
                input.push('q');
                app.mark_dirty();
            }
        }
        KeyCode::Enter => {
            if let OverlayState::Dialectic {
                input,
                messages,
                in_flight,
                project_id,
                ..
            } = &mut app.overlay
            {
                let trimmed = input.trim().to_string();
                if trimmed.is_empty() {
                    return;
                }

                messages.push(("user".to_string(), trimmed.clone()));
                let history: Vec<(String, String)> = messages.clone();
                let proj_id = *project_id;
                input.clear();
                *in_flight = true;

                // Spawn RPC call on a dedicated connection (non-blocking)
                let socket_path = app.client.socket_path().to_path_buf();
                let (tx, rx) = tokio::sync::oneshot::channel();
                app.dialectic_rx = Some(rx);

                tokio::spawn(async move {
                    let mut client = crate::client::DaemonClient::new(socket_path);
                    let result = match client.connect().await {
                        Ok(()) => client
                            .query_memory(&trimmed, proj_id, &history)
                            .await
                            .map_err(|e| e.to_string()),
                        Err(e) => Err(e.to_string()),
                    };
                    let _ = tx.send(result);
                });
                app.mark_dirty();
            }
        }
        KeyCode::Backspace => {
            if let OverlayState::Dialectic { input, .. } = &mut app.overlay {
                input.pop();
                app.mark_dirty();
            }
        }
        KeyCode::Char('s')
            if matches!(
                &app.overlay,
                OverlayState::Dialectic { input, .. } if input.is_empty()
            ) =>
        {
            // Toggle sources expansion when input is empty
            if let OverlayState::Dialectic {
                sources_expanded, ..
            } = &mut app.overlay
            {
                *sources_expanded = !*sources_expanded;
                app.mark_dirty();
            }
        }
        KeyCode::Up => {
            if let OverlayState::Dialectic { scroll_offset, .. } = &mut app.overlay {
                *scroll_offset = scroll_offset.saturating_sub(1);
                app.mark_dirty();
            }
        }
        KeyCode::Down => {
            if let OverlayState::Dialectic { scroll_offset, .. } = &mut app.overlay {
                *scroll_offset = scroll_offset.saturating_add(1);
                app.mark_dirty();
            }
        }
        KeyCode::Char(c) => {
            if let OverlayState::Dialectic { input, .. } = &mut app.overlay {
                input.push(c);
                app.mark_dirty();
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::DaemonClient;
    use crate::state::{DevState, PersistedState};
    use crate::types::{OverlayState, Pane};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::path::PathBuf;

    fn test_app() -> App {
        DevState::clear();
        PersistedState::default().save();
        let mut app = App::new(DaemonClient::new(PathBuf::from("/tmp/test.sock")));
        if let Some(Pane::SessionList {
            selected_index,
            selected_session,
            ..
        }) = app.focused_pane_mut()
        {
            *selected_index = 0;
            *selected_session = None;
        }
        app
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn open_dialectic_overlay(app: &mut App) {
        open_dialectic(app, None);
    }

    #[tokio::test]
    async fn test_esc_with_empty_input_closes_overlay() {
        let mut app = test_app();
        open_dialectic_overlay(&mut app);
        assert!(matches!(app.overlay, OverlayState::Dialectic { .. }));

        handle_dialectic_key(&mut app, key(KeyCode::Esc)).await;
        assert!(matches!(app.overlay, OverlayState::None));
    }

    #[tokio::test]
    async fn test_esc_with_nonempty_input_clears_input_first() {
        let mut app = test_app();
        open_dialectic_overlay(&mut app);

        // Type some text into the input
        handle_dialectic_key(&mut app, key(KeyCode::Char('h'))).await;
        handle_dialectic_key(&mut app, key(KeyCode::Char('i'))).await;

        // Verify input was recorded
        if let OverlayState::Dialectic { input, .. } = &app.overlay {
            assert_eq!(input, "hi");
        } else {
            panic!("Expected Dialectic overlay");
        }

        // First Esc should clear input, not close
        handle_dialectic_key(&mut app, key(KeyCode::Esc)).await;
        assert!(
            matches!(&app.overlay, OverlayState::Dialectic { input, .. } if input.is_empty()),
            "Overlay should still be open with cleared input"
        );

        // Second Esc with empty input should close
        handle_dialectic_key(&mut app, key(KeyCode::Esc)).await;
        assert!(matches!(app.overlay, OverlayState::None));
    }

    #[tokio::test]
    async fn test_enter_with_empty_input_is_noop() {
        let mut app = test_app();
        open_dialectic_overlay(&mut app);

        handle_dialectic_key(&mut app, key(KeyCode::Enter)).await;

        // Overlay should still be open with empty messages
        assert!(
            matches!(&app.overlay, OverlayState::Dialectic { messages, input, .. } if messages.is_empty() && input.is_empty()),
            "Enter on empty input should be a no-op"
        );
    }

    #[tokio::test]
    async fn test_char_input_appends_to_buffer() {
        let mut app = test_app();
        open_dialectic_overlay(&mut app);

        handle_dialectic_key(&mut app, key(KeyCode::Char('a'))).await;
        handle_dialectic_key(&mut app, key(KeyCode::Char('b'))).await;
        handle_dialectic_key(&mut app, key(KeyCode::Char('c'))).await;

        if let OverlayState::Dialectic { input, .. } = &app.overlay {
            assert_eq!(input, "abc");
        } else {
            panic!("Expected Dialectic overlay");
        }
    }

    #[tokio::test]
    async fn test_backspace_removes_last_character() {
        let mut app = test_app();
        open_dialectic_overlay(&mut app);

        handle_dialectic_key(&mut app, key(KeyCode::Char('a'))).await;
        handle_dialectic_key(&mut app, key(KeyCode::Char('b'))).await;
        handle_dialectic_key(&mut app, key(KeyCode::Char('c'))).await;
        handle_dialectic_key(&mut app, key(KeyCode::Backspace)).await;

        if let OverlayState::Dialectic { input, .. } = &app.overlay {
            assert_eq!(input, "ab");
        } else {
            panic!("Expected Dialectic overlay");
        }
    }

    #[tokio::test]
    async fn test_backspace_on_empty_input_is_noop() {
        let mut app = test_app();
        open_dialectic_overlay(&mut app);

        handle_dialectic_key(&mut app, key(KeyCode::Backspace)).await;

        if let OverlayState::Dialectic { input, .. } = &app.overlay {
            assert!(input.is_empty());
        } else {
            panic!("Expected Dialectic overlay");
        }
    }

    #[tokio::test]
    async fn test_s_toggles_sources_when_input_empty() {
        let mut app = test_app();
        open_dialectic_overlay(&mut app);

        // Verify initial state
        if let OverlayState::Dialectic {
            sources_expanded, ..
        } = &app.overlay
        {
            assert!(!sources_expanded, "Sources should be collapsed initially");
        } else {
            panic!("Expected Dialectic overlay");
        }

        // Press 's' to expand sources
        handle_dialectic_key(&mut app, key(KeyCode::Char('s'))).await;
        if let OverlayState::Dialectic {
            sources_expanded, ..
        } = &app.overlay
        {
            assert!(*sources_expanded, "Sources should be expanded after 's'");
        } else {
            panic!("Expected Dialectic overlay");
        }

        // Press 's' again to collapse
        handle_dialectic_key(&mut app, key(KeyCode::Char('s'))).await;
        if let OverlayState::Dialectic {
            sources_expanded, ..
        } = &app.overlay
        {
            assert!(
                !sources_expanded,
                "Sources should be collapsed after second 's'"
            );
        } else {
            panic!("Expected Dialectic overlay");
        }
    }

    #[tokio::test]
    async fn test_s_appends_to_input_when_nonempty() {
        let mut app = test_app();
        open_dialectic_overlay(&mut app);

        // Type some text first (input is non-empty)
        handle_dialectic_key(&mut app, key(KeyCode::Char('a'))).await;

        // Press 's' — should append to input, not toggle sources
        handle_dialectic_key(&mut app, key(KeyCode::Char('s'))).await;

        if let OverlayState::Dialectic {
            input,
            sources_expanded,
            ..
        } = &app.overlay
        {
            assert_eq!(input, "as", "s should append when input is non-empty");
            assert!(!sources_expanded, "Sources should remain collapsed");
        } else {
            panic!("Expected Dialectic overlay");
        }
    }

    #[tokio::test]
    async fn test_q_closes_when_input_empty() {
        let mut app = test_app();
        open_dialectic_overlay(&mut app);

        handle_dialectic_key(&mut app, key(KeyCode::Char('q'))).await;
        assert!(matches!(app.overlay, OverlayState::None));
    }

    #[tokio::test]
    async fn test_q_appends_to_input_when_nonempty() {
        let mut app = test_app();
        open_dialectic_overlay(&mut app);

        handle_dialectic_key(&mut app, key(KeyCode::Char('a'))).await;
        handle_dialectic_key(&mut app, key(KeyCode::Char('q'))).await;

        if let OverlayState::Dialectic { input, .. } = &app.overlay {
            assert_eq!(input, "aq");
        } else {
            panic!("Expected Dialectic overlay");
        }
    }

    #[tokio::test]
    async fn test_scroll_up_decrements_offset() {
        let mut app = test_app();
        open_dialectic_overlay(&mut app);

        // Set scroll offset to non-zero first
        if let OverlayState::Dialectic { scroll_offset, .. } = &mut app.overlay {
            *scroll_offset = 5;
        }

        handle_dialectic_key(&mut app, key(KeyCode::Up)).await;
        if let OverlayState::Dialectic { scroll_offset, .. } = &app.overlay {
            assert_eq!(*scroll_offset, 4);
        } else {
            panic!("Expected Dialectic overlay");
        }
    }

    #[tokio::test]
    async fn test_scroll_down_increments_offset() {
        let mut app = test_app();
        open_dialectic_overlay(&mut app);

        handle_dialectic_key(&mut app, key(KeyCode::Down)).await;
        if let OverlayState::Dialectic { scroll_offset, .. } = &app.overlay {
            assert_eq!(*scroll_offset, 1);
        } else {
            panic!("Expected Dialectic overlay");
        }
    }

    #[tokio::test]
    async fn test_scroll_up_saturates_at_zero() {
        let mut app = test_app();
        open_dialectic_overlay(&mut app);

        // Scroll offset already 0 — should not underflow
        handle_dialectic_key(&mut app, key(KeyCode::Up)).await;
        if let OverlayState::Dialectic { scroll_offset, .. } = &app.overlay {
            assert_eq!(*scroll_offset, 0);
        } else {
            panic!("Expected Dialectic overlay");
        }
    }

    #[tokio::test]
    async fn test_open_dialectic_with_initial_query() {
        let mut app = test_app();
        open_dialectic(&mut app, Some("what did I work on today?".to_string()));

        if let OverlayState::Dialectic {
            input, messages, ..
        } = &app.overlay
        {
            assert_eq!(input, "what did I work on today?");
            assert!(messages.is_empty());
        } else {
            panic!("Expected Dialectic overlay");
        }
    }
}
