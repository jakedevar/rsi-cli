use crate::{overlay::fleet::FleetState, ui::theme};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use rsi_common::fleet::FLEET_WINDOWS;
fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}
fn panel(title: &str) -> Block<'_> {
    Block::default()
        .title(title)
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme::dim_metadata()))
}
pub fn render(frame: &mut Frame, area: Rect, state: &FleetState) {
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(
            Style::default()
                .bg(theme::glass_panel_bg())
                .fg(theme::text()),
        ),
        area,
    );
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length((area.height / 3).clamp(5, 12)),
        Constraint::Min(4),
    ])
    .split(area);
    let freshness = state
        .snapshot
        .as_ref()
        .map(|s| format!("{} UTC", s.as_of.format("%H:%M:%S")))
        .unwrap_or_else(|| {
            if state.error.is_some() {
                "Unavailable".into()
            } else {
                "Loading…".into()
            }
        });
    let limit = state
        .snapshot
        .as_ref()
        .is_some_and(|s| s.agents_truncated || s.usage_truncated);
    let active = state.snapshot.as_ref().map_or(0, |s| s.agents.len());
    let warning = if state.error.is_some() {
        if state.snapshot.is_some() {
            " · STALE"
        } else {
            " · ERROR"
        }
    } else {
        ""
    };
    let header = format!(
        "FLEET{warning}{}   /   ALL PROJECTS     ● {active} active     [{}] [{}]   {freshness}",
        if limit { " · PARTIAL (limit)" } else { "" },
        state.dimension(),
        ["5 min", "1 hour", "24 hours"][state.window]
    );
    let second = if let Some(error) = &state.error {
        clean(error)
    } else if state.sorting {
        if state.focus_groups {
            "Sort: 1 group · 2 active · 3 tokens · 4 cost · 5 errors".into()
        } else {
            "Sort: 1 project · 2 provider · 3 model · 4 status · 5 context".into()
        }
    } else {
        format!(
            "{} /{}{}   sort: {} {}   ? help",
            if state.focus_groups {
                "Usage"
            } else {
                "Agents"
            },
            clean(&state.filter),
            if state.editing { "▏" } else { "" },
            state.sort_label(),
            if state.reverse { "↓" } else { "↑" }
        )
    };
    let total = state
        .snapshot
        .as_ref()
        .map(|s| {
            let u = &s.totals[state.window];
            format!(
                "Fleet total  {:.0} tok/min   ${:.2}/hr   {} invocations   {:.1}% errors{}",
                u.tokens_per_minute(FLEET_WINDOWS[state.window]),
                u.cost_per_hour(FLEET_WINDOWS[state.window]),
                u.invocations,
                u.error_pct(),
                if u.unknown_usage > 0 {
                    " · usage partial"
                } else {
                    ""
                }
            )
        })
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(header),
            Line::from(second),
            Line::from(total),
        ])
        .style(Style::default().fg(theme::text())),
        chunks[0],
    );
    let seconds = FLEET_WINDOWS[state.window];
    let groups = state.groups();
    let rows = groups.iter().map(|g| {
        let u = &g.windows[state.window];
        let mut cells = vec![
            clean(&g.label),
            g.active.to_string(),
            u.input.to_string(),
            u.output.to_string(),
            format!("{}/{}", u.cache_read, u.cache_write),
            format!("${:.3}", u.cost),
            format!("{:.0}", u.tokens_per_minute(seconds)),
            format!("${:.2}", u.cost_per_hour(seconds)),
            u.invocations.to_string(),
            format!(
                "{:.1}%{}",
                u.error_pct(),
                if u.unknown_usage > 0 { " ?" } else { "" }
            ),
        ];
        if area.width < 100 {
            cells.drain(2..5);
        }
        Row::new(cells)
    });
    let mut widths = vec![
        Constraint::Min(18),
        Constraint::Length(6),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(15),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Length(7),
        Constraint::Length(8),
    ];
    let mut headers = vec![
        "GROUP",
        "ACTIVE",
        "INPUT",
        "OUTPUT",
        "CACHE R/W",
        "EST COST",
        "TOK/MIN",
        "$/HR",
        "CALLS",
        "ERRORS",
    ];
    if area.width < 100 {
        widths.drain(2..5);
        headers.drain(2..5);
    }
    frame.render_stateful_widget(
        Table::new(rows, widths)
            .row_highlight_style(Style::default().bg(theme::selected_row_bg()))
            .highlight_symbol(if state.focus_groups { "▎" } else { " " })
            .header(Row::new(headers).style(Style::default().fg(theme::table_header_text())))
            .block(panel(
                " Usage · by invocation start · ? partial usage · b group · w window ",
            )),
        chunks[1],
        &mut TableState::default().with_selected(Some(state.selected_group)),
    );
    let body = if area.width >= 150 {
        Layout::horizontal([Constraint::Percentage(70), Constraint::Percentage(30)])
            .split(chunks[2])
    } else {
        Layout::vertical([Constraint::Min(3), Constraint::Length(6)]).split(chunks[2])
    };
    let agents = state.agents();
    let now = chrono::Utc::now();
    let rows = agents.iter().map(|a| {
        let mut cells = vec![
            clean(&a.project),
            clean(&a.role),
            format!("{:?}", a.session.provider),
            clean(a.session.model.as_deref().unwrap_or("◌")),
            clean(a.session.effort.as_deref().unwrap_or("default")),
            format!(
                "{} {}",
                crate::ui::session::status_icon(a.session.status),
                crate::ui::session::status_text(a.session.status)
            ),
            a.session
                .context_fill_pct
                .map_or("◌".into(), |p| format!("{p:.0}%")),
            a.turn_started_at.map_or("◌".into(), |t| {
                format!("{}s", now.signed_duration_since(t).num_seconds().max(0))
            }),
        ];
        if body[0].width < 100 {
            cells.remove(4);
            cells.remove(2);
        }
        Row::new(cells)
    });
    let mut widths = vec![
        Constraint::Min(12),
        Constraint::Length(12),
        Constraint::Length(13),
        Constraint::Min(18),
        Constraint::Length(8),
        Constraint::Length(15),
        Constraint::Length(5),
        Constraint::Length(8),
    ];
    let mut headers = vec![
        "PROJECT", "ROLE", "PROVIDER", "MODEL", "EFFORT", "STATUS", "CTX", "TURN",
    ];
    if body[0].width < 100 {
        widths.remove(4);
        widths.remove(2);
        headers.remove(4);
        headers.remove(2);
    }
    let mut selection = TableState::default().with_selected(Some(state.selected));
    let title = if agents.is_empty() {
        if state.filter.is_empty() {
            " Active agents · no active agents "
        } else {
            " Active agents · no matches "
        }
    } else {
        " Active agents "
    };
    frame.render_stateful_widget(
        Table::new(rows, widths)
            .header(Row::new(headers).style(Style::default().fg(theme::table_header_text())))
            .block(panel(title))
            .row_highlight_style(
                Style::default()
                    .bg(theme::selected_row_bg())
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(if state.focus_groups { " " } else { "▎" }),
        body[0],
        &mut selection,
    );
    let detail = if state.focus_groups {
        if let Some(g) = groups.get(state.selected_group) {
            let mut lines = vec![format!("{} · {} active", clean(&g.label), g.active)];
            let i = state.window;
            let u = &g.windows[i];
            lines.push(format!("{}: {} calls · {:.1}% errors\n{} in / {} out / {} cache read / {} cache write\n${:.3} · {:.0} tok/min · ${:.2}/hr · {} unknown usage",["5 min","1 hour","24 hours"][i],u.invocations,u.error_pct(),u.input,u.output,u.cache_read,u.cache_write,u.cost,u.tokens_per_minute(FLEET_WINDOWS[i]),u.cost_per_hour(FLEET_WINDOWS[i]),u.unknown_usage));
            lines.join("\n")
        } else {
            "No usage groups match the filter.".into()
        }
    } else if let Some(a) = agents.get(state.selected) {
        format!(
            "{:?} · {}
{}
{} / {:?} / {} / {}
Context {} · current turn {}
{}
Enter opens this session",
            a.session.status,
            clean(&a.role),
            clean(a.session.title.as_deref().unwrap_or(&a.session.query)),
            clean(&a.project),
            a.session.provider,
            clean(a.session.model.as_deref().unwrap_or("Unknown")),
            clean(a.session.effort.as_deref().unwrap_or("default")),
            a.session
                .context_fill_pct
                .map_or("unknown".into(), |p| format!("{p:.0}%")),
            a.turn_started_at.map_or("unknown".into(), |t| format!(
                "{}s",
                now.signed_duration_since(t).num_seconds().max(0)
            )),
            a.session.id
        )
    } else {
        "Select an active agent.
Usage includes all projects, including completed agents.
Unknown context or turn age: ◌"
            .into()
    };
    frame.render_widget(
        Paragraph::new(detail)
            .wrap(Wrap { trim: false })
            .block(panel(" Inspector ")),
        body[1],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    fn screen(state: &FleetState, width: u16, height: u16) -> String {
        let _guard = theme::test_render_guard();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| render(f, f.area(), state)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_string()
    }
    #[test]
    fn fleet_full_workspace_renders_at_narrow_and_wide_sizes() {
        let mut state = FleetState::default();
        state.install(crate::overlay::fleet::tests::snapshot());
        for (width, height) in [(120, 40), (200, 58), (240, 70), (80, 24)] {
            let text = screen(&state, width, height);
            for needle in [
                "FLEET",
                "ALL PROJECTS",
                "Atlas",
                "Research Atlas",
                "Inspector",
            ] {
                assert!(text.contains(needle), "{needle}:\n{text}");
            }
            if let Ok(dir) = std::env::var("RSI_FLEET_DUMPS") {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(format!("{dir}/fleet-{width}x{height}.txt"), text).unwrap();
            }
        }
        state.snapshot.as_mut().unwrap().usage_truncated = true;
        assert!(screen(&state, 80, 24).contains("PARTIAL (limit)"));
        state.error = Some("Permission denied; retry".into());
        let text = screen(&state, 120, 40);
        assert!(text.contains("STALE"));
        assert!(text.contains("Permission denied"));
        state.snapshot = None;
        state.error = None;
        assert!(screen(&state, 120, 40).contains("Loading"));
        state.install(crate::overlay::fleet::tests::snapshot());
        state.filter = "no results".into();
        assert!(screen(&state, 120, 40).contains("no matches"));
    }
    #[test]
    fn fleet_uses_live_semantic_theme_roles() {
        theme::with_theme_state(|| {
            let mut state = FleetState::default();
            state.install(crate::overlay::fleet::tests::snapshot());
            for index in 0..theme::THEME_COUNT {
                theme::set_theme_by_index(index);
                let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
                terminal.draw(|f| render(f, f.area(), &state)).unwrap();
                assert_eq!(terminal.backend().buffer()[(0, 0)].fg, theme::text());
                assert_eq!(
                    terminal.backend().buffer()[(119, 39)].bg,
                    theme::glass_panel_bg()
                );
            }
            theme::set_theme_role_override(
                crate::ui::theme_roles::ThemeRole::PrimaryText,
                Some([171, 193, 217]),
            );
            let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal.draw(|f| render(f, f.area(), &state)).unwrap();
            assert_eq!(terminal.backend().buffer()[(0, 0)].fg, theme::text());
        });
    }
}
