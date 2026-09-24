use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::ui::theme;
use crate::ui::theme_roles::{ContrastAssessment, ContrastWarning, ThemeRole};

use super::fixed_centered_rect;

pub fn render_theme_role_editor(
    frame: &mut Frame,
    area: Rect,
    role: ThemeRole,
    input: &str,
    assessment: Option<ContrastAssessment>,
    committed: bool,
) {
    let popup = fixed_centered_rect(area, 72, 11);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Block::default()
            .title(format!(" {} ", role.label()))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::focused_border()))
            .style(Style::default().bg(theme::semantic_color(
                crate::ui::theme_roles::ThemeRole::ElevatedSurface,
            ))),
        popup,
    );
    let inner = Rect::new(popup.x + 2, popup.y + 2, popup.width - 4, popup.height - 4);
    let assessment = assessment
        .map(assessment_text)
        .unwrap_or_else(|| "Edit #RRGGBB; valid input previews immediately.".to_string());
    let lines = vec![
        Line::from(Span::styled(
            format!("Role: {} ({})", role.label(), role.key()),
            Style::default().fg(theme::subtext1()),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("Color  ", Style::default().fg(theme::subtext1())),
            Span::styled(
                input,
                Style::default()
                    .fg(theme::text())
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            assessment,
            Style::default().fg(if committed {
                theme::semantic_color(ThemeRole::Success)
            } else {
                theme::semantic_color(ThemeRole::Warning)
            }),
        )),
        Line::from(""),
        Line::from(Span::styled(
            if committed {
                "Saved · type to edit again · ? help · Esc close"
            } else {
                "Enter commit · Delete reset role · Esc restore opening state"
            },
            Style::default().fg(theme::subtext1()),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

fn assessment_text(assessment: ContrastAssessment) -> String {
    match assessment {
        ContrastAssessment::Invalid { reason } => format!("Invalid: {reason}"),
        ContrastAssessment::Readable { ratio, quantized } => format!(
            "Readable: {ratio:.2}:1{}",
            if quantized {
                " (xterm-256 quantized)"
            } else {
                ""
            }
        ),
        ContrastAssessment::LowContrast {
            ratio,
            minimum,
            quantized,
        } => format!(
            "Low contrast: {ratio:.2}:1 < {minimum:.1}:1{}; Enter again",
            if quantized {
                " (xterm-256 quantized)"
            } else {
                ""
            }
        ),
        ContrastAssessment::Unverifiable {
            warning: ContrastWarning::DefaultBackground,
        } => "Unverifiable: terminal default background is unknown; Enter again".to_string(),
        ContrastAssessment::Unverifiable {
            warning: ContrastWarning::TerminalCapability,
        } => "Unverifiable: terminal capability is unknown/ANSI-16; Enter again".to_string(),
    }
}
