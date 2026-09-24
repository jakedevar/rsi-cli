//! AI command overlay — small floating input bar for text transformation instructions.
//!
//! User types a natural language instruction (e.g., "replace foo with bar"), presses Enter,
//! and the model transforms the source text. Result is delivered through `corrected_preview`
//! on the originating InputSurface.

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent};

pub(super) async fn handle_ai_command_key(app: &mut App, key: KeyEvent) -> bool {
    let (command, source_text, source, in_flight) = match &mut app.overlay {
        OverlayState::AiCommand {
            command,
            source_text,
            source,
            in_flight,
        } => (command, source_text, source, in_flight),
        _ => return false,
    };

    // Block input while in-flight (only allow Esc to cancel)
    if *in_flight {
        if key.code == KeyCode::Esc {
            close_ai_command(app);
        }
        return true;
    }

    match key.code {
        KeyCode::Esc => {
            close_ai_command(app);
        }
        KeyCode::Enter => {
            if command.trim().is_empty() {
                return true;
            }
            // Build the user message: instruction + source text
            let user_message = format!(
                "INSTRUCTION: {}\n\nSOURCE TEXT:\n{}",
                command.trim(),
                source_text
            );
            let config = app.settings.prompt_processor.clone();
            let (tx, rx) = tokio::sync::oneshot::channel();

            // Clone source for the receiver
            let source_clone = source.clone();
            app.ai_command_rx = Some((source_clone, rx));
            *in_flight = true;

            tokio::spawn(async move {
                let result = match crate::prompt_processor::build_processor(&config) {
                    Some(p) => p
                        .send(
                            crate::prompt_processor::AI_COMMAND_SYSTEM_PROMPT,
                            &user_message,
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
            command.pop();
            app.mark_dirty();
        }
        KeyCode::Char(c) => {
            command.push(c);
            app.mark_dirty();
        }
        _ => {}
    }
    true
}

fn close_ai_command(app: &mut App) {
    // Restore stacked overlay if present
    app.restore_previous_overlay();
    app.ai_command_rx = None;
    app.mark_dirty();
}
