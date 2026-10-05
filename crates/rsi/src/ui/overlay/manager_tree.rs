//! `:manager tree` (#890): render the read-only manager hierarchy.

use crate::overlay::manager_tree::{HINTS, ManagerTreeState, row_text};
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;

fn safe(value: &str) -> String {
    value.chars().filter(|ch| !ch.is_control()).collect()
}

/// The text of the visible rows (indent, fold marker, row text) in order.
pub(crate) fn tree_lines(state: &ManagerTreeState) -> Vec<String> {
    state
        .visible()
        .into_iter()
        .map(|index| {
            let row = &state.rows[index];
            let marker = if !state.has_children(index) {
                "  "
            } else if state.collapsed.contains(&row.key) {
                "▸ "
            } else {
                "▾ "
            };
            format!(
                "{}{}{}",
                "  ".repeat(usize::from(row.depth)),
                marker,
                safe(&row_text(row))
            )
        })
        .collect()
}

pub(super) fn render(frame: &mut Frame, area: Rect, state: &ManagerTreeState) {
    let popup = fixed_centered_rect(
        area,
        area.width.saturating_sub(2),
        area.height.saturating_sub(2),
    );
    frame.render_widget(Clear, popup);
    let block = theme::overlay_block()
        .title(Line::from(Span::styled(
            " Manager tree · read-only ",
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )))
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 3 {
        return;
    }
    let body_height = usize::from(inner.height) - 2;
    let lines = tree_lines(state);
    let offset = state.selected.saturating_sub(body_height.saturating_sub(1));
    let mut out: Vec<Line> = vec![Line::from(Span::styled(
        state.count_line(),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    if lines.is_empty() {
        out.push(Line::from("(no managers or grants to show)"));
    }
    for (index, text) in lines.iter().enumerate().skip(offset).take(body_height) {
        let style = if index == state.selected {
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(theme::text())
        };
        out.push(Line::from(Span::styled(text.clone(), style)));
    }
    let footer = state
        .error
        .as_deref()
        .map_or_else(|| HINTS.to_string(), |error| format!("{error}   {HINTS}"));
    let mut paragraph_lines = out;
    while paragraph_lines.len() < usize::from(inner.height) - 1 {
        paragraph_lines.push(Line::default());
    }
    paragraph_lines.push(Line::from(Span::styled(
        footer,
        Style::default().fg(if state.error.is_some() {
            theme::error_status()
        } else {
            theme::text()
        }),
    )));
    frame.render_widget(Paragraph::new(paragraph_lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::manager_tree::{
        GetManagerTreeResultV1, ManagerTreeKindV1, ManagerTreeLoadV1, ManagerTreeRowV1,
    };

    fn node(key: &str, parent: Option<&str>, depth: u16) -> ManagerTreeRowV1 {
        ManagerTreeRowV1 {
            key: key.into(),
            parent_key: parent.map(str::to_string),
            depth,
            kind: ManagerTreeKindV1::Project,
            label: key.into(),
            project_id: None,
            node_id: None,
            epic_id: None,
            scope: None,
            seat: None,
            focus_session_id: None,
            grant: None,
            load: ManagerTreeLoadV1::default(),
            complete: true,
        }
    }

    #[test]
    fn lines_are_indented_with_fold_markers_and_hide_collapsed_children() {
        let mut state = ManagerTreeState::default();
        state.install(
            vec![node("a", None, 0), node("b", Some("a"), 1)],
            &GetManagerTreeResultV1 {
                rows: vec![],
                next_after: None,
                total_rows: 2,
                complete: true,
                global_grant_version: None,
            },
        );
        let lines = tree_lines(&state);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("▾ PROJECT a"), "{lines:?}");
        assert!(lines[1].starts_with("    PROJECT b"), "{lines:?}");
        state.collapsed.insert("a".into());
        let lines = tree_lines(&state);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("▸ PROJECT a"), "{lines:?}");
    }
}
