//! Notification browser rendering.

use crate::app::App;
use crate::types::{NotificationKind, NotificationPriority};
use crate::ui::{session, theme};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;

pub(super) fn render_notification_browser(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    selected_index: usize,
) {
    // Build display list: active notifications first, then history (newest first)
    struct DisplayItem {
        kind: NotificationKind,
        priority: NotificationPriority,
        message: String,
        age: String,
        session_id: Option<uuid::Uuid>,
        is_active: bool,
    }

    let mut items: Vec<DisplayItem> = Vec::new();

    for n in app.notifications.iter() {
        let elapsed = n.created_at.elapsed().as_secs();
        let age = if elapsed < 60 {
            format!("{}s ago", elapsed)
        } else {
            format!("{}m ago", elapsed / 60)
        };
        items.push(DisplayItem {
            kind: n.kind,
            priority: n.priority,
            message: n.message.clone(),
            age,
            session_id: n.session_id,
            is_active: true,
        });
    }

    // History: newest first (history is stored oldest-first, so reverse)
    for n in app.notification_history.iter().rev() {
        let elapsed = n.created_at.elapsed().as_secs();
        let age = if elapsed < 60 {
            format!("{}s ago", elapsed)
        } else if elapsed < 3600 {
            format!("{}m ago", elapsed / 60)
        } else {
            format!("{}h ago", elapsed / 3600)
        };
        items.push(DisplayItem {
            kind: n.kind,
            priority: n.priority,
            message: n.message.clone(),
            age,
            session_id: n.session_id,
            is_active: false,
        });
    }

    let visible_count = (items.len() as u16).min(20).max(3);
    let popup_height = visible_count + 4;
    let popup_area = fixed_centered_rect(area, 60, popup_height);

    frame.render_widget(Clear, popup_area);

    let block = theme::overlay_block()
        .title(Line::from(vec![Span::styled(
            " Notifications ",
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )]))
        .padding(Padding::horizontal(1));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if inner.height < 2 {
        return;
    }

    if items.is_empty() {
        let empty =
            Paragraph::new("No notifications").style(Style::default().fg(theme::subtext0()));
        frame.render_widget(empty, inner);
        return;
    }

    let list_height = inner.height.saturating_sub(1) as usize;
    let mut lines = Vec::new();

    for (i, item) in items.iter().enumerate() {
        let is_selected = i == selected_index;

        let icon = match item.kind {
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
        };

        let msg_color = if !item.is_active {
            theme::subtext0()
        } else {
            match item.priority {
                NotificationPriority::High => theme::red(),
                NotificationPriority::Medium => theme::text(),
                NotificationPriority::Low => theme::subtext0(),
            }
        };

        let bg = if is_selected {
            theme::surface2()
        } else {
            theme::root_bg()
        };

        // Truncate message to fit
        let max_msg = 42;
        let msg = if item.message.len() > max_msg {
            let boundary = session::floor_char_boundary(&item.message, max_msg);
            format!("{}…", &item.message[..boundary])
        } else {
            item.message.clone()
        };

        let nav_indicator = if item.session_id.is_some() {
            "→"
        } else {
            " "
        };

        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", icon), Style::default().fg(msg_color).bg(bg)),
            Span::styled(format!("{} ", msg), Style::default().fg(msg_color).bg(bg)),
            Span::styled(
                format!("{} {} ", item.age, nav_indicator),
                Style::default().fg(theme::subtext0()).bg(bg),
            ),
        ]));
    }

    // Scroll
    let scroll = if selected_index >= list_height {
        selected_index - list_height + 1
    } else {
        0
    };
    let visible_lines: Vec<Line> = lines.into_iter().skip(scroll).take(list_height).collect();

    let list_widget = Paragraph::new(visible_lines);
    let list_area = Rect {
        height: inner.height.saturating_sub(1),
        ..inner
    };
    frame.render_widget(list_widget, list_area);

    // Hint bar
    let hint = Line::from(vec![
        Span::styled("x", Style::default().fg(theme::peach())),
        Span::styled(" dismiss  ", Style::default().fg(theme::subtext0())),
        Span::styled("Enter", Style::default().fg(theme::peach())),
        Span::styled(" go to session  ", Style::default().fg(theme::subtext0())),
        Span::styled("N", Style::default().fg(theme::peach())),
        Span::styled(" dismiss all  ", Style::default().fg(theme::subtext0())),
        Span::styled("q", Style::default().fg(theme::peach())),
        Span::styled(" close", Style::default().fg(theme::subtext0())),
    ]);
    let hint_area = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: 1,
        ..inner
    };
    frame.render_widget(Paragraph::new(hint), hint_area);
}
