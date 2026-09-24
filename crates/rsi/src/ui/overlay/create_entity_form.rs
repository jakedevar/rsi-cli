//! Unified entity-creation form rendering.
//!
//! TODO: render snapshot once snapshot infrastructure is added
//! (no insta crate in crates/rsi/Cargo.toml as of 2026-05-18).

use crate::overlay::create_entity_form::{topology_filtered_indices, visibility};
use crate::types::{ChipStatus, CreateEntityField, TagChip};
use crate::ui::{session::kind_color, theme};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Padding, Paragraph};
use rsi_common::types::{SessionKind, Topology, legal_children};

use super::fixed_centered_rect;

/// Render the unified entity-creation popup. Identity (Kind / Name / Tag)
/// rows + parent indicator + (optional) topology row + (optional) execution
/// group + error banner + hint bar. Popup height grows with the focused
/// kind's visibility list.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_create_entity_form(
    frame: &mut Frame,
    area: Rect,
    kind: SessionKind,
    name: &str,
    tags: &[TagChip],
    focused_field: CreateEntityField,
    insert_mode: bool,
    parent_id: Option<uuid::Uuid>,
    error: Option<&str>,
    app: &crate::app::App,
) {
    // Pull P2.3 fields directly from the overlay state for renderers that
    // need them (topology dropdown, execution rows, preview pane).
    let (
        topology_id,
        topology_filter,
        topology_selected_index,
        topology_choices,
        preview_open,
        provider,
        model,
        effort,
        sandbox,
    ) = match &app.overlay {
        crate::types::OverlayState::CreateEntityForm {
            topology_id,
            topology_filter,
            topology_selected_index,
            topology_choices,
            preview_open,
            provider,
            model,
            effort,
            sandbox,
            ..
        } => (
            *topology_id,
            topology_filter.clone(),
            *topology_selected_index,
            topology_choices.clone(),
            *preview_open,
            *provider,
            model.clone(),
            effort.clone(),
            *sandbox,
        ),
        _ => (
            None,
            String::new(),
            0,
            Vec::new(),
            false,
            None,
            None,
            None,
            false,
        ),
    };

    // Popup geometry — base 12 rows for Kind+Name+Tag+Parent+hint.
    // Add 1 for error banner, +2 per visible extra field (topology / each
    // execution row).
    let visibility_list = visibility::field_visibility(kind);
    let mut extra_rows: u16 = 0;
    for f in visibility_list {
        if matches!(
            f,
            CreateEntityField::Topology
                | CreateEntityField::Provider
                | CreateEntityField::Model
                | CreateEntityField::Effort
                | CreateEntityField::Sandbox
        ) {
            extra_rows = extra_rows.saturating_add(2);
        }
    }
    // Topology dropdown grows the popup when focused with a visible filter
    // list — reserve 6 rows for visible filter results.
    let topology_open = focused_field == CreateEntityField::Topology;
    let topology_open_rows: u16 = if topology_open { 6 } else { 0 };

    // Body region (S1, Decision 3): fixed 1 label row + 4 textarea rows
    // (+1 spacer ⇒ +6), leaf kinds only. Body is deliberately EXCLUDED from
    // the +2 extra-field loop above — it contributes only its own +6
    // (double-count guard). Longer bodies scroll instead of growing the
    // modal (render_wrapped_textarea).
    let body_visible = visibility_list.contains(&CreateEntityField::Body);
    let mut body_rows: u16 = if body_visible { 4 } else { 0 };

    let mut popup_height: u16 = 12
        + extra_rows
        + topology_open_rows
        + u16::from(error.is_some())
        + if body_visible { body_rows + 2 } else { 0 };
    // Small-terminal shrink: when the computed height exceeds the viewport,
    // shrink the body textarea rows from 4 down to a floor of 2 (26 → 24 at
    // 80×24) BEFORE fixed_centered_rect clamping, so the hint bar, Sandbox
    // row, and error banner are never clipped; body content scrolls within
    // whatever rows remain.
    while body_visible && popup_height > area.height && body_rows > 2 {
        body_rows -= 1;
        popup_height -= 1;
    }
    let popup_area = fixed_centered_rect(area, 64, popup_height);
    frame.render_widget(Clear, popup_area);

    let kind_fg = kind_color(kind).unwrap_or_else(theme::accent);

    let title_spans = build_title_spans(kind, kind_fg, parent_id);
    let block = theme::overlay_block()
        .title(Line::from(title_spans))
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if inner.height < 6 {
        return;
    }

    let mut y = inner.y;

    // Row: Kind segmented selector.
    render_kind_selector(frame, inner, y, kind, focused_field, parent_id, app);
    y += 2;

    // Row: Name input.
    render_name_row(frame, inner, y, name, focused_field, insert_mode);
    y += 2;

    // Body region (leaf kinds only): label + multi-line textarea + spacer.
    if body_visible {
        if let crate::types::OverlayState::CreateEntityForm { body, .. } = &app.overlay {
            render_body_region(frame, inner, y, body, focused_field, body_rows);
        }
        y += 1 + body_rows + 1;
    }

    // Row: Tag chips.
    render_tags_row(frame, inner, y, tags, focused_field, insert_mode);
    y += 2;

    // Row: Parent indicator (always visible in V2).
    render_parent_row(frame, inner, y, parent_id, focused_field, app);
    y += 2;

    // Topology row (Epic only).
    if visibility_list.contains(&CreateEntityField::Topology) {
        render_topology_row(
            frame,
            inner,
            y,
            topology_id,
            &topology_filter,
            topology_selected_index,
            &topology_choices,
            focused_field,
            preview_open,
        );
        y += 2 + topology_open_rows;
    }

    // Execution group (leaf kinds only).
    if visibility_list.contains(&CreateEntityField::Provider) {
        render_provider_row(frame, inner, y, provider, focused_field);
        y += 2;
    }
    if visibility_list.contains(&CreateEntityField::Model) {
        render_model_row(frame, inner, y, model.as_deref(), focused_field);
        y += 2;
    }
    if visibility_list.contains(&CreateEntityField::Effort) {
        render_effort_row(frame, inner, y, effort.as_deref(), focused_field);
        y += 2;
    }
    if visibility_list.contains(&CreateEntityField::Sandbox) {
        render_sandbox_row(frame, inner, y, sandbox, focused_field);
        y += 2;
    }

    // Optional error banner (theme::red() bg).
    if let Some(err) = error {
        render_error_banner(frame, inner, y, err);
    }

    // Hint bar at the bottom of the popup.
    render_hint_bar(frame, inner, focused_field);

    // Topology preview pane — below the popup, only when topology focused
    // AND `preview_open == true` AND a topology row is highlighted.
    if topology_open && preview_open && !topology_choices.is_empty() {
        let visible = crate::overlay::create_entity_form::topology_filtered_indices(
            &topology_choices,
            &topology_filter,
        );
        let highlighted = visible
            .get(topology_selected_index)
            .and_then(|i| topology_choices.get(*i));
        if let Some(topology) = highlighted {
            render_preview_pane(frame, area, popup_area, topology);
        }
    }

    // Model sub-overlay — when open, anchor the dropdown below the popup.
    // Painter's algorithm renders this LAST so it overlaps the form rows.
    if let crate::types::OverlayState::CreateEntityForm {
        model_dropdown: Some(state),
        ..
    } = &app.overlay
    {
        if state.open {
            let provider_available = !state.models.is_empty();
            // Anchor the dropdown at the bottom of the popup so it
            // overlays the row labels below.
            let anchor = Rect::new(
                popup_area.x,
                popup_area.y,
                popup_area.width,
                popup_area.height,
            );
            crate::ui::widget::model_dropdown::render_model_dropdown(
                frame,
                area,
                anchor,
                state,
                model.as_deref(),
                provider_available,
            );
        }
    }
}

/// Render the topology preview pane in a separate boxed region BELOW the
/// main form popup. Width matches the popup; height grows up to 10 rows
/// (8 content + 2 border). If there is no room below the popup, the pane
/// is skipped silently (degraded fallback per Decision 1).
fn render_preview_pane(frame: &mut Frame, area: Rect, popup_area: Rect, topology: &Topology) {
    use ratatui::widgets::Block;
    let preview_height: u16 = 10;
    let preview_y = popup_area.y.saturating_add(popup_area.height);
    // Bail if the preview would clip off the bottom of the screen.
    if preview_y.saturating_add(preview_height) > area.y + area.height {
        return;
    }
    let preview_area = Rect::new(popup_area.x, preview_y, popup_area.width, preview_height);
    frame.render_widget(Clear, preview_area);

    let block = theme::overlay_block()
        .title(Line::from(vec![
            Span::styled("\u{2588}", Style::default().fg(theme::epic_purple())),
            Span::styled(
                format!(" topology preview · {} ", topology.name),
                Style::default()
                    .fg(theme::overlay_hint())
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
        .padding(Padding::horizontal(1));
    let inner = block.inner(preview_area);
    frame.render_widget(block, preview_area);

    if inner.height < 2 {
        return;
    }
    let lines = crate::ui::overlay::topology_preview::render_dag_ascii(
        &topology.definition,
        inner.height as usize,
    );
    let para = Paragraph::new(lines);
    frame.render_widget(para, inner);
    // Allow the block to remain a Block (no-op return — keeps the import live).
    let _ = Block::default();
}

fn build_title_spans(
    kind: SessionKind,
    kind_fg: Color,
    parent_id: Option<uuid::Uuid>,
) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if kind == SessionKind::Epic {
        spans.push(Span::styled(
            "\u{2588}",
            Style::default().fg(theme::epic_purple()),
        ));
    }
    spans.push(Span::styled(
        format!(" {kind:?} "),
        Style::default()
            .bg(kind_fg)
            .fg(theme::overlay_bg())
            .add_modifier(Modifier::BOLD),
    ));
    let parent_label = match parent_id {
        Some(_) => " parent: ancestor ".to_string(),
        None => " parent: [root] ".to_string(),
    };
    spans.push(Span::styled(
        parent_label,
        Style::default().fg(theme::overlay_hint()),
    ));
    spans
}

fn render_kind_selector(
    frame: &mut Frame,
    inner: Rect,
    y: u16,
    selected: SessionKind,
    focused_field: CreateEntityField,
    parent_id: Option<uuid::Uuid>,
    app: &crate::app::App,
) {
    let parent_kind =
        parent_id.and_then(|id| app.sessions.get(&id).map(|s| s.session.session_kind));
    let allowed = legal_children(parent_kind);
    let all_kinds = [
        SessionKind::Group,
        SessionKind::Epic,
        SessionKind::Story,
        SessionKind::Task,
        SessionKind::Bug,
    ];

    let label_style = Style::default()
        .fg(theme::mauve())
        .add_modifier(Modifier::BOLD);
    let mut spans: Vec<Span<'static>> = vec![Span::styled("  Kind: ", label_style)];

    for kind in all_kinds {
        let fg = kind_color(kind).unwrap_or_else(theme::accent);
        let mut style = Style::default().bg(fg).fg(theme::overlay_bg());
        if kind == selected && focused_field == CreateEntityField::Kind {
            style = style.add_modifier(Modifier::REVERSED);
        } else if kind == selected {
            style = style.add_modifier(Modifier::BOLD);
        } else if !allowed.contains(&kind) {
            style = style.add_modifier(Modifier::DIM);
        }
        spans.push(Span::styled(format!(" {kind:?} "), style));
        spans.push(Span::raw(" "));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(inner.x, y, inner.width, 1),
    );
}

fn render_name_row(
    frame: &mut Frame,
    inner: Rect,
    y: u16,
    name: &str,
    focused_field: CreateEntityField,
    insert_mode: bool,
) {
    let label_style = Style::default()
        .fg(theme::mauve())
        .add_modifier(Modifier::BOLD);
    let mut spans = vec![
        Span::styled("  Name: ", label_style),
        Span::styled(name.to_string(), Style::default().fg(theme::text())),
    ];
    if focused_field == CreateEntityField::Name && insert_mode {
        spans.push(Span::styled("\u{2588}", Style::default().fg(theme::text())));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(inner.x, y, inner.width, 1),
    );
}

/// Render the multi-line Body region (leaf kinds only; S1): a label row with
/// a focus/mode affordance, then a `body_rows`-tall wrapped textarea via the
/// shared `render_wrapped_textarea` (scrolloff scrolling — the modal never
/// grows with content). Cursor: Hidden unfocused / Beam Insert / Block Normal
/// (ProviderForm precedent, F-025/F-026).
fn render_body_region(
    frame: &mut Frame,
    inner: Rect,
    y: u16,
    body: &crate::input_surface::InputSurface,
    focused_field: CreateEntityField,
    body_rows: u16,
) {
    use crate::types::PopupMode;
    use crate::ui::session::{self, CursorStyle};

    let focused = focused_field == CreateEntityField::Body;
    let label_style = Style::default()
        .fg(theme::mauve())
        .add_modifier(Modifier::BOLD);
    let mut spans: Vec<Span<'static>> = vec![Span::styled("  Body: ", label_style)];
    if focused {
        let mode_hint = match body.mode {
            PopupMode::Insert => "-- INSERT --",
            PopupMode::Normal => "-- NORMAL --",
        };
        spans.push(Span::styled(
            mode_hint,
            Style::default().fg(theme::overlay_hint()),
        ));
    } else if !body.has_content() {
        spans.push(Span::styled(
            "(b: edit prompt)",
            Style::default().fg(theme::overlay_hint()),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(inner.x, y, inner.width, 1),
    );

    let body_area = Rect::new(inner.x + 2, y + 1, inner.width.saturating_sub(2), body_rows);
    body.wrap_width.set(body_area.width as usize);
    let cursor_style = if focused {
        if body.mode == PopupMode::Insert {
            CursorStyle::Beam
        } else {
            CursorStyle::Block
        }
    } else {
        CursorStyle::Hidden
    };
    let visual_sel = if focused && body.vim_state.visual.is_some() {
        body.textarea.selection_range()
    } else {
        None
    };
    session::render_wrapped_textarea(
        frame,
        body_area,
        &body.textarea,
        cursor_style,
        visual_sel,
        theme::overlay_bg(),
    );
}

fn render_tags_row(
    frame: &mut Frame,
    inner: Rect,
    y: u16,
    tags: &[TagChip],
    focused_field: CreateEntityField,
    insert_mode: bool,
) {
    // B7 cold-start signal (P2.4): when the tag row carries no committed chips
    // AND the field is not focused (focused state already lights it up via
    // insert-mode cursor), flip the label color to red to signal the
    // mandatory-attention requirement. Cleared by `tags.is_empty()` becoming
    // false on first submitted chip.
    let label_color = if tags.is_empty() && focused_field != CreateEntityField::Tag {
        theme::red()
    } else {
        theme::mauve()
    };
    let label_style = Style::default()
        .fg(label_color)
        .add_modifier(Modifier::BOLD);
    let mut spans: Vec<Span<'static>> = vec![Span::styled("  Tags: ", label_style)];

    for chip in tags {
        match &chip.status {
            ChipStatus::Committed => {
                spans.push(Span::styled(
                    format!(" {} ", chip.value),
                    Style::default().bg(theme::accent()).fg(theme::overlay_bg()),
                ));
                spans.push(Span::raw(" "));
            }
            ChipStatus::Pending => {
                spans.push(Span::styled(
                    chip.value.clone(),
                    Style::default().fg(theme::text()),
                ));
                if focused_field == CreateEntityField::Tag && insert_mode {
                    spans.push(Span::styled("\u{2588}", Style::default().fg(theme::text())));
                }
            }
            ChipStatus::Invalid(err) => {
                spans.push(Span::styled(
                    format!(" {} ", chip.value),
                    Style::default().bg(theme::red()).fg(theme::overlay_bg()),
                ));
                spans.push(Span::styled(
                    format!(" ({err})"),
                    Style::default().fg(theme::red()),
                ));
                spans.push(Span::raw(" "));
            }
        }
    }

    // Pending input cursor when tag focused + insert mode but no Pending chip exists.
    if focused_field == CreateEntityField::Tag
        && insert_mode
        && !tags.iter().any(|c| c.status == ChipStatus::Pending)
    {
        spans.push(Span::styled("\u{2588}", Style::default().fg(theme::text())));
    }

    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(inner.x, y, inner.width, 1),
    );
}

/// Render the read-only Parent indicator row. The chord hint `gp` is shown
/// when focused (P2.3 Phase 5 will wire the chord to open the picker).
fn render_parent_row(
    frame: &mut Frame,
    inner: Rect,
    y: u16,
    parent_id: Option<uuid::Uuid>,
    focused_field: CreateEntityField,
    app: &crate::app::App,
) {
    let label_style = Style::default()
        .fg(theme::mauve())
        .add_modifier(Modifier::BOLD);
    let label = "  Parent: ";
    let parent_label: String = match parent_id {
        Some(pid) => app
            .sessions
            .get(&pid)
            .map(|s| {
                crate::types::resolve_session_display_identity(&s.session, &app.sessions)
                    .effective_title
            })
            .unwrap_or_else(|| pid.to_string()),
        None => "[root]".to_string(),
    };
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(label, label_style),
        Span::styled(parent_label, Style::default().fg(theme::text())),
    ];
    if focused_field == CreateEntityField::Parent {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            "(gp to change)",
            Style::default().fg(theme::overlay_hint()),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(inner.x, y, inner.width, 1),
    );
}

/// Render the Topology row (Epic only) — Pattern 2 inline filter dropdown.
/// Always renders the selection summary line; when focused, expands the
/// dropdown with up to 6 visible filtered rows.
#[allow(clippy::too_many_arguments)]
fn render_topology_row(
    frame: &mut Frame,
    inner: Rect,
    y: u16,
    topology_id: Option<uuid::Uuid>,
    topology_filter: &str,
    topology_selected_index: usize,
    topology_choices: &[Topology],
    focused_field: CreateEntityField,
    preview_open: bool,
) {
    let label_style = Style::default()
        .fg(theme::mauve())
        .add_modifier(Modifier::BOLD);
    let focused = focused_field == CreateEntityField::Topology;
    // Vertical accent stripe (epic_purple) per INDEX §1.5.
    let stripe_span = Span::styled("\u{2588}", Style::default().fg(theme::epic_purple()));
    let label_span = Span::styled(" Topology: ", label_style);

    let summary: String = if topology_choices.is_empty() {
        "[topologies unavailable]".to_string()
    } else {
        match topology_id.and_then(|id| topology_choices.iter().find(|t| t.id == id)) {
            Some(t) => t.name.clone(),
            None => "<no topology>".to_string(),
        }
    };
    let summary_color = if focused {
        theme::accent()
    } else {
        theme::text()
    };
    let mut spans: Vec<Span<'static>> = vec![
        stripe_span,
        label_span,
        Span::styled(summary, Style::default().fg(summary_color)),
    ];
    if focused {
        spans.push(Span::raw("  "));
        let hint = if preview_open {
            "(Space: close preview, Enter: pick, Esc: clear)"
        } else {
            "(Space: preview, Enter: pick, Esc: clear)"
        };
        spans.push(Span::styled(
            hint,
            Style::default().fg(theme::overlay_hint()),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(inner.x, y, inner.width, 1),
    );

    // Dropdown body — visible when focused.
    if !focused {
        return;
    }
    if topology_choices.is_empty() {
        return;
    }
    let visible = topology_filtered_indices(topology_choices, topology_filter);
    let row_count = visible.len().min(6) as u16;
    if row_count == 0 {
        // Filter narrowed to nothing — show the live filter buffer.
        let filter_span = Span::styled(
            format!("    filter: {}\u{2588}", topology_filter),
            Style::default().fg(theme::overlay_hint()),
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![filter_span])),
            Rect::new(inner.x, y + 1, inner.width, 1),
        );
        return;
    }
    for (row_idx, &choice_idx) in visible.iter().take(6).enumerate() {
        let topology = &topology_choices[choice_idx];
        let is_selected = row_idx == topology_selected_index;
        let prefix = if is_selected { " > " } else { "   " };
        let mut style = Style::default().fg(theme::text());
        if is_selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        let line = Line::from(vec![Span::styled(
            format!("{prefix}{}", topology.name),
            style,
        )]);
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(inner.x, y + 1 + row_idx as u16, inner.width, 1),
        );
    }
    let _ = row_count; // silence unused
}

/// Render the Provider segmented row (leaf only). Pattern 1.
fn render_provider_row(
    frame: &mut Frame,
    inner: Rect,
    y: u16,
    provider: Option<rsi_common::types::SessionProvider>,
    focused_field: CreateEntityField,
) {
    use rsi_common::types::SessionProvider;
    let label_style = Style::default()
        .fg(theme::mauve())
        .add_modifier(Modifier::BOLD);
    let focused = focused_field == CreateEntityField::Provider;
    let mut spans: Vec<Span<'static>> = vec![Span::styled("  Provider: ", label_style)];
    let builtin = [
        ("Claude", SessionProvider::Claude),
        ("Codex", SessionProvider::Codex),
        ("Pioneer", SessionProvider::Pioneer),
        ("OpenRouter", SessionProvider::OpenRouter),
        ("Bedrock", SessionProvider::Bedrock),
        ("Local", SessionProvider::Local),
        ("Antigravity", SessionProvider::Antigravity),
        ("Harness", SessionProvider::Harness),
    ];
    for (label, p) in builtin {
        let selected = provider == Some(p);
        let mut style = Style::default().fg(theme::text());
        if selected && focused {
            style = style
                .bg(theme::accent())
                .fg(theme::overlay_bg())
                .add_modifier(Modifier::REVERSED);
        } else if selected {
            style = style
                .add_modifier(Modifier::BOLD)
                .bg(theme::accent())
                .fg(theme::overlay_bg());
        }
        spans.push(Span::styled(format!(" {label} "), style));
        spans.push(Span::raw(" "));
    }
    if provider.is_none() {
        spans.push(Span::styled(
            "(p / Shift-p to cycle)",
            Style::default().fg(theme::overlay_hint()),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(inner.x, y, inner.width, 1),
    );
}

/// Render the Model row (leaf only). Shows the chosen model name + a hint
/// to open the model dropdown via `m`.
fn render_model_row(
    frame: &mut Frame,
    inner: Rect,
    y: u16,
    model: Option<&str>,
    focused_field: CreateEntityField,
) {
    let label_style = Style::default()
        .fg(theme::mauve())
        .add_modifier(Modifier::BOLD);
    let focused = focused_field == CreateEntityField::Model;
    let model_label = model.unwrap_or("<default>");
    let model_color = if focused {
        theme::accent()
    } else {
        theme::text()
    };
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled("  Model: ", label_style),
        Span::styled(model_label.to_string(), Style::default().fg(model_color)),
    ];
    if focused {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            "(m: open picker)",
            Style::default().fg(theme::overlay_hint()),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(inner.x, y, inner.width, 1),
    );
}

/// Render the Effort segmented row (leaf only). Pattern 1.
fn render_effort_row(
    frame: &mut Frame,
    inner: Rect,
    y: u16,
    effort: Option<&str>,
    focused_field: CreateEntityField,
) {
    let label_style = Style::default()
        .fg(theme::mauve())
        .add_modifier(Modifier::BOLD);
    let focused = focused_field == CreateEntityField::Effort;
    let mut spans: Vec<Span<'static>> = vec![Span::styled("  Effort: ", label_style)];
    let cells: [(&str, Option<&str>); 6] = [
        ("none", None),
        ("low", Some("low")),
        ("med", Some("medium")),
        ("high", Some("high")),
        ("max", Some("max")),
        ("xhi", Some("xhigh")),
    ];
    for (label, value) in cells {
        let selected = effort == value;
        let mut style = Style::default().fg(theme::text());
        if selected && focused {
            style = style
                .bg(theme::accent())
                .fg(theme::overlay_bg())
                .add_modifier(Modifier::REVERSED);
        } else if selected {
            style = style
                .bg(theme::accent())
                .fg(theme::overlay_bg())
                .add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(format!(" {label} "), style));
        spans.push(Span::raw(" "));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(inner.x, y, inner.width, 1),
    );
}

/// Render the Sandbox toggle row (leaf only). Bool toggle.
fn render_sandbox_row(
    frame: &mut Frame,
    inner: Rect,
    y: u16,
    sandbox: bool,
    focused_field: CreateEntityField,
) {
    let label_style = Style::default()
        .fg(theme::mauve())
        .add_modifier(Modifier::BOLD);
    let focused = focused_field == CreateEntityField::Sandbox;
    let toggle_label = if sandbox { "[on]" } else { "[off]" };
    let toggle_color = if focused {
        theme::accent()
    } else {
        theme::text()
    };
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled("  Sandbox: ", label_style),
        Span::styled(toggle_label.to_string(), Style::default().fg(toggle_color)),
    ];
    if focused {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            "(s: toggle)",
            Style::default().fg(theme::overlay_hint()),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(inner.x, y, inner.width, 1),
    );
}

fn render_error_banner(frame: &mut Frame, inner: Rect, y: u16, err: &str) {
    let banner = Line::from(vec![Span::styled(
        format!(" {err} "),
        Style::default().bg(theme::red()).fg(theme::overlay_bg()),
    )]);
    frame.render_widget(
        Paragraph::new(banner),
        Rect::new(inner.x, y, inner.width, 1),
    );
}

fn render_hint_bar(frame: &mut Frame, inner: Rect, focused_field: CreateEntityField) {
    let hint_y = inner.y + inner.height.saturating_sub(1);
    // Body-focused hints differ: plain Enter is a newline there (S1).
    let hint_text = if focused_field == CreateEntityField::Body {
        "Ctrl+Enter submit · Enter newline · Tab next field · Esc normal/close"
    } else {
        "Tab: cycle  n/b/k/T: focus  i: insert  Esc: save  C-d: discard  Enter: submit"
    };
    let hint = Line::from(vec![Span::styled(
        hint_text,
        Style::default().fg(theme::overlay_hint()),
    )]);
    frame.render_widget(
        Paragraph::new(hint),
        Rect::new(inner.x, hint_y, inner.width, 1),
    );
}
