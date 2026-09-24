//! Graph review overlay renderer — visual workflow graph display.

use super::graph_layout::{
    GraphCanvasEdgeKind, GraphEdgeRoute, GraphLayout, GraphNodeLayout, MIN_GRAPH_CANVAS_HEIGHT,
    MIN_GRAPH_CANVAS_WIDTH, OrthogonalSegment, VirtualPoint, VirtualRect, layout_workflow_graph,
    viewport_origin,
};
use ratatui::buffer::Buffer;
use ratatui::prelude::*;
use ratatui::widgets::*;

use crate::types::{
    GraphDraftPersistenceState, GraphEditField, GraphMode, GraphPickerKind, GraphViewOrigin,
    GraphViewport, RecursiveDagBrowserState,
};
use crate::ui::theme;
use rsi_common::recursive_dag::RecursiveTaskGraphSummary;
use rsi_common::types::{
    Workflow, WorkflowExecutionSnapshot, WorkflowExecutionStatus, WorkflowNodeExecutionState,
    WorkflowValidationReport,
};
use rsi_graph::format::*;
use rsi_graph::generate::templates::starter_templates;
use std::collections::{HashMap, HashSet};

#[allow(clippy::too_many_arguments)]
pub fn render_graph_review(
    frame: &mut Frame,
    area: Rect,
    workflow: &WorkflowDefinition,
    persistence_state: GraphDraftPersistenceState,
    mode: GraphMode,
    viewport: GraphViewport,
    selected_node: usize,
    collapsed: &HashSet<String>,
    edit_buffer: &str,
    selected_edge: usize,
    execution: Option<&WorkflowExecutionSnapshot>,
    saved_workflows: &[Workflow],
    recursive_graphs: &[RecursiveTaskGraphSummary],
    view_origin: &GraphViewOrigin,
    picker_list_state: &mut ListState,
    gv_info_dashboard: bool,
    dashboard_focused: bool,
    dashboard_state: Option<&RecursiveDagBrowserState>,
    app: &crate::app::App,
) {
    let validation = rsi_graph::validate_executable_workflow(workflow);

    let popup_area = Rect::new(
        area.x,
        area.y.saturating_add(1),
        area.width,
        area.height.saturating_sub(2),
    );

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::overlay_border()))
        .style(Style::default().bg(theme::overlay_bg()))
        .title(render_title(
            workflow,
            persistence_state,
            mode,
            viewport,
            &validation,
            execution,
            view_origin,
            selected_node,
        ))
        .padding(Padding::new(1, 1, 0, 0));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    if inner.height < 4 {
        return;
    }

    let detail_visible =
        mode.shows_detail_panel() && !workflow.nodes.is_empty() && !mode.is_picker();
    let bottom_height = if detail_visible { 12 } else { 3 };

    let layout =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(bottom_height)]).split(inner);
    let graph_area = layout[0];
    let bottom_area = layout[1];

    if let Some((kind, selected_index)) = mode.picker() {
        render_picker_panel(
            frame,
            graph_area,
            kind,
            selected_index,
            saved_workflows,
            recursive_graphs,
            picker_list_state,
        );
    } else if workflow.nodes.is_empty() {
        render_empty_state(frame, graph_area);
    } else {
        if gv_info_dashboard && matches!(view_origin, GraphViewOrigin::BridgedRecursive { .. }) {
            let chunks =
                Layout::horizontal([Constraint::Fill(1), Constraint::Length(35)]).split(graph_area);

            let canvas_area = chunks[0];
            let dashboard_area = chunks[1];

            render_graph_canvas(
                frame,
                canvas_area,
                workflow,
                mode,
                viewport,
                selected_node,
                collapsed,
                &validation,
                execution,
            );

            let dashboard_block = Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(theme::overlay_border()))
                .title(Span::styled(
                    if dashboard_focused {
                        " INFO DASHBOARD "
                    } else {
                        " info dashboard "
                    },
                    Style::default()
                        .fg(if dashboard_focused {
                            theme::yellow()
                        } else {
                            theme::overlay_hint()
                        })
                        .add_modifier(Modifier::BOLD),
                ));
            let inner_dashboard_area = dashboard_block.inner(dashboard_area);
            frame.render_widget(dashboard_block, dashboard_area);

            if inner_dashboard_area.height > 0 && inner_dashboard_area.width > 0 {
                let mut lines = Vec::new();
                if let Some(state) = dashboard_state {
                    lines = crate::ui::overlay::recursive_dag::build_dashboard_lines(
                        app,
                        state,
                        inner_dashboard_area.width as usize,
                    );
                    let max_scroll = lines
                        .len()
                        .saturating_sub(inner_dashboard_area.height as usize);
                    let offset = state.scroll_offset.min(max_scroll);
                    lines = lines
                        .into_iter()
                        .skip(offset)
                        .take(inner_dashboard_area.height as usize)
                        .collect();
                } else {
                    lines.push(Line::from(vec![Span::styled(
                        "loading dashboard...",
                        Style::default().fg(theme::overlay_hint()),
                    )]));
                }
                frame.render_widget(Paragraph::new(lines), inner_dashboard_area);
            }
        } else {
            render_graph_canvas(
                frame,
                graph_area,
                workflow,
                mode,
                viewport,
                selected_node,
                collapsed,
                &validation,
                execution,
            );
        }
    }

    if detail_visible {
        render_detail_panel(
            frame,
            bottom_area,
            workflow,
            selected_node,
            mode,
            edit_buffer,
            selected_edge,
            execution,
            view_origin,
        );
    } else {
        render_status_bar(
            frame,
            bottom_area,
            workflow,
            mode,
            viewport,
            &validation,
            execution,
            saved_workflows.is_empty(),
        );
    }
}

fn render_title(
    workflow: &WorkflowDefinition,
    persistence_state: GraphDraftPersistenceState,
    mode: GraphMode,
    viewport: GraphViewport,
    validation: &WorkflowValidationReport,
    execution: Option<&WorkflowExecutionSnapshot>,
    view_origin: &GraphViewOrigin,
    selected_node: usize,
) -> Line<'static> {
    let camera_badge = match mode.camera() {
        crate::types::GraphCamera::FollowSelection => {
            Span::styled("[follow]", Style::default().fg(theme::surface1()))
        }
        crate::types::GraphCamera::Manual if !viewport.is_centered() => Span::styled(
            format!("[manual {:+},{:+}]", viewport.offset_x, viewport.offset_y),
            Style::default().fg(theme::surface1()),
        ),
        crate::types::GraphCamera::Manual => {
            Span::styled("[manual]", Style::default().fg(theme::surface1()))
        }
    };

    let mut spans = vec![
        Span::styled(
            " Graph Review: ",
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(workflow.name.clone(), Style::default().fg(theme::green())),
        Span::raw(" "),
        render_persistence_badge(persistence_state),
        Span::raw(" "),
        Span::styled(
            format!("[{}]", mode.label()),
            Style::default().fg(theme::overlay_hint()),
        ),
        Span::raw(" "),
        camera_badge,
    ];

    if validation.has_errors() {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("[invalid:{}]", validation.error_count()),
            Style::default().fg(theme::red()),
        ));
    } else if validation.warning_count() > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("[warn:{}]", validation.warning_count()),
            Style::default().fg(theme::yellow()),
        ));
    } else {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            "[executable]",
            Style::default().fg(theme::green()),
        ));
    }

    if let Some(execution) = execution {
        spans.push(Span::raw(" "));
        spans.push(render_execution_badge(execution));
    }

    // Honesty badge (§4.4): bridged read-only/editable origins are labeled
    match view_origin {
        GraphViewOrigin::AuthoredWorkflow => {}
        GraphViewOrigin::BridgedRecursive { .. } => {
            spans.push(Span::raw(" "));
            let lock_label = if selected_node != usize::MAX
                && let Some(node) = workflow.nodes.get(selected_node)
                && let Some(rsi_graph::data::Value::String(locks_str)) =
                    workflow.metadata.get("recursive_node_locks")
                && let Ok(locks) = serde_json::from_str::<serde_json::Value>(locks_str)
                && let node_lock = &locks[&node.id]
                && !node_lock.is_null()
            {
                let locked = node_lock["locked"].as_bool().unwrap_or(true);
                if locked {
                    let reason = node_lock["reason"].as_str().unwrap_or("locked");
                    format!("recursive [locked: {}]", reason)
                } else {
                    "recursive [editable]".to_string()
                }
            } else {
                "recursive (read-only)".to_string()
            };
            spans.push(Span::styled(
                lock_label,
                Style::default().fg(theme::yellow()),
            ));
        }
        GraphViewOrigin::BridgedTopology { .. } => {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                "topology (read-only)",
                Style::default().fg(theme::yellow()),
            ));
        }
    }

    Line::from(spans)
}

fn render_graph_canvas(
    frame: &mut Frame,
    area: Rect,
    workflow: &WorkflowDefinition,
    mode: GraphMode,
    viewport: GraphViewport,
    selected_node: usize,
    collapsed: &HashSet<String>,
    validation: &WorkflowValidationReport,
    execution: Option<&WorkflowExecutionSnapshot>,
) {
    if workflow.nodes.is_empty() || area.width == 0 || area.height == 0 {
        return;
    }

    let layout = layout_workflow_graph(workflow);
    let banner = graph_banner(validation, &layout);
    let canvas_area = if let Some(line) = banner {
        let chunks = Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]).split(area);
        frame.render_widget(
            Paragraph::new(line)
                .style(Style::default().bg(theme::structural_bg(theme::surface1()))),
            chunks[0],
        );
        chunks[1]
    } else {
        area
    };

    if canvas_area.width < MIN_GRAPH_CANVAS_WIDTH || canvas_area.height < MIN_GRAPH_CANVAS_HEIGHT {
        render_terminal_too_small(frame, canvas_area);
        return;
    }

    let origin = viewport_origin(
        &layout,
        canvas_area.width,
        canvas_area.height,
        selected_node,
        mode.camera(),
        viewport,
    );

    frame.render_widget(
        GraphCanvasWidget {
            layout: &layout,
            workflow,
            selected_node,
            collapsed,
            validation,
            execution,
            origin,
        },
        canvas_area,
    );
}

fn graph_banner(
    validation: &WorkflowValidationReport,
    layout: &GraphLayout,
) -> Option<Line<'static>> {
    if validation.has_errors() {
        let mut tags = Vec::new();
        if layout.has_cycle_edges
            || validation
                .diagnostics
                .iter()
                .any(|diag| diag.code == "workflow.edge.cycle")
        {
            tags.push("cycle");
        }
        if validation
            .diagnostics
            .iter()
            .any(|diag| diag.code == "workflow.node.unsupported_kind")
        {
            tags.push("unsupported nodes");
        }
        if validation.diagnostics.iter().any(|diag| {
            matches!(
                diag.code.as_str(),
                "workflow.edge.missing_source" | "workflow.edge.missing_target"
            )
        }) {
            tags.push("dangling edges");
        }
        if validation
            .diagnostics
            .iter()
            .any(|diag| diag.code.starts_with("workflow.graph_view."))
        {
            tags.push("graph_view");
        }
        let summary = if tags.is_empty() {
            format!("{} blocking issue(s)", validation.error_count())
        } else {
            tags.join(" · ")
        };

        return Some(Line::from(vec![
            Span::styled(
                " ! ",
                Style::default()
                    .fg(theme::crust())
                    .bg(theme::red())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("non-executable: {}", summary),
                Style::default()
                    .fg(theme::text())
                    .bg(theme::structural_bg(theme::surface1()))
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
    }

    if validation.warning_count() > 0 {
        return Some(Line::from(vec![
            Span::styled(
                " ? ",
                Style::default()
                    .fg(theme::crust())
                    .bg(theme::yellow())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "{} warning(s) in workflow metadata",
                    validation.warning_count()
                ),
                Style::default()
                    .fg(theme::text())
                    .bg(theme::structural_bg(theme::surface1())),
            ),
        ]));
    }

    None
}

struct GraphCanvasWidget<'a> {
    layout: &'a GraphLayout,
    workflow: &'a WorkflowDefinition,
    selected_node: usize,
    collapsed: &'a HashSet<String>,
    validation: &'a WorkflowValidationReport,
    execution: Option<&'a WorkflowExecutionSnapshot>,
    origin: VirtualPoint,
}

impl Widget for GraphCanvasWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Clear cells to spaces (transparent background) without painting
        // a fill color — the overlay's Clear widget and the bottom-pane
        // bordered block already handle the backdrop.
        fill_area(
            buf,
            area,
            Style::default().fg(theme::text()).bg(theme::overlay_bg()),
        );

        let execution_states = execution_state_map(self.execution);
        let invalid_nodes = blocking_node_ids(self.validation);
        let executable_invalid = self.validation.has_errors();

        let mut edge_layer = EdgeLayer::new(area.width, area.height);
        for edge in &self.layout.edges {
            edge_layer.draw_route(edge, self.origin, edge_style(edge.kind, executable_invalid));
        }
        edge_layer.render(area, buf);

        for node_layout in &self.layout.nodes {
            draw_node_box(
                buf,
                area,
                self.origin,
                &self.workflow.nodes[node_layout.node_index],
                node_layout,
                node_layout.node_index == self.selected_node,
                self.collapsed
                    .contains(&self.workflow.nodes[node_layout.node_index].id),
                invalid_nodes.contains(self.workflow.nodes[node_layout.node_index].id.as_str()),
                execution_states
                    .get(self.workflow.nodes[node_layout.node_index].id.as_str())
                    .copied(),
            );
        }

        for edge in &self.layout.edges {
            draw_edge_label(
                buf,
                area,
                self.origin,
                edge,
                edge_style(edge.kind, executable_invalid),
            );
        }

        for edge in &self.layout.edges {
            draw_char(
                buf,
                area,
                self.origin,
                edge.arrow.position,
                edge.arrow.glyph,
                edge_style(edge.kind, executable_invalid).add_modifier(Modifier::BOLD),
            );
        }
    }
}

struct EdgeLayer {
    width: u16,
    height: u16,
    masks: Vec<u8>,
    styles: Vec<Style>,
}

impl EdgeLayer {
    fn new(width: u16, height: u16) -> Self {
        let len = width as usize * height as usize;
        Self {
            width,
            height,
            masks: vec![0; len],
            styles: vec![Style::default(); len],
        }
    }

    fn draw_route(&mut self, route: &GraphEdgeRoute, origin: VirtualPoint, style: Style) {
        for segment in &route.segments {
            self.draw_segment(*segment, origin, style);
        }
    }

    fn draw_segment(&mut self, segment: OrthogonalSegment, origin: VirtualPoint, style: Style) {
        if segment.start.y == segment.end.y {
            let y = segment.start.y - origin.y;
            let min_x = segment.start.x.min(segment.end.x) - origin.x;
            let max_x = segment.start.x.max(segment.end.x) - origin.x;
            for x in min_x..max_x {
                self.connect((x, y), (x + 1, y), style);
            }
        } else if segment.start.x == segment.end.x {
            let x = segment.start.x - origin.x;
            let min_y = segment.start.y.min(segment.end.y) - origin.y;
            let max_y = segment.start.y.max(segment.end.y) - origin.y;
            for y in min_y..max_y {
                self.connect((x, y), (x, y + 1), style);
            }
        }
    }

    fn connect(&mut self, from: (i32, i32), to: (i32, i32), style: Style) {
        let dx = to.0 - from.0;
        let dy = to.1 - from.1;
        let (from_mask, to_mask) = match (dx, dy) {
            (1, 0) => (RIGHT, LEFT),
            (-1, 0) => (LEFT, RIGHT),
            (0, 1) => (DOWN, UP),
            (0, -1) => (UP, DOWN),
            _ => return,
        };
        self.add_mask(from.0, from.1, from_mask, style);
        self.add_mask(to.0, to.1, to_mask, style);
    }

    fn add_mask(&mut self, x: i32, y: i32, mask: u8, style: Style) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }
        let index = y as usize * self.width as usize + x as usize;
        self.masks[index] |= mask;
        self.styles[index] = style;
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        for y in 0..self.height {
            for x in 0..self.width {
                let index = y as usize * self.width as usize + x as usize;
                let mask = self.masks[index];
                if mask == 0 {
                    continue;
                }
                write_char(
                    buf,
                    area.x + x,
                    area.y + y,
                    mask_to_char(mask),
                    self.styles[index],
                );
            }
        }
    }
}

const UP: u8 = 0b0001;
const DOWN: u8 = 0b0010;
const LEFT: u8 = 0b0100;
const RIGHT: u8 = 0b1000;

fn mask_to_char(mask: u8) -> char {
    match mask {
        m if m == (LEFT | RIGHT) => '─',
        m if m == (UP | DOWN) => '│',
        m if m == (RIGHT | DOWN) => '┌',
        m if m == (LEFT | DOWN) => '┐',
        m if m == (RIGHT | UP) => '└',
        m if m == (LEFT | UP) => '┘',
        m if m == (LEFT | RIGHT | DOWN) => '┬',
        m if m == (LEFT | RIGHT | UP) => '┴',
        m if m == (UP | DOWN | RIGHT) => '├',
        m if m == (UP | DOWN | LEFT) => '┤',
        m if m == (UP | DOWN | LEFT | RIGHT) => '┼',
        mask if mask & (LEFT | RIGHT) != 0 => '─',
        _ => '│',
    }
}

fn fill_area(buf: &mut Buffer, area: Rect, style: Style) {
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            write_char(buf, x, y, ' ', style);
        }
    }
}

fn fill_virtual_rect(
    buf: &mut Buffer,
    area: Rect,
    origin: VirtualPoint,
    rect: VirtualRect,
    style: Style,
) {
    for y in rect.y..=rect.bottom() {
        for x in rect.x..=rect.right() {
            draw_char(buf, area, origin, VirtualPoint { x, y }, ' ', style);
        }
    }
}

fn draw_node_box(
    buf: &mut Buffer,
    area: Rect,
    origin: VirtualPoint,
    node: &NodeDef,
    node_layout: &GraphNodeLayout,
    selected: bool,
    collapsed: bool,
    invalid: bool,
    execution_state: Option<WorkflowNodeExecutionState>,
) {
    let bg = if selected {
        theme::surface2()
    } else {
        theme::overlay_bg()
    };
    let fill_style = Style::default().fg(theme::text()).bg(bg);
    fill_virtual_rect(buf, area, origin, node_layout.rect, fill_style);

    let border_color = if invalid {
        theme::red()
    } else if let Some(state) = execution_state {
        execution_state_color(state)
    } else {
        node_type_color(&node.node_type)
    };
    let border_style = Style::default()
        .fg(border_color)
        .bg(bg)
        .add_modifier(if selected {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });

    let rect = node_layout.rect;
    draw_char(
        buf,
        area,
        origin,
        VirtualPoint {
            x: rect.x,
            y: rect.y,
        },
        '┌',
        border_style,
    );
    draw_char(
        buf,
        area,
        origin,
        VirtualPoint {
            x: rect.right(),
            y: rect.y,
        },
        '┐',
        border_style,
    );
    draw_char(
        buf,
        area,
        origin,
        VirtualPoint {
            x: rect.x,
            y: rect.bottom(),
        },
        '└',
        border_style,
    );
    draw_char(
        buf,
        area,
        origin,
        VirtualPoint {
            x: rect.right(),
            y: rect.bottom(),
        },
        '┘',
        border_style,
    );
    for x in rect.x + 1..rect.right() {
        draw_char(
            buf,
            area,
            origin,
            VirtualPoint { x, y: rect.y },
            '─',
            border_style,
        );
        draw_char(
            buf,
            area,
            origin,
            VirtualPoint {
                x,
                y: rect.bottom(),
            },
            '─',
            border_style,
        );
    }
    draw_char(
        buf,
        area,
        origin,
        VirtualPoint {
            x: rect.x,
            y: rect.y + 1,
        },
        '│',
        border_style,
    );
    draw_char(
        buf,
        area,
        origin,
        VirtualPoint {
            x: rect.right(),
            y: rect.y + 1,
        },
        '│',
        border_style,
    );

    let badge = node_badge_glyph(invalid, execution_state);
    let badge_style = Style::default()
        .fg(if invalid {
            theme::red()
        } else if let Some(state) = execution_state {
            execution_state_color(state)
        } else {
            theme::overlay_hint()
        })
        .bg(bg)
        .add_modifier(
            if invalid || matches!(execution_state, Some(WorkflowNodeExecutionState::Running)) {
                Modifier::BOLD
            } else {
                Modifier::empty()
            },
        );
    let type_style = Style::default()
        .fg(node_type_color(&node.node_type))
        .bg(bg)
        .add_modifier(if selected {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });
    let label_style = Style::default()
        .fg(theme::text())
        .bg(bg)
        .add_modifier(if selected {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });
    let title = truncate_graph_label(&node.name, node_layout.inner_width.saturating_sub(4));

    draw_char(
        buf,
        area,
        origin,
        VirtualPoint {
            x: rect.x + 1,
            y: rect.y + 1,
        },
        badge,
        badge_style,
    );
    draw_char(
        buf,
        area,
        origin,
        VirtualPoint {
            x: rect.x + 3,
            y: rect.y + 1,
        },
        node_type_glyph(&node.node_type, collapsed),
        type_style,
    );
    draw_text(
        buf,
        area,
        origin,
        VirtualPoint {
            x: rect.x + 5,
            y: rect.y + 1,
        },
        &title,
        label_style,
    );
}

fn draw_edge_label(
    buf: &mut Buffer,
    area: Rect,
    origin: VirtualPoint,
    edge: &GraphEdgeRoute,
    style: Style,
) {
    let Some(label) = edge
        .label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
    else {
        return;
    };
    let text = format!(" {} ", label);
    let label_width = text.chars().count() as i32;
    let Some(segment) = edge.segments.iter().copied().find(|segment| {
        segment.is_horizontal() && (segment.end.x - segment.start.x).abs() + 1 >= label_width
    }) else {
        return;
    };

    let min_x = segment.start.x.min(segment.end.x);
    let max_x = segment.start.x.max(segment.end.x);
    let start_x = min_x + ((max_x - min_x + 1 - label_width) / 2);
    draw_text(
        buf,
        area,
        origin,
        VirtualPoint {
            x: start_x,
            y: segment.start.y,
        },
        &text,
        style.bg(theme::root_bg()),
    );
}

fn draw_text(
    buf: &mut Buffer,
    area: Rect,
    origin: VirtualPoint,
    start: VirtualPoint,
    text: &str,
    style: Style,
) {
    for (offset, ch) in text.chars().enumerate() {
        draw_char(
            buf,
            area,
            origin,
            VirtualPoint {
                x: start.x + offset as i32,
                y: start.y,
            },
            ch,
            style,
        );
    }
}

fn draw_char(
    buf: &mut Buffer,
    area: Rect,
    origin: VirtualPoint,
    point: VirtualPoint,
    ch: char,
    style: Style,
) {
    let screen_x = point.x - origin.x;
    let screen_y = point.y - origin.y;
    if screen_x < 0
        || screen_y < 0
        || screen_x >= area.width as i32
        || screen_y >= area.height as i32
    {
        return;
    }

    write_char(
        buf,
        area.x + screen_x as u16,
        area.y + screen_y as u16,
        ch,
        style,
    );
}

fn write_char(buf: &mut Buffer, x: u16, y: u16, ch: char, style: Style) {
    // Out-of-bounds coordinates can occur when the terminal is resized smaller
    // than the canvas computed for the previous frame, or when a graph layout
    // overflows the available area. Skip silently — the renderer is best-effort
    // (the next frame will recompute layout for the new size). Panicking here
    // crashed the TUI loop on small terminals; see hardening backlog #P0-1.
    let Some(cell) = buf.cell_mut((x, y)) else {
        return;
    };
    let mut scratch = [0; 4];
    let symbol = ch.encode_utf8(&mut scratch);
    cell.set_symbol(symbol);
    cell.set_style(style);
}

fn execution_state_map(
    execution: Option<&WorkflowExecutionSnapshot>,
) -> HashMap<&str, WorkflowNodeExecutionState> {
    let mut states = HashMap::new();
    if let Some(execution) = execution {
        for update in &execution.updates {
            if let (Some(node_id), Some(node_state)) =
                (update.node_id.as_deref(), update.node_state)
            {
                states.insert(node_id, node_state);
            }
        }
    }
    states
}

fn blocking_node_ids(validation: &WorkflowValidationReport) -> HashSet<&str> {
    validation
        .diagnostics
        .iter()
        .filter(|diag| diag.is_blocking())
        .filter_map(|diag| diag.subject.node_id.as_deref())
        .collect()
}

fn edge_style(kind: GraphCanvasEdgeKind, executable_invalid: bool) -> Style {
    match kind {
        GraphCanvasEdgeKind::Executable if executable_invalid => {
            Style::default().fg(theme::red()).bg(theme::overlay_bg())
        }
        GraphCanvasEdgeKind::Executable => Style::default()
            .fg(theme::overlay1())
            .bg(theme::overlay_bg()),
        GraphCanvasEdgeKind::VisualLoopArrow => Style::default()
            .fg(theme::yellow())
            .bg(theme::overlay_bg())
            .add_modifier(Modifier::BOLD),
    }
}

fn node_type_color(node_type: &NodeType) -> Color {
    match node_type {
        NodeType::Action => theme::blue(),
        NodeType::Topology => theme::mauve(),
        NodeType::Subgraph => theme::peach(),
    }
}

fn execution_state_color(state: WorkflowNodeExecutionState) -> Color {
    match state {
        WorkflowNodeExecutionState::Running | WorkflowNodeExecutionState::Succeeded => {
            theme::green()
        }
        WorkflowNodeExecutionState::Failed => theme::red(),
        _ => theme::overlay1(),
    }
}

fn node_badge_glyph(invalid: bool, execution_state: Option<WorkflowNodeExecutionState>) -> char {
    if invalid {
        '!'
    } else {
        match execution_state {
            Some(WorkflowNodeExecutionState::Running) => '▶',
            Some(WorkflowNodeExecutionState::Succeeded) => '✓',
            Some(WorkflowNodeExecutionState::Failed) => '✕',
            Some(WorkflowNodeExecutionState::Skipped) => '-',
            None => '·',
            Some(_) => '?',
        }
    }
}

fn node_type_glyph(node_type: &NodeType, collapsed: bool) -> char {
    match node_type {
        NodeType::Action => '◆',
        NodeType::Topology => {
            if collapsed {
                '▶'
            } else {
                '▼'
            }
        }
        NodeType::Subgraph => {
            if collapsed {
                '▸'
            } else {
                '▾'
            }
        }
    }
}

fn truncate_graph_label(label: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }

    let label_len = label.chars().count();
    if label_len <= max_chars {
        return label.to_string();
    }
    if max_chars == 1 {
        return "…".to_string();
    }

    let mut truncated: String = label.chars().take(max_chars - 1).collect();
    truncated.push('…');
    truncated
}

fn graph_node_visual_edge_summaries(workflow: &WorkflowDefinition, node_id: &str) -> Vec<String> {
    workflow
        .graph_view_metadata()
        .ok()
        .flatten()
        .map(|graph_view| {
            graph_view
                .visual_edges
                .into_iter()
                .filter(|edge| edge.source == node_id || edge.target == node_id)
                .map(|edge| format_visual_edge_summary(&edge))
                .collect()
        })
        .unwrap_or_default()
}

fn format_visual_edge_summary(edge: &GraphViewVisualEdge) -> String {
    let kind = match edge.kind {
        GraphViewVisualEdgeKind::LoopArrow => "loop",
    };
    let mut summary = format!(
        "{} {} -> {}",
        kind,
        format_visual_edge_endpoint(&edge.source, edge.source_port.as_deref()),
        format_visual_edge_endpoint(&edge.target, edge.target_port.as_deref()),
    );
    if let Some(label) = edge
        .label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
    {
        summary.push_str(" [");
        summary.push_str(label);
        summary.push(']');
    }
    summary
}

fn format_visual_edge_endpoint(node_id: &str, port: Option<&str>) -> String {
    match port {
        Some(port) if !port.is_empty() => format!("{node_id}:{port}"),
        _ => node_id.to_string(),
    }
}

fn render_terminal_too_small(frame: &mut Frame, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                "terminal too small",
                Style::default()
                    .fg(theme::text())
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "Need at least 32x8 for the graph canvas.",
                Style::default().fg(theme::overlay_hint()),
            )),
        ])
        .alignment(Alignment::Center),
        area,
    );
}

fn render_empty_state(frame: &mut Frame, area: Rect) {
    let text = Paragraph::new(vec![
        Line::from(Span::styled(
            "Empty workflow draft",
            Style::default()
                .fg(theme::text())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Pick a starter topology, reopen a saved workflow, or close the overlay.",
            Style::default().fg(theme::overlay_hint()),
        )),
        Line::from(vec![
            Span::styled("t", Style::default().fg(theme::blue())),
            Span::styled(":topology  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("o", Style::default().fg(theme::blue())),
            Span::styled(":saved  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("q", Style::default().fg(theme::blue())),
            Span::styled(":close", Style::default().fg(theme::overlay_hint())),
        ]),
    ])
    .alignment(Alignment::Center)
    .wrap(Wrap { trim: false });
    frame.render_widget(text, area);
}

#[allow(clippy::redundant_pub_crate)]
pub(crate) fn render_picker_panel(
    frame: &mut Frame,
    area: Rect,
    kind: GraphPickerKind,
    selected_index: usize,
    saved_workflows: &[Workflow],
    recursive_graphs: &[RecursiveTaskGraphSummary],
    list_state: &mut ListState,
) {
    // Centering: cap inner width at 100 cols; fall through to full width on
    // narrow terminals. Centered horizontally inside the supplied area.
    let max_width = 100u16.min(area.width);
    let pad = area.width.saturating_sub(max_width) / 2;
    let centered = Rect {
        x: area.x.saturating_add(pad),
        y: area.y,
        width: max_width,
        height: area.height,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::surface1()))
        .style(Style::default().bg(theme::overlay_bg()))
        .title(Span::styled(
            format!(" {} Picker ", kind.label()),
            Style::default().fg(theme::overlay_title()),
        ));
    let inner = block.inner(centered);
    frame.render_widget(block, centered);

    let items: Vec<ListItem> = match kind {
        GraphPickerKind::Topology => {
            let templates = starter_templates();
            if templates.is_empty() {
                vec![ListItem::new(Line::from(Span::styled(
                    "No starter templates available.",
                    Style::default().fg(theme::overlay_hint()),
                )))]
            } else {
                templates
                    .iter()
                    .enumerate()
                    .map(|(index, template)| {
                        let label_line = Line::from(vec![
                            Span::styled(
                                format!("{:>2}. ", index + 1),
                                Style::default().fg(theme::subtext0()),
                            ),
                            Span::styled(
                                template.label.to_string(),
                                Style::default()
                                    .fg(theme::text())
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                format!("  {}", truncate(template.description, 60)),
                                Style::default().fg(theme::overlay_hint()),
                            ),
                        ]);
                        ListItem::new(vec![label_line, Line::raw("")])
                    })
                    .collect()
            }
        }
        GraphPickerKind::SavedWorkflow => {
            if saved_workflows.is_empty() {
                vec![ListItem::new(Line::from(Span::styled(
                    "No saved workflows are loaded for this project.",
                    Style::default().fg(theme::overlay_hint()),
                )))]
            } else {
                saved_workflows
                    .iter()
                    .enumerate()
                    .map(|(index, workflow)| {
                        let title = truncate(&workflow.title, 60);
                        let stage = format!("[{:?}]", workflow.stage);
                        let label_line = Line::from(vec![
                            Span::styled(
                                format!("{:>3}. ", index + 1),
                                Style::default().fg(theme::subtext0()),
                            ),
                            Span::styled(
                                title,
                                Style::default()
                                    .fg(theme::text())
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                format!("  {stage}"),
                                Style::default().fg(theme::overlay_hint()),
                            ),
                        ]);
                        ListItem::new(vec![label_line, Line::raw("")])
                    })
                    .collect()
            }
        }
        GraphPickerKind::RecursiveGraphs => {
            if recursive_graphs.is_empty() {
                vec![ListItem::new(Line::from(Span::styled(
                    "No recursive graphs are available.",
                    Style::default().fg(theme::overlay_hint()),
                )))]
            } else {
                recursive_graphs
                    .iter()
                    .enumerate()
                    .map(|(index, summary)| {
                        let title = truncate(&summary.title, 60);
                        let status = format!("[{:?}]", summary.status);
                        let label_line = Line::from(vec![
                            Span::styled(
                                format!("{:>3}. ", index + 1),
                                Style::default().fg(theme::subtext0()),
                            ),
                            Span::styled(
                                title,
                                Style::default()
                                    .fg(theme::text())
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                format!("  {status}"),
                                Style::default().fg(theme::overlay_hint()),
                            ),
                        ]);
                        ListItem::new(vec![label_line, Line::raw("")])
                    })
                    .collect()
            }
        }
    };

    let list = List::new(items)
        .highlight_style(Style::default().fg(theme::text()).bg(theme::surface2()))
        .highlight_symbol("\u{25b8} ");

    list_state.select(Some(selected_index));
    frame.render_stateful_widget(list, inner, list_state);
}

/// Truncate `s` to at most `max` chars (NOT bytes), appending a U+2026
/// ellipsis when truncation occurred. Unicode-safe via char iteration.
#[allow(clippy::redundant_pub_crate)]
pub(crate) fn truncate(s: &str, max: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}\u{2026}")
    }
}

fn render_detail_panel(
    frame: &mut Frame,
    area: Rect,
    workflow: &WorkflowDefinition,
    selected_node: usize,
    mode: GraphMode,
    edit_buffer: &str,
    selected_edge: usize,
    execution: Option<&WorkflowExecutionSnapshot>,
    view_origin: &GraphViewOrigin,
) {
    let Some(node) = workflow.nodes.get(selected_node) else {
        return;
    };

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(theme::surface1()))
        .style(Style::default().bg(theme::overlay_bg()))
        .title(Span::styled(
            " Node Detail ",
            Style::default().fg(theme::overlay_title()),
        ));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let editing_field = mode.editing_field();
    // Determine node lock status for BridgedRecursive
    let (node_locked, lock_reason) = if let GraphViewOrigin::BridgedRecursive { .. } = view_origin {
        if let Some(locks_str) = workflow.metadata.get("recursive_node_locks")
            && let rsi_graph::data::Value::String(s) = locks_str
            && let Ok(locks) = serde_json::from_str::<serde_json::Value>(s)
            && let node_lock = &locks[&node.id]
            && !node_lock.is_null()
        {
            let locked = node_lock["locked"].as_bool().unwrap_or(true);
            let reason = node_lock["reason"].as_str().unwrap_or("locked").to_string();
            (locked, Some(reason))
        } else {
            (true, Some("locked (unknown)".to_string()))
        }
    } else {
        (false, None)
    };

    let name_read_only = mode.is_executing() || *view_origin != GraphViewOrigin::AuthoredWorkflow;
    let instructions_read_only = match view_origin {
        GraphViewOrigin::AuthoredWorkflow => mode.is_executing(),
        GraphViewOrigin::BridgedRecursive { .. } => node_locked,
        _ => true,
    };
    let read_only = instructions_read_only;

    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(vec![
        Span::styled("ID: ", Style::default().fg(theme::overlay_hint())),
        Span::styled(&node.id, Style::default().fg(theme::text())),
        Span::raw("  "),
        Span::styled("Type: ", Style::default().fg(theme::overlay_hint())),
        Span::styled(
            format!("{:?}", node.node_type),
            Style::default().fg(theme::blue()),
        ),
    ]));

    let name_line = if matches!(editing_field, Some(GraphEditField::Name)) {
        Line::from(vec![
            Span::styled("Name: ", Style::default().fg(theme::overlay_hint())),
            Span::styled(
                edit_buffer,
                Style::default().fg(theme::text()).bg(theme::surface2()),
            ),
            Span::styled("\u{2588}", Style::default().fg(theme::blue())),
        ])
    } else {
        let suffix = if name_read_only {
            "  [read-only]"
        } else {
            "  [i to edit]"
        };
        Line::from(vec![
            Span::styled("Name: ", Style::default().fg(theme::overlay_hint())),
            Span::styled(&node.name, Style::default().fg(theme::text())),
            Span::styled(suffix, Style::default().fg(theme::surface1())),
        ])
    };
    lines.push(name_line);

    // The node card renders each field as a single ratatui `Line`, which collapses
    // embedded newlines and jams the surrounding words together. Flatten newlines to
    // spaces for display only; the stored `node.instructions` keeps its multi-line
    // structure for the spawned worker's prompt.
    let instructions_display = if node.instructions.is_empty() {
        "(none)".to_string()
    } else {
        node.instructions
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let instructions_line = if matches!(editing_field, Some(GraphEditField::Instructions)) {
        Line::from(vec![
            Span::styled("Instructions: ", Style::default().fg(theme::overlay_hint())),
            Span::styled(
                edit_buffer,
                Style::default().fg(theme::text()).bg(theme::surface2()),
            ),
            Span::styled("\u{2588}", Style::default().fg(theme::blue())),
        ])
    } else {
        let suffix = if instructions_read_only {
            if let Some(reason) = &lock_reason {
                format!("  [locked: {}]", reason)
            } else {
                "  [read-only]".to_string()
            }
        } else {
            "  [I to edit]".to_string()
        };
        Line::from(vec![
            Span::styled("Instructions: ", Style::default().fg(theme::overlay_hint())),
            Span::styled(instructions_display, Style::default().fg(theme::text())),
            Span::styled(suffix, Style::default().fg(theme::surface1())),
        ])
    };
    lines.push(instructions_line);

    if let GraphViewOrigin::BridgedRecursive { .. } = view_origin {
        let (integration_val, verification_val) = if let Some(strat_str) =
            workflow.metadata.get("recursive_node_strategies")
            && let rsi_graph::data::Value::String(s) = strat_str
            && let Ok(strat) = serde_json::from_str::<serde_json::Value>(s)
            && let node_strat = &strat[&node.id]
            && !node_strat.is_null()
        {
            let integration = node_strat["integration_strategy"]
                .as_str()
                .map(ToString::to_string);
            let verification = node_strat["verification_strategy"]
                .as_str()
                .map(ToString::to_string);
            (integration, verification)
        } else {
            (None, None)
        };

        // Integration strategy
        let integration_line = if matches!(editing_field, Some(GraphEditField::IntegrationStrategy))
        {
            Line::from(vec![
                Span::styled(
                    "Integration Strategy: ",
                    Style::default().fg(theme::overlay_hint()),
                ),
                Span::styled(
                    edit_buffer,
                    Style::default().fg(theme::text()).bg(theme::surface2()),
                ),
                Span::styled("\u{2588}", Style::default().fg(theme::blue())),
            ])
        } else {
            let suffix = if instructions_read_only {
                "  [locked]"
            } else {
                "  [s to edit]"
            };
            let integration_display = integration_val.unwrap_or_else(|| "(none)".to_string());
            Line::from(vec![
                Span::styled(
                    "Integration Strategy: ",
                    Style::default().fg(theme::overlay_hint()),
                ),
                Span::styled(integration_display, Style::default().fg(theme::text())),
                Span::styled(suffix, Style::default().fg(theme::surface1())),
            ])
        };
        lines.push(integration_line);

        // Verification strategy
        let verification_line =
            if matches!(editing_field, Some(GraphEditField::VerificationStrategy)) {
                Line::from(vec![
                    Span::styled(
                        "Verification Strategy: ",
                        Style::default().fg(theme::overlay_hint()),
                    ),
                    Span::styled(
                        edit_buffer,
                        Style::default().fg(theme::text()).bg(theme::surface2()),
                    ),
                    Span::styled("\u{2588}", Style::default().fg(theme::blue())),
                ])
            } else {
                let suffix = if instructions_read_only {
                    "  [locked]"
                } else {
                    "  [S to edit]"
                };
                let verification_display = verification_val.unwrap_or_else(|| "(none)".to_string());
                Line::from(vec![
                    Span::styled(
                        "Verification Strategy: ",
                        Style::default().fg(theme::overlay_hint()),
                    ),
                    Span::styled(verification_display, Style::default().fg(theme::text())),
                    Span::styled(suffix, Style::default().fg(theme::surface1())),
                ])
            };
        lines.push(verification_line);
    }

    let node_edges: Vec<(usize, &EdgeDef)> = workflow
        .edges
        .iter()
        .enumerate()
        .filter(|(_, edge)| edge.source == node.id || edge.target == node.id)
        .collect();
    if !node_edges.is_empty() {
        lines.push(Line::from(Span::styled(
            "Edges:",
            Style::default().fg(theme::overlay_hint()),
        )));
        for (list_idx, (_global_idx, edge)) in node_edges.iter().enumerate() {
            let is_sel = list_idx == selected_edge;
            let arrow = if edge.source == node.id {
                format!("  \u{2192} {}", edge.target)
            } else {
                format!("  {} \u{2192}", edge.source)
            };
            let style = if is_sel {
                Style::default().fg(theme::text()).bg(theme::surface2())
            } else {
                Style::default().fg(theme::subtext0())
            };
            lines.push(Line::from(Span::styled(arrow, style)));
        }
    }

    let visual_edge_summaries = graph_node_visual_edge_summaries(workflow, &node.id);
    if !visual_edge_summaries.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("Visual: ", Style::default().fg(theme::overlay_hint())),
            Span::styled(
                visual_edge_summaries.join(" | "),
                Style::default().fg(theme::yellow()),
            ),
            Span::styled("  [canvas-only]", Style::default().fg(theme::surface1())),
        ]));
    }

    if let Some(execution) = execution {
        lines.push(Line::from(vec![
            Span::styled("Run: ", Style::default().fg(theme::overlay_hint())),
            render_execution_badge(execution),
        ]));
    }

    let hint_line = if editing_field.is_some() {
        Line::from(vec![
            Span::styled("Enter", Style::default().fg(theme::blue())),
            Span::styled(":save  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("Esc", Style::default().fg(theme::blue())),
            Span::styled(":cancel", Style::default().fg(theme::overlay_hint())),
        ])
    } else if read_only {
        Line::from(vec![
            Span::styled("j/k", Style::default().fg(theme::blue())),
            Span::styled(":edges  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("[/]", Style::default().fg(theme::blue())),
            Span::styled(":graph  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("x", Style::default().fg(theme::blue())),
            Span::styled(":interrupt  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("HJKL", Style::default().fg(theme::blue())),
            Span::styled(":pan  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("c", Style::default().fg(theme::blue())),
            Span::styled(":center  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("Esc", Style::default().fg(theme::blue())),
            Span::styled(":back  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("q", Style::default().fg(theme::blue())),
            Span::styled(":close", Style::default().fg(theme::overlay_hint())),
        ])
    } else {
        Line::from(vec![
            Span::styled("i", Style::default().fg(theme::blue())),
            Span::styled(":name  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("I", Style::default().fg(theme::blue())),
            Span::styled(":instr  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("j/k", Style::default().fg(theme::blue())),
            Span::styled(":edges  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("x", Style::default().fg(theme::blue())),
            Span::styled(":del edge  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("HJKL", Style::default().fg(theme::blue())),
            Span::styled(":pan  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("c", Style::default().fg(theme::blue())),
            Span::styled(":center  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("Esc", Style::default().fg(theme::blue())),
            Span::styled(":back", Style::default().fg(theme::overlay_hint())),
        ])
    };
    lines.push(hint_line);

    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_status_bar(
    frame: &mut Frame,
    area: Rect,
    workflow: &WorkflowDefinition,
    mode: GraphMode,
    viewport: GraphViewport,
    validation: &WorkflowValidationReport,
    execution: Option<&WorkflowExecutionSnapshot>,
    no_saved_workflows: bool,
) {
    let validity_line = if workflow.nodes.is_empty() {
        Line::from(vec![
            Span::styled("draft", Style::default().fg(theme::overlay_hint())),
            Span::styled(": empty", Style::default().fg(theme::surface1())),
            Span::raw("  "),
            Span::styled("saved", Style::default().fg(theme::overlay_hint())),
            Span::styled(
                if no_saved_workflows {
                    ": none loaded"
                } else {
                    ": available"
                },
                Style::default().fg(theme::surface1()),
            ),
        ])
    } else if validation.has_errors() {
        Line::from(vec![
            Span::styled("validation", Style::default().fg(theme::red())),
            Span::styled(
                format!(": {} blocking issue(s)", validation.error_count()),
                Style::default().fg(theme::surface1()),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled("validation", Style::default().fg(theme::green())),
            Span::styled(": executable", Style::default().fg(theme::surface1())),
        ])
    };

    let execution_line = if let Some(execution) = execution {
        Line::from(vec![
            Span::styled("run", Style::default().fg(theme::overlay_hint())),
            Span::styled(": ", Style::default().fg(theme::surface1())),
            render_execution_badge(execution),
            if execution.dry_run {
                Span::styled(" [dry-run]", Style::default().fg(theme::yellow()))
            } else {
                Span::raw("")
            },
        ])
    } else {
        Line::from(vec![
            Span::styled("mode", Style::default().fg(theme::overlay_hint())),
            Span::styled(
                format!(": {} / {}", mode.label(), camera_status(mode, viewport)),
                Style::default().fg(theme::surface1()),
            ),
        ])
    };

    let hint_line = match mode {
        GraphMode::Navigate { .. } if workflow.nodes.is_empty() => Line::from(vec![
            Span::styled("t", Style::default().fg(theme::blue())),
            Span::styled(":topology  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("o", Style::default().fg(theme::blue())),
            Span::styled(":saved  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("q", Style::default().fg(theme::blue())),
            Span::styled(":close", Style::default().fg(theme::overlay_hint())),
        ]),
        GraphMode::Picker { kind, .. } => {
            let label = match kind {
                GraphPickerKind::Topology => "select template",
                GraphPickerKind::SavedWorkflow => "open workflow",
                GraphPickerKind::RecursiveGraphs => "open recursive graph",
            };
            Line::from(vec![
                Span::styled("j/k", Style::default().fg(theme::blue())),
                Span::styled(":move  ", Style::default().fg(theme::overlay_hint())),
                Span::styled("Enter", Style::default().fg(theme::blue())),
                Span::styled(
                    format!(":{}  ", label),
                    Style::default().fg(theme::overlay_hint()),
                ),
                Span::styled("Esc", Style::default().fg(theme::blue())),
                Span::styled(":back", Style::default().fg(theme::overlay_hint())),
            ])
        }
        GraphMode::Executing { .. } => Line::from(vec![
            Span::styled("hjkl", Style::default().fg(theme::blue())),
            Span::styled(":move  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("HJKL", Style::default().fg(theme::blue())),
            Span::styled(":pan  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("c", Style::default().fg(theme::blue())),
            Span::styled(":center  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("[/]", Style::default().fg(theme::blue())),
            Span::styled(":graph  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("Enter", Style::default().fg(theme::blue())),
            Span::styled(":inspect  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("x", Style::default().fg(theme::blue())),
            Span::styled(":interrupt  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("q", Style::default().fg(theme::blue())),
            Span::styled(":close", Style::default().fg(theme::overlay_hint())),
        ]),
        _ => Line::from(vec![
            Span::styled("hjkl", Style::default().fg(theme::blue())),
            Span::styled(":move  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("HJKL", Style::default().fg(theme::blue())),
            Span::styled(":pan  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("c", Style::default().fg(theme::blue())),
            Span::styled(":center  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("[/]", Style::default().fg(theme::blue())),
            Span::styled(":graph  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("Enter", Style::default().fg(theme::blue())),
            Span::styled(":detail  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("t", Style::default().fg(theme::blue())),
            Span::styled(":topology  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("o", Style::default().fg(theme::blue())),
            Span::styled(":saved  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("r", Style::default().fg(theme::blue())),
            Span::styled(":run  ", Style::default().fg(theme::overlay_hint())),
            Span::styled("q", Style::default().fg(theme::blue())),
            Span::styled(":close", Style::default().fg(theme::overlay_hint())),
        ]),
    };

    frame.render_widget(
        Paragraph::new(vec![validity_line, execution_line, hint_line]).wrap(Wrap { trim: false }),
        area,
    );
}

fn camera_status(mode: GraphMode, viewport: GraphViewport) -> String {
    match mode.camera() {
        crate::types::GraphCamera::FollowSelection => "follow".to_string(),
        crate::types::GraphCamera::Manual if !viewport.is_centered() => {
            format!("manual {:+},{:+}", viewport.offset_x, viewport.offset_y)
        }
        crate::types::GraphCamera::Manual => "manual".to_string(),
    }
}

pub fn render_missing_graph_review(frame: &mut Frame, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::overlay_border()))
        .title(Span::styled(
            " Graph Review ",
            Style::default()
                .fg(theme::overlay_title())
                .add_modifier(Modifier::BOLD),
        ))
        .padding(Padding::new(1, 1, 1, 1));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new("Graph draft is unavailable. Close and reopen `gv` to resync.")
            .style(Style::default().fg(theme::overlay_hint()))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        GraphCamera, GraphDraftPersistenceState, RecursiveDagPanel, RecursiveDagSelectedGraphData,
    };
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
    use rsi_common::RecursiveTaskGraphDetail;
    use rsi_graph::generate::templates::build_starter_workflow;

    fn test_app() -> crate::app::App {
        let mut app = crate::app::App::new(crate::client::DaemonClient::new(
            std::path::PathBuf::from("/tmp/test.sock"),
        ));
        app.poll.connected = true;
        app
    }

    fn render_graph_review_lines(
        workflow: &WorkflowDefinition,
        mode: GraphMode,
        selected_node: usize,
    ) -> Vec<String> {
        render_graph_review_lines_with_origin(
            workflow,
            mode,
            selected_node,
            &GraphViewOrigin::AuthoredWorkflow,
        )
    }

    fn render_graph_review_lines_with_origin(
        workflow: &WorkflowDefinition,
        mode: GraphMode,
        selected_node: usize,
        view_origin: &GraphViewOrigin,
    ) -> Vec<String> {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).expect("test terminal should initialize");
        let mut picker_state = ListState::default();
        let app = test_app();
        terminal
            .draw(|frame| {
                render_graph_review(
                    frame,
                    frame.area(),
                    workflow,
                    GraphDraftPersistenceState::Clean,
                    mode,
                    GraphViewport::default(),
                    selected_node,
                    &HashSet::new(),
                    "",
                    0,
                    None,
                    &[],
                    &[],
                    view_origin,
                    &mut picker_state,
                    false,
                    false,
                    None,
                    &app,
                );
            })
            .expect("graph review should render");

        buffer_lines(terminal.backend().buffer())
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
    fn detail_panel_surfaces_visual_edges_for_selected_node() {
        let workflow = build_starter_workflow("pingpong").expect("template should exist");
        let selected_node = workflow
            .nodes
            .iter()
            .position(|node| node.id == "revision")
            .expect("revision node should exist");

        let lines = render_graph_review_lines(
            &workflow,
            GraphMode::Detail {
                camera: GraphCamera::FollowSelection,
            },
            selected_node,
        );
        let rendered = lines.join("\n");

        assert!(rendered.contains("Visual:"));
        assert!(rendered.contains("loop revision -> critique [iterate]"));
        assert!(rendered.contains("[canvas-only]"));
    }

    #[test]
    fn gv_render_byte_identical_for_authored_workflow() {
        let workflow = build_starter_workflow("pingpong").expect("template should exist");
        let mode = GraphMode::Detail {
            camera: GraphCamera::FollowSelection,
        };

        // The pre-V2 baseline == the default-origin render. The new `view_origin`
        // param is a strict no-op for `AuthoredWorkflow`.
        let baseline = render_graph_review_lines(&workflow, mode, 0);
        let with_authored = render_graph_review_lines_with_origin(
            &workflow,
            mode,
            0,
            &GraphViewOrigin::AuthoredWorkflow,
        );
        assert_eq!(baseline, with_authored);

        // No honesty badge for authored drafts; detail panel offers editing.
        let rendered = baseline.join("\n");
        assert!(!rendered.contains("recursive (read-only)"));
        assert!(rendered.contains("[i to edit]"));
    }

    #[test]
    fn gv_render_badges_recursive_read_only_and_detail_panel_honest() {
        let mut workflow = build_starter_workflow("pingpong").expect("template should exist");
        let mode = GraphMode::Detail {
            camera: GraphCamera::FollowSelection,
        };
        let origin = GraphViewOrigin::BridgedRecursive {
            recursive_graph_id: uuid::Uuid::new_v4(),
        };

        // Add recursive_node_locks to workflow metadata. Make node 0 locked, node 1 editable.
        let node_id_0 = &workflow.nodes[0].id;
        let node_id_1 = &workflow.nodes[1].id;
        let locks = serde_json::json!({
            node_id_0: { "locked": true, "reason": "planning" },
            node_id_1: { "locked": false, "reason": "" }
        });
        workflow.metadata.insert(
            "recursive_node_locks".to_string(),
            rsi_graph::data::Value::String(serde_json::to_string(&locks).unwrap()),
        );

        // Node 0 is selected -> should show locked
        let lines_0 = render_graph_review_lines_with_origin(&workflow, mode, 0, &origin);
        let rendered_0 = lines_0.join("\n");
        assert!(rendered_0.contains("recursive [locked: planning]"));
        assert!(rendered_0.contains("[locked: planning]"));

        // Node 1 is selected -> should show editable
        let lines_1 = render_graph_review_lines_with_origin(&workflow, mode, 1, &origin);
        let rendered_1 = lines_1.join("\n");
        assert!(rendered_1.contains("recursive [editable]"));
        assert!(rendered_1.contains("[I to edit]"));
    }

    #[test]
    fn gv_info_dashboard_disabled_is_byte_identical_to_baseline() {
        let workflow = build_starter_workflow("pingpong").expect("template should exist");
        let mode = GraphMode::Detail {
            camera: GraphCamera::FollowSelection,
        };
        let app = test_app();

        // Render with gv_info_dashboard = false
        let backend_false = TestBackend::new(120, 32);
        let mut terminal_false =
            Terminal::new(backend_false).expect("test terminal should initialize");
        let mut picker_state_false = ListState::default();
        terminal_false
            .draw(|frame| {
                render_graph_review(
                    frame,
                    frame.area(),
                    &workflow,
                    GraphDraftPersistenceState::Clean,
                    mode,
                    GraphViewport::default(),
                    0,
                    &HashSet::new(),
                    "",
                    0,
                    None,
                    &[],
                    &[],
                    &GraphViewOrigin::AuthoredWorkflow,
                    &mut picker_state_false,
                    false,
                    false,
                    None,
                    &app,
                );
            })
            .expect("render should succeed");
        let lines_false = buffer_lines(terminal_false.backend().buffer());

        // Render with the helper (which uses false)
        let lines_helper = render_graph_review_lines(&workflow, mode, 0);

        assert_eq!(lines_false, lines_helper);
    }

    #[test]
    fn gv_info_dashboard_enabled_renders_split_scaffold() {
        let workflow = build_starter_workflow("pingpong").expect("template should exist");
        let mode = GraphMode::Detail {
            camera: GraphCamera::FollowSelection,
        };
        let app = test_app();

        // Render with gv_info_dashboard = true
        let backend_true = TestBackend::new(120, 32);
        let mut terminal_true =
            Terminal::new(backend_true).expect("test terminal should initialize");
        let mut picker_state_true = ListState::default();
        terminal_true
            .draw(|frame| {
                render_graph_review(
                    frame,
                    frame.area(),
                    &workflow,
                    GraphDraftPersistenceState::Clean,
                    mode,
                    GraphViewport::default(),
                    0,
                    &HashSet::new(),
                    "",
                    0,
                    None,
                    &[],
                    &[],
                    &GraphViewOrigin::BridgedRecursive {
                        recursive_graph_id: uuid::Uuid::new_v4(),
                    },
                    &mut picker_state_true,
                    true,
                    false,
                    None,
                    &app,
                );
            })
            .expect("render should succeed");
        let lines_true = buffer_lines(terminal_true.backend().buffer());
        let rendered_true = lines_true.join("\n");

        // The dashboard split should render the left border separator
        assert!(rendered_true.contains("│"));
    }

    #[test]
    fn gv_info_dashboard_renders_sections_when_bridged_recursive() {
        use rsi_common::recursive_dag::RecursiveTaskGraphSummary;
        let workflow = build_starter_workflow("pingpong").expect("template should exist");
        let mode = GraphMode::Detail {
            camera: GraphCamera::FollowSelection,
        };
        let origin = GraphViewOrigin::BridgedRecursive {
            recursive_graph_id: uuid::Uuid::new_v4(),
        };

        let mut app = test_app();
        let caps = rsi_common::rpc::DaemonCapabilities {
            recursive_dag_inspection: true,
            recursive_dag_run_inspection: true,
            recursive_dag_recovery_status: true,
            recursive_dag_live_status_inspection: true,
            ..Default::default()
        };

        let graph_id = rsi_common::RecursiveTaskGraphId(uuid::Uuid::new_v4());
        let root_task_id = rsi_common::RecursiveTaskId(uuid::Uuid::new_v4());

        let graph_summary = RecursiveTaskGraphSummary {
            id: graph_id,
            root_task_id,
            title: "test graph".to_string(),
            objective: "test objective".to_string(),
            status: rsi_common::RecursiveGraphStatus::Active,
            project_id: None,
            workflow_id: None,
            topology_id: None,
            parent_session_id: None,
            source_execution_id: None,
            source_eval_id: None,
            execution_mode: rsi_common::RecursiveExecutionMode::Fake,
            max_depth: 4,
            max_fanout: 4,
            max_descendants: 16,
            step_limit: 32,
            last_stop_reason: None,
            malformed_reason: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            recovered_at: None,
            quarantined_at: None,
            quarantine_reason: None,
            recovery_checked_at: None,
        };

        let detail = RecursiveDagSelectedGraphData {
            graph: RecursiveTaskGraphDetail {
                graph: graph_summary.clone(),
                nodes: Vec::new(),
                edges: Vec::new(),
                attempts: Vec::new(),
                injection_batches: Vec::new(),
                lifecycle_events: Vec::new(),
                artifacts: Vec::new(),
            },
            operational_status: None,
            scheduler_runs: Vec::new(),
            selected_run_detail: None,
            run_events: Vec::new(),
            cancellation_requests: Vec::new(),
            recovery_status: None,
            live_attempts: Vec::new(),
            stale_heartbeats: Vec::new(),
            live_recovery_status: None,
            validation_results: Vec::new(),
            artifact_summary_page: crate::types::RecursiveDagArtifactSummaryPageState::ready(
                graph_id,
                Vec::new(),
                crate::types::RECURSIVE_DAG_ARTIFACT_LIMIT as u32,
                None,
                false,
                Vec::new(),
            ),
            warnings: Vec::new(),
        };

        let mut browser_state = RecursiveDagBrowserState::ready(
            None,
            caps,
            vec![graph_summary],
            Some(graph_id),
            Some(detail),
        );
        browser_state.panel = RecursiveDagPanel::Runs;

        let backend = TestBackend::new(120, 80);
        let mut terminal = Terminal::new(backend).expect("test terminal should initialize");
        let mut picker_state = ListState::default();

        terminal
            .draw(|frame| {
                render_graph_review(
                    frame,
                    frame.area(),
                    &workflow,
                    GraphDraftPersistenceState::Clean,
                    mode,
                    GraphViewport::default(),
                    0,
                    &HashSet::new(),
                    "",
                    0,
                    None,
                    &[],
                    &[],
                    &origin,
                    &mut picker_state,
                    true,
                    true, // dashboard_focused
                    Some(&browser_state),
                    &app,
                );
            })
            .expect("render should succeed");

        let lines = buffer_lines(terminal.backend().buffer());
        let rendered = lines.join("\n");

        assert!(rendered.contains("INFO DASHBOARD"));
        assert!(rendered.contains("SCHEDULER RUNS"));
        assert!(rendered.contains("RECOVERY / DEFERRED"));
        assert!(rendered.contains("CANCELLATION REQUESTS"));
        assert!(rendered.contains("HEARTBEATS / LEASES"));
        assert!(rendered.contains("LIVE INTERRUPTS"));
    }
}

fn render_execution_badge(execution: &WorkflowExecutionSnapshot) -> Span<'static> {
    match execution.status {
        WorkflowExecutionStatus::Accepted => {
            Span::styled("[accepted]", Style::default().fg(theme::blue()))
        }
        WorkflowExecutionStatus::Running => {
            Span::styled("[running]", Style::default().fg(theme::green()))
        }
        WorkflowExecutionStatus::Succeeded => {
            Span::styled("[succeeded]", Style::default().fg(theme::green()))
        }
        WorkflowExecutionStatus::Failed => {
            Span::styled("[failed]", Style::default().fg(theme::red()))
        }
        WorkflowExecutionStatus::Interrupted => {
            Span::styled("[interrupted]", Style::default().fg(theme::yellow()))
        }
        WorkflowExecutionStatus::Blocked => Span::styled(
            "[blocked: :topology-resolve]",
            Style::default().fg(theme::yellow()),
        ),
        _ => Span::styled("[unknown]", Style::default().fg(theme::overlay_hint())),
    }
}

fn render_persistence_badge(persistence_state: GraphDraftPersistenceState) -> Span<'static> {
    match persistence_state {
        GraphDraftPersistenceState::Clean => {
            Span::styled("[clean]", Style::default().fg(theme::overlay_hint()))
        }
        GraphDraftPersistenceState::Dirty => {
            Span::styled("[dirty]", Style::default().fg(theme::yellow()))
        }
        GraphDraftPersistenceState::Saving => {
            Span::styled("[saving]", Style::default().fg(theme::blue()))
        }
        GraphDraftPersistenceState::SaveFailed => {
            Span::styled("[save failed]", Style::default().fg(theme::red()))
        }
    }
}
