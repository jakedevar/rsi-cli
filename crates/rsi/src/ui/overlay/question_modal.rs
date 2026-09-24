use crate::app::App;
use crate::types::PopupMode;
use crate::ui::theme;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

pub fn render_question_modal(
    frame: &mut Frame,
    area: Rect,
    _app: &App,
    questions: &[rsi_common::types::QuestionItem],
    current_question: usize,
    cursor: &[usize],
    selections: &[crate::types::QuestionSelection],
    textarea: &tui_textarea::TextArea<'static>,
    mode: &PopupMode,
) {
    let q = &questions[current_question];

    // Calculate layout
    let total_options = q.options.len();
    let height_needed = 2 + 1 + total_options as u16 + 1 + 3 + 2; // borders + question + options + spacing + textarea + footer
    let popup_width = (area.width * 60 / 100).clamp(60, 120);
    let popup_height = height_needed.max(15);

    let x = area.x + (area.width.saturating_sub(popup_width)) / 2;
    let y = area.y + (area.height / 3).saturating_sub(popup_height / 2).max(1);
    let popup_area = Rect::new(x, y, popup_width, popup_height);

    frame.render_widget(Clear, popup_area);

    let title = format!(" Question {}/{} ", current_question + 1, questions.len());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::overlay_border()))
        .title(title);

    let inner_area = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),                    // Question text + blank line
            Constraint::Length(total_options as u16), // Options
            Constraint::Length(1),                    // Blank line
            Constraint::Min(3),                       // Textarea
            Constraint::Length(1),                    // Footer
        ])
        .split(inner_area);

    // 1. Render Question
    let question_text = Paragraph::new(q.question.as_str()).style(
        Style::default()
            .fg(theme::text())
            .add_modifier(Modifier::BOLD),
    );
    frame.render_widget(question_text, chunks[0]);

    // 2. Render Options
    if total_options > 0 {
        let mut options_text = Vec::new();
        for (i, opt) in q.options.iter().enumerate() {
            let is_cursor = cursor[current_question] == i;
            let is_selected = match &selections[current_question] {
                crate::types::QuestionSelection::Single(opt) => *opt == Some(i),
                crate::types::QuestionSelection::Multi(opts) => opts.contains(&i),
            };

            let prefix = if q.multi_select {
                if is_selected { "[x] " } else { "[ ] " }
            } else if is_cursor {
                " ›  "
            } else {
                "    "
            };

            let (style, prefix_style) = if is_cursor {
                (
                    Style::default()
                        .fg(theme::text())
                        .add_modifier(Modifier::BOLD),
                    Style::default()
                        .fg(theme::peach())
                        .add_modifier(Modifier::BOLD),
                )
            } else if is_selected {
                (
                    Style::default().fg(theme::text()),
                    Style::default().fg(theme::peach()),
                )
            } else {
                (
                    Style::default().fg(theme::subtext0()),
                    Style::default().fg(theme::subtext0()),
                )
            };

            options_text.push(Line::from(vec![
                Span::styled(prefix, prefix_style),
                Span::styled(format!("{}. {}", i + 1, opt.label), style),
                Span::styled(
                    format!(" - {}", opt.description),
                    Style::default().fg(theme::surface2()),
                ),
            ]));
        }
        frame.render_widget(Paragraph::new(options_text), chunks[1]);
    }

    // 3. Render Textarea
    let mut ta = textarea.clone();
    let is_insert = matches!(mode, PopupMode::Insert);
    let border_color = if is_insert {
        theme::peach()
    } else {
        theme::overlay_border()
    };
    let border_title = if is_insert {
        " INSERT "
    } else {
        " Other / Free text "
    };

    ta.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(border_title),
    );

    if is_insert {
        ta.set_cursor_line_style(Style::default().add_modifier(Modifier::UNDERLINED));
        ta.set_cursor_style(Style::default().bg(theme::text()).fg(theme::base()));
    } else {
        ta.set_cursor_line_style(Style::default());
        ta.set_cursor_style(Style::default());
    }

    frame.render_widget(&ta, chunks[3]);

    // 4. Render Footer
    let footer_text = if is_insert {
        "  INSERT  Esc normal "
    } else if q.multi_select {
        "  NORMAL  Space toggle · Enter next · Ctrl+Enter submit · d decline · Esc dismiss  (select all that apply)"
    } else {
        "  NORMAL  q close · Ctrl+Enter submit · j/k navigate · 1-9 select · i insert · d decline · Esc dismiss"
    };

    let footer = Paragraph::new(footer_text).style(Style::default().fg(theme::subtext0()));
    frame.render_widget(footer, chunks[4]);
}
