//! `:manager tree` (#890, #1214): render the manager hierarchy as a guided
//! tree with aligned columns (node, seat health, scope and grant, workload),
//! a detail pane for the selected node with its action availability, and the
//! in-tree preview, editor and confirm modals.

use crate::overlay::global_manager_command::CapField;
use crate::overlay::manager_tree::actions::{NODE_FIELDS, availability};
use crate::overlay::manager_tree::{
    GlobalEditor, HINTS, ManagerTreeState, NodeEditor, PendingAction, Tone, TreeModal, grant_text,
    kind_tag, launches_text, seat_text,
};
use crate::ui::theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};
use rsi_common::manager_tree::{ManagerTreeKindV1, ManagerTreeRowV1};
use rsi_common::types::SessionStatus;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::fixed_centered_rect;

/// Lines the detail pane takes (separator + four fields + reasons).
const DETAIL_HEIGHT: u16 = 6;
/// Below this inner height the detail pane yields its room to the tree.
const DETAIL_MIN_INNER: u16 = 22;
/// `run N  rep N  esc N  dec N ` (four 7-column cells).
const LOAD_WIDTH: usize = 28;
/// A scope column narrower than this is dropped (the detail pane keeps it).
const MIN_SCOPE_WIDTH: usize = 10;

fn safe(value: &str) -> String {
    value.chars().filter(|ch| !ch.is_control()).collect()
}

/// Pad or cut `text` to exactly `width` columns; a cut ends in `…`.
fn fit(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let text = safe(text);
    if text.width() <= width {
        let pad = width - text.width();
        return format!("{text}{}", " ".repeat(pad));
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    used += 1;
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    out
}

/// Greedily pack `pieces` joined by `sep` into lines of at most `width`
/// columns; a piece wider than a line is cut with `…`.
fn pack<'a>(pieces: impl Iterator<Item = &'a str>, sep: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for piece in pieces.filter(|piece| !piece.is_empty()) {
        if current.is_empty() {
            current = piece.to_string();
        } else if current.width() + sep.width() + piece.width() <= width {
            current.push_str(sep);
            current.push_str(piece);
        } else {
            lines.push(std::mem::take(&mut current));
            current = piece.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
        .into_iter()
        .map(|line| {
            if line.width() > width {
                fit(&line, width).trim_end().to_string()
            } else {
                line
            }
        })
        .collect()
}

fn kind_color(kind: ManagerTreeKindV1) -> Color {
    match kind {
        ManagerTreeKindV1::Global | ManagerTreeKindV1::Portfolio => theme::accent(),
        ManagerTreeKindV1::Project => theme::overlay_title(),
        ManagerTreeKindV1::Area => theme::model_text(),
        ManagerTreeKindV1::Epic => theme::effort_text(),
    }
}

/// A visible row with its tree guide and fold marker.
pub(crate) struct DrawnRow {
    pub index: usize,
    pub guide: String,
    pub marker: &'static str,
}

/// Guides (`├─`, `└─`, `│`) for the visible rows, in order.
pub(crate) fn drawn_rows(state: &ManagerTreeState) -> Vec<DrawnRow> {
    use std::collections::{HashMap, HashSet};
    let visible = state.visible();
    let mut last = vec![false; visible.len()];
    let mut seen: HashSet<Option<&str>> = HashSet::new();
    for (position, index) in visible.iter().enumerate().rev() {
        last[position] = seen.insert(state.rows[*index].parent_key.as_deref());
    }
    let mut child_prefix: HashMap<&str, String> = HashMap::new();
    let mut out = Vec::with_capacity(visible.len());
    for (position, index) in visible.iter().enumerate() {
        let row = &state.rows[*index];
        let parent = row
            .parent_key
            .as_deref()
            .and_then(|key| child_prefix.get(key).cloned());
        let (guide, children) = match parent {
            Some(prefix) => (
                format!("{prefix}{}", if last[position] { "└─" } else { "├─" }),
                format!("{prefix}{}", if last[position] { "  " } else { "│ " }),
            ),
            None => (String::new(), String::new()),
        };
        child_prefix.insert(row.key.as_str(), children);
        let marker = if !state.has_children(*index) {
            "· "
        } else if state.collapsed.contains(&row.key) {
            "▸ "
        } else {
            "▾ "
        };
        out.push(DrawnRow {
            index: *index,
            guide,
            marker,
        });
    }
    out
}

/// The text of the visible rows (guide, fold marker, row text) in order.
#[cfg(test)]
pub(crate) fn tree_lines(state: &ManagerTreeState) -> Vec<String> {
    drawn_rows(state)
        .into_iter()
        .map(|drawn| {
            format!(
                "{}{}{}",
                drawn.guide,
                drawn.marker,
                safe(&crate::overlay::manager_tree::row_text(
                    &state.rows[drawn.index]
                ))
            )
        })
        .collect()
}

struct Columns {
    node: usize,
    seat: usize,
    scope: usize,
}

fn columns(width: usize) -> Columns {
    let node = (width * 34 / 100).clamp(24, 56);
    let seat = (width * 22 / 100).clamp(18, 44);
    let scope = width.saturating_sub(node + seat + LOAD_WIDTH + 3);
    let scope = if scope < MIN_SCOPE_WIDTH { 0 } else { scope };
    Columns { node, seat, scope }
}

/// Seat health for a row cell, most important first: status, context fill,
/// last activity, then the model when it fits (the detail pane has it all).
fn seat_cell(row: &ManagerTreeRowV1, width: usize) -> String {
    let Some(seat) = &row.seat else {
        return seat_text(row);
    };
    let status = match seat.status {
        SessionStatus::WaitingApproval => "Waiting".to_string(),
        SessionStatus::Completed => "Done".to_string(),
        other => format!("{other:?}"),
    };
    let mut text = status;
    for part in [
        seat.context_fill_pct
            .map_or_else(|| "ctx ?".into(), |pct| format!("ctx {pct:.0}%")),
        seat.updated_at.format("%H:%M").to_string(),
        seat.model.clone().unwrap_or_default(),
    ] {
        if part.is_empty() || text.width() + 1 + part.width() > width {
            break;
        }
        text.push(' ');
        text.push_str(&part);
    }
    text
}

fn count_span(label: &str, value: Option<i64>, alert: bool, base: Style) -> Vec<Span<'static>> {
    let (text, style) = match value {
        None => ("?".to_string(), base.fg(theme::warning_status())),
        Some(0) => ("0".to_string(), base.fg(theme::dim_metadata())),
        Some(n) if alert => (
            n.to_string(),
            base.fg(theme::warning_status())
                .add_modifier(Modifier::BOLD),
        ),
        Some(n) => (n.to_string(), base.fg(theme::count_text())),
    };
    vec![
        Span::styled(format!("{label} "), base.fg(theme::dim_metadata())),
        Span::styled(format!("{text:<3}"), style),
    ]
}

/// #1239: who granted the row's seat: `operator`, or the granting node's
/// tier label and short id (`node <short id>` when it is not in the tree).
fn grantor_text(state: &ManagerTreeState, row: &ManagerTreeRowV1) -> Option<String> {
    let grantor = row.grantor.as_deref()?;
    let Some(id) = grantor.strip_prefix("node:") else {
        return Some(safe(grantor));
    };
    let short = id.get(..8).unwrap_or(id);
    let label = uuid::Uuid::parse_str(id).ok().and_then(|node| {
        state
            .rows
            .iter()
            .find(|other| other.kind == ManagerTreeKindV1::Portfolio && other.node_id == Some(node))
            .and_then(|other| other.tier_label.clone())
    });
    Some(match label {
        Some(label) => format!("{} {}", safe(&label), safe(short)),
        None => format!("node {}", safe(short)),
    })
}

fn row_line(
    state: &ManagerTreeState,
    drawn: &DrawnRow,
    cols: &Columns,
    selected: bool,
) -> Line<'static> {
    let row = &state.rows[drawn.index];
    let base = if selected {
        Style::default()
            .bg(theme::selected_row_bg())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let mut spans = vec![
        Span::styled(drawn.guide.clone(), base.fg(theme::dim_metadata())),
        Span::styled(drawn.marker, base.fg(theme::dim_metadata())),
    ];
    let tag = format!("{:<7} ", kind_tag(row.kind));
    let used = drawn.guide.width() + drawn.marker.width() + tag.width();
    spans.push(Span::styled(
        tag,
        base.fg(kind_color(row.kind)).add_modifier(Modifier::BOLD),
    ));
    let label_style = if matches!(
        row.kind,
        ManagerTreeKindV1::Global | ManagerTreeKindV1::Portfolio | ManagerTreeKindV1::Project
    ) {
        base.fg(theme::text()).add_modifier(Modifier::BOLD)
    } else {
        base.fg(theme::text())
    };
    let label_width = cols.node.saturating_sub(used).max(4);
    // Without a scope column the partial flag rides on the label.
    let flag = !row.complete && cols.scope == 0;
    spans.push(Span::styled(
        fit(&row.label, label_width - 2 * usize::from(flag)),
        label_style,
    ));
    if flag {
        spans.push(Span::styled(
            " !",
            base.fg(theme::warning_status())
                .add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled(" ", base));
    let (dot, seat_style) = match &row.seat {
        Some(seat) => ("● ", base.fg(theme::status_color(seat.status))),
        None if row.focus_session_id.is_some() => ("○ ", base.fg(theme::warning_status())),
        None => ("○ ", base.fg(theme::dim_metadata())),
    };
    spans.push(Span::styled(dot, seat_style));
    spans.push(Span::styled(
        fit(
            &seat_cell(row, cols.seat.saturating_sub(2)),
            cols.seat.saturating_sub(2),
        ),
        seat_style,
    ));
    spans.push(Span::styled(" ", base));
    if cols.scope > 0 {
        let mut width = cols.scope;
        if !row.complete {
            spans.push(Span::styled(
                "! partial · ",
                base.fg(theme::warning_status())
                    .add_modifier(Modifier::BOLD),
            ));
            width = width.saturating_sub("! partial · ".width());
        }
        let mut scope = row.scope.clone().unwrap_or_default();
        if let Some(grantor) = grantor_text(state, row) {
            if !scope.is_empty() {
                scope.push_str(" · ");
            }
            scope.push_str(&format!("by {grantor}"));
        }
        if let Some(grant) = grant_text(row) {
            if !scope.is_empty() {
                scope.push_str(" · ");
            }
            scope.push_str(&grant);
        }
        if let Some(launches) = launches_text(row) {
            if !scope.is_empty() {
                scope.push_str(" · ");
            }
            scope.push_str(&launches);
        }
        spans.push(Span::styled(fit(&scope, width), base.fg(theme::subtext1())));
        spans.push(Span::styled(" ", base));
    }
    spans.extend(count_span("run", row.load.running_workers, false, base));
    spans.extend(count_span("rep", row.load.direct_reports, false, base));
    spans.extend(count_span("esc", row.load.pending_escalations, true, base));
    spans.extend(count_span("dec", row.load.pending_decisions, true, base));
    Line::from(spans)
}

fn detail_lines(state: &ManagerTreeState, width: usize) -> Vec<Line<'static>> {
    let Some(row) = state.selected_row() else {
        return vec![Line::from(Span::styled(
            fit("── no node selected ", width),
            Style::default().fg(theme::dim_metadata()),
        ))];
    };
    let label = Style::default().fg(theme::section_label_text());
    let value = Style::default().fg(theme::text());
    // A cut detail field ends in `…`; `p` lists everything in full.
    let field = |name: &str, text: String| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("{name:<9} "), label),
            Span::styled(
                fit(&text, width.saturating_sub(10)).trim_end().to_string(),
                value,
            ),
        ])
    };
    let mut title = format!("── {} {} ", kind_tag(row.kind), safe(&row.label));
    let pad = width.saturating_sub(title.width());
    title.push_str(&"─".repeat(pad));
    let mut seat = seat_text(row);
    if let Some(id) = row.focus_session_id {
        seat.push_str(&format!(" · session {}", &id.to_string()[..8]));
    }
    if let Some(grantor) = grantor_text(state, row) {
        seat.push_str(&format!(" · granted by {grantor}"));
    }
    let mut scope = row.scope.clone().unwrap_or_else(|| "none".into());
    if let Some(grant) = grant_text(row) {
        scope.push_str(&format!(" · grant {grant}"));
    }
    if let Some(launches) = launches_text(row) {
        scope.push_str(&format!(" · {launches}"));
    }
    let load = format!(
        "{} running · {} direct reports · {} pending escalations · {} pending decisions · {}",
        count(row.load.running_workers),
        count(row.load.direct_reports),
        count(row.load.pending_escalations),
        count(row.load.pending_decisions),
        if row.complete {
            "children complete"
        } else {
            "children incomplete (counts are lower bounds)"
        }
    );
    let available = availability(row, state.candidate.as_ref());
    let mut actions: Vec<Span<'static>> = vec![Span::styled(format!("{:<9} ", "Actions"), label)];
    let mut reasons = Vec::new();
    for entry in &available {
        match &entry.disabled {
            None => {
                actions.push(Span::styled(
                    format!("{} {}", entry.action.key(), safe(&entry.label)),
                    Style::default()
                        .fg(theme::accent())
                        .add_modifier(Modifier::BOLD),
                ));
                actions.push(Span::raw("  "));
            }
            Some(reason) => {
                reasons.push(format!("{} {}: {reason}", entry.action.key(), entry.label))
            }
        }
    }
    vec![
        Line::from(Span::styled(
            title,
            Style::default().fg(theme::dim_metadata()),
        )),
        field("Seat", seat),
        field("Scope", scope),
        field("Load", load),
        Line::from(actions),
        Line::from(vec![
            Span::styled(format!("{:<9} ", "Disabled"), label),
            Span::styled(
                if reasons.is_empty() {
                    "none".into()
                } else {
                    fit(&reasons.join(" · "), width.saturating_sub(10))
                        .trim_end()
                        .to_string()
                },
                Style::default().fg(theme::dim_metadata()),
            ),
        ]),
    ]
}

fn count(value: Option<i64>) -> String {
    value.map_or_else(|| "?".into(), |n| n.to_string())
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
            " Manager tree ",
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        )))
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height < 4 || inner.width < 20 {
        return;
    }
    let width = usize::from(inner.width);
    let detail = if inner.height >= DETAIL_MIN_INNER {
        DETAIL_HEIGHT
    } else {
        0
    };
    let mut header = state.count_line();
    if let Some(at) = state.loaded_at {
        header.push_str(&format!(" · snapshot {}", at.format("%H:%M:%S")));
    }
    // Wrap rather than cut: the count line carries the truthfulness markers.
    let header_lines = pack(header.split(" · "), " · ", width);
    let mut hint_lines = pack(HINTS.split(" · "), " · ", width);
    let (notice, notice_style) = match (&state.error, &state.notice) {
        (Some(error), _) => (error.clone(), Style::default().fg(theme::error_status())),
        (None, Some(notice)) => (
            notice.text.clone(),
            Style::default().fg(match notice.tone {
                Tone::Info => theme::text(),
                Tone::Success => theme::status_running(),
                Tone::Error => theme::error_status(),
            }),
        ),
        (None, None) => (String::new(), Style::default()),
    };
    let mut notice_lines = pack(safe(&notice).split(' '), " ", width);
    notice_lines.truncate(2);
    if notice_lines.is_empty() {
        notice_lines.push(String::new());
    }
    let fixed =
        |hints: usize| header_lines.len() + 1 + usize::from(detail) + notice_lines.len() + hints;
    // Tree rows outrank key hints on a short terminal.
    let room = usize::from(inner.height);
    if room.saturating_sub(fixed(hint_lines.len())) < 3 {
        hint_lines = vec![fit(HINTS, width).trim_end().to_string()];
        if room.saturating_sub(fixed(1)) < 2 {
            hint_lines.clear();
        }
    }
    let body_height = room.saturating_sub(fixed(hint_lines.len()));
    let drawn = drawn_rows(state);
    // A partial tree reserves its last body row for the "more" marker.
    let marker = state.next_after.is_some() && body_height > 1;
    let list_height = body_height - usize::from(marker);
    state.viewport.set(list_height.max(1));
    let mut offset = state.scroll.get();
    if state.selected < offset {
        offset = state.selected;
    } else if list_height > 0 && state.selected >= offset + list_height {
        offset = state.selected + 1 - list_height;
    }
    offset = offset.min(drawn.len().saturating_sub(list_height));
    state.scroll.set(offset);

    let cols = columns(width);
    let mut out: Vec<Line> = header_lines
        .into_iter()
        .map(|line| {
            Line::from(Span::styled(
                line,
                Style::default().add_modifier(Modifier::BOLD),
            ))
        })
        .collect();
    let mut column_header = format!(
        "{}{}",
        fit("NODE", cols.node + 1),
        fit("SEAT HEALTH", cols.seat + 1)
    );
    if cols.scope > 0 {
        column_header.push_str(&fit("SCOPE · GRANT", cols.scope + 1));
    }
    column_header.push_str("LOAD");
    let shown = if drawn.is_empty() {
        "no rows".to_string()
    } else {
        format!(
            "rows {}–{} of {}",
            offset + 1,
            (offset + list_height).min(drawn.len()),
            drawn.len()
        )
    };
    if column_header.width() + shown.width() + 1 > width {
        column_header = fit(&column_header, width.saturating_sub(shown.width() + 1));
    }
    let gap = width.saturating_sub(column_header.width() + shown.width());
    out.push(Line::from(vec![
        Span::styled(
            column_header,
            Style::default()
                .fg(theme::table_header_text())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" ".repeat(gap.max(1))),
        Span::styled(shown, Style::default().fg(theme::dim_metadata())),
    ]));
    let list_top = out.len();
    if drawn.is_empty() {
        out.push(Line::from(Span::styled(
            "(no managers or grants to show)",
            Style::default().fg(theme::empty_state()),
        )));
    }
    for (position, row) in drawn.iter().enumerate().skip(offset).take(list_height) {
        out.push(row_line(state, row, &cols, position == state.selected));
    }
    while out.len() < list_top + list_height {
        out.push(Line::default());
    }
    if marker {
        out.push(Line::from(Span::styled(
            fit(
                &format!(
                    "… {} more node(s) not loaded · n loads the next page",
                    state.unloaded_rows()
                ),
                width,
            ),
            Style::default().fg(theme::warning_status()),
        )));
    }
    if detail > 0 {
        out.extend(detail_lines(state, width));
    }
    for line in notice_lines {
        out.push(Line::from(Span::styled(line, notice_style)));
    }
    for line in hint_lines {
        out.push(Line::from(Span::styled(
            line,
            Style::default().fg(theme::overlay_hint()),
        )));
    }
    frame.render_widget(Paragraph::new(out), inner);

    if let Some(modal) = &state.modal {
        render_modal(frame, inner, modal);
    }
}

fn modal_frame(frame: &mut Frame, area: Rect, title: &str, alert: bool, lines: usize) -> Rect {
    let width = area.width.saturating_sub(4).min(110);
    let height = u16::try_from(lines + 2)
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_sub(2));
    let rect = fixed_centered_rect(area, width, height);
    frame.render_widget(Clear, rect);
    let color = if alert {
        theme::error_status()
    } else {
        theme::overlay_title()
    };
    let block = theme::overlay_block()
        .border_style(Style::default().fg(color))
        .title(Line::from(Span::styled(
            format!(" {} ", safe(title)),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )))
        .padding(Padding::horizontal(1));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    inner
}

/// Fit `lines` into `height`, ending in an explicit "N more" marker rather
/// than cutting silently.
fn bounded(mut lines: Vec<Line<'static>>, height: usize) -> Vec<Line<'static>> {
    if lines.len() > height && height > 0 {
        let hidden = lines.len() - (height - 1);
        lines.truncate(height - 1);
        lines.push(Line::from(Span::styled(
            format!("… {hidden} more line(s): enlarge the terminal to read them"),
            Style::default().fg(theme::warning_status()),
        )));
    }
    lines
}

/// Modal content before wrapping: text and its style.
type Row = (String, Style);

fn plain(text: &str) -> Row {
    let style = if text.starts_with('!') {
        Style::default().fg(theme::warning_status())
    } else {
        Style::default().fg(theme::text())
    };
    (safe(text), style)
}

fn hint(text: &str) -> Row {
    (text.to_string(), Style::default().fg(theme::overlay_hint()))
}

/// Wrap modal rows at word boundaries, keeping a row's leading indent.
fn wrap(rows: Vec<Row>, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (text, style) in rows {
        if text.width() <= width {
            out.push(Line::from(Span::styled(text, style)));
            continue;
        }
        let indent = text.len() - text.trim_start().len();
        for (n, line) in pack(
            text.trim_start().split(' '),
            " ",
            width.saturating_sub(indent + 2),
        )
        .into_iter()
        .enumerate()
        {
            let pad = if n == 0 { indent } else { indent + 2 };
            out.push(Line::from(Span::styled(
                format!("{}{line}", " ".repeat(pad)),
                style,
            )));
        }
    }
    out
}

fn render_modal(frame: &mut Frame, area: Rect, modal: &TreeModal) {
    let (title, alert, lines) = match modal {
        TreeModal::Preview { title, lines } => {
            let mut out: Vec<Row> = lines.iter().map(|line| plain(line)).collect();
            out.push(plain(""));
            out.push(hint("Esc/Enter close · nothing is sent from a preview"));
            (title.clone(), false, out)
        }
        TreeModal::Confirm(pending) => (
            format!(
                "{}{}",
                if pending.destructive { "! " } else { "" },
                pending.title
            ),
            pending.destructive,
            confirm_lines(pending),
        ),
        TreeModal::EditGlobal(editor) => (
            format!(
                "Edit {} grant · projects and caps",
                editor
                    .node
                    .as_ref()
                    .map_or("global", |node| node.tier_label.as_str())
            ),
            false,
            global_editor_lines(editor),
        ),
        TreeModal::EditNode(editor) => (
            format!("Edit allowance · {}", editor.row_label),
            false,
            node_editor_lines(editor),
        ),
    };
    let width = usize::from(area.width.saturating_sub(4).min(110).saturating_sub(4));
    let lines = wrap(lines, width);
    let inner = modal_frame(frame, area, &title, alert, lines.len());
    let lines = bounded(lines, usize::from(inner.height));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn confirm_lines(pending: &PendingAction) -> Vec<Row> {
    let mut out: Vec<Row> = pending.lines.iter().map(|line| plain(line)).collect();
    out.push(plain(""));
    out.push((
        format!("RPC: {}", safe(&pending.rpc)),
        Style::default().fg(theme::dim_metadata()),
    ));
    out.push(hint(if pending.destructive {
        "y confirm · n/Esc cancel (destructive: Enter does not confirm)"
    } else {
        "y/Enter confirm · n/Esc cancel"
    }));
    out
}

fn global_editor_lines(editor: &GlobalEditor) -> Vec<Row> {
    let mut out = vec![plain(&format!(
        "Global grant v{} · seat {} · Space toggles a project; the caps below apply to every project manager it appoints",
        editor.grant.grant_version,
        &editor.grant.seat_session_id.to_string()[..8]
    ))];
    let row_style = |position: usize| {
        if position == editor.cursor {
            Style::default()
                .bg(theme::selected_row_bg())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::text())
        }
    };
    for (position, (_, name, granted)) in editor.projects.iter().enumerate() {
        out.push((
            format!("{} {}", if *granted { "[x]" } else { "[ ]" }, safe(name)),
            row_style(position),
        ));
    }
    out.push(plain(
        "Per-project manager caps (h/l or -/+ adjust, H/L ±10)",
    ));
    for (offset, field) in CapField::ALL.into_iter().enumerate() {
        out.push((
            format!(
                "{:<24} ◂ {:>6} ▸   was {}",
                field.label(),
                editor.caps.describe(field),
                editor.original_caps.describe(field)
            ),
            row_style(editor.projects.len() + offset),
        ));
    }
    if let Some(error) = &editor.error {
        out.push((safe(error), Style::default().fg(theme::error_status())));
    }
    out.push(hint(
        "j/k move · Space toggle · h/l adjust a cap · Enter review · Esc cancel",
    ));
    out
}

fn node_editor_lines(editor: &NodeEditor) -> Vec<Row> {
    let parent = editor.parent.grant.as_ref().map(|grant| {
        [
            grant.allowance.max_active_sessions,
            grant.allowance.max_created_sessions,
            grant.max_direct_reports,
        ]
    });
    let mut out = vec![plain(&format!(
        "Node grant v{} · parent grant v{} · the new allowance must stay strictly narrower than the parent's",
        editor.node.grant_version, editor.parent.grant_version
    ))];
    for (position, field) in NODE_FIELDS.iter().enumerate() {
        let style = if position == editor.cursor {
            Style::default()
                .bg(theme::selected_row_bg())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::text())
        };
        out.push((
            format!(
                "{field:<22} ◂ {:>4} ▸   was {:<4} parent {}",
                editor.values[position],
                editor.original[position],
                parent.map_or_else(|| "?".into(), |values| values[position].to_string())
            ),
            style,
        ));
    }
    if let Some(error) = &editor.error {
        out.push((safe(error), Style::default().fg(theme::error_status())));
    }
    out.push(hint(
        "j/k field · h/l or -/+ adjust · Enter review · Esc cancel",
    ));
    out
}

#[cfg(test)]
mod tests;
