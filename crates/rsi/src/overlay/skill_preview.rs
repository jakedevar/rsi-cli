//! Read-only `SKILL.md` preview overlay.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::App;
use crate::claude_config;
use crate::types::OverlayState;

/// Open the SKILL.md preview overlay for the given user skill name.
pub fn open_skill_preview(app: &mut App, name: &str) {
    let content = match claude_config::read_skill_content(name) {
        Ok(s) => s,
        Err(e) => {
            app.notify_error(format!("Failed to read SKILL.md: {}", e));
            return;
        }
    };
    app.overlay = OverlayState::SkillPreview {
        name: name.to_string(),
        content,
        scroll_offset: 0,
    };
}

/// Handle keys inside the SkillPreview overlay.
pub(super) fn handle_skill_preview_key(app: &mut App, key: KeyEvent) {
    let total_lines = match &app.overlay {
        OverlayState::SkillPreview { content, .. } => content.lines().count(),
        _ => return,
    };

    match key.code {
        KeyCode::Char('j') | KeyCode::Down => {
            if let OverlayState::SkillPreview { scroll_offset, .. } = &mut app.overlay {
                if *scroll_offset + 1 < total_lines {
                    *scroll_offset += 1;
                }
            }
        }
        KeyCode::Char('k') | KeyCode::Up => {
            if let OverlayState::SkillPreview { scroll_offset, .. } = &mut app.overlay {
                *scroll_offset = scroll_offset.saturating_sub(1);
            }
        }
        KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let OverlayState::SkillPreview { scroll_offset, .. } = &mut app.overlay {
                *scroll_offset = (*scroll_offset + 10).min(total_lines.saturating_sub(1));
            }
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let OverlayState::SkillPreview { scroll_offset, .. } = &mut app.overlay {
                *scroll_offset = scroll_offset.saturating_sub(10);
            }
        }
        KeyCode::Char('G') | KeyCode::End => {
            if let OverlayState::SkillPreview { scroll_offset, .. } = &mut app.overlay {
                *scroll_offset = total_lines.saturating_sub(1);
            }
        }
        KeyCode::Char('g') | KeyCode::Home => {
            if let OverlayState::SkillPreview { scroll_offset, .. } = &mut app.overlay {
                *scroll_offset = 0;
            }
        }
        KeyCode::Char('q') | KeyCode::Esc => {
            app.overlay = OverlayState::None;
        }
        _ => {}
    }
}
