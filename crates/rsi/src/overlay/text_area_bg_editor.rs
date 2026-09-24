//! Single-field hex editor overlay for the text-area background fill color.
//!
//! Bound to `UserSettings::text_area_backfill_hex`. Empty string ⇒ caller falls
//! back to per-role theme defaults (when the toggle is on); otherwise the hex
//! is used as a uniform fill across all text areas.

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent};

/// Open the text-area-bg hex editor, pre-populated with the current setting.
pub fn open_text_area_bg_editor(app: &mut App) {
    let input = app.settings.text_area_backfill_hex.clone();
    let error = !input.is_empty() && parse_hex_color(&input).is_none();
    app.overlay = OverlayState::TextAreaBgEditor { input, error };
}

/// Handle a key event for the text-area-bg hex editor overlay.
pub(super) fn handle_text_area_bg_editor_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.overlay = OverlayState::None;
        }
        KeyCode::Enter => {
            commit_input(app);
        }
        KeyCode::Backspace => {
            if let OverlayState::TextAreaBgEditor { input, error } = &mut app.overlay {
                input.pop();
                *error = false;
            }
        }
        KeyCode::Delete => {
            if let OverlayState::TextAreaBgEditor { input, error } = &mut app.overlay {
                input.clear();
                *error = false;
            }
            app.settings.text_area_backfill_hex = String::new();
        }
        KeyCode::Char(c) => {
            if let OverlayState::TextAreaBgEditor { input, error } = &mut app.overlay {
                let c_upper = c.to_ascii_uppercase();
                let is_valid_char = c == '#' || c_upper.is_ascii_hexdigit();
                if is_valid_char && input.len() < 7 {
                    input.push(c_upper);
                    *error = false;
                    if input.len() == 7 {
                        // Auto-commit when fully typed.
                        commit_input(app);
                    }
                }
            }
        }
        _ => {}
    }
}

fn commit_input(app: &mut App) {
    let input_clone = match &app.overlay {
        OverlayState::TextAreaBgEditor { input, .. } => input.clone(),
        _ => return,
    };

    let trimmed = input_clone.trim();
    if trimmed.is_empty() {
        app.settings.text_area_backfill_hex = String::new();
        if let OverlayState::TextAreaBgEditor { error, .. } = &mut app.overlay {
            *error = false;
        }
        return;
    }

    match parse_hex_color(trimmed) {
        Some([r, g, b]) => {
            app.settings.text_area_backfill_hex = format!("#{:02X}{:02X}{:02X}", r, g, b);
            if let OverlayState::TextAreaBgEditor { error, .. } = &mut app.overlay {
                *error = false;
            }
        }
        None => {
            if let OverlayState::TextAreaBgEditor { error, .. } = &mut app.overlay {
                *error = true;
            }
        }
    }
}

/// Parse `#RRGGBB` or `RRGGBB` (case-insensitive) to `[r, g, b]`.
pub(crate) fn parse_hex_color(s: &str) -> Option<[u8; 3]> {
    let s = s.trim();
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some([r, g, b])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_with_hash() {
        assert_eq!(parse_hex_color("#1E1E2E"), Some([0x1E, 0x1E, 0x2E]));
    }

    #[test]
    fn parse_without_hash() {
        assert_eq!(parse_hex_color("FFFFFF"), Some([0xFF, 0xFF, 0xFF]));
    }

    #[test]
    fn parse_invalid_length() {
        assert_eq!(parse_hex_color("#FFF"), None);
    }

    #[test]
    fn parse_invalid_char() {
        assert_eq!(parse_hex_color("#GGGGGG"), None);
    }
}
