//! Model dropdown widget rendering.
//!
//! Renders an anchor-relative dropdown below a given `Rect`. Uses painter's
//! algorithm (rendered last to overlap other content). Fixed width of 50 chars.

use crate::types::ModelDropdownState;
use crate::ui::{glyphs, theme};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};

/// Render the model dropdown anchored below `anchor`.
/// Uses fixed width of 50 chars. Rendered via painter's algorithm (call last).
pub fn render_model_dropdown(
    frame: &mut Frame,
    full_area: Rect,
    anchor: Rect,
    state: &ModelDropdownState,
    current_model: Option<&str>,
    provider_available: bool,
) {
    if state.models.is_empty() && !state.open {
        return;
    }

    let fixed_width: u16 = 50;
    // height = title border(1) + models + hint(1) + bottom border(1)
    let content_height = u16::try_from(state.models.len())
        .unwrap_or(u16::MAX)
        .saturating_add(1); // models + hint
    let total_height = content_height.saturating_add(2).min(full_area.height); // +2 for borders

    let width = fixed_width.min(full_area.width);

    // X: left-aligned with anchor, clamped to fit
    let x = anchor
        .x
        .min(full_area.x + full_area.width.saturating_sub(width));

    // Y: prefer below anchor, fall back to above if it would go off screen
    let below_y = anchor.y + anchor.height;
    let (y, height) = if below_y + total_height <= full_area.y + full_area.height {
        (below_y, total_height)
    } else if anchor.y >= total_height {
        // Render above the anchor
        (anchor.y - total_height, total_height)
    } else {
        // Clamp to available space below
        let avail = (full_area.y + full_area.height).saturating_sub(below_y);
        if avail >= 4 {
            (below_y, avail)
        } else {
            // Not enough space; try above with clamping
            let avail_above = anchor.y.saturating_sub(full_area.y);
            if avail_above >= 4 {
                (anchor.y.saturating_sub(avail_above), avail_above)
            } else {
                return; // truly no space
            }
        }
    };

    let popup_area = Rect::new(x, y, width, height);

    // Clear + draw block
    frame.render_widget(Clear, popup_area);

    let provider_label = provider_label(state.provider);
    let title_style = Style::default()
        .fg(theme::overlay_title())
        .add_modifier(Modifier::BOLD);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::overlay_border()))
        .style(Style::default().bg(theme::overlay_bg()))
        .title(Line::from(vec![
            Span::styled(" Model [", title_style),
            Span::styled(
                glyphs::provider_glyph(state.provider),
                title_style.fg(glyphs::provider_color(state.provider)),
            ),
            Span::styled(
                format!(
                    " {}{}] ",
                    provider_label,
                    if provider_available { "" } else { " (offline)" }
                ),
                title_style,
            ),
        ]))
        .padding(Padding::horizontal(1));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if inner.height < 2 {
        return;
    }

    // Render model rows
    let max_rows = inner.height.saturating_sub(1) as usize; // reserve 1 for hint
    let start = state
        .selected_index
        .saturating_sub(max_rows / 2)
        .min(state.models.len().saturating_sub(max_rows));
    for (i, (model_id, display_name)) in state.models.iter().enumerate().skip(start).take(max_rows)
    {
        let is_selected = i == state.selected_index;
        let is_current = current_model == Some(model_id.as_str());

        let marker = if is_current { "\u{2713} " } else { "  " };
        let number = format!("{} ", i + 1);

        let row_style = if is_selected {
            Style::default()
                .fg(theme::text())
                .bg(theme::surface2())
                .add_modifier(Modifier::REVERSED)
        } else {
            Style::default().bg(theme::overlay_bg())
        };

        let line = Line::from(vec![
            Span::styled(marker, Style::default().fg(theme::green())),
            Span::styled(number, Style::default().fg(theme::overlay_hint())),
            Span::styled(
                display_name.as_str(),
                Style::default()
                    .fg(theme::text())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                model_id.as_str(),
                Style::default().fg(theme::overlay_hint()),
            ),
        ]);

        let row_area = Rect::new(inner.x, inner.y + (i - start) as u16, inner.width, 1);
        frame.render_widget(Paragraph::new(line).style(row_style), row_area);
    }

    // Hint bar at the bottom of inner area
    let hint_y = inner.y + inner.height - 1;
    let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
    let hint = Line::from(vec![Span::styled(
        "Tab: provider  j/k: navigate  Enter: select  1-9: direct  Esc: cancel",
        Style::default().fg(theme::overlay_hint()),
    )]);
    frame.render_widget(Paragraph::new(hint), hint_area);
}

/// Get a human-readable label for a provider.
fn provider_label(provider: rsi_common::types::SessionProvider) -> &'static str {
    use rsi_common::types::SessionProvider;
    match provider {
        SessionProvider::Claude => "Claude",
        SessionProvider::Codex => "Codex",
        SessionProvider::Pioneer => "Pioneer",
        SessionProvider::OpenRouter => "OpenRouter",
        SessionProvider::Bedrock => "Bedrock",
        SessionProvider::Local => "Local",
        SessionProvider::Antigravity => "Antigravity",
        SessionProvider::CodexAppServer => "Codex(AS)",
        SessionProvider::Harness => "Harness",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use rsi_common::types::SessionProvider;

    #[test]
    fn model_dropdown_title_places_colored_glyph_beside_provider_name() {
        let providers = [
            SessionProvider::Claude,
            SessionProvider::Codex,
            SessionProvider::Pioneer,
            SessionProvider::OpenRouter,
            SessionProvider::Bedrock,
            SessionProvider::Local,
            SessionProvider::Antigravity,
            SessionProvider::CodexAppServer,
            SessionProvider::Harness,
        ];

        for provider in providers {
            for available in [true, false] {
                let state = ModelDropdownState::new(
                    provider,
                    vec![("model-id".into(), "Model name".into())],
                    None,
                );
                let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
                terminal
                    .draw(|frame| {
                        render_model_dropdown(
                            frame,
                            frame.area(),
                            Rect::new(0, 0, 50, 1),
                            &state,
                            None,
                            available,
                        );
                    })
                    .unwrap();

                let buffer = terminal.backend().buffer();
                let title_cells = &buffer.content[60..120];
                let title = title_cells
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                let offline = if available { "" } else { " (offline)" };
                assert!(
                    title.contains(&format!(
                        "Model [{} {}{}]",
                        glyphs::provider_glyph(provider),
                        provider_label(provider),
                        offline
                    )),
                    "provider {provider:?}: {title:?}"
                );
                let glyph_cell = title_cells
                    .iter()
                    .find(|cell| cell.symbol() == glyphs::provider_glyph(provider))
                    .unwrap();
                assert_eq!(glyph_cell.fg, glyphs::provider_color(provider));
            }
        }
    }
}
