//! Project picker popup rendering.

use crate::app::App;
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;
use crate::ui::parse_hex_color;

/// Render the project picker popup.
pub(super) fn render_project_picker(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    filter: &str,
    selected_index: usize,
    context: &crate::types::ProjectPickerContext,
) {
    use ratatui::style::Color;

    // Build list of entries: "All", projects, "(unassigned)"
    // Each entry is (name, path, color, session_count, is_special)
    struct PickerEntry {
        name: String,
        path: Option<String>,
        color: Color,
        session_count: usize,
        #[allow(dead_code)]
        project_id: Option<uuid::Uuid>, // None = "All" or "unassigned"
        #[allow(dead_code)]
        is_unassigned: bool,
    }

    let mut entries: Vec<PickerEntry> = Vec::new();

    // Determine if in reassign mode
    let reassign_mode = matches!(
        context,
        crate::types::ProjectPickerContext::SessionReassign(_)
    );

    // Count sessions per project
    let mut project_session_counts: std::collections::HashMap<uuid::Uuid, usize> =
        std::collections::HashMap::new();
    let mut unassigned_count = 0;
    for s in app.sessions.values() {
        if let Some(pid) = s.session.project_id {
            *project_session_counts.entry(pid).or_insert(0) += 1;
        } else {
            unassigned_count += 1;
        }
    }

    // "all projects" entry — always first in workspace (GlobalFilter) mode
    if !reassign_mode {
        entries.push(PickerEntry {
            name: "all projects".to_string(),
            path: None,
            color: theme::subtext0(),
            session_count: app.sessions.len(),
            project_id: None,
            is_unassigned: false,
        });
    }

    // Projects (sorted by name)
    let mut sorted_projects = app.projects.clone();
    sorted_projects.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    for project in &sorted_projects {
        let color = parse_hex_color(&project.color).unwrap_or(theme::blue());
        let path_display = project.path.as_ref().map(|p| {
            let s = p.to_string_lossy();
            s.replacen(
                &dirs::home_dir().map_or(String::new(), |h| h.to_string_lossy().to_string()),
                "~",
                1,
            )
        });
        entries.push(PickerEntry {
            name: project.name.clone(),
            path: path_display,
            color,
            session_count: *project_session_counts.get(&project.id).unwrap_or(&0),
            project_id: Some(project.id),
            is_unassigned: false,
        });
    }

    // "(unassigned)" entry — only in reassign mode
    if reassign_mode {
        entries.push(PickerEntry {
            name: "(unassigned)".to_string(),
            path: None,
            color: theme::overlay1(),
            session_count: unassigned_count,
            project_id: None,
            is_unassigned: true,
        });
    }

    // Filter entries by fuzzy match on name (case-insensitive)
    let filter_lower = filter.to_lowercase();
    let filtered_entries: Vec<(usize, &PickerEntry)> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            if filter.is_empty() {
                true
            } else {
                e.name.to_lowercase().contains(&filter_lower)
            }
        })
        .collect();

    // Popup size: width 55, height = min(filtered + 4, 20)
    let popup_height = (filtered_entries.len() as u16 + 5).min(20);
    let popup_area = fixed_centered_rect(area, 55, popup_height);

    frame.render_widget(Clear, popup_area);

    let title_text = if reassign_mode {
        " Assign Project "
    } else {
        " Workspaces "
    };

    let block = theme::overlay_block()
        .title(Line::from(vec![Span::styled(
            title_text,
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )]))
        .padding(Padding::horizontal(1));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if inner.height < 3 {
        return;
    }

    // Row 0: filter input
    let filter_line = Line::from(vec![
        Span::styled("> ", Style::default().fg(theme::mauve())),
        Span::styled(filter, Style::default().fg(theme::text())),
        Span::styled("█", Style::default().fg(theme::text())), // cursor
    ]);
    let filter_area = Rect::new(inner.x, inner.y, inner.width, 1);
    frame.render_widget(Paragraph::new(filter_line), filter_area);

    // Separator
    let sep_area = Rect::new(inner.x, inner.y + 1, inner.width, 1);
    let sep_char = "─".repeat(inner.width as usize);
    frame.render_widget(
        Paragraph::new(sep_char).style(Style::default().fg(theme::surface2())),
        sep_area,
    );

    // List entries
    let list_start_y = inner.y + 2;
    let list_height = inner.height.saturating_sub(3); // filter + sep + hint bar

    for (visible_idx, (_original_idx, entry)) in filtered_entries
        .iter()
        .enumerate()
        .take(list_height as usize)
    {
        let is_selected = visible_idx == selected_index;

        let row_style = if is_selected {
            Style::default().bg(theme::surface2())
        } else {
            Style::default().bg(theme::overlay_bg())
        };

        // Color swatch
        let swatch = Span::styled("■ ", Style::default().fg(entry.color));

        // Name (bold if selected)
        let name_style = if is_selected {
            Style::default()
                .fg(entry.color)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(entry.color)
        };
        let name = Span::styled(&entry.name, name_style);

        // Path (dimmed, if present)
        let path_span = if let Some(ref p) = entry.path {
            // Truncate path to fit
            let max_path = (inner.width as usize).saturating_sub(entry.name.len() + 15);
            let path_display = if p.len() > max_path && max_path > 3 {
                format!("…{}", &p[p.len() - max_path + 1..])
            } else {
                p.clone()
            };
            Span::styled(
                format!("  {}", path_display),
                Style::default().fg(theme::overlay_hint()),
            )
        } else {
            Span::raw("")
        };

        // Session count (right-aligned)
        let count = Span::styled(
            format!("{:>3} sess", entry.session_count),
            Style::default().fg(theme::overlay_hint()),
        );

        // Build the line with spacing
        let line = Line::from(vec![swatch, name, path_span, Span::raw("  "), count]);

        let row_y = list_start_y + visible_idx as u16;
        if row_y < inner.y + inner.height - 1 {
            let row_area = Rect::new(inner.x, row_y, inner.width, 1);
            frame.render_widget(Paragraph::new(line).style(row_style), row_area);
        }
    }

    // Hint bar at the bottom
    let hint_y = inner.y + inner.height - 1;
    let hint_area = Rect::new(inner.x, hint_y, inner.width, 1);
    let hint = Line::from(vec![Span::styled(
        if reassign_mode {
            "j/k: navigate  Enter: assign  ^n new  Esc: cancel"
        } else {
            "j/k: navigate  Enter: open workspace  ^n new  ^d delete  Esc: cancel"
        },
        Style::default().fg(theme::overlay_hint()),
    )]);
    frame.render_widget(Paragraph::new(hint), hint_area);
}
