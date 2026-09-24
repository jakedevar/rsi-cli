//! Prompt Creator/Editor UI rendering.
//!
//! Full-screen view for managing prompt files stored in `~/.flywheel/prompts/`.
//! List mode shows prompt cards; editor mode splits into list + file viewer.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;
use crate::ui::theme;

/// Main entry point for rendering the prompt creator view.
pub fn render_prompt_creator(frame: &mut Frame, area: Rect, focused: bool, app: &mut App) {
    frame.render_widget(Clear, area);

    if app.prompt_creator_state.editing {
        render_split_view(frame, area, focused, app);
    } else {
        render_list_view(frame, area, focused, app);
    }
}

/// Render the full-width prompt list (no editor open).
fn render_list_view(frame: &mut Frame, area: Rect, focused: bool, app: &mut App) {
    let centered = center_area(area, 60);
    render_prompt_list(frame, centered, focused, app);
}

/// Render the split view: list on left, editor on right.
fn render_split_view(frame: &mut Frame, area: Rect, focused: bool, app: &mut App) {
    let tab = &app.tabs[app.active_tab];
    let list_pct = tab.session_list_width_pct.max(25).min(50) as u16;

    let chunks = Layout::horizontal([
        Constraint::Percentage(list_pct),
        Constraint::Percentage(100 - list_pct),
    ])
    .split(area);

    render_prompt_list(frame, chunks[0], false, app);
    render_editor(frame, chunks[1], focused, app);
}

/// Render the prompt list with cards.
fn render_prompt_list(frame: &mut Frame, area: Rect, focused: bool, app: &mut App) {
    let title = build_title(app);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default().fg(if focused && !app.prompt_creator_state.editing {
                theme::overlay_border()
            } else {
                theme::surface1()
            }),
        )
        .title(title);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 2 || inner.width < 10 {
        return;
    }

    // Model dropdown rendering (if open)
    if let Some(ref dropdown) = app.prompt_creator_state.model_dropdown {
        if dropdown.open {
            render_model_dropdown(frame, inner, dropdown);
            return;
        }
    }

    let prompts = &app.prompt_creator_state.prompts;

    if prompts.is_empty() {
        let hint = Paragraph::new(Line::from(vec![Span::styled(
            "No prompts yet. Press Ctrl+n to create one.",
            Style::default().fg(theme::subtext0()),
        )]));
        frame.render_widget(hint, inner);
        return;
    }

    let selected = app.prompt_creator_state.selected_index;
    let card_height: usize = 4; // filename, preview, meta, separator
    let visible_cards = (inner.height as usize) / card_height;

    // Scroll offset management
    if visible_cards > 0 {
        if selected < app.prompt_creator_state.scroll_offset {
            app.prompt_creator_state.scroll_offset = selected;
        } else if selected >= app.prompt_creator_state.scroll_offset + visible_cards {
            app.prompt_creator_state.scroll_offset = selected - visible_cards + 1;
        }
    }

    let scroll = app.prompt_creator_state.scroll_offset;
    let mut y = inner.y;

    for (i, prompt) in prompts.iter().enumerate().skip(scroll) {
        if y + card_height as u16 > inner.y + inner.height {
            break;
        }

        let is_selected = i == selected;
        let fg = if is_selected {
            theme::text()
        } else {
            theme::subtext0()
        };

        // Line 1: filename (bold)
        let filename_line = Line::from(vec![Span::styled(
            &prompt.filename,
            Style::default().fg(fg).add_modifier(Modifier::BOLD),
        )]);
        frame.render_widget(
            Paragraph::new(filename_line),
            Rect::new(inner.x + 1, y, inner.width.saturating_sub(2), 1),
        );

        // Line 2: first line preview (dimmed)
        let preview = if prompt.first_line.is_empty() {
            "(empty)".to_string()
        } else {
            let max_len = inner.width.saturating_sub(4) as usize;
            if prompt.first_line.len() > max_len {
                format!("{}...", &prompt.first_line[..max_len.saturating_sub(3)])
            } else {
                prompt.first_line.clone()
            }
        };
        let preview_line = Line::from(vec![Span::styled(
            preview,
            Style::default().fg(theme::subtext0()),
        )]);
        frame.render_widget(
            Paragraph::new(preview_line),
            Rect::new(inner.x + 1, y + 1, inner.width.saturating_sub(2), 1),
        );

        // Line 3: metadata (relative time + size)
        let time_str = crate::prompt_creator::relative_time(prompt.modified_at);
        let size_str = crate::prompt_creator::format_size(prompt.size_bytes);
        let meta_line = Line::from(vec![Span::styled(
            format!("{time_str}  {size_str}"),
            Style::default().fg(theme::subtext0()),
        )]);
        frame.render_widget(
            Paragraph::new(meta_line),
            Rect::new(inner.x + 1, y + 2, inner.width.saturating_sub(2), 1),
        );

        // Line 4: separator
        let sep = if is_selected { "─" } else { " " };
        let sep_line = Line::from(vec![Span::styled(
            sep.repeat(inner.width.saturating_sub(2) as usize),
            Style::default().fg(theme::surface1()),
        )]);
        frame.render_widget(
            Paragraph::new(sep_line),
            Rect::new(inner.x + 1, y + 3, inner.width.saturating_sub(2), 1),
        );

        y += card_height as u16;
    }
}

/// Render the editor pane (right side in split view).
fn render_editor(frame: &mut Frame, area: Rect, focused: bool, app: &mut App) {
    let title = app
        .prompt_creator_viewer
        .as_ref()
        .map(|v| {
            let name = v
                .file_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("untitled");
            let dirty = if v.dirty { " [+]" } else { "" };
            format!(" {name}{dirty} ")
        })
        .unwrap_or_else(|| " Editor ".to_string());

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused {
            theme::overlay_border()
        } else {
            theme::surface1()
        }))
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.height < 2 || inner.width < 10 {
        return;
    }

    if let Some(ref mut viewer) = app.prompt_creator_viewer {
        super::file_renderer::render_file_viewer_content(frame, inner, viewer, focused);
    }
}

/// Build the title line for the prompt list, including model badge.
fn build_title(app: &App) -> Line<'static> {
    let mut spans = vec![Span::styled(
        " Prompts ",
        Style::default()
            .fg(theme::overlay_title())
            .add_modifier(Modifier::BOLD),
    )];

    // Model badge
    if let Some(ref model) = app.prompt_creator_state.selected_model {
        spans.push(Span::styled(
            format!(" [{model}] "),
            Style::default().fg(theme::blue()),
        ));
    } else if let Some(ref dropdown) = app.prompt_creator_state.model_dropdown {
        if let Some((id, _)) = dropdown.models.get(dropdown.selected_index) {
            spans.push(Span::styled(
                format!(" [{id}] "),
                Style::default().fg(theme::blue()),
            ));
        }
    }

    Line::from(spans)
}

/// Render the model dropdown overlay.
fn render_model_dropdown(
    frame: &mut Frame,
    area: Rect,
    dropdown: &crate::types::ModelDropdownState,
) {
    let max_items = area.height.saturating_sub(1) as usize;
    let items: Vec<Line> = dropdown
        .models
        .iter()
        .enumerate()
        .take(max_items)
        .map(|(i, (id, label))| {
            let is_selected = i == dropdown.selected_index;
            let prefix = if is_selected { "▸ " } else { "  " };
            let display = if label.is_empty() || label == id {
                id.clone()
            } else {
                format!("{label} ({id})")
            };
            Line::from(vec![Span::styled(
                format!("{prefix}{display}"),
                Style::default().fg(if is_selected {
                    theme::text()
                } else {
                    theme::subtext0()
                }),
            )])
        })
        .collect();

    let paragraph = Paragraph::new(items);
    frame.render_widget(paragraph, area);
}

/// Center an area horizontally at a given percentage width.
fn center_area(area: Rect, pct: u16) -> Rect {
    let pct = pct.min(100);
    let width = (area.width as u32 * pct as u32 / 100) as u16;
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    Rect::new(x, area.y, width, area.height)
}
