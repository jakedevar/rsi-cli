//! Rendering of the operator to-do pop-up (#2950). State and keys live in
//! `crate::todo_panel` and `crate::overlay::todo_popup`.

use crate::app::App;
use crate::todo_panel::{
    TodoLink, TodoPopupState, age_label, hard_wrap, priority_label, source_label,
};
use crate::ui::theme;
use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};
use rsi_common::operator_todos::{OperatorTodoStatusV1, OperatorTodoV1};

use super::fixed_centered_rect;

/// The footer row: the prompt while a close waits for `y`, else the actions.
/// `undo_left` is the seconds an undo is still offered, when it is.
#[must_use]
pub(super) fn footer_text(
    state: &TodoPopupState,
    item: Option<&OperatorTodoV1>,
    undo_left: Option<i64>,
) -> String {
    if let Some(status) = state.confirm {
        let verb = match status {
            OperatorTodoStatusV1::Dismissed => "Dismiss",
            _ => "Mark done",
        };
        let title = item.map_or("", |item| item.title.as_str());
        return format!("{verb} “{title}”?  y/Enter yes · n/Esc no");
    }
    let mut footer = String::from("y yank · d done · x dismiss");
    if item.is_some_and(|item| item.link.as_deref().and_then(TodoLink::parse).is_some()) {
        footer.push_str(" · o open link");
    }
    if let Some(seconds) = undo_left {
        footer.push_str(&format!(" · u undo ({seconds}s)"));
    }
    footer.push_str(" · n/p item · j/k scroll · Esc close");
    footer
}

/// The pop-up body for `item`, wrapped to `width` cells.
#[must_use]
pub(super) fn body_lines(
    item: &OperatorTodoV1,
    now: DateTime<Utc>,
    project: Option<&str>,
    width: usize,
) -> Vec<Line<'static>> {
    let dim = Style::default().fg(theme::dim_metadata());
    let text = Style::default().fg(theme::text());
    let mut lines: Vec<Line<'static>> = hard_wrap(&item.title, width)
        .into_iter()
        .map(|row| Line::from(Span::styled(row, text.add_modifier(Modifier::BOLD))))
        .collect();
    let mut meta = format!(
        "P{} {} · {} · {}",
        item.priority,
        priority_label(item.priority),
        age_label(now, item.created_at),
        source_label(item, project)
    );
    if let Some(link) = item.link.as_deref() {
        meta.push_str(" · ");
        meta.push_str(&TodoLink::parse(link).map_or_else(|| link.to_string(), |l| l.label()));
    }
    for row in hard_wrap(&meta, width) {
        lines.push(Line::from(Span::styled(row, dim)));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled("Command", dim)));
    for row in hard_wrap(&item.how, width.saturating_sub(2)) {
        lines.push(Line::from(vec![
            Span::styled("  ", dim),
            Span::styled(row, text),
        ]));
    }
    lines
}

/// Draw the pop-up over the main layout.
pub(super) fn render_todo_popup(frame: &mut Frame, area: Rect, app: &App, state: &TodoPopupState) {
    let popup_w = area.width.saturating_sub(8).clamp(20, 100);
    let max_h = area.height.saturating_sub(4).max(6);
    let inner_w = usize::from(popup_w.saturating_sub(4)).max(8);
    let now = Utc::now();
    let selected = app.todo_panel.selected();
    let project = selected
        .and_then(|item| item.project_id)
        .and_then(|id| app.projects.iter().find(|p| p.id == id))
        .map(|p| p.name.clone());
    let body = match selected {
        Some(item) => body_lines(item, now, project.as_deref(), inner_w),
        None => vec![Line::from(Span::styled(
            "This to-do is no longer open.",
            Style::default().fg(theme::dim_metadata()),
        ))],
    };
    let wanted_h = u16::try_from(body.len())
        .unwrap_or(u16::MAX)
        .saturating_add(4);
    let popup_area = fixed_centered_rect(area, popup_w, wanted_h.clamp(6, max_h));
    frame.render_widget(Clear, popup_area);

    let block = theme::overlay_block()
        .title(Line::from(Span::styled(
            " To-do ",
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )))
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);
    if inner.height < 2 {
        return;
    }
    let body_area = Rect::new(inner.x, inner.y, inner.width, inner.height - 1);
    let visible: Vec<Line<'static>> = body
        .into_iter()
        .skip(state.scroll)
        .take(usize::from(body_area.height))
        .collect();
    frame.render_widget(Paragraph::new(visible), body_area);

    let undo_left = app
        .todo_panel
        .undo_available(now)
        .map(|e| e.seconds_left(now));
    let footer = footer_text(state, selected, undo_left);
    let style = if state.confirm.is_some() {
        Style::default()
            .fg(theme::warning_status())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme::overlay_hint())
    };
    let footer_area = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(footer, style))),
        footer_area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::operator_todos::OperatorTodoListResultV1;
    use uuid::Uuid;

    fn item(how: &str, link: Option<&str>) -> OperatorTodoV1 {
        OperatorTodoV1 {
            id: Uuid::new_v4(),
            scope: "portfolio:global".into(),
            project_id: None,
            todo_key: "k".into(),
            title: "Restore the grants after the outage".into(),
            how: how.into(),
            link: link.map(str::to_string),
            priority: 1,
            added_by: "session:abc".into(),
            status: OperatorTodoStatusV1::Open,
            evidence: None,
            evidence_observed: false,
            closed_by: None,
            created_at: Utc::now() - chrono::Duration::hours(3),
            updated_at: Utc::now(),
            closed_at: None,
        }
    }

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_body_shows_title_priority_link_and_the_whole_command() {
        let how = format!(
            "rsi grant restore --scope portfolio:global {}",
            "--caps v8 ".repeat(12)
        );
        let todo = item(&how, Some("issue:#2950"));
        let rendered = text(&body_lines(&todo, Utc::now(), None, 40));
        assert!(
            rendered.contains("Restore the grants after the"),
            "{rendered}"
        );
        assert!(rendered.contains("P1 urgent"), "{rendered}");
        assert!(rendered.contains("issue #2950"), "{rendered}");
        assert!(rendered.contains("Command"), "{rendered}");
        // Every word of the command is on screen: nothing is cut with an ellipsis.
        assert!(!rendered.contains('…'));
        let command: String = rendered
            .lines()
            .skip_while(|row| *row != "Command")
            .skip(1)
            .map(|row| row.trim_start())
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(command.replace(' ', ""), how.replace(' ', ""));
    }

    #[test]
    fn the_footer_lists_the_actions_and_offers_undo_only_inside_the_window() {
        let linked = item("x", Some("issue:#1"));
        let plain = item("x", None);
        let view = TodoPopupState::default();
        let with = footer_text(&view, Some(&linked), Some(42));
        assert!(with.contains("y yank"), "{with}");
        assert!(with.contains("d done"), "{with}");
        assert!(with.contains("x dismiss"), "{with}");
        assert!(with.contains("o open link"), "{with}");
        assert!(with.contains("u undo (42s)"), "{with}");
        let without = footer_text(&view, Some(&plain), None);
        assert!(!without.contains("open link") && !without.contains("u undo"));
    }

    #[test]
    fn the_prompt_names_the_item_and_the_keys() {
        let todo = item("x", None);
        let done = TodoPopupState {
            scroll: 0,
            confirm: Some(OperatorTodoStatusV1::Done),
        };
        let prompt = footer_text(&done, Some(&todo), None);
        assert!(
            prompt.starts_with("Mark done “Restore the grants"),
            "{prompt}"
        );
        assert!(prompt.contains("y/Enter yes · n/Esc no"), "{prompt}");
        let dismiss = TodoPopupState {
            scroll: 0,
            confirm: Some(OperatorTodoStatusV1::Dismissed),
        };
        assert!(footer_text(&dismiss, Some(&todo), None).starts_with("Dismiss “"));
    }

    #[test]
    fn the_popup_draws_the_selected_item_in_a_terminal_buffer() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let (mut app, _) = crate::app::app_test_helpers::with_session_detail();
        app.todo_panel.apply(OperatorTodoListResultV1 {
            open: vec![item("codex login --device-auth", None)],
            ..OperatorTodoListResultV1::default()
        });
        let state = TodoPopupState {
            scroll: 0,
            confirm: Some(OperatorTodoStatusV1::Done),
        };
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| render_todo_popup(frame, frame.area(), &app, &state))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let screen: String = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("codex login --device-auth"), "{screen}");
        assert!(screen.contains("Mark done “Restore the grants"), "{screen}");
    }
}
