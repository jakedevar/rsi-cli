//! Model dropdown widget rendering.
//!
//! Renders an anchor-relative dropdown below a given `Rect`. Uses painter's
//! algorithm (rendered last to overlap other content). Width capped at 50 chars.

use crate::types::ModelDropdownState;
use crate::ui::{glyphs, theme};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph};

/// Render the model dropdown anchored below `anchor`.
/// Uses a width capped at 50 chars. Rendered via painter's algorithm (call last).
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
    // Bound large catalogs; selection scrolls within the available model rows.
    // Sized from the whole catalog, not the matches, so the popup holds still
    // while a filter is typed.
    let content_height = (state.models.len().clamp(1, 14) as u16).saturating_add(2); // search + models + hint
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
    } else if anchor.y.saturating_sub(full_area.y) >= total_height {
        // Render above the anchor
        (anchor.y - total_height, total_height)
    } else {
        // Clamp to available space below
        let avail = (full_area.y + full_area.height).saturating_sub(below_y);
        if avail >= 5 {
            (below_y, avail)
        } else {
            // Not enough space; try above with clamping
            let avail_above = anchor.y.saturating_sub(full_area.y);
            if avail_above >= 5 {
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
            Span::styled(" Model  ", title_style),
            Span::styled(
                glyphs::provider_glyph(state.provider),
                title_style.fg(glyphs::provider_color(state.provider)),
            ),
            Span::styled(
                format!(
                    " {}{} ",
                    provider_label,
                    if provider_available { "" } else { " (offline)" }
                ),
                title_style,
            ),
        ]))
        .padding(Padding::horizontal(1));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    render_model_dropdown_content(frame, inner, state, current_model);
}

/// Shared content for anchored pickers and the prompt creator's inline list:
/// a search line, the (filtered) model rows and a key hint.
pub fn render_model_dropdown_content(
    frame: &mut Frame,
    inner: Rect,
    state: &ModelDropdownState,
    current_model: Option<&str>,
) {
    if inner.height < 3 || inner.width == 0 {
        return;
    }
    let indices = state.filtered_indices();
    frame.render_widget(
        Paragraph::new(search_line(state, indices.len(), inner.width)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );

    let max_rows = inner.height.saturating_sub(2) as usize;
    let selected = indices
        .iter()
        .position(|i| *i == state.selected_index)
        .unwrap_or(0);
    let start = selected
        .saturating_sub(max_rows / 2)
        .min(indices.len().saturating_sub(max_rows));
    if indices.is_empty() {
        let message = if state.models.is_empty() {
            "No models in this catalog".to_string()
        } else {
            format!(
                "No models match \u{201c}{}\u{201d}",
                state.filter_query.trim()
            )
        };
        frame.render_widget(
            Paragraph::new(message).style(Style::default().fg(theme::warning_status())),
            Rect::new(inner.x, inner.y + 1, inner.width, 1),
        );
    }
    let terms = state.filter_terms();
    let name_style = Style::default()
        .fg(theme::text())
        .add_modifier(Modifier::BOLD);
    let id_style = Style::default().fg(theme::overlay_hint());
    let match_style = Style::default()
        .fg(theme::accent())
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    for (visible_index, source_index) in indices.iter().enumerate().skip(start).take(max_rows) {
        let (model_id, display_name) = &state.models[*source_index];
        let is_selected = *source_index == state.selected_index;
        let marker = if current_model == Some(model_id.as_str()) {
            "✓ "
        } else {
            "  "
        };
        let row_style = if is_selected {
            Style::default()
                .fg(theme::text())
                .bg(theme::surface2())
                .add_modifier(Modifier::REVERSED)
        } else {
            Style::default().bg(theme::overlay_bg())
        };
        let mut spans = vec![
            Span::styled(marker, Style::default().fg(theme::green())),
            Span::styled(
                format!("{} ", visible_index + 1),
                Style::default().fg(theme::overlay_hint()),
            ),
        ];
        spans.extend(highlighted_spans(
            display_name,
            &terms,
            name_style,
            match_style,
        ));
        spans.push(Span::raw("  "));
        spans.extend(highlighted_spans(model_id, &terms, id_style, match_style));
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(row_style),
            Rect::new(
                inner.x,
                inner.y + 1 + (visible_index - start) as u16,
                inner.width,
                1,
            ),
        );
    }

    let hint = if state.filter_editing {
        MODEL_DROPDOWN_SEARCH_HINT
    } else if !state.filter_query.is_empty() {
        MODEL_DROPDOWN_FILTERED_HINT
    } else {
        MODEL_DROPDOWN_HINT
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(theme::overlay_hint())),
        Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
    );
}

/// The picker's first row: a `/` prompt with the query (or a placeholder)
/// on the left and the match count on the right.
fn search_line(state: &ModelDropdownState, matches: usize, width: u16) -> Line<'static> {
    let total = state.models.len();
    let searching = state.filter_editing || !state.filter_query.is_empty();
    let prompt_style = Style::default()
        .fg(if state.filter_editing {
            theme::accent()
        } else {
            theme::overlay_hint()
        })
        .add_modifier(Modifier::BOLD);
    let mut left = vec![Span::styled("/ ", prompt_style)];
    let count = if searching {
        if state.filter_editing && crate::field_edit::standard_frame() {
            left.extend(
                state
                    .filter_cursor
                    .spans(&state.filter_query, Style::default().fg(theme::text())),
            );
        } else {
            left.push(Span::styled(
                state.filter_query.clone(),
                Style::default().fg(theme::text()),
            ));
            if state.filter_editing {
                left.push(Span::styled("▏", Style::default().fg(theme::accent())));
            }
        }
        format!("{matches} of {total}")
    } else {
        left.push(Span::styled(
            "filter",
            Style::default()
                .fg(theme::overlay_hint())
                .add_modifier(Modifier::ITALIC),
        ));
        if total == 1 {
            "1 model".to_string()
        } else {
            format!("{total} models")
        }
    };
    let count_style = Style::default().fg(if searching && matches == 0 {
        theme::warning_status()
    } else {
        theme::overlay_hint()
    });
    let used: usize = left.iter().map(|span| span.width()).sum();
    let gap = (width as usize).saturating_sub(used + count.chars().count());
    if gap > 0 {
        left.push(Span::raw(" ".repeat(gap)));
        left.push(Span::styled(count, count_style));
    }
    Line::from(left)
}

/// Split `text` into spans, styling every case-insensitive occurrence of a
/// search term with `matched`. Terms that only match with punctuation
/// ignored (`opus45`) leave the text unhighlighted.
fn highlighted_spans<'a>(
    text: &'a str,
    terms: &[String],
    base: Style,
    matched: Style,
) -> Vec<Span<'a>> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let lower: Vec<char> = chars
        .iter()
        .map(|(_, c)| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    let mut mask = vec![false; chars.len()];
    for term in terms {
        let needle: Vec<char> = term.chars().collect();
        if needle.is_empty() || needle.len() > lower.len() {
            continue;
        }
        for start in 0..=lower.len() - needle.len() {
            if lower[start..start + needle.len()] == needle[..] {
                mask[start..start + needle.len()].fill(true);
            }
        }
    }
    let mut spans = Vec::new();
    let mut run_start = 0;
    for i in 1..=chars.len() {
        if i == chars.len() || mask[i] != mask[run_start] {
            let from = chars[run_start].0;
            let to = chars.get(i).map_or(text.len(), |(byte, _)| *byte);
            let style = if mask[run_start] { matched } else { base };
            spans.push(Span::styled(&text[from..to], style));
            run_start = i;
        }
    }
    spans
}

/// Key hints sized to the 46-cell interior of the fixed 50-cell popup.
const MODEL_DROPDOWN_HINT: &str = "Tab provider · j/k · Enter/1-9 pick · Esc";
const MODEL_DROPDOWN_SEARCH_HINT: &str = "↑↓ · Enter pick · Tab provider · Esc done";
const MODEL_DROPDOWN_FILTERED_HINT: &str = "j/k · Enter/1-9 pick · / edit · Esc close";

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

    fn render_text(state: &ModelDropdownState, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                render_model_dropdown(
                    frame,
                    frame.area(),
                    Rect::new(0, 0, width, 1),
                    state,
                    None,
                    true,
                );
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn model_dropdown_render_shows_query_count_and_filtered_number() {
        let mut state = ModelDropdownState::new(
            SessionProvider::Claude,
            vec![
                ("first".into(), "First".into()),
                ("needle".into(), "Needle Two".into()),
            ],
            None,
        );
        state.filter_query = "needle".into();
        state.filter_editing = true;
        state.reconcile_filter_selection();
        let text = render_text(&state, 60, 8);
        assert!(text.contains("/ needle▏"), "{text}");
        assert!(text.contains("1 of 2"), "{text}");
        assert!(text.contains("1 Needle Two"), "{text}");
        assert!(text.contains(MODEL_DROPDOWN_SEARCH_HINT), "{text}");

        state.filter_editing = false;
        let text = render_text(&state, 60, 8);
        assert!(text.contains("/ needle "), "{text}");
        assert!(text.contains(MODEL_DROPDOWN_FILTERED_HINT), "{text}");
    }

    #[test]
    fn model_dropdown_render_empty_results_and_small_scrolled_catalog() {
        let models = (0..100)
            .map(|i| (format!("id-{i}"), format!("Model {i}")))
            .collect();
        let mut state = ModelDropdownState::new(SessionProvider::Claude, models, Some("id-99"));
        let text = render_text(&state, 50, 6);
        assert!(text.contains("100 Model 99"), "{text}");
        assert!(text.contains("/ filter"), "{text}");
        assert!(text.contains("100 models"), "{text}");
        state.filter_query = "unmatched".into();
        let text = render_text(&state, 50, 6);
        assert!(text.contains("/ unmatched"), "{text}");
        assert!(text.contains("0 of 100"), "{text}");
        assert!(
            text.contains("No models match \u{201c}unmatched\u{201d}"),
            "{text}"
        );
    }

    /// Filtering does not resize the popup, so typing never makes it jump.
    #[test]
    fn model_dropdown_popup_height_holds_while_filtering() {
        let models: Vec<(String, String)> = (0..6)
            .map(|i| (format!("id-{i}"), format!("Model {i}")))
            .collect();
        let mut state = ModelDropdownState::new(SessionProvider::Claude, models, None);
        let rows_with_border = |state: &ModelDropdownState| {
            render_text(state, 50, 20)
                .chars()
                .collect::<Vec<_>>()
                .chunks(50)
                .filter(|row| row.contains(&'\u{2502}'))
                .count()
        };
        let before = rows_with_border(&state);
        state.filter_query = "Model 3".into();
        state.filter_editing = true;
        assert_eq!(rows_with_border(&state), before);
    }

    #[test]
    fn model_dropdown_render_highlights_matched_text() {
        let _theme = crate::ui::theme::pin_theme_state();
        let mut state = ModelDropdownState::new(
            SessionProvider::Claude,
            vec![("claude-opus-5-5".into(), "Opus 5.5".into())],
            None,
        );
        state.filter_query = "OPUS".into();
        let mut terminal = Terminal::new(TestBackend::new(50, 8)).unwrap();
        terminal
            .draw(|frame| {
                render_model_dropdown(
                    frame,
                    frame.area(),
                    Rect::new(0, 0, 50, 1),
                    &state,
                    None,
                    true,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row: Vec<_> = (0..8)
            .map(|y| (0..50).map(|x| buffer[(x, y)].clone()).collect::<Vec<_>>())
            .find(|cells| {
                cells
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .contains("Opus 5.5")
            })
            .expect("model row");
        let text: String = row.iter().map(|cell| cell.symbol()).collect();
        let name_at = text.find("Opus 5.5").unwrap();
        let name_col = text[..name_at].chars().count();
        for offset in 0..4 {
            assert!(
                row[name_col + offset]
                    .modifier
                    .contains(Modifier::UNDERLINED),
                "matched cell {offset} of {text:?}"
            );
        }
        assert!(
            row[name_col + 5].modifier.contains(Modifier::BOLD)
                && !row[name_col + 5].modifier.contains(Modifier::UNDERLINED),
            "unmatched name text keeps the base style"
        );
    }

    #[test]
    fn highlighted_spans_cover_every_term_occurrence() {
        let base = Style::default();
        let hit = Style::default().add_modifier(Modifier::UNDERLINED);
        let spans = highlighted_spans("gpt-5-gpt", &["gpt".into(), "5".into()], base, hit);
        let parts: Vec<(&str, bool)> = spans
            .iter()
            .map(|span| (span.content.as_ref(), span.style == hit))
            .collect();
        assert_eq!(
            parts,
            vec![
                ("gpt", true),
                ("-", false),
                ("5", true),
                ("-", false),
                ("gpt", true)
            ]
        );
        let spans = highlighted_spans("Opus 4.5", &[], base, hit);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, "Opus 4.5");
    }

    #[test]
    fn model_dropdown_render_fits_offset_area_above_anchor() {
        let state = ModelDropdownState::new(
            SessionProvider::Claude,
            vec![("id".into(), "Visible Model".into())],
            None,
        );
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal
            .draw(|frame| {
                render_model_dropdown(
                    frame,
                    Rect::new(5, 7, 50, 9),
                    Rect::new(5, 14, 50, 1),
                    &state,
                    None,
                    true,
                );
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Visible Model"), "{text}");
    }

    #[test]
    fn model_dropdown_title_places_colored_glyph_beside_provider_name() {
        // Rendered and expected glyph colours both read the global theme.
        let _theme = crate::ui::theme::pin_theme_state();
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
                        "Model  {} {}{} ",
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

    /// The key hint used to overflow the fixed popup and lose its last keys.
    #[test]
    fn model_dropdown_hint_shows_every_key_inside_the_popup() {
        let state = ModelDropdownState::new(
            SessionProvider::Claude,
            vec![("claude-sonnet-5".into(), "Sonnet 5".into())],
            None,
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
        terminal
            .draw(|frame| {
                render_model_dropdown(
                    frame,
                    frame.area(),
                    Rect::new(0, 0, 1, 1),
                    &state,
                    None,
                    true,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..8)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        assert!(
            rows.iter().any(|row| row.contains(MODEL_DROPDOWN_HINT)),
            "hint must render whole:\n{}",
            rows.join("\n")
        );
    }

    #[test]
    fn model_dropdown_checks_the_current_model() {
        let state = ModelDropdownState::new(
            SessionProvider::Claude,
            vec![
                ("claude-opus-5-5".into(), "Opus 5.5".into()),
                ("claude-sonnet-5".into(), "Sonnet 5".into()),
            ],
            Some("claude-sonnet-5"),
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
        terminal
            .draw(|frame| {
                render_model_dropdown(
                    frame,
                    frame.area(),
                    Rect::new(0, 0, 1, 1),
                    &state,
                    Some("claude-sonnet-5"),
                    true,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let current = (0..8)
            .map(|y| (0..80).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .find(|row| row.contains("claude-sonnet-5"))
            .expect("current model row");
        assert!(current.contains("\u{2713} 2 Sonnet 5"), "{current:?}");
    }
}
