use ratatui::layout::{Alignment, Rect};
use ratatui::prelude::*;
use ratatui::widgets::{Clear, List, ListItem, ListState, Padding, Paragraph};
use rsi_common::types::ScheduledJob;

use crate::ui::theme;

pub fn render_schedule_browser(
    frame: &mut Frame,
    area: Rect,
    jobs: &[ScheduledJob],
    selected_index: usize,
    loading: bool,
) {
    let width = 100.min(area.width.saturating_sub(4));
    let height = 30.min(area.height.saturating_sub(4));
    let popup = super::fixed_centered_rect(area, width, height);

    frame.render_widget(Clear, popup);
    let block = theme::overlay_block()
        .title(" Scheduled Jobs ")
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
            let line = format!(
                "{cursor}[{enabled_icon}] {:<24} {:<16} next: {next_str}",
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
    fn selected_job_scrolls_into_view() {
        let jobs: Vec<_> = (0..40).map(job).collect();
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("test terminal should initialize");

        terminal
            .draw(|frame| {
                render_schedule_browser(frame, frame.area(), &jobs, 39, false);
            })
            .expect("schedule browser should render");

        let text = buffer_lines(terminal.backend().buffer()).join("\n");
        assert!(text.contains("job-39"));
        assert!(text.contains("> [+] job-39"));
        assert!(!text.contains("j/k: nav"));
    }
}
