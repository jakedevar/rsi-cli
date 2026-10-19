//! Drawing of the operator to-do panel (#1747 S2): the right-hand column of
//! session detail and the status-line badge. State and rules live in
//! `crate::todo_panel`; this file only turns them into cells.

use crate::app::App;
use crate::todo_panel::{
    PANEL_MIN_AREA_WIDTH, PANEL_WIDTH, TodoLink, TodoPanel, age_label, source_label,
};
use crate::ui::theme;
use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use rsi_common::operator_todos::{OperatorTodoStatusV1, OperatorTodoV1};
use unicode_width::UnicodeWidthChar;
use uuid::Uuid;

/// Keys listed in the panel footer (kept in step with the `<Space>d` chords
/// registered in `action_registry.rs`).
const FOOTER: &str = "<Space>d  t hide · j/k move · v view · o open · y yank · d done · x dismiss · u undo · a caught up";

/// Split `area` into the session-detail area and the panel column (separator
/// included), or leave it whole when the panel is hidden or does not fit.
#[must_use]
pub(crate) fn split_panel_area(area: Rect, panel_wanted: bool) -> (Rect, Option<Rect>) {
    if !panel_wanted || area.width < PANEL_MIN_AREA_WIDTH {
        return (area, None);
    }
    let panel_w = PANEL_WIDTH + 1; // separator column
    let rest = Rect::new(area.x, area.y, area.width - panel_w, area.height);
    let panel = Rect::new(area.x + rest.width, area.y, panel_w, area.height);
    (rest, Some(panel))
}

/// Break `text` into at most `max_lines` lines of `width` cells, ending the
/// last with `…` when text is left over.
fn wrap(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_w = 0usize;
    let mut truncated = false;
    'words: for word in text.split_whitespace() {
        let word_w: usize = word.chars().map(|c| c.width().unwrap_or(0)).sum();
        let sep = usize::from(current_w > 0);
        if current_w + sep + word_w <= width {
            if sep == 1 {
                current.push(' ');
            }
            current.push_str(word);
            current_w += sep + word_w;
            continue;
        }
        if !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_w = 0;
        }
        for ch in word.chars() {
            let w = ch.width().unwrap_or(0);
            if current_w + w > width {
                lines.push(std::mem::take(&mut current));
                current_w = 0;
            }
            if lines.len() >= max_lines {
                truncated = true;
                break 'words;
            }
            current.push(ch);
            current_w += w;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.len() > max_lines {
        lines.truncate(max_lines);
        truncated = true;
    }
    if truncated && let Some(last) = lines.last_mut() {
        while last.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>() + 1 > width {
            last.pop();
        }
        last.push('…');
    }
    lines
}

/// The panel's body: every row, plus the row range of the selected item so
/// the renderer can scroll it into view.
pub(crate) struct PanelBody {
    pub lines: Vec<Line<'static>>,
    pub selected_rows: Option<(usize, usize)>,
}

fn dim() -> Style {
    Style::default().fg(theme::dim_metadata())
}

/// Build the panel body for a column `width` cells wide.
pub(crate) fn panel_body(
    panel: &TodoPanel,
    now: DateTime<Utc>,
    project_name: &dyn Fn(Uuid) -> Option<String>,
    width: usize,
) -> PanelBody {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut selected_rows = None;
    let inner = width.saturating_sub(3).max(8);

    let new = panel.new_count();
    let mut header = vec![Span::styled(
        format!("To-do · {} open", panel.open_count()),
        Style::default()
            .fg(theme::text())
            .add_modifier(Modifier::BOLD),
    )];
    if new > 0 {
        header.push(Span::styled(
            format!(" · {new} new"),
            Style::default()
                .fg(theme::green())
                .add_modifier(Modifier::BOLD),
        ));
    }
    lines.push(Line::from(header));
    lines.push(Line::default());

    if panel.open_items().is_empty() {
        let message = if panel.loaded() {
            "Nothing needs you."
        } else {
            "Loading…"
        };
        lines.push(Line::from(Span::styled(message, dim())));
    }

    let selected_id = panel.selected().map(|item| item.id);
    for item in panel.open_items() {
        let is_selected = Some(item.id) == selected_id;
        let is_new = panel.is_new(item);
        let start = lines.len();
        let marker = if is_selected { "▸ " } else { "  " };
        let row_bg = if is_selected {
            theme::selected_row_bg()
        } else {
            Color::Reset
        };
        let with_bg = |style: Style| {
            if is_selected { style.bg(row_bg) } else { style }
        };
        let title_color = if is_new {
            theme::green()
        } else {
            theme::text()
        };
        let urgent = item.priority <= 2;
        let tag_w = if is_new { 4 } else { 0 };
        let title_lines = wrap(&item.title, inner.saturating_sub(tag_w + 2), 2);
        for (index, title) in title_lines.iter().enumerate() {
            let mut spans = vec![Span::styled(
                marker.to_string(),
                with_bg(Style::default().fg(theme::accent())),
            )];
            if index == 0 {
                spans.push(Span::styled(
                    if urgent { "! " } else { "  " },
                    with_bg(Style::default().fg(theme::warning_status())),
                ));
            } else {
                spans.push(Span::styled("  ", with_bg(Style::default())));
            }
            spans.push(Span::styled(
                title.clone(),
                with_bg(
                    Style::default()
                        .fg(title_color)
                        .add_modifier(Modifier::BOLD),
                ),
            ));
            if is_new && index == 0 {
                spans.push(Span::styled(
                    " NEW",
                    with_bg(
                        Style::default()
                            .fg(theme::green())
                            .add_modifier(Modifier::BOLD),
                    ),
                ));
            }
            lines.push(Line::from(spans));
        }
        for how in wrap(&item.how, inner.saturating_sub(2), 2) {
            lines.push(Line::from(vec![
                Span::styled("    ", with_bg(Style::default())),
                Span::styled("$ ", with_bg(dim())),
                Span::styled(how, with_bg(Style::default().fg(theme::text()))),
            ]));
        }
        let project = item.project_id.and_then(project_name);
        let source = source_label(item, project.as_deref());
        let mut meta = format!("{} · {source}", age_label(now, item.created_at));
        if let Some(link) = item.link.as_deref() {
            meta.push_str(" · ");
            meta.push_str(
                &TodoLink::parse(link).map_or_else(|| link.to_string(), |parsed| parsed.label()),
            );
        }
        for meta_line in wrap(&meta, inner.saturating_sub(2), 1) {
            lines.push(Line::from(vec![
                Span::styled("    ", with_bg(Style::default())),
                Span::styled(meta_line, with_bg(dim())),
            ]));
        }
        if is_selected {
            selected_rows = Some((start, lines.len()));
        }
        lines.push(Line::default());
    }

    let closed = panel.closed_since_visit();
    if !closed.is_empty() {
        lines.push(Line::from(Span::styled(
            "Closed since your last visit",
            Style::default()
                .fg(theme::green())
                .add_modifier(Modifier::BOLD),
        )));
        for item in closed {
            lines.extend(closed_rows(item, inner));
        }
    }
    PanelBody {
        lines,
        selected_rows,
    }
}

fn closed_rows(item: &OperatorTodoV1, inner: usize) -> Vec<Line<'static>> {
    let (glyph, word) = match item.status {
        OperatorTodoStatusV1::Dismissed => ("×", "dismissed"),
        _ => ("✓", "done"),
    };
    let text = format!("{} ({word})", item.title);
    wrap(&text, inner.saturating_sub(2), 2)
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            Line::from(vec![
                Span::styled(
                    if index == 0 {
                        format!("{glyph} ")
                    } else {
                        "  ".to_string()
                    },
                    Style::default().fg(theme::green()),
                ),
                Span::styled(row, Style::default().fg(theme::green())),
            ])
        })
        .collect()
}

/// First visible row so the selected item's rows fit in `height` rows.
fn scroll_for(selected_rows: Option<(usize, usize)>, height: usize) -> usize {
    let Some((start, end)) = selected_rows else {
        return 0;
    };
    if height == 0 || end <= height {
        return 0;
    }
    end.saturating_sub(height).min(start)
}

/// Draw the panel column (separator included) into `area`.
pub(crate) fn render_todo_panel(frame: &mut Frame, area: Rect, app: &App) {
    if area.width < 2 || area.height == 0 {
        return;
    }
    // Paint the whole column first: the gutter between the separator and the
    // list, and any rows the list leaves empty, must not fall through to the
    // terminal default background.
    frame.render_widget(
        Paragraph::new("").style(Style::default().bg(theme::root_bg())),
        area,
    );
    let sep = Rect::new(area.x, area.y, 1, area.height);
    frame.render_widget(
        Paragraph::new("│".repeat(usize::from(area.height)))
            .style(Style::default().fg(theme::surface2()).bg(theme::root_bg())),
        sep,
    );
    let body_area = Rect::new(area.x + 1, area.y, area.width - 1, area.height);
    let name = |id: Uuid| {
        app.projects
            .iter()
            .find(|project| project.id == id)
            .map(|project| project.name.clone())
    };
    let footer_rows = wrap(FOOTER, usize::from(body_area.width).saturating_sub(2), 3);
    let footer_h = u16::try_from(footer_rows.len())
        .unwrap_or(0)
        .min(body_area.height / 2);
    let list_h = body_area.height.saturating_sub(footer_h);
    let body = panel_body(
        &app.todo_panel,
        Utc::now(),
        &name,
        usize::from(body_area.width),
    );
    let scroll = scroll_for(body.selected_rows, usize::from(list_h));
    let visible: Vec<Line<'static>> = body
        .lines
        .into_iter()
        .skip(scroll)
        .take(usize::from(list_h))
        .collect();
    let list_area = Rect::new(body_area.x + 1, body_area.y, body_area.width - 1, list_h);
    frame.render_widget(
        Paragraph::new(visible).style(Style::default().bg(theme::root_bg())),
        list_area,
    );
    if footer_h > 0 {
        let footer_area = Rect::new(
            body_area.x + 1,
            body_area.y + list_h,
            body_area.width - 1,
            footer_h,
        );
        let footer: Vec<Line<'static>> = footer_rows
            .into_iter()
            .map(|row| Line::from(Span::styled(row, dim())))
            .collect();
        frame.render_widget(
            Paragraph::new(footer).style(Style::default().bg(theme::root_bg())),
            footer_area,
        );
    }
}

/// The status-line badge: `☐ 3 todo`, bright with `+N` when some are new.
/// Empty when nothing is open.
#[must_use]
pub(crate) fn badge_spans(panel: &TodoPanel, bg: Color) -> Vec<Span<'static>> {
    let open = panel.open_count();
    if open == 0 {
        return Vec::new();
    }
    let new = panel.new_count();
    let closed = panel.closed_since_visit().len();
    let color = if new + closed > 0 {
        theme::warning_status()
    } else {
        theme::dim_metadata()
    };
    let mut text = format!("\u{2610} {open} todo");
    if new > 0 {
        text.push_str(&format!(" +{new}"));
    }
    if closed > 0 {
        text.push_str(&format!(" \u{2713}{closed}"));
    }
    text.push(' ');
    vec![Span::styled(
        text,
        Style::default()
            .fg(color)
            .bg(bg)
            .add_modifier(if new + closed > 0 {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
    )]
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use rsi_common::operator_todos::OperatorTodoListResultV1;

    fn at(minutes: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 8, 0, 0, 0).unwrap() + chrono::Duration::minutes(minutes)
    }

    fn todo(title: &str, how: &str, priority: u8, created: i64) -> OperatorTodoV1 {
        OperatorTodoV1 {
            id: Uuid::new_v4(),
            scope: "portfolio:global".into(),
            project_id: None,
            todo_key: title.into(),
            title: title.into(),
            how: how.into(),
            link: Some("issue:#1747".into()),
            priority,
            added_by: "session:abc".into(),
            status: OperatorTodoStatusV1::Open,
            evidence: None,
            evidence_observed: false,
            closed_by: None,
            created_at: at(created),
            updated_at: at(created),
            closed_at: None,
        }
    }

    fn text(body: &PanelBody) -> String {
        body.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn no_names(_: Uuid) -> Option<String> {
        None
    }

    #[test]
    fn a_row_shows_title_how_age_source_and_link() {
        let mut panel = TodoPanel::restored(true, None);
        panel.apply(OperatorTodoListResultV1 {
            open: vec![todo("Log in to Codex", "codex login", 1, 0)],
            ..OperatorTodoListResultV1::default()
        });
        let body = panel_body(&panel, at(90), &no_names, 40);
        let rendered = text(&body);
        assert!(rendered.contains("To-do · 1 open"), "{rendered}");
        assert!(rendered.contains("▸ ! Log in to Codex"), "{rendered}");
        assert!(rendered.contains("$ codex login"), "{rendered}");
        assert!(
            rendered.contains("1h · portfolio · issue #1747"),
            "{rendered}"
        );
        assert_eq!(body.selected_rows, Some((2, 5)));
    }

    #[test]
    fn new_items_and_closed_items_are_called_out_for_a_returning_operator() {
        let mut panel = TodoPanel::restored(true, Some(at(10)));
        let mut done = todo("Turn on phone answers", "rsi phone on", 3, 0);
        done.status = OperatorTodoStatusV1::Done;
        done.closed_by = Some("session:abc".into());
        done.closed_at = Some(at(30));
        panel.apply(OperatorTodoListResultV1 {
            open: vec![todo("Old", "x", 3, 5), todo("Fresh", "y", 3, 20)],
            recently_closed: vec![done],
            truncated: false,
        });
        let rendered = text(&panel_body(&panel, at(60), &no_names, 40));
        assert!(rendered.contains("To-do · 2 open · 1 new"), "{rendered}");
        assert!(rendered.contains("Fresh NEW"), "{rendered}");
        assert!(
            rendered.contains("Closed since your last visit"),
            "{rendered}"
        );
        assert!(
            rendered.contains("✓ Turn on phone answers (done)"),
            "{rendered}"
        );
        let old_line = rendered.lines().find(|l| l.contains("Old")).unwrap();
        assert!(!old_line.contains("NEW"), "{rendered}");
    }

    #[test]
    fn an_empty_list_says_nothing_needs_the_operator() {
        let mut panel = TodoPanel::restored(true, None);
        assert!(text(&panel_body(&panel, at(0), &no_names, 40)).contains("Loading…"));
        panel.apply(OperatorTodoListResultV1::default());
        assert!(text(&panel_body(&panel, at(0), &no_names, 40)).contains("Nothing needs you."));
    }

    #[test]
    fn a_project_item_is_sourced_by_the_project_name() {
        let project = Uuid::new_v4();
        let mut item = todo("Approve", "press y", 3, 0);
        item.scope = format!("project:{project}");
        item.project_id = Some(project);
        item.link = None;
        let mut panel = TodoPanel::restored(true, None);
        panel.apply(OperatorTodoListResultV1 {
            open: vec![item],
            ..OperatorTodoListResultV1::default()
        });
        let names = move |id: Uuid| (id == project).then(|| "rsi".to_string());
        let rendered = text(&panel_body(&panel, at(3), &names, 40));
        assert!(rendered.contains("3m · rsi"), "{rendered}");
    }

    #[test]
    fn long_text_wraps_and_a_cut_ends_in_an_ellipsis() {
        let lines = wrap("one two three four five six seven", 10, 2);
        assert_eq!(lines, vec!["one two".to_string(), "three fou…".to_string()]);
        let whole = wrap("one two", 10, 2);
        assert_eq!(whole, vec!["one two".to_string()]);
        let long = wrap("abcdefghijklmnop", 6, 2);
        assert_eq!(long, vec!["abcdef".to_string(), "ghijk…".to_string()]);
    }

    #[test]
    fn the_selected_item_is_scrolled_into_view() {
        assert_eq!(scroll_for(None, 10), 0);
        assert_eq!(scroll_for(Some((2, 6)), 10), 0);
        // Rows 30..34 must end inside the window.
        let top = scroll_for(Some((30, 34)), 10);
        assert!(top <= 30 && 34 <= top + 10, "top {top}");
    }

    #[test]
    fn the_panel_hides_on_a_narrow_pane_and_otherwise_takes_its_column() {
        let narrow = Rect::new(0, 0, PANEL_MIN_AREA_WIDTH - 1, 30);
        assert_eq!(split_panel_area(narrow, true), (narrow, None));
        let wide = Rect::new(5, 2, 160, 30);
        assert_eq!(split_panel_area(wide, false), (wide, None));
        let (rest, panel) = split_panel_area(wide, true);
        let panel = panel.unwrap();
        assert_eq!(rest.width + panel.width, wide.width);
        assert_eq!(panel.x, rest.x + rest.width);
        assert_eq!(panel.width, PANEL_WIDTH + 1);
    }

    #[test]
    fn the_badge_counts_open_items_and_flags_catch_up() {
        let bg = Color::Reset;
        let mut panel = TodoPanel::restored(true, Some(at(10)));
        assert!(badge_spans(&panel, bg).is_empty());
        panel.apply(OperatorTodoListResultV1 {
            open: vec![todo("a", "x", 3, 5), todo("b", "y", 3, 20)],
            ..OperatorTodoListResultV1::default()
        });
        let spans = badge_spans(&panel, bg);
        let rendered: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rendered, "\u{2610} 2 todo +1 ");
    }

    fn seeded_app(width: u16) -> (App, Terminal<TestBackend>) {
        let (mut app, _session) = crate::app::app_test_helpers::with_session_detail();
        app.enter_session();
        app.todo_panel = TodoPanel::restored(true, Some(at(-60)));
        app.todo_panel.apply(OperatorTodoListResultV1 {
            open: vec![
                todo("Log in to Codex", "codex login", 1, 0),
                todo("Turn on phone answers", "rsi phone on", 3, 5),
            ],
            ..OperatorTodoListResultV1::default()
        });
        let terminal = Terminal::new(TestBackend::new(width, 40)).expect("terminal");
        (app, terminal)
    }

    fn screen(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn a_wide_session_detail_shows_the_panel_and_the_status_line_badge() {
        let (mut app, mut terminal) = seeded_app(170);
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .expect("draw");
        let shown = screen(&terminal);
        assert!(shown.contains("To-do · 2 open"), "{shown}");
        assert!(shown.contains("Log in to Codex"), "{shown}");
        assert!(shown.contains("$ codex login"), "{shown}");
        assert!(shown.contains("2 todo +2"), "{shown}");
    }

    #[test]
    fn a_narrow_terminal_keeps_the_badge_when_the_panel_does_not_fit() {
        let (mut app, mut terminal) = seeded_app(90);
        terminal
            .draw(|frame| crate::ui::render(frame, &mut app))
            .expect("draw");
        assert!(screen(&terminal).contains("2 todo +2"));
    }

    #[test]
    fn the_footer_names_the_vim_keys_and_fits_the_panel() {
        let rows = wrap(FOOTER, usize::from(PANEL_WIDTH) - 2, 3);
        let joined = rows.join(" ");
        assert!(!joined.contains('…'), "footer was cut: {rows:?}");
        for key in ["y yank", "d done", "x dismiss", "u undo", "v view"] {
            assert!(joined.contains(key), "{key} missing from {rows:?}");
        }
    }
}
