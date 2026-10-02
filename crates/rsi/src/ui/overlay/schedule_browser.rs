use ratatui::layout::{Alignment, Rect};
use ratatui::prelude::*;
use ratatui::widgets::{Clear, List, ListItem, ListState, Padding, Paragraph};
use rsi_common::types::ScheduledJob;

use crate::ui::theme;

pub fn render_schedule_browser(
    frame: &mut Frame,
    area: Rect,
    jobs: &[ScheduledJob],
    holds: &std::collections::HashMap<uuid::Uuid, rsi_common::child_autonomy::ScheduledJobHoldV1>,
    selected_index: usize,
    loading: bool,
    paging: &crate::types::SchedulePaging,
) {
    let width = 100.min(area.width.saturating_sub(4));
    let height = 30.min(area.height.saturating_sub(4));
    let popup = super::fixed_centered_rect(area, width, height);

    frame.render_widget(Clear, popup);
    let block = theme::overlay_block()
        .title(title(paging))
        .title_alignment(Alignment::Center)
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    if loading {
        let loading_text =
            Paragraph::new("Loading...").style(Style::default().fg(theme::overlay_hint()));
        frame.render_widget(loading_text, inner);
        return;
    }

    if jobs.is_empty() {
        let empty =
            Paragraph::new("No scheduled jobs").style(Style::default().fg(theme::overlay_hint()));
        frame.render_widget(empty, inner);
        return;
    }

    let list_area = inner;

    let items: Vec<ListItem> = jobs
        .iter()
        .enumerate()
        .map(|(i, job)| {
            let enabled_icon = if job.enabled { "+" } else { "-" };
            let recurrence_str = format_recurrence(&job.schedule.recurrence);
            let next_str = job.next_fire_at.format("%Y-%m-%d %H:%M").to_string();
            let cursor = if i == selected_index { "> " } else { "  " };
            let held = holds.get(&job.id).map_or_else(String::new, hold_badge);
            let line = format!(
                "{cursor}[{enabled_icon}] {:<24} {:<16} next: {next_str}{held}",
                truncate(&job.name, 24),
                recurrence_str,
            );

            let style = if i == selected_index {
                Style::default()
                    .fg(theme::accent())
                    .add_modifier(Modifier::BOLD)
            } else if !job.enabled {
                Style::default().fg(theme::disabled())
            } else {
                Style::default().fg(theme::text())
            };

            ListItem::new(Line::from(Span::styled(line, style)))
        })
        .collect();

    let list = List::new(items);
    let mut list_state = ListState::default();
    list_state.select(Some(selected_index));
    frame.render_stateful_widget(list, list_area, &mut list_state);
}

/// Title naming the active filter so the operator knows whether history is
/// hidden, and whether more pages remain (`m` loads them).
fn title(paging: &crate::types::SchedulePaging) -> String {
    let filter = if paging.include_history {
        "all history"
    } else {
        "active"
    };
    let more = if paging.next_cursor.is_some() {
        " · more: m"
    } else {
        ""
    };
    format!(" Scheduled Jobs · {filter} · H: history{more} ")
}

/// `  HELD xN until HH:MM`: why a due wake has not fired, N running children (#794 S3).
fn hold_badge(hold: &rsi_common::child_autonomy::ScheduledJobHoldV1) -> String {
    format!(
        "  HELD x{} until {}",
        hold.running_children.len(),
        hold.release_at
            .with_timezone(&chrono::Local)
            .format("%H:%M")
    )
}

fn format_recurrence(r: &rsi_common::types::Recurrence) -> String {
    use rsi_common::types::Recurrence::*;
    match r {
        Once => "once".into(),
        EverySeconds(n) => format!("every {n}s"),
        EveryMinutes(n) => format!("every {n}m"),
        EveryHours(n) => format!("every {n}h"),
        EveryDays(n) => format!("every {n}d"),
        EveryWeeks(n) => format!("every {n}w"),
        EveryMonths(n) => format!("every {n}mo"),
        EveryYears(n) => format!("every {n}y"),
        _ => "?".into(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}...", &s[..max.saturating_sub(3)])
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
    use rsi_common::types::{Recurrence, ScheduleSpec, WakeMode};
    use uuid::Uuid;

    fn job(index: usize) -> ScheduledJob {
        let now = Utc::now();
        ScheduledJob {
            id: Uuid::new_v4(),
            name: format!("job-{index}"),
            message: "test job".to_string(),
            schedule: ScheduleSpec {
                recurrence: Recurrence::Once,
                anchor: now,
            },
            last_fired_at: None,
            next_fire_at: now,
            enabled: true,
            working_dir: None,
            provider: None,
            model: None,
            project_id: None,
            created_at: now,
            updated_at: now,
            wake_mode: WakeMode::Fresh,
            wake_session_id: None,
        }
    }

    fn buffer_lines(buffer: &Buffer) -> Vec<String> {
        let width = buffer.area.width as usize;
        buffer
            .content
            .chunks(width)
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect()
    }

    #[test]
    fn a_held_wake_shows_its_children_and_release_time() {
        let jobs: Vec<_> = (0..2).map(job).collect();
        let held = rsi_common::child_autonomy::ScheduledJobHoldV1 {
            job_id: jobs[1].id,
            parent_session_id: Uuid::new_v4(),
            running_children: vec![Uuid::new_v4(), Uuid::new_v4()],
            held_since: Utc::now(),
            release_at: Utc::now() + chrono::Duration::seconds(1500),
        };
        let holds = std::collections::HashMap::from([(held.job_id, held)]);
        let backend = TestBackend::new(140, 20);
        let mut terminal = Terminal::new(backend).expect("test terminal should initialize");
        terminal
            .draw(|frame| {
                render_schedule_browser(
                    frame,
                    frame.area(),
                    &jobs,
                    &holds,
                    0,
                    false,
                    &Default::default(),
                )
            })
            .expect("schedule browser should render");
        let lines = buffer_lines(terminal.backend().buffer());
        let held_line = lines
            .iter()
            .find(|line| line.contains("job-1"))
            .expect("held job row");
        assert!(held_line.contains("HELD x2 until"), "{held_line}");
        let plain = lines
            .iter()
            .find(|line| line.contains("job-0"))
            .expect("plain job row");
        assert!(
            plain.contains("next:") && !plain.contains("HELD"),
            "{plain}"
        );
    }

    #[test]
    fn selected_job_scrolls_into_view() {
        let jobs: Vec<_> = (0..40).map(job).collect();
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("test terminal should initialize");

        terminal
            .draw(|frame| {
                render_schedule_browser(
                    frame,
                    frame.area(),
                    &jobs,
                    &std::collections::HashMap::new(),
                    39,
                    false,
                    &Default::default(),
                );
            })
            .expect("schedule browser should render");

        let text = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(text.contains("job-39"));
        assert!(text.contains("> [+] job-39"));
        assert!(!text.contains("j/k: nav"));
    }

    #[test]
    fn title_names_the_active_filter_and_pending_pages() {
        let render = |paging: &crate::types::SchedulePaging| {
            let jobs: Vec<_> = (0..2).map(job).collect();
            let backend = TestBackend::new(120, 20);
            let mut terminal = Terminal::new(backend).expect("test terminal should initialize");
            terminal
                .draw(|frame| {
                    render_schedule_browser(
                        frame,
                        frame.area(),
                        &jobs,
                        &std::collections::HashMap::new(),
                        0,
                        false,
                        paging,
                    );
                })
                .expect("schedule browser should render");
            buffer_lines(terminal.backend().buffer()).join("\n")
        };

        let active = render(&Default::default());
        assert!(active.contains("Scheduled Jobs · active · H: history"));
        assert!(!active.contains("more: m"));

        let history = render(&crate::types::SchedulePaging {
            include_history: true,
            next_cursor: Some("next".to_string()),
        });
        assert!(history.contains("Scheduled Jobs · all history · H: history · more: m"));
    }
}
