//! #2502: the Budget panel's rendering, in #2237's design system: the
//! breadcrumb header (`<scope> / Budget`), one section rule per project, the
//! accent rail and selected band on the cursor row, and a status glyph plus
//! colour on the binding cap.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use rsi_common::manager_budget::{BudgetCapV1, BudgetNodeKindV1, BudgetNodeV1};

use crate::overlay::global_manager_workspace::GlobalManagerWorkspaceState;
use crate::overlay::gm_budget::{BudgetPanel, PendingRaise, describe};
use crate::ui::{glyphs, session, theme};

fn safe(value: &str) -> String {
    value.chars().filter(|ch| !ch.is_control()).collect()
}

fn header(state: &GlobalManagerWorkspaceState, panel: &BudgetPanel, width: u16) -> Line<'static> {
    let name = state
        .node
        .as_ref()
        .filter(|_| state.target.is_some())
        .map_or_else(
            || "Above all projects".to_string(),
            |node| safe(&node.label),
        );
    let sep = Style::default().fg(theme::overlay1());
    let left = vec![
        Span::raw(" "),
        Span::styled(
            name,
            Style::default()
                .fg(theme::green())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" / ", sep),
        Span::styled(
            "Budget",
            Style::default()
                .fg(theme::accent())
                .add_modifier(Modifier::BOLD),
        ),
    ];
    let projects = panel
        .snapshot
        .as_ref()
        .map_or(0, |snapshot| snapshot.projects.len());
    let freshness = match (&panel.loaded_at, &panel.error) {
        (_, Some(_)) if panel.snapshot.is_some() => "stale".to_string(),
        (Some(at), _) => format!("as of {}", at.format("%H:%M:%S")),
        (None, _) => "loading".to_string(),
    };
    let right = vec![
        Span::styled(
            projects.to_string(),
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "{} · {freshness} ",
                if projects == 1 {
                    " project"
                } else {
                    " projects"
                }
            ),
            Style::default().fg(theme::dim_metadata()),
        ),
    ];
    let used = Line::from(left.clone()).width() + Line::from(right.clone()).width();
    let pad = usize::from(width).saturating_sub(used);
    let mut spans = left;
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
        spans.extend(right);
    }
    Line::from(spans)
}

fn node_title(node: &BudgetNodeV1) -> Line<'static> {
    let (glyph, tag, color) = match node.kind {
        BudgetNodeKindV1::Portfolio => (glyphs::MANAGERS, "PORTFOLIO", theme::accent()),
        BudgetNodeKindV1::Project => ("●", "PM", theme::status_running()),
        BudgetNodeKindV1::Delegate => (glyphs::EPIC_CONTAINER, "DELEGATE", theme::accent()),
    };
    let fence = match node.kind {
        BudgetNodeKindV1::Portfolio => format!("grant v{}", node.version),
        _ => format!("policy v{}", node.version),
    };
    let since = node.counted_since.as_deref().map_or_else(
        || "counted over its scope".to_string(),
        |since| {
            chrono::DateTime::parse_from_rfc3339(since).map_or_else(
                |_| format!("counted since {since}"),
                |at| format!("counted since {}", at.format("%m-%d %H:%M")),
            )
        },
    );
    Line::from(vec![
        Span::styled(format!("  {glyph} "), Style::default().fg(color)),
        Span::styled(
            format!("{tag} {}", safe(&node.label)),
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · governs {}", node.kind.governs()),
            Style::default().fg(theme::overlay_title()),
        ),
        Span::styled(
            format!(" · {fence} · {since}"),
            Style::default().fg(theme::dim_metadata()),
        ),
    ])
}

/// One cap row: rail, binding glyph, label, used / limit, room left.
fn cap_line(cap: &BudgetCapV1, selected: bool) -> Line<'static> {
    let bg = selected.then(theme::selected_row_bg);
    let on = |style: Style| bg.map_or(style, |bg| style.bg(bg));
    let room = cap.headroom();
    let blocked = cap.blocked_unknown_spend();
    let spent = room.is_some_and(|room| room <= 0.0);
    let (glyph, glyph_color) = match (cap.binding, spent) {
        (true, true) => (glyphs::FAILED, theme::error_status()),
        (true, false) => (glyphs::NEEDS_YOU, theme::warning_status()),
        _ => (" ", theme::dim_metadata()),
    };
    let value_color = if cap.binding {
        glyph_color
    } else {
        theme::count_text()
    };
    let left = match room {
        Some(_) if blocked => "blocked".to_string(),
        Some(room) => format!("{} left", describe(cap.field, Some(room))),
        None => "no cap".to_string(),
    };
    let mut spans = vec![
        Span::styled(
            if selected { "▌" } else { " " },
            on(Style::default()
                .fg(theme::active_row_rail())
                .add_modifier(Modifier::BOLD)),
        ),
        Span::styled(
            format!("   {glyph} "),
            on(Style::default()
                .fg(glyph_color)
                .add_modifier(Modifier::BOLD)),
        ),
        Span::styled(
            format!("{:<18}", cap.field.label()),
            on(Style::default().fg(theme::text())),
        ),
        Span::styled(
            format!(
                "{:>9} / {:<9}",
                describe(cap.field, Some(cap.used)),
                describe(cap.field, cap.limit)
            ),
            on(Style::default()
                .fg(value_color)
                .add_modifier(Modifier::BOLD)),
        ),
        Span::styled(
            format!(" {left:<12}"),
            on(Style::default().fg(theme::dim_metadata())),
        ),
    ];
    if blocked && !cap.binding {
        spans.push(Span::styled(
            " spend unknown: launches refused (manager_v2_spend_unknown)",
            on(Style::default()
                .fg(theme::warning_status())
                .add_modifier(Modifier::BOLD)),
        ));
    }
    if cap.binding {
        spans.push(Span::styled(
            if blocked {
                " binding · spend unknown: launches refused (manager_v2_spend_unknown); raising the cap will not admit"
            } else if spent {
                " binding · spent: raise this one"
            } else {
                " binding: the lowest room along the chain"
            },
            on(Style::default()
                .fg(glyph_color)
                .add_modifier(Modifier::BOLD)),
        ));
    }
    let line = Line::from(spans);
    match bg {
        Some(bg) => line.style(Style::default().bg(bg)),
        None => line,
    }
}

/// Every body line and the line index of the selected cap.
#[must_use]
pub(crate) fn body_lines(panel: &BudgetPanel, width: usize) -> (Vec<Line<'static>>, Option<usize>) {
    let dim = Style::default().fg(theme::dim_metadata());
    let Some(snapshot) = &panel.snapshot else {
        return (
            vec![Line::from(Span::styled("Loading the budget…", dim))],
            None,
        );
    };
    let mut lines = Vec::new();
    let mut selected_line = None;
    let mut row = 0usize;
    if snapshot.projects.is_empty() {
        lines.push(Line::from(Span::styled(
            "This node covers no project yet.",
            dim,
        )));
    }
    for project in &snapshot.projects {
        lines.push(session::section_header_line(
            "",
            &safe(&project.project_name).to_uppercase(),
            &format!("  {}  ", project.chain.len()),
            theme::accent(),
            theme::overlay_bg(),
            width,
        ));
        if project.chain.is_empty() {
            lines.push(Line::from(Span::styled(
                "  no manager covers this project",
                dim,
            )));
        }
        for node in &project.chain {
            lines.push(node_title(node));
            for cap in &node.caps {
                let selected = row == panel.selected;
                if selected {
                    selected_line = Some(lines.len());
                }
                lines.push(cap_line(cap, selected));
                row += 1;
            }
        }
        lines.push(Line::default());
    }
    (lines, selected_line)
}

fn review_lines(pending: &PendingRaise) -> Vec<Line<'static>> {
    let (rpc, effect) = match pending.kind {
        BudgetNodeKindV1::Portfolio => (
            format!(
                "ConfigurePortfolioNode (expects grant v{} · epoch {})",
                pending.version, pending.epoch
            ),
            "Governs this node and everything under it. Saving opens a new grant epoch: created sessions and containers count again from zero; spend already charged to work this node still covers is NOT reset.",
        ),
        _ => (
            format!(
                "ConfigureHarnessManagerPolicy (expects policy v{} · scope {})",
                pending.version, pending.epoch
            ),
            "Governs this PM's own launches only; the portfolio caps above still apply.",
        ),
    };
    vec![
        Line::from(Span::styled(
            format!(
                "Set {} {} {} in {}: {} → {}",
                pending.kind.as_str(),
                safe(&pending.node_label),
                pending.field.label(),
                safe(&pending.project_name),
                describe(pending.field, pending.from),
                describe(pending.field, pending.to),
            ),
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            effect,
            Style::default().fg(theme::overlay_title()),
        )),
        Line::from(Span::styled(
            format!("RPC: {rpc}"),
            Style::default().fg(theme::dim_metadata()),
        )),
        Line::from(Span::styled(
            "Enter/y save and read back · + one more step · Esc/n cancel",
            Style::default().fg(theme::overlay_hint()),
        )),
    ]
}

const HINTS: &str =
    "j/k move · + raise one step · 0-9 type a limit · r reload · ? keys · b/Esc back";

/// Draw the panel into the workspace's inner area.
pub(crate) fn render(
    frame: &mut Frame,
    area: Rect,
    state: &GlobalManagerWorkspaceState,
    panel: &BudgetPanel,
) {
    let width = area.width;
    let resets = panel
        .snapshot
        .as_ref()
        .map_or_else(String::new, |snapshot| {
            format!(" {}", safe(&snapshot.resets))
        });
    let mut top = vec![header(state, panel, width)];
    top.push(Line::from(Span::styled(
        resets,
        Style::default().fg(theme::dim_metadata()),
    )));
    let wrap_width = usize::from(width.saturating_sub(1)).max(1);
    let top_height = u16::try_from(1 + top[1].width().div_ceil(wrap_width).max(1))
        .unwrap_or(2)
        .min(area.height);
    frame.render_widget(
        Paragraph::new(top).wrap(Wrap { trim: false }),
        Rect::new(area.x, area.y, width, top_height),
    );
    let mut bottom: Vec<Line<'static>> = Vec::new();
    if let Some(pending) = &panel.pending {
        bottom.extend(review_lines(pending));
    } else if let Some(entry) = &panel.entry {
        let field = panel.selected_row().map_or("cap", |row| row.field.label());
        bottom.push(Line::from(Span::styled(
            format!("New {field}: {entry}▏ · Enter review · Esc cancel"),
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::BOLD),
        )));
    }
    let status = match (&panel.error, &panel.notice) {
        (Some(error), _) => Some((error.clone(), theme::error_status())),
        (None, Some(notice)) => Some((notice.clone(), theme::status_completed())),
        _ => None,
    };
    if let Some((text, color)) = status {
        bottom.push(Line::from(Span::styled(
            format!(" {}", safe(&text)),
            Style::default().fg(color),
        )));
    }
    bottom.push(Line::from(Span::styled(
        format!(
            " {}",
            if state.coverage.is_some() {
                "Read-only · j/k move · r reload · b/Esc back to draft"
            } else {
                HINTS
            }
        ),
        Style::default().fg(theme::dim_metadata()),
    )));
    let bottom_height = u16::try_from(bottom.len())
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_sub(top_height));
    let body_height = area.height.saturating_sub(top_height + bottom_height + 1);
    let (lines, selected) = body_lines(panel, usize::from(width));
    let offset = selected.map_or(0, |line| {
        line.saturating_sub(usize::from(body_height).saturating_sub(1))
    });
    let body: Vec<Line> = lines
        .into_iter()
        .skip(offset)
        .take(usize::from(body_height))
        .collect();
    frame.render_widget(
        Paragraph::new(body),
        Rect::new(area.x, area.y + top_height + 1, width, body_height),
    );
    frame.render_widget(
        Paragraph::new(bottom).wrap(Wrap { trim: false }),
        Rect::new(
            area.x,
            area.y + area.height.saturating_sub(bottom_height),
            width,
            bottom_height,
        ),
    );
}
