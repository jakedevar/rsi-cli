//! AI chat overlay — multi-turn Q&A about the source text.
//!
//! User asks questions about the text, receives responses. Source text is never modified.
//! Conversation history persists for the overlay's lifetime, resets on close.

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent};

pub(super) async fn handle_ai_chat_key(app: &mut App, key: KeyEvent) -> bool {
    let (messages, input, source_text, in_flight, scroll_offset) = match &mut app.overlay {
        OverlayState::AiChat {
            messages,
            input,
            source_text,
            in_flight,
            scroll_offset,
            ..
        } => (messages, input, source_text, in_flight, scroll_offset),
        _ => return false,
    };

    // While in-flight, only allow Esc
    if *in_flight {
        if key.code == KeyCode::Esc {
            close_ai_chat(app);
        }
        return true;
    }

    match key.code {
        KeyCode::Esc => {
            if input.is_empty() {
                close_ai_chat(app);
            } else {
                // Clear input first, close on second Esc
                input.clear();
                app.mark_dirty();
            }
        }
        KeyCode::Char('q') if input.is_empty() => {
            close_ai_chat(app);
        }
        KeyCode::Enter => {
            if input.trim().is_empty() {
                return true;
            }
            let user_msg = input.trim().to_string();
            messages.push(("user".to_string(), user_msg));
            input.clear();

            // Build conversation for the model
            let mut conversation =
                format!("SOURCE TEXT:\n{}\n\n---\n\nCONVERSATION:\n", source_text);
            for (role, content) in messages.iter() {
                conversation.push_str(&format!("{}: {}\n", role.to_uppercase(), content));
            }

            let config = app.settings.prompt_processor.clone();
            let (tx, rx) = tokio::sync::oneshot::channel();
            app.ai_chat_rx = Some(rx);
            *in_flight = true;

            tokio::spawn(async move {
                let result = match crate::prompt_processor::build_processor(&config) {
                    Some(p) => p
                        .send(
                            crate::prompt_processor::AI_CHAT_SYSTEM_PROMPT,
                            &conversation,
                        )
                        .await
                        .map_err(|e| e.user_facing_message()),
                    None => Err("Processor disabled".to_string()),
                };
                let _ = tx.send(result);
            });
            app.mark_dirty();
        }
        KeyCode::Backspace => {
            input.pop();
            app.mark_dirty();
        }
        KeyCode::Char(c) => {
            input.push(c);
            app.mark_dirty();
        }
        // Scroll conversation
        KeyCode::Up => {
            *scroll_offset = scroll_offset.saturating_sub(1);
            app.mark_dirty();
        }
        KeyCode::Down => {
            *scroll_offset = scroll_offset.saturating_add(1);
            app.mark_dirty();
        }
        _ => {}
    }
    true
}

fn close_ai_chat(app: &mut App) {
    app.restore_previous_overlay();
    app.ai_chat_rx = None;
    app.mark_dirty();
}
