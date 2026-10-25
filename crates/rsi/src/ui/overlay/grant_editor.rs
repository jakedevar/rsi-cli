//! Grant editor rendering (#2237): one view for the global create flow and the
//! new-session modal's grant page. Sections read like the session list's
//! (`★ NAME 3 ────` headers), the selected row carries the list's accent
//! marker, and a pinned footer says what the row does, so the same widget
//! looks the same wherever a manager's grant is edited.

use crate::overlay::grant_editor::{GrantEditor, GrantRow, GrantSection};
use crate::ui::{glyphs, session, theme};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

const LABEL_WIDTH: usize = 14;

fn section_color(section: GrantSection) -> Color {
    match section {
        GrantSection::Authority => theme::accent(),
        GrantSection::Capabilities => theme::status_running(),
        GrantSection::Allowances => theme::warning_status(),
        GrantSection::Launches => theme::blue(),
    }
}

fn section_glyph(section: GrantSection) -> &'static str {
    match section {
        GrantSection::Authority => glyphs::MANAGERS,
        GrantSection::Capabilities => glyphs::DONE,
        GrantSection::Allowances => glyphs::COST,
        GrantSection::Launches => glyphs::NEXT,
    }
}

/// The count shown after a section's name.
fn section_count(editor: &GrantEditor, section: GrantSection) -> String {
    let rows = editor.rows();
    let in_section = |row: &&GrantRow| row.section() == section;
    let on = |row: &GrantRow| editor.value(*row) == "[x]";
    match section {
        GrantSection::Capabilities | GrantSection::Launches => {
            let total = rows.iter().filter(in_section).count();
            let checked = rows.iter().filter(in_section).filter(|row| on(row)).count();
            format!("  {checked}/{total}  ")
        }
        _ => "  ".to_string(),
    }
}

fn row_line(editor: &GrantEditor, row: GrantRow, selected: bool) -> Line<'static> {
    let accent = Style::default().fg(theme::accent());
    let marker = Span::styled(if selected { "▸ " } else { "  " }, accent);
    let label_style = if selected {
        accent.add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::subtext1())
    };
    let value = editor.value(row);
    match row {
        GrantRow::Capability(_) | GrantRow::Launch(_) => {
            let on = value == "[x]";
            let hint = editor.launch_hint(row);
            let mut spans = vec![
                marker,
                Span::styled(
                    value,
                    Style::default()
                        .fg(if on {
                            theme::status_running()
                        } else {
                            theme::overlay_hint()
                        })
                        .add_modifier(if on {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Span::raw(" "),
                Span::styled(editor.label(row), label_style),
            ];
            if let Some(hint) = hint {
                spans.push(Span::styled(
                    format!("  {hint}"),
                    Style::default().fg(theme::peach()),
                ));
            }
            Line::from(spans)
        }
        GrantRow::Preset | GrantRow::Mode => {
            let mut spans = vec![
                marker,
                Span::styled(format!("{:<LABEL_WIDTH$}", editor.label(row)), label_style),
            ];
            if selected {
                spans.push(Span::styled("‹ ", accent));
            }
            spans.push(Span::styled(value, Style::default().fg(theme::text())));
            if selected {
                spans.push(Span::styled(" ›", accent));
            }
            Line::from(spans)
        }
        _ => {
            let typing = selected && !editor.typed().is_empty();
            let mut spans = vec![
                marker,
                Span::styled(format!("{:<LABEL_WIDTH$}", editor.label(row)), label_style),
                Span::styled(value, Style::default().fg(theme::text())),
            ];
            if typing {
                spans.push(Span::styled("▏", accent));
            }
            Line::from(spans)
        }
    }
}

/// Every editor line (header, sections and rows) and the line index of the
/// selected row.
pub(crate) fn grant_lines(
    editor: &GrantEditor,
    width: usize,
    bg: Color,
) -> (Vec<Line<'static>>, usize) {
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "GRANT",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · {}", editor.summary()),
            Style::default().fg(theme::dim_metadata()),
        ),
    ])];
    let selected = editor.selected_row();
    let mut selected_line = 0;
    let mut section = None;
    for row in editor.rows() {
        if section != Some(row.section()) {
            section = Some(row.section());
            lines.push(session::section_header_line(
                section_glyph(row.section()),
                row.section().title(),
                &section_count(editor, row.section()),
                section_color(row.section()),
                bg,
                width,
            ));
        }
        if row == selected {
            selected_line = lines.len();
        }
        lines.push(row_line(editor, row, row == selected));
    }
    (lines, selected_line)
}

/// The pinned footer: what the selected row does, advisories, then any error.
fn footer_lines(editor: &GrantEditor) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        editor.description(editor.selected_row()),
        Style::default()
            .fg(theme::subtext0())
            .add_modifier(Modifier::ITALIC),
    ))];
    for warning in editor.warnings() {
        lines.push(Line::from(Span::styled(
            warning,
            Style::default().fg(theme::peach()),
        )));
    }
    if let Some(error) = &editor.error {
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default().fg(theme::error_status()),
        )));
    }
    lines
}

fn wrapped_height(lines: &[Line<'static>], width: u16) -> u16 {
    let rows = Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .line_count(width.max(1));
    u16::try_from(rows).unwrap_or(u16::MAX)
}

/// Draw the editor into `area`: the scrolling rows above a pinned footer.
pub(crate) fn render_grant(frame: &mut Frame, area: Rect, editor: &GrantEditor, bg: Color) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let footer = footer_lines(editor);
    let footer_height = wrapped_height(&footer, area.width)
        .min(area.height.saturating_sub(3))
        .max(u16::from(area.height > 3));
    let rows_height = area.height.saturating_sub(footer_height);
    let (lines, selected_line) = grant_lines(editor, usize::from(area.width), bg);
    let height = usize::from(rows_height);
    let max_offset = lines.len().saturating_sub(height);
    let offset = selected_line.saturating_sub(height / 2).min(max_offset);
    let visible: Vec<Line<'static>> = lines.into_iter().skip(offset).take(height).collect();
    frame.render_widget(
        Paragraph::new(visible).style(Style::default().bg(bg)),
        Rect::new(area.x, area.y, area.width, rows_height),
    );
    if footer_height > 0 {
        frame.render_widget(
            Paragraph::new(footer)
                .wrap(Wrap { trim: false })
                .style(Style::default().bg(bg)),
            Rect::new(area.x, area.y + rows_height, area.width, footer_height),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::global_manager_command::{
        default_allowed_launches, default_project_policy,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use rsi_common::harness_manager::HarnessManagerScopeModeV1;

    fn render(editor: &GrantEditor, width: u16, height: u16) -> String {
        let _theme = crate::ui::theme::pin_theme_state();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                render_grant(frame, frame.area(), editor, theme::overlay_bg());
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn editor() -> GrantEditor {
        GrantEditor::for_manager(
            default_project_policy(),
            HarnessManagerScopeModeV1::Project,
            default_allowed_launches(),
        )
    }

    #[test]
    fn the_editor_shows_sections_values_and_the_selected_rows_description() {
        let text = render(&editor(), 90, 40);
        for needle in [
            "GRANT · Execute · 8 capabilities",
            "AUTHORITY",
            "▸ Preset",
            "‹ Execute ›",
            "CAPABILITIES  8/15",
            "[x] WorkPlan",
            "[ ] Deploy",
            "ALLOWANCES",
            "Sessions",
            "LAUNCHES  0/10",
            "Observe watches; Execute runs work",
        ] {
            assert!(text.contains(needle), "{needle} in\n{text}");
        }
    }

    #[test]
    fn a_launch_the_ancestor_grant_does_not_cover_carries_the_hint() {
        let launches = default_allowed_launches();
        let mut policy = default_project_policy();
        policy.allowed_launches = launches[..2].to_vec();
        let mut editor =
            GrantEditor::for_manager(policy, HarnessManagerScopeModeV1::Project, launches.clone());
        editor.set_ancestor_launches(Some(vec![launches[0].clone()]));
        editor.select_row(GrantRow::Launch(1));
        let text = render(&editor, 120, 50);
        let line = |needle: &str| {
            text.lines()
                .find(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("{needle} in\n{text}"))
                .to_string()
        };
        assert!(
            line(&launches[1].display()).contains("not granted by global manager"),
            "{text}"
        );
        assert!(
            text.contains("add with :manager portfolio launch add"),
            "the footer names the fix:\n{text}"
        );
    }

    #[test]
    fn a_short_area_scrolls_to_keep_the_selected_row_visible() {
        let mut editor = editor();
        editor.select_row(GrantRow::Launch(9));
        let text = render(&editor, 90, 12);
        assert!(
            text.contains("▸ [ ] Codex/gpt-6-luna/xhigh"),
            "the last launch is on screen:\n{text}"
        );
        assert!(
            text.contains("Launches this manager may use"),
            "the footer stays pinned:\n{text}"
        );
    }
}
