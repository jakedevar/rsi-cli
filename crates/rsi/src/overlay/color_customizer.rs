//! Color customizer overlay input handling.
//!
//! Lets the user set per-role message border colors using `#RRGGBB` hex input.
//! Changes apply live; press Esc to close (changes are kept).

use crate::app::App;
use crate::types::OverlayState;
use crossterm::event::{KeyCode, KeyEvent};

pub(crate) const FIELD_COUNT: usize = 7;
pub(crate) const FIELD_LABELS: [&str; FIELD_COUNT] = [
    "Assistant border",
    "User border",
    "Tool border (unselected)",
    "Tool border (selected)",
    "Normal cursor",
    "Insert cursor",
    "Visual selection",
];

/// Open the color customizer overlay, pre-populating fields with current overrides.
pub fn open_color_customizer(app: &mut App) {
    let inputs: Vec<String> = (0..FIELD_COUNT)
        .map(
            |slot| match crate::ui::theme::get_border_color_override(slot) {
                Some([r, g, b]) => format!("#{:02X}{:02X}{:02X}", r, g, b),
                None => String::new(),
            },
        )
        .collect();

    app.overlay = OverlayState::ColorCustomizer {
        focused_field: 0,
        inputs,
        errors: vec![false; FIELD_COUNT],
    };
}

/// Handle a key event for the color customizer overlay.
pub(super) fn handle_color_customizer_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.overlay = OverlayState::None;
        }
        KeyCode::Tab | KeyCode::Down | KeyCode::Char('j') => {
            if let OverlayState::ColorCustomizer { focused_field, .. } = &mut app.overlay {
                *focused_field = (*focused_field + 1) % FIELD_COUNT;
            }
        }
        KeyCode::BackTab | KeyCode::Up | KeyCode::Char('k') => {
            if let OverlayState::ColorCustomizer { focused_field, .. } = &mut app.overlay {
                *focused_field = focused_field.checked_sub(1).unwrap_or(FIELD_COUNT - 1);
            }
        }
        KeyCode::Enter => {
            apply_focused_field(app);
        }
        KeyCode::Backspace => {
            if let OverlayState::ColorCustomizer {
                focused_field,
                inputs,
                errors,
            } = &mut app.overlay
            {
                let field = *focused_field;
                if let Some(input) = inputs.get_mut(field) {
                    input.pop();
                    errors[field] = false;
                }
            }
        }
        KeyCode::Char(c) => {
            if let OverlayState::ColorCustomizer {
                focused_field,
                inputs,
                errors,
            } = &mut app.overlay
            {
                let field = *focused_field;
                if let Some(input) = inputs.get_mut(field) {
                    let c_upper = c.to_ascii_uppercase();
                    let is_valid_char = c == '#' || c_upper.is_ascii_hexdigit();
                    if is_valid_char && input.len() < 7 {
                        input.push(c_upper);
                        errors[field] = false;
                        if input.len() == 7 {
                            let _ = input;
                            let _ = errors;
                            apply_focused_field(app);
                        }
                    }
                }
            }
        }
        KeyCode::Delete => {
            if let OverlayState::ColorCustomizer {
                focused_field,
                inputs,
                errors,
            } = &mut app.overlay
            {
                let field = *focused_field;
                inputs[field].clear();
                errors[field] = false;
                crate::ui::theme::set_border_color_override(field, None);
            }
        }
        _ => {}
    }
}

/// Parse the focused field's input and apply the override (or mark as error).
fn apply_focused_field(app: &mut App) {
    let (field, input_clone) = match &app.overlay {
        OverlayState::ColorCustomizer {
            focused_field,
            inputs,
            ..
        } => (*focused_field, inputs[*focused_field].clone()),
        _ => return,
    };

    if input_clone.is_empty() {
        crate::ui::theme::set_border_color_override(field, None);
        return;
    }

    match parse_hex_color(&input_clone) {
        Some(rgb) => {
            crate::ui::theme::set_border_color_override(field, Some(rgb));
            if let OverlayState::ColorCustomizer { errors, .. } = &mut app.overlay {
                errors[field] = false;
            }
        }
        None => {
            if let OverlayState::ColorCustomizer { errors, .. } = &mut app.overlay {
                errors[field] = true;
            }
        }
    }
}

/// Parse `#RRGGBB` (case-insensitive) to `[r, g, b]`. Returns `None` on failure.
pub(crate) fn parse_hex_color(s: &str) -> Option<[u8; 3]> {
    let s = s.trim();
    let hex = if s.starts_with('#') { &s[1..] } else { s };
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some([r, g, b])
}
