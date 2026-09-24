use ratatui::prelude::*;
use ratatui::widgets::{Clear, List, ListItem, ListState, Padding, Paragraph, Wrap};
use rsi_common::types::SessionKind;

use crate::overlay::harness_manager::HarnessManagerScopeState;
use crate::ui::theme;

use super::fixed_centered_rect;

pub(super) fn render(frame: &mut Frame, area: Rect, state: &HarnessManagerScopeState) {
    let visible = state.visible_indices();
    let height = (visible.len().min(16) as u16 + 9).max(12);
    let popup = fixed_centered_rect(area, 88, height);
    frame.render_widget(Clear, popup);
    let block = theme::overlay_block()
        .title(if state.appointing {
            " Appoint Harness Manager "
        } else {
            " Manager Scope "
        })
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 7 || inner.width < 4 {
        return;
    }
    let regions = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .split(inner);
    frame.render_widget(
        Paragraph::new(format!("{} · {}", state.manager_name, state.project_name))
            .style(Style::default().fg(theme::text()).bold()),
        regions[0],
    );
    frame.render_widget(
        Paragraph::new(if state.all_project {
            format!(
                "Whole project · current and future Epics · revision {}",
                state.expected_row_version
            )
        } else {
            format!(
                "{} Groups + {} individual Epics · revision {}{}",
                state.selected_groups.len(),
                state.selected_epics.len(),
                state.expected_row_version,
                if state.selected_epics.is_empty() && state.selected_groups.is_empty() {
                    " · empty scope revokes"
                } else {
                    ""
                }
            )
        })
        .style(Style::default().fg(theme::overlay_hint())),
        regions[1],
    );
    frame.render_widget(
        Paragraph::new(format!(
            "/ {}{}",
            state.query,
            if state.searching { "▏" } else { "" }
        ))
        .style(Style::default().fg(theme::text())),
        regions[2],
    );
    if visible.is_empty() {
        frame.render_widget(
            Paragraph::new(if state.rows.is_empty() {
                "No Groups/Epics yet. Whole project includes future Epics."
            } else {
                "No matching Groups/Epics. Press / to change the filter."
            })
            .wrap(Wrap { trim: true }),
            regions[3],
        );
    } else {
        let rows = visible.iter().map(|&index| {
            let epic = &state.rows[index];
            ListItem::new(format!(
                "[{}] {} {}{}{} · {}",
                if state.chosen(epic) {
                    "x"
                } else if state.inherited(epic) && epic.available {
                    "+"
                } else {
                    " "
                },
                if epic.kind == SessionKind::Group {
                    "Group"
                } else {
                    "  Epic"
                },
                epic.name,
                if epic.available {
                    ""
                } else {
                    " (unavailable; deselect)"
                },
                if epic.group_name.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", epic.group_name)
                },
                &epic.id.to_string()[..8],
            ))
        });
        let list = List::new(rows)
            .style(Style::default().fg(theme::text()))
            .highlight_style(Style::default().bg(theme::surface2()).bold())
            .highlight_symbol("› ");
        let mut selection = ListState::default().with_selected(Some(state.selected));
        frame.render_stateful_widget(list, regions[3], &mut selection);
    }
    frame.render_widget(
        Paragraph::new(
            state
                .error
                .as_deref()
                .unwrap_or(if state.expected_row_version > 0 {
                    "A changed scope revokes any saved V2 policy; re-save policy after. Space selects; a selects whole project."
                } else {
                    "Space selects Group/Epic scope; a selects whole project. [+] inherited coverage. Groups include future Epics."
                }),
        )
        .style(Style::default().fg(if state.error.is_some() {
            theme::red()
        } else {
            theme::overlay_hint()
        }))
        .wrap(Wrap { trim: true }),
        regions[4],
    );
    frame.render_widget(
        Paragraph::new(if state.searching {
            "Type filter · Enter/Esc finish filter"
        } else {
            "j/k move · Space toggle · a all project · / filter · Enter save · Esc cancel"
        })
        .style(Style::default().fg(theme::overlay_hint())),
        regions[5],
    );
}
