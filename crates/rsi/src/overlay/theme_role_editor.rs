use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::Color;

use crate::app::App;
use crate::types::{NotificationPriority, OverlayState};
use crate::ui::theme;
use crate::ui::theme_roles::{
    ContrastAssessment, ContrastWarning, ThemeRole, assess_contrast, parse_rgb,
};

pub fn open_theme_role_editor(app: &mut App, role: ThemeRole) {
    let rgb = theme::get_theme_role_override(role)
        .or_else(|| color_rgb(theme::semantic_color(role)))
        .unwrap_or([255, 255, 255]);
    app.overlay = OverlayState::ThemeRoleEditor {
        role,
        input: format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2]),
        opening_overrides: theme::snapshot_theme_role_overrides(),
        assessment: None,
        committed: false,
        pending_acknowledgement: None,
    };
    app.mark_dirty();
}

fn color_rgb(color: Color) -> Option<[u8; 3]> {
    match color {
        Color::Rgb(r, g, b) => Some([r, g, b]),
        Color::White => Some([255, 255, 255]),
        Color::Black => Some([0, 0, 0]),
        _ => None,
    }
}

fn opening_role_value(
    opening_overrides: &[(ThemeRole, [u8; 3])],
    role: ThemeRole,
) -> Option<[u8; 3]> {
    opening_overrides
        .iter()
        .find_map(|&(candidate, rgb)| (candidate == role).then_some(rgb))
}

fn update_preview(app: &mut App) {
    let (role, candidate, opening) = match &app.overlay {
        OverlayState::ThemeRoleEditor {
            role,
            input,
            opening_overrides,
            ..
        } => (
            *role,
            parse_rgb(input).ok(),
            opening_role_value(opening_overrides, *role),
        ),
        _ => return,
    };
    theme::set_theme_role_preview(role, candidate.or(opening));
    app.mark_dirty();
}

fn assessment_message(assessment: ContrastAssessment) -> String {
    match assessment {
        ContrastAssessment::Invalid { reason } => reason.to_string(),
        ContrastAssessment::Readable { ratio, quantized } => {
            if quantized {
                format!("Readable after xterm-256 quantization ({ratio:.2}:1)")
            } else {
                format!("Readable ({ratio:.2}:1)")
            }
        }
        ContrastAssessment::LowContrast {
            ratio,
            minimum,
            quantized,
        } => format!(
            "Low contrast{}: {ratio:.2}:1, minimum {minimum:.1}:1 — Enter again to save",
            if quantized {
                " after xterm-256 quantization"
            } else {
                ""
            }
        ),
        ContrastAssessment::Unverifiable {
            warning: ContrastWarning::DefaultBackground,
        } => "Terminal default background is unknown; no ratio available — Enter again to save"
            .to_string(),
        ContrastAssessment::Unverifiable {
            warning: ContrastWarning::TerminalCapability,
        } => {
            "Terminal color capability is unknown/ANSI-16; no reliable ratio — Enter again to save"
                .to_string()
        }
    }
}

fn commit_candidate(app: &mut App) {
    let (role, input, pending) = match &app.overlay {
        OverlayState::ThemeRoleEditor {
            role,
            input,
            pending_acknowledgement,
            ..
        } => (*role, input.clone(), *pending_acknowledgement),
        _ => return,
    };
    let rgb = match parse_rgb(&input) {
        Ok(rgb) => rgb,
        Err(reason) => {
            if let OverlayState::ThemeRoleEditor {
                assessment,
                pending_acknowledgement,
                ..
            } = &mut app.overlay
            {
                *assessment = Some(ContrastAssessment::Invalid { reason });
                *pending_acknowledgement = None;
            }
            app.notify_error(reason);
            return;
        }
    };

    theme::set_theme_role_preview(role, Some(rgb));
    let descriptor = role.descriptor();
    let background = descriptor
        .contrast_against
        .map(theme::semantic_color)
        .unwrap_or(Color::Reset);
    let assessment = assess_contrast(
        Color::Rgb(rgb[0], rgb[1], rgb[2]),
        background,
        descriptor.minimum_contrast,
        app.terminal_color_capability,
    );
    let acknowledgement = (role, rgb, assessment);
    let should_commit = !assessment.requires_acknowledgement() || pending == Some(acknowledgement);

    if should_commit {
        theme::set_theme_role_override(role, Some(rgb));
        let committed_overrides = theme::snapshot_theme_role_overrides();
        if let OverlayState::ThemeRoleEditor {
            opening_overrides,
            assessment: current,
            committed,
            pending_acknowledgement,
            ..
        } = &mut app.overlay
        {
            *opening_overrides = committed_overrides;
            *current = Some(assessment);
            *committed = true;
            *pending_acknowledgement = None;
        }
        app.notify_success(format!("{} color saved", role.label()));
    } else {
        if let OverlayState::ThemeRoleEditor {
            assessment: current,
            committed,
            pending_acknowledgement,
            ..
        } = &mut app.overlay
        {
            *current = Some(assessment);
            *committed = false;
            *pending_acknowledgement = Some(acknowledgement);
        }
        app.push_notification(
            crate::types::NotificationKind::Info,
            NotificationPriority::Medium,
            assessment_message(assessment),
            None,
        );
    }
    app.mark_dirty();
}

pub fn handle_theme_role_editor_key(app: &mut App, key: KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            let opening = match &app.overlay {
                OverlayState::ThemeRoleEditor {
                    opening_overrides, ..
                } => opening_overrides.clone(),
                _ => return,
            };
            theme::apply_theme_role_overrides(&opening);
            app.overlay = OverlayState::None;
            app.mark_dirty();
        }
        KeyCode::Enter => commit_candidate(app),
        KeyCode::Delete => {
            let role = match app.overlay {
                OverlayState::ThemeRoleEditor { role, .. } => role,
                _ => return,
            };
            theme::set_theme_role_override(role, None);
            app.notify_success(format!("{} reset to built-in", role.label()));
            app.overlay = OverlayState::None;
            app.mark_dirty();
        }
        KeyCode::Backspace => {
            if let OverlayState::ThemeRoleEditor {
                input,
                assessment,
                pending_acknowledgement,
                committed,
                ..
            } = &mut app.overlay
            {
                input.pop();
                *assessment = None;
                *pending_acknowledgement = None;
                *committed = false;
            }
            update_preview(app);
        }
        KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            if let OverlayState::ThemeRoleEditor {
                input,
                assessment,
                pending_acknowledgement,
                committed,
                ..
            } = &mut app.overlay
            {
                input.push(character);
                *assessment = None;
                *pending_acknowledgement = None;
                *committed = false;
            }
            update_preview(app);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::app_test_helpers::with_session_list;

    #[tokio::test]
    async fn invalid_input_does_not_change_committed_override() {
        theme::with_theme_state(|| {
            let mut app = with_session_list(0);
            theme::set_theme_role_override(ThemeRole::Accent, Some([1, 2, 3]));
            open_theme_role_editor(&mut app, ThemeRole::Accent);
            if let OverlayState::ThemeRoleEditor { input, .. } = &mut app.overlay {
                *input = "bad".to_string();
            }
            commit_candidate(&mut app);
            assert_eq!(
                theme::snapshot_theme_role_overrides(),
                vec![(ThemeRole::Accent, [1, 2, 3])]
            );
        });
    }

    #[tokio::test]
    async fn warning_requires_same_candidate_twice() {
        theme::with_theme_state(|| {
            let mut app = with_session_list(0);
            app.terminal_color_capability =
                crate::ui::theme_roles::TerminalColorCapability::Unknown;
            open_theme_role_editor(&mut app, ThemeRole::Accent);
            if let OverlayState::ThemeRoleEditor { input, .. } = &mut app.overlay {
                *input = "#010203".to_string();
            }
            commit_candidate(&mut app);
            assert!(theme::snapshot_theme_role_overrides().is_empty());
            commit_candidate(&mut app);
            assert_eq!(
                theme::snapshot_theme_role_overrides(),
                vec![(ThemeRole::Accent, [1, 2, 3])]
            );
        });
    }

    #[tokio::test]
    async fn escape_after_readable_commit_retains_committed_override() {
        theme::with_theme_state(|| {
            let mut app = with_session_list(0);
            theme::set_theme_by_index(0);
            app.terminal_color_capability =
                crate::ui::theme_roles::TerminalColorCapability::TrueColor;
            open_theme_role_editor(&mut app, ThemeRole::PrimaryText);
            if let OverlayState::ThemeRoleEditor { input, .. } = &mut app.overlay {
                *input = "#FFFFFF".to_string();
            }

            commit_candidate(&mut app);
            assert!(matches!(
                app.overlay,
                OverlayState::ThemeRoleEditor {
                    committed: true,
                    ..
                }
            ));
            handle_theme_role_editor_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

            assert!(matches!(app.overlay, OverlayState::None));
            assert_eq!(
                theme::snapshot_theme_role_overrides(),
                vec![(ThemeRole::PrimaryText, [255, 255, 255])]
            );
        });
    }

    #[test]
    fn question_mark_remains_literal_editor_input() {
        theme::with_theme_state(|| {
            let mut app = with_session_list(0);
            open_theme_role_editor(&mut app, ThemeRole::Accent);
            let before = match &app.overlay {
                OverlayState::ThemeRoleEditor { input, .. } => input.clone(),
                _ => panic!("role editor should be open"),
            };
            handle_theme_role_editor_key(
                &mut app,
                KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
            );
            assert!(matches!(
                app.overlay,
                OverlayState::ThemeRoleEditor { ref input, .. }
                    if input == &format!("{before}?")
            ));
        });
    }

    #[tokio::test]
    async fn question_mark_routes_to_help_only_after_text_entry_commits() {
        let mut app = with_session_list(0);
        app.overlay = OverlayState::ThemeRoleEditor {
            role: ThemeRole::Accent,
            input: "#010203".to_string(),
            opening_overrides: Vec::new(),
            assessment: None,
            committed: false,
            pending_acknowledgement: None,
        };

        crate::overlay::handle_overlay_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
        )
        .await;
        assert!(matches!(
            app.overlay,
            OverlayState::ThemeRoleEditor { ref input, .. } if input == "#010203?"
        ));

        if let OverlayState::ThemeRoleEditor {
            input, committed, ..
        } = &mut app.overlay
        {
            input.pop();
            *committed = true;
        }
        crate::overlay::handle_overlay_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
        )
        .await;
        assert!(matches!(app.overlay, OverlayState::KeybindingsHelp { .. }));
        assert!(matches!(
            app.previous_overlay(),
            Some(OverlayState::ThemeRoleEditor {
                committed: true,
                ..
            })
        ));
    }
}
