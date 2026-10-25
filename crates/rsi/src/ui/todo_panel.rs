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
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::navigator_layout::truncate_cells_with_ellipsis;
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
    if width == 0 || max_lines == 0 {
        return Vec::new();
    }
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

/// Collapse embedded line breaks in metadata and titles before clipping them.
fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A bounded instruction preview: preserve spaces/newlines and wrap by grapheme
/// cells. The full instructions remain available in the pop-up and clipboard.
fn command_preview(text: &str, width: usize, limit: usize) -> Vec<String> {
    if width == 0 || limit == 0 {
        return Vec::new();
    }
    let mut rows = vec![String::new()];
    let mut cells = 0;
    let mut truncated = false;
    for grapheme in text.graphemes(true) {
        if grapheme == "\n" || grapheme == "\r\n" {
            if rows.len() == limit {
                truncated = true;
                break;
            }
            rows.push(String::new());
            cells = 0;
            continue;
        }
        if grapheme.chars().any(char::is_control) {
            continue;
        }
        let w = grapheme.width();
        if cells + w > width {
            if rows.len() == limit {
                truncated = true;
                break;
            }
            if cells > 0 {
                rows.push(String::new());
                cells = 0;
            }
        }
        // A two-cell glyph cannot fit in a one-cell preview. Show a visible
        // continuation mark rather than letting it overwrite the frame.
        let grapheme = if w > width { "…" } else { grapheme };
        rows.last_mut().unwrap().push_str(grapheme);
        cells += grapheme.width();
    }
    if truncated {
        let last = rows.last_mut().unwrap();
        // Appending first also handles an exactly-full row and combining marks.
        *last = truncate_cells_with_ellipsis(&format!("{last}…"), width);
    }
    rows
}

fn priority_marker(priority: u8) -> (&'static str, Color) {
    match priority {
        1 => ("● ", theme::error_status()),
        2 => ("◆ ", theme::warning_status()),
        3 => ("• ", theme::accent()),
        _ => ("· ", theme::subtext1()),
    }
}

/// One padded row; explicit spaces paint the entire card surface, including
/// the area after short text. Clipping is by cells, never bytes or characters.
fn card_row(content: Vec<Span<'static>>, width: usize, bg: Color, selected: bool) -> Line<'static> {
    let border = Style::default().fg(theme::neutral_border()).bg(bg);
    let left = if selected {
        border.fg(theme::accent())
    } else {
        border
    };
    let mut spans = vec![Span::styled("│", left), Span::styled(" ", border)];
    let mut remaining = width.saturating_sub(4);
    for span in content {
        let text = truncate_cells_with_ellipsis(&span.content, remaining);
        remaining = remaining.saturating_sub(text.width());
        spans.push(Span::styled(text, span.style.bg(bg)));
    }
    spans.push(Span::styled(" ".repeat(remaining + 1), border));
    spans.push(Span::styled("│", border));
    Line::from(spans)
}

fn card_edge(width: usize, bg: Color, selected: bool, top: bool) -> Line<'static> {
    let border = Style::default().fg(theme::neutral_border()).bg(bg);
    Line::from(vec![
        Span::styled(
            if top { "╭" } else { "╰" },
            if selected {
                border.fg(theme::accent())
            } else {
                border
            },
        ),
        Span::styled("─".repeat(width.saturating_sub(2)), border),
        Span::styled(if top { "╮" } else { "╯" }, border),
    ])
}

fn card_lines(
    item: &OperatorTodoV1,
    now: DateTime<Utc>,
    project: Option<&str>,
    width: usize,
    selected: bool,
    is_new: bool,
) -> Vec<Line<'static>> {
    if width < 8 {
        return vec![Line::from(Span::styled(
            truncate_cells_with_ellipsis(&single_line(&item.title), width),
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::BOLD),
        ))];
    }
    let closed = item.status != OperatorTodoStatusV1::Open;
    let bg = if selected {
        theme::selected_row_bg()
    } else {
        theme::glass_panel_bg()
    };
    let muted = Style::default().fg(theme::subtext1());
    let row = |spans| card_row(spans, width, bg, selected);
    let inner = width - 4;
    // Leave at least one title cell on tiny rectangles before offering NEW.
    let new_tag = if is_new && inner >= 9 { " NEW" } else { "" };
    let (marker, color) = priority_marker(item.priority);
    let mut lines = vec![card_edge(width, bg, selected, true)];
    lines.push(row(vec![
        Span::styled(
            if selected { "▸ " } else { "  " },
            Style::default().fg(theme::accent()),
        ),
        Span::styled(marker, Style::default().fg(color)),
        Span::styled(
            truncate_cells_with_ellipsis(
                &single_line(&item.title),
                inner.saturating_sub(4 + new_tag.len()),
            ),
            (if closed {
                muted
            } else {
                Style::default().fg(theme::text())
            })
            .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            new_tag,
            Style::default()
                .fg(theme::green())
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    if closed {
        let (status, color) = match item.status {
            OperatorTodoStatusV1::Dismissed => ("× dismissed", theme::dim_metadata()),
            _ => ("✓ done", theme::green()),
        };
        lines.push(row(vec![Span::styled(status, Style::default().fg(color))]));
    }
    for how in command_preview(&item.how, inner.saturating_sub(2), 2) {
        lines.push(row(vec![
            Span::styled("│ ", dim()),
            Span::styled(how, muted),
        ]));
    }
    lines.push(row(vec![Span::styled(
        truncate_cells_with_ellipsis(
            &format!("scope · {}", single_line(&source_label(item, project))),
            inner,
        ),
        muted,
    )]));
    let age = age_label(now, item.created_at);
    let footer = match item.link.as_deref() {
        Some(link) if inner > age.width() + 3 => {
            let label = TodoLink::parse(link).map_or_else(|| link.to_string(), |link| link.label());
            format!(
                "{} · {age}",
                truncate_cells_with_ellipsis(&single_line(&label), inner - age.width() - 3)
            )
        }
        _ => age,
    };
    lines.push(row(vec![Span::styled(footer, dim())]));
    lines.push(card_edge(width, bg, selected, false));
    lines
}

/// Build the panel body at its exact drawable width (the gutter is excluded).
pub(crate) fn panel_body(
    panel: &TodoPanel,
    now: DateTime<Utc>,
    project_name: &dyn Fn(Uuid) -> Option<String>,
    width: usize,
) -> PanelBody {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut selected_rows = None;
    let new = panel.new_count();
    let header = if new > 0 {
        format!("To-do · {} open · {new} new", panel.open_count())
    } else {
        format!("To-do · {} open", panel.open_count())
    };
    lines.push(Line::from(Span::styled(
        truncate_cells_with_ellipsis(&header, width),
        Style::default()
            .fg(theme::text())
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());
    if panel.open_items().is_empty() {
        let message = if panel.loaded() {
            "Nothing needs you."
        } else {
            "Loading…"
        };
        lines.push(Line::from(Span::styled(
            truncate_cells_with_ellipsis(message, width),
            dim(),
        )));
    }
    let selected_id = panel.selected().map(|item| item.id);
    for item in panel.open_items() {
        let selected = Some(item.id) == selected_id;
        let start = lines.len();
        let project = item.project_id.and_then(project_name);
        lines.extend(card_lines(
            item,
            now,
            project.as_deref(),
            width,
            selected,
            panel.is_new(item),
        ));
        if selected {
            selected_rows = Some((start, lines.len()));
        }
        lines.push(Line::default());
    }
    let closed = panel.closed_since_visit();
    if !closed.is_empty() {
        lines.push(Line::from(Span::styled(
            truncate_cells_with_ellipsis("Closed since your last visit", width),
            dim().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::default());
        for item in closed {
            let project = item.project_id.and_then(project_name);
            lines.extend(card_lines(
                item,
                now,
                project.as_deref(),
                width,
                false,
                false,
            ));
            lines.push(Line::default());
        }
    }
    PanelBody {
        lines,
        selected_rows,
    }
}

/// First visible row so the selected item's rows fit in `height` rows.
fn scroll_for(selected_rows: Option<(usize, usize)>, height: usize) -> usize {
    let Some((start, end)) = selected_rows else {
        return 0;
    };
    if height == 0 || end <= height {
        return 0;
    }
    if end - start > height {
        return start;
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
        Paragraph::new(vec![Line::from("│"); usize::from(area.height)])
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
        usize::from(body_area.width.saturating_sub(1)),
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
        assert!(rendered.contains("▸ ● Log in to Codex"), "{rendered}");
        assert!(rendered.contains("│ codex login"), "{rendered}");
        assert!(
            rendered.contains("scope · portfolio") && rendered.contains("issue #1747 · 1h"),
            "{rendered}"
        );
        assert_eq!(body.selected_rows, Some((2, 8)));
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
            rendered.contains("Turn on phone answers") && rendered.contains("✓ done"),
            "{rendered}"
        );
        let old_line = rendered.lines().find(|l| l.contains("Old")).unwrap();
        assert!(old_line.contains("• Old"), "{rendered}");
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
        assert!(
            rendered.contains("scope · rsi") && rendered.contains("3m"),
            "{rendered}"
        );
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
        for width in [PANEL_MIN_AREA_WIDTH, 170] {
            let (mut app, mut terminal) = seeded_app(width);
            terminal
                .draw(|frame| crate::ui::render(frame, &mut app))
                .expect("draw");
            let shown = screen(&terminal);
            assert!(shown.contains("To-do · 2 open"), "{shown}");
            assert!(shown.contains("Log in to Codex"), "{shown}");
            assert!(shown.contains("│ codex login"), "{shown}");
            assert!(shown.contains("2 todo +2"), "{shown}");
        }
    }

    #[test]
    fn a_narrow_terminal_keeps_the_badge_when_the_panel_does_not_fit() {
        for width in [90, PANEL_MIN_AREA_WIDTH - 1] {
            let (mut app, mut terminal) = seeded_app(width);
            terminal
                .draw(|frame| crate::ui::render(frame, &mut app))
                .expect("draw");
            assert!(screen(&terminal).contains("2 todo +2"));
        }
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

    fn review_panel() -> TodoPanel {
        let mut panel = TodoPanel::restored(true, Some(at(-60)));
        let mut done = todo("Grant repaired", "rsi grant show", 2, 0);
        done.status = OperatorTodoStatusV1::Done;
        done.closed_by = Some("session:abc".into());
        done.closed_at = Some(at(30));
        let mut dismissed = done.clone();
        dismissed.id = Uuid::new_v4();
        dismissed.title = "Old request".into();
        dismissed.status = OperatorTodoStatusV1::Dismissed;
        panel.apply(OperatorTodoListResultV1 {
            open: vec![
                todo("Log in to Codex", "codex login", 1, 0),
                todo(
                    "Restore the grants after the outage",
                    "rsi grant restore --scope portfolio:global --caps v8 --caps v8 --caps v8",
                    3,
                    5,
                ),
            ],
            recently_closed: vec![done, dismissed],
            truncated: false,
        });
        panel
    }

    fn render_body(panel: &TodoPanel, width: u16) -> Terminal<TestBackend> {
        let body = panel_body(panel, at(180), &no_names, usize::from(width));
        let mut terminal = Terminal::new(TestBackend::new(width, body.lines.len() as u16)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new(body.lines).style(Style::default().bg(theme::root_bg())),
                    frame.area(),
                );
            })
            .unwrap();
        terminal
    }

    #[test]
    fn card_render_snapshots_pin_narrow_standard_and_wide_layouts() {
        theme::with_theme_state(|| {
            theme::set_theme_by_name("goth");
            let panel = review_panel();
            for width in [28, 39, 55] {
                let terminal = render_body(&panel, width);
                let snapshot = screen(&terminal)
                    .lines()
                    .map(str::trim_end)
                    .collect::<Vec<_>>()
                    .join("\n");
                insta::assert_snapshot!(format!("todo_cards_{width}"), snapshot);
            }
        });
    }

    #[test]
    fn cards_use_theme_surfaces_and_semantic_colors_in_light_and_dark() {
        theme::with_theme_state(|| {
            for name in ["goth", "light"] {
                assert!(theme::set_theme_by_name(name));
                theme::apply_theme_role_overrides(&[]);
                let terminal = render_body(&review_panel(), 39);
                let buffer = terminal.backend().buffer();
                let title = &buffer[(6, 3)];
                assert_eq!(title.symbol(), "L");
                assert_eq!(title.fg, theme::text());
                assert!(title.modifier.contains(Modifier::BOLD));
                assert_eq!(buffer[(4, 3)].symbol(), "●");
                assert_eq!(buffer[(4, 3)].fg, theme::error_status());
                assert_eq!(buffer[(0, 3)].fg, theme::accent());
                assert_eq!(buffer[(38, 3)].fg, theme::neutral_border());
                // Every cell, including the outline/padding, carries selection.
                for y in 2..8 {
                    for x in 0..39 {
                        assert_eq!(buffer[(x, y)].bg, theme::selected_row_bg());
                    }
                }
                assert_eq!(buffer[(4, 4)].fg, theme::subtext1());
                assert_eq!(buffer[(2, 6)].fg, theme::dim_metadata());
                assert_eq!(buffer[(4, 10)].symbol(), "•");
                assert_eq!(buffer[(4, 10)].fg, theme::accent());
                assert_eq!(buffer[(0, 10)].bg, theme::glass_panel_bg());
                assert_eq!(buffer[(0, 8)].bg, theme::root_bg());
                theme::set_theme_role_override(
                    super::super::theme_roles::ThemeRole::SelectedRow,
                    Some([12, 34, 56]),
                );
                theme::set_theme_role_override(
                    super::super::theme_roles::ThemeRole::PrimaryText,
                    Some([65, 43, 21]),
                );
                let overridden = render_body(&review_panel(), 39);
                assert_eq!(
                    overridden.backend().buffer()[(6, 3)].bg,
                    Color::Rgb(12, 34, 56)
                );
                assert_eq!(
                    overridden.backend().buffer()[(6, 3)].fg,
                    Color::Rgb(65, 43, 21)
                );
            }
        });
    }

    #[test]
    fn command_preview_keeps_spacing_lines_and_graphemes_and_marks_cuts() {
        assert_eq!(
            command_preview("one  two\n  three", 12, 2),
            ["one  two", "  three"]
        );
        assert_eq!(command_preview("abcdefghijklm", 6, 2), ["abcdef", "ghijk…"]);
        assert_eq!(command_preview("one\ntwo\nthree", 6, 2), ["one", "two…"]);
        assert_eq!(
            command_preview("e\u{301}界👩‍💻abc", 5, 2),
            ["e\u{301}界👩‍💻", "abc"]
        );
        assert_eq!(command_preview("界", 1, 2), ["…"]);
        assert!(command_preview("x", 0, 2).is_empty());
    }

    #[test]
    fn unicode_long_links_and_scope_stay_inside_the_frame_and_keep_age() {
        let mut item = todo(
            "界界 e\u{301} 👩‍💻 very long title\nsecond line",
            "  界界 --long-command-without-any-spaces-and-more\n  next",
            4,
            0,
        );
        let project = Uuid::nil();
        item.scope = format!("project:{project}");
        item.project_id = Some(project);
        item.link = Some("https://example.test/very/long/path/that/must/be/clipped".into());
        let mut panel = TodoPanel::default();
        panel.apply(OperatorTodoListResultV1 {
            open: vec![item.clone()],
            ..Default::default()
        });
        for width in 0..=60 {
            let lines = card_lines(
                &item,
                at(180),
                Some("a project with a very long name"),
                width,
                true,
                true,
            );
            for line in lines {
                assert!(line.width() <= width, "width {width}: {line:?}");
                if width >= 8 {
                    assert_eq!(line.width(), width);
                }
            }
            for line in panel_body(&panel, at(180), &no_names, width).lines {
                assert!(line.width() <= width, "width {width}: {line:?}");
            }
        }
        let rows = card_lines(
            &item,
            at(180),
            Some("a project with a very long name"),
            28,
            false,
            false,
        );
        let rendered = text(&PanelBody {
            lines: rows,
            selected_rows: None,
        });
        assert!(rendered.contains("https://example.te… · 3h"), "{rendered}");
        assert!(rendered.contains("scope · a project with …"), "{rendered}");
        assert_eq!(priority_marker(4).0, "· ");
        assert_eq!(priority_marker(2).0, "◆ ");
    }

    #[test]
    fn selection_and_footer_fit_short_viewports_and_tiny_rectangles() {
        let body = panel_body(&review_panel(), at(180), &no_names, 39);
        assert_eq!(scroll_for(body.selected_rows, 3), 2);
        let mut panel = review_panel();
        panel.select_next();
        let body = panel_body(&panel, at(180), &no_names, 39);
        let (start, end) = body.selected_rows.unwrap();
        let top = scroll_for(body.selected_rows, 12);
        assert!(top <= start && end <= top + 12);
        for width in [0, 1, 2, 3, 8, 28, 40] {
            for height in [0, 1, 2, 3, 7, 12] {
                let (app, mut terminal) = seeded_app(50);
                terminal
                    .draw(|frame| render_todo_panel(frame, Rect::new(0, 0, width, height), &app))
                    .unwrap();
            }
        }
        let (mut app, _) = seeded_app(50);
        app.todo_panel = panel;
        let mut terminal = Terminal::new(TestBackend::new(41, 12)).unwrap();
        terminal
            .draw(|frame| render_todo_panel(frame, frame.area(), &app))
            .unwrap();
        let shown = screen(&terminal);
        assert!(shown.contains("Restore the grants"), "{shown}");
        assert!(shown.contains("j/k move"), "{shown}");
        // The separator runs the full height, including the fixed action footer.
        for y in 0..12 {
            assert_eq!(terminal.backend().buffer()[(0, y)].symbol(), "│");
        }
    }
    // Export actual TestBackend cells as a reviewable vector image. Golden
    // comparisons pin colors and padding as well as glyph positions.
    fn svg_snapshot(terminal: &Terminal<TestBackend>) -> String {
        use std::fmt::Write;
        fn rgb(color: Color) -> String {
            match color {
                Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
                Color::Reset => rgb(theme::text()),
                other => panic!("RGB snapshot expected, got {other:?}"),
            }
        }
        fn escape(text: &str) -> String {
            text.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        }
        let buffer = terminal.backend().buffer();
        let width = buffer.area.width;
        let height = buffer.area.height;
        let mut svg = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\">\n<title>Operator to-do cards</title>\n<rect width=\"100%\" height=\"100%\" fill=\"{}\"/>\n<g font-family=\"JetBrainsMono Nerd Font Mono, monospace\" font-size=\"15\" xml:space=\"preserve\">\n",
            width * 9 + 18,
            height * 18 + 18,
            width * 9 + 18,
            height * 18 + 18,
            rgb(theme::root_bg()),
        );
        for y in 0..height {
            let mut x = 0;
            while x < width {
                let start = x;
                let bg = buffer[(x, y)].bg;
                while x < width && buffer[(x, y)].bg == bg {
                    x += 1;
                }
                writeln!(
                    svg,
                    "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"18\" fill=\"{}\"/>",
                    start * 9 + 9,
                    y * 18 + 9,
                    (x - start) * 9,
                    rgb(bg)
                )
                .unwrap();
            }
            let mut x = 0;
            while x < width {
                let start = x;
                let fg = buffer[(x, y)].fg;
                let bold = buffer[(x, y)].modifier.contains(Modifier::BOLD);
                let mut text = String::new();
                while x < width
                    && buffer[(x, y)].fg == fg
                    && buffer[(x, y)].modifier.contains(Modifier::BOLD) == bold
                {
                    text.push_str(buffer[(x, y)].symbol());
                    x += 1;
                }
                if !text.trim().is_empty() {
                    writeln!(
                        svg,
                        "<text x=\"{}\" y=\"{}\" fill=\"{}\" font-weight=\"{}\" textLength=\"{}\" lengthAdjust=\"spacingAndGlyphs\">{}</text>",
                        start * 9 + 9,
                        y * 18 + 23,
                        rgb(fg),
                        if bold { "bold" } else { "normal" },
                        (x - start) * 9,
                        escape(&text)
                    )
                    .unwrap();
                }
            }
        }
        svg.push_str("</g>\n</svg>\n");
        svg
    }

    #[test]
    fn styled_card_render_snapshots_match_review_images() {
        theme::with_theme_state(|| {
            theme::apply_theme_role_overrides(&[]);
            for (name, expected) in [
                ("goth", include_str!("snapshots/todo-cards-after-goth.svg")),
                (
                    "light",
                    include_str!("snapshots/todo-cards-after-light.svg"),
                ),
            ] {
                assert!(theme::set_theme_by_name(name));
                assert_eq!(
                    svg_snapshot(&render_body(&review_panel(), 39)),
                    expected,
                    "theme {name}"
                );
            }
        });
    }
}
