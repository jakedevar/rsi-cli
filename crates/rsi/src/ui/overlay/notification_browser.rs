//! Notification browser rendering.

use crate::app::App;
use crate::types::{ModalGeometry, Notification, NotificationKind, NotificationPriority};
use crate::ui::{navigator_layout, theme};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::{apply_geometry_deltas, fixed_centered_rect};

fn popup_rect(area: Rect, count: usize, geom: &ModalGeometry) -> Rect {
    // Three rows per notice: metadata, message, and space before the next one.
    let height = (count.min(6) as u16 * 3 + 7)
        .max(10)
        .min(area.height.saturating_sub(2).max(5));
    let width = 78.min(area.width.saturating_sub(4).max(20));
    let base = fixed_centered_rect(area, width, height);
    apply_geometry_deltas(base, geom, area)
}

fn icon(kind: NotificationKind) -> &'static str {
    match kind {
        NotificationKind::TaskRabbitComplete
        | NotificationKind::BugComplete
        | NotificationKind::OperationSuccess
        | NotificationKind::Connected => "✓",
        NotificationKind::TaskRabbitFailed
        | NotificationKind::BugFailed
        | NotificationKind::OperationFailed => "✗",
        NotificationKind::ConnectionFailed | NotificationKind::ConnectionLost => "⚡",
        NotificationKind::SessionLaunching | NotificationKind::SessionResuming => "…",
        NotificationKind::Info => "ℹ",
        NotificationKind::SessionStalled => "⏸",
        NotificationKind::SessionClassified => "⚖",
    }
}

fn age(notification: &Notification) -> String {
    let seconds = notification.created_at.elapsed().as_secs();
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

pub(super) fn render_notification_browser(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    selected_index: usize,
    geom: &ModalGeometry,
) {
    let active_count = app.notifications.len();
    let history_count = app.notification_history.len();
    let count = active_count + history_count;
    let popup_area = popup_rect(area, count, geom);
    frame.render_widget(Clear, popup_area);

    let block = theme::overlay_block()
        .title(Line::from(Span::styled(
            " Notifications ",
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )))
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let content = Rect::new(
        inner.x.saturating_add(1),
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    let summary = format!("{active_count} active  ·  {history_count} history  ·  newest first");
    frame.render_widget(
        Paragraph::new(summary).style(Style::default().fg(theme::subtext0())),
        Rect::new(content.x, content.y, content.width, 1),
    );

    let body_y = content.y.saturating_add(2);
    let body_height = content.height.saturating_sub(5);
    if count == 0 {
        frame.render_widget(
            Paragraph::new("All clear. New notifications will appear here.")
                .style(Style::default().fg(theme::subtext0())),
            Rect::new(content.x, body_y, content.width, body_height),
        );
    } else if body_height >= 2 {
        let card_height = if body_height >= 3 { 3 } else { 2 };
        let visible = (body_height / card_height).max(1) as usize;
        let selected = selected_index.min(count - 1);
        let start = selected.saturating_sub(visible - 1);
        let notifications = app
            .notifications
            .iter()
            .rev()
            .map(|notice| (notice, true))
            .chain(
                app.notification_history
                    .iter()
                    .rev()
                    .map(|notice| (notice, false)),
            );

        for (offset, (notice, active)) in notifications.skip(start).take(visible).enumerate() {
            let row_y = body_y + offset as u16 * card_height;
            let selected_row = start + offset == selected;
            let row_style = Style::default().bg(if selected_row {
                theme::surface2()
            } else {
                theme::overlay_bg()
            });
            let message_color = if !active {
                theme::subtext0()
            } else {
                match notice.priority {
                    NotificationPriority::High => theme::red(),
                    NotificationPriority::Medium => theme::text(),
                    NotificationPriority::Low => theme::subtext0(),
                }
            };
            let status = if active { "ACTIVE" } else { "HISTORY" };
            let target = if notice.session_id.is_some() {
                "  ↗ session"
            } else {
                ""
            };
            let meta = Line::from(vec![
                Span::styled(
                    if selected_row { "› " } else { "  " },
                    Style::default().fg(theme::peach()),
                ),
                Span::styled(
                    format!("{}  ", icon(notice.kind)),
                    Style::default().fg(message_color),
                ),
                Span::styled(
                    status,
                    Style::default()
                        .fg(theme::overlay_title())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  ·  {}{target}", age(notice)),
                    Style::default().fg(theme::subtext0()),
                ),
            ]);
            frame.render_widget(
                Paragraph::new(meta).style(row_style),
                Rect::new(content.x, row_y, content.width, 1),
            );
            let message = notice.message.replace('\n', " ").replace('\r', " ");
            let message = navigator_layout::truncate_cells_with_ellipsis(
                &message,
                content.width.saturating_sub(4) as usize,
            );
            frame.render_widget(
                Paragraph::new(format!("    {message}")).style(row_style.fg(message_color)),
                Rect::new(content.x, row_y + 1, content.width, 1),
            );
        }
    }

    if content.height >= 2 {
        let hints_y = content.y + content.height - 2;
        let selected_notice = app
            .notifications
            .iter()
            .rev()
            .chain(app.notification_history.iter().rev())
            .nth(selected_index);
        let command_hint = match (
            selected_notice.and_then(|n| n.session_id),
            selected_index < active_count,
        ) {
            (Some(_), true) => {
                "j/k select  ·  Enter open session  ·  x dismiss  ·  N clear  ·  Esc close"
            }
            (Some(_), false) => "j/k select  ·  Enter open session  ·  N clear  ·  Esc close",
            (None, true) => {
                "j/k select  ·  No linked session  ·  x dismiss  ·  N clear  ·  Esc close"
            }
            (None, false) => "j/k select  ·  No linked session  ·  N clear  ·  Esc close",
        };
        frame.render_widget(
            Paragraph::new(command_hint).style(Style::default().fg(theme::overlay_hint())),
            Rect::new(content.x, hints_y, content.width, 1),
        );
        frame.render_widget(
            Paragraph::new("Ctrl+Arrow move  ·  Ctrl+Shift+Arrow resize  ·  Ctrl+0 reset")
                .style(Style::default().fg(theme::subtext0())),
            Rect::new(content.x, hints_y + 1, content.width, 1),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{NotificationKind, NotificationPriority};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn popup_uses_prompt_geometry_rules() {
        let viewport = Rect::new(0, 0, 100, 30);
        let base = popup_rect(viewport, 2, &ModalGeometry::default());
        let moved = popup_rect(
            viewport,
            2,
            &ModalGeometry {
                dx: 4,
                dy: 2,
                dw: 6,
                dh: 4,
            },
        );
        assert_eq!(moved.x, base.x + 4);
        assert_eq!(moved.y, base.y + 2);
        assert_eq!(moved.width, base.width + 6);
        assert_eq!(moved.height, base.height + 4);
        assert_eq!(popup_rect(viewport, 2, &ModalGeometry::default()), base);
    }

    #[test]
    fn renders_newest_first_with_spaced_rows_and_session_cue() {
        let mut app = crate::app::app_test_helpers::with_session_list(0);
        let first = uuid::Uuid::new_v4();
        let second = uuid::Uuid::new_v4();
        app.push_notification(
            NotificationKind::Info,
            NotificationPriority::Medium,
            "older notice".into(),
            Some(first),
        );
        app.push_notification(
            NotificationKind::Info,
            NotificationPriority::Medium,
            "newer notice".into(),
            Some(second),
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| {
                render_notification_browser(
                    frame,
                    frame.area(),
                    &app,
                    0,
                    &ModalGeometry::default(),
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows: Vec<String> = (0..30)
            .map(|y| (0..100).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let newer_row = rows
            .iter()
            .position(|row| row.contains("newer notice"))
            .unwrap();
        let older_row = rows
            .iter()
            .position(|row| row.contains("older notice"))
            .unwrap();
        assert_eq!(older_row, newer_row + 3);
        assert!(rows[newer_row - 1].contains("↗ session"));
        assert!(rows.iter().any(|row| row.contains("Ctrl+Arrow move")));
    }
}
