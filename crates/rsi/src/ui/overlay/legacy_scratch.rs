//! Rendering for the operator legacy-scratch adoption overlay (#1147).

use crate::overlay::legacy_scratch::refusal_label;
use crate::types::LegacyScratchOverlayState;
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};

use super::fixed_centered_rect;

pub(super) fn render(frame: &mut Frame, area: Rect, state: &LegacyScratchOverlayState) {
    let width = area.width.saturating_sub(4).min(140).max(20);
    let height = area.height.saturating_sub(4).min(46).max(10);
    let popup = fixed_centered_rect(area, width, height);
    frame.render_widget(Clear, popup);
    let block = theme::overlay_block()
        .title(Line::from(Span::styled(
            " Legacy scratch — adopt so reclaim may delete it ",
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )))
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 4 {
        return;
    }

    if let Some(paths) = &state.confirm {
        render_confirm(frame, inner, paths, state.confirm_scroll);
        return;
    }

    let mut lines = vec![
        Line::from(vec![
            key("j/k"),
            muted(" select  "),
            key("a/Enter"),
            muted(" adopt (asks first)  "),
            key("A"),
            muted(" adopt all adoptable (asks first)  "),
            key("r"),
            muted(" refresh  "),
            key("Esc"),
            muted(" close"),
        ]),
        muted_line(
            "Adopting records a directory after the full reclaim proof; scratch reclaim (enabled) deletes it later.",
        ),
    ];

    // Rows reserved for the footer: result lines and the error.
    let footer = state.last_result.len() + usize::from(state.last_error.is_some());
    let list_rows = usize::from(inner.height)
        .saturating_sub(lines.len() + footer + 3)
        .max(1);
    if state.candidates.is_empty() {
        lines.push(muted_line("No unrecorded scratch directories."));
    } else {
        let first = state
            .selected_index
            .saturating_sub(list_rows.saturating_sub(1));
        for (index, candidate) in state
            .candidates
            .iter()
            .enumerate()
            .skip(first)
            .take(list_rows)
        {
            let selected = index == state.selected_index;
            let verdict =
                candidate
                    .blocker
                    .map_or("adoptable".to_string(), |b| match &candidate.detail {
                        Some(detail) => format!("{} ({detail})", refusal_label(b)),
                        None => refusal_label(b).to_string(),
                    });
            let style = if selected {
                Style::default().fg(theme::text()).bg(theme::surface2())
            } else {
                Style::default().fg(theme::subtext0())
            };
            lines.push(Line::from(Span::styled(
                format!(
                    "{} {}  [{}] {} KiB  {verdict}",
                    if selected { "▸" } else { " " },
                    candidate.path,
                    candidate.kind,
                    candidate.bytes / 1024,
                ),
                style,
            )));
        }
    }
    if state.budget_exhausted || state.refused_roots > 0 {
        lines.push(muted_line(&format!(
            "listing incomplete: budget exhausted {}, refused roots {}",
            state.budget_exhausted, state.refused_roots
        )));
    }
    lines.extend(state.last_result.iter().map(|line| {
        Line::from(Span::styled(
            line.clone(),
            Style::default().fg(theme::text()),
        ))
    }));
    if let Some(error) = &state.last_error {
        lines.push(Line::from(Span::styled(
            error.clone(),
            Style::default()
                .fg(theme::error_status())
                .add_modifier(Modifier::BOLD),
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme::overlay_bg())),
        inner,
    );
}

/// The confirmation step: every full path, the count, and the consequence.
fn render_confirm(frame: &mut Frame, inner: Rect, paths: &[String], scroll: usize) {
    let mut lines = vec![
        Line::from(vec![
            key("y"),
            muted(" confirm and adopt  "),
            key("j/k"),
            muted(" scroll  "),
            key("Esc/n"),
            muted(" cancel"),
        ]),
        Line::from(Span::styled(
            format!(
                "Adopt {} director{}? Each becomes eligible for AUTOMATIC DELETION.",
                paths.len(),
                if paths.len() == 1 { "y" } else { "ies" }
            ),
            Style::default()
                .fg(theme::error_status())
                .add_modifier(Modifier::BOLD),
        )),
        muted_line(
            "Scratch reclaim is enabled: once a directory is old enough and unheld the daemon deletes it without asking again.",
        ),
        Line::default(),
    ];
    let rows = usize::from(inner.height)
        .saturating_sub(lines.len() + 1)
        .max(1);
    for (index, path) in paths.iter().enumerate().skip(scroll).take(rows) {
        lines.push(Line::from(Span::styled(
            format!("{:>3}. {path}", index + 1),
            Style::default().fg(theme::text()),
        )));
    }
    if scroll + rows < paths.len() {
        lines.push(muted_line(&format!(
            "… {} more (j to scroll)",
            paths.len() - scroll - rows
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme::overlay_bg())),
        inner,
    );
}

fn key(value: &'static str) -> Span<'static> {
    Span::styled(
        value,
        Style::default()
            .fg(theme::accent())
            .add_modifier(Modifier::BOLD),
    )
}

fn muted(value: &'static str) -> Span<'static> {
    Span::styled(value, Style::default().fg(theme::subtext0()))
}

fn muted_line(value: &str) -> Line<'static> {
    Line::from(Span::styled(
        value.to_string(),
        Style::default().fg(theme::subtext0()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use rsi_common::scratch_adopt::{LegacyScratchCandidateV1, ScratchAdoptRefusal};

    fn rendered(state: &LegacyScratchOverlayState) -> String {
        let mut terminal = Terminal::new(TestBackend::new(150, 30)).expect("terminal");
        terminal
            .draw(|frame| render(frame, frame.area(), state))
            .expect("render legacy scratch");
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }

    #[test]
    fn the_overlay_shows_the_list_verdicts_and_the_adopt_result() {
        let state = LegacyScratchOverlayState {
            candidates: vec![
                LegacyScratchCandidateV1 {
                    path: "/h/.cache/rsi-a-tmp".to_string(),
                    kind: "worker_tmp".to_string(),
                    blocker: None,
                    bytes: 8192,
                    detail: None,
                },
                LegacyScratchCandidateV1 {
                    path: "/var/tmp/rsi-b".to_string(),
                    kind: "var_tmp".to_string(),
                    blocker: Some(ScratchAdoptRefusal::Held),
                    bytes: 0,
                    detail: None,
                },
            ],
            last_result: vec![
                "Adopted 1 of 2 (recorded only; nothing deleted)".to_string(),
                "refused  /var/tmp/rsi-b: held by a live process".to_string(),
            ],
            ..LegacyScratchOverlayState::default()
        };
        let text = rendered(&state);
        assert!(text.contains("/h/.cache/rsi-a-tmp"), "{text}");
        assert!(text.contains("adoptable"), "{text}");
        assert!(text.contains("/var/tmp/rsi-b"), "{text}");
        assert!(text.contains("held by a live process"), "{text}");
        assert!(
            text.contains("Adopted 1 of 2 (recorded only; nothing deleted)"),
            "{text}"
        );
    }

    #[test]
    fn the_confirmation_names_every_full_path_and_the_deletion_consequence() {
        let state = LegacyScratchOverlayState {
            confirm: Some(vec![
                "/home/u/.cache/rsi-w1-tmp".to_string(),
                "/var/tmp/rsi-some-long-directory-name".to_string(),
            ]),
            ..LegacyScratchOverlayState::default()
        };
        let text = rendered(&state);
        assert!(text.contains("/home/u/.cache/rsi-w1-tmp"), "{text}");
        assert!(
            text.contains("/var/tmp/rsi-some-long-directory-name"),
            "{text}"
        );
        assert!(text.contains("Adopt 2 directories?"), "{text}");
        assert!(text.contains("AUTOMATIC DELETION"), "{text}");
        assert!(text.contains("y"), "{text}");
    }

    #[test]
    fn an_empty_listing_says_there_is_nothing_to_adopt() {
        let text = rendered(&LegacyScratchOverlayState::default());
        assert!(
            text.contains("No unrecorded scratch directories."),
            "{text}"
        );
    }
}
