//! Card editor overlay input handling — view/edit entity card facts.
//!
//! Keybindings:
//! - `j`/`k` or Down/Up: navigate facts
//! - `a`: add new fact (enters inline edit mode at end of list)
//! - `e` or `i`: edit selected fact inline
//! - `dd`: delete selected fact
//! - `J`/`K` (Shift+j/k): move selected fact down/up (reorder)
//! - `Enter` (in edit mode): confirm edit
//! - `Esc` (in edit mode): cancel edit
//! - `Esc` (in nav mode): save and close

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rsi_common::types::EntityCard;

/// Handle keys in the card editor overlay.
pub(super) async fn handle_card_editor_key(app: &mut App, key: KeyEvent) {
    let is_editing = matches!(
        &app.overlay,
        OverlayState::CardEditor {
            editing: Some(_),
            ..
        }
    );

    if is_editing {
        handle_edit_mode(app, key).await;
    } else {
        handle_nav_mode(app, key).await;
    }
}

/// Extract mutable CardEditor fields. Returns None if overlay is not a CardEditor.
#[allow(clippy::type_complexity)]
fn card_fields(
    overlay: &mut OverlayState,
) -> Option<(
    &mut Vec<String>,
    &mut usize,
    &mut usize,
    &mut Option<String>,
    &mut bool,
)> {
    match overlay {
        OverlayState::CardEditor {
            facts,
            selected_index,
            scroll_offset,
            editing,
            pending_delete,
            ..
        } => Some((
            facts,
            selected_index,
            scroll_offset,
            editing,
            pending_delete,
        )),
        _ => None,
    }
}

/// Handle keys in navigation mode (browsing facts).
async fn handle_nav_mode(app: &mut App, key: KeyEvent) {
    // Check for pending `d` chord first
    let is_pending_delete = matches!(
        &app.overlay,
        OverlayState::CardEditor {
            pending_delete: true,
            ..
        }
    );

    if is_pending_delete {
        let Some((facts, selected_index, _, _, pending_delete)) = card_fields(&mut app.overlay)
        else {
            return;
        };
        *pending_delete = false;
        if key.code == KeyCode::Char('d') && key.modifiers.is_empty() {
            // dd: delete selected fact
            if !facts.is_empty() {
                facts.remove(*selected_index);
                if *selected_index >= facts.len() && !facts.is_empty() {
                    *selected_index = facts.len() - 1;
                }
                save_card(app).await;
            }
            return;
        }
        // Not `d` -- fall through to normal nav handling
    }

    match key.code {
        // Navigation
        KeyCode::Char('j') | KeyCode::Down if key.modifiers.is_empty() => {
            let Some((facts, selected_index, scroll_offset, _, _)) = card_fields(&mut app.overlay)
            else {
                return;
            };
            if !facts.is_empty() && *selected_index < facts.len() - 1 {
                *selected_index += 1;
                // Scroll to keep selection visible (assume ~20 visible rows)
                if *selected_index >= *scroll_offset + 20 {
                    *scroll_offset = selected_index.saturating_sub(19);
                }
            }
        }

        KeyCode::Char('k') | KeyCode::Up if key.modifiers.is_empty() => {
            let Some((_, selected_index, scroll_offset, _, _)) = card_fields(&mut app.overlay)
            else {
                return;
            };
            if *selected_index > 0 {
                *selected_index -= 1;
                if *selected_index < *scroll_offset {
                    *scroll_offset = *selected_index;
                }
            }
        }

        // Jump to top
        KeyCode::Char('g') if key.modifiers.is_empty() => {
            let Some((_, selected_index, scroll_offset, _, _)) = card_fields(&mut app.overlay)
            else {
                return;
            };
            *selected_index = 0;
            *scroll_offset = 0;
        }

        // Jump to bottom
        KeyCode::Char('G') if key.modifiers.is_empty() => {
            let Some((facts, selected_index, scroll_offset, _, _)) = card_fields(&mut app.overlay)
            else {
                return;
            };
            if !facts.is_empty() {
                *selected_index = facts.len() - 1;
                *scroll_offset = selected_index.saturating_sub(19);
            }
        }

        // Add new fact
        KeyCode::Char('a') if key.modifiers.is_empty() => {
            // Check capacity before taking mutable borrow
            let at_capacity = matches!(
                &app.overlay,
                OverlayState::CardEditor { facts, .. } if facts.len() >= EntityCard::MAX_FACTS
            );
            if at_capacity {
                app.notify_error(format!(
                    "Card is full ({} facts max)",
                    EntityCard::MAX_FACTS
                ));
                return;
            }
            let Some((facts, selected_index, _, editing, _)) = card_fields(&mut app.overlay) else {
                return;
            };
            // Add empty fact at end and enter edit mode
            facts.push(String::new());
            *selected_index = facts.len() - 1;
            *editing = Some(String::new());
        }

        // Edit selected fact
        KeyCode::Char('e') | KeyCode::Char('i') if key.modifiers.is_empty() => {
            let Some((facts, selected_index, _, editing, _)) = card_fields(&mut app.overlay) else {
                return;
            };
            if !facts.is_empty() {
                *editing = Some(facts[*selected_index].clone());
            }
        }

        // Delete chord: first `d`
        KeyCode::Char('d') if key.modifiers.is_empty() => {
            let Some((_, _, _, _, pending_delete)) = card_fields(&mut app.overlay) else {
                return;
            };
            *pending_delete = true;
        }

        // Move fact down (Shift+J)
        KeyCode::Char('J') if key.modifiers.is_empty() => {
            let Some((facts, selected_index, _, _, _)) = card_fields(&mut app.overlay) else {
                return;
            };
            if !facts.is_empty() && *selected_index < facts.len() - 1 {
                facts.swap(*selected_index, *selected_index + 1);
                *selected_index += 1;
                save_card(app).await;
            }
        }

        // Move fact up (Shift+K)
        KeyCode::Char('K') if key.modifiers.is_empty() => {
            let Some((facts, selected_index, _, _, _)) = card_fields(&mut app.overlay) else {
                return;
            };
            if *selected_index > 0 {
                facts.swap(*selected_index, *selected_index - 1);
                *selected_index -= 1;
                save_card(app).await;
            }
        }

        // Ctrl+S: save without closing
        KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            save_card(app).await;
            app.notify_success("Card saved");
        }

        // Esc: save and close
        KeyCode::Esc => {
            save_card(app).await;
            app.overlay = OverlayState::None;
        }

        // q: close (same as Esc)
        KeyCode::Char('q') if key.modifiers.is_empty() => {
            save_card(app).await;
            app.overlay = OverlayState::None;
        }

        _ => {}
    }
}

/// Handle keys in edit mode (editing a single fact inline).
async fn handle_edit_mode(app: &mut App, key: KeyEvent) {
    match key.code {
        // Confirm edit
        KeyCode::Enter => {
            let Some((facts, selected_index, _, editing, _)) = card_fields(&mut app.overlay) else {
                return;
            };
            if let Some(text) = editing.take() {
                let text = text.trim().to_string();
                if text.is_empty() {
                    // Empty edit = delete the fact
                    facts.remove(*selected_index);
                    if *selected_index >= facts.len() && !facts.is_empty() {
                        *selected_index = facts.len() - 1;
                    }
                } else {
                    facts[*selected_index] = text;
                }
                save_card(app).await;
            }
        }

        // Cancel edit
        KeyCode::Esc => {
            let Some((facts, selected_index, _, editing, _)) = card_fields(&mut app.overlay) else {
                return;
            };
            if editing.take().is_some() {
                // If the fact was newly added (empty), remove it
                if facts.get(*selected_index).is_some_and(|f| f.is_empty()) {
                    facts.remove(*selected_index);
                    if *selected_index >= facts.len() && !facts.is_empty() {
                        *selected_index = facts.len() - 1;
                    }
                }
            }
        }

        // Typing
        KeyCode::Char(c) => {
            let Some((_, _, _, editing, _)) = card_fields(&mut app.overlay) else {
                return;
            };
            if let Some(text) = editing.as_mut() {
                text.push(c);
            }
        }

        // Backspace
        KeyCode::Backspace => {
            let Some((_, _, _, editing, _)) = card_fields(&mut app.overlay) else {
                return;
            };
            if let Some(text) = editing.as_mut() {
                text.pop();
            }
        }

        _ => {}
    }
}

/// Save the current card facts to the daemon via RPC.
async fn save_card(app: &mut App) {
    let (entity_type, entity_id, facts) = match &app.overlay {
        OverlayState::CardEditor {
            entity_type,
            entity_id,
            facts,
            ..
        } => (entity_type.clone(), entity_id.clone(), facts.clone()),
        _ => return,
    };

    if let Err(e) = app
        .client
        .set_entity_card(&entity_type, &entity_id, facts)
        .await
    {
        app.notify_error(format!("Failed to save card: {}", e));
    }
}
