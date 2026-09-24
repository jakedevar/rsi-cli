//! Deterministic virtual layout and routing for the graph review overlay.

use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::types::{GraphCamera, GraphViewport};
use rsi_graph::format::{GraphViewVisualEdgeKind, WorkflowDefinition};

pub(crate) const NODE_BOX_HEIGHT: i32 = 3;
pub(crate) const NODE_LABEL_MIN_WIDTH: usize = 14;
pub(crate) const NODE_LABEL_MAX_WIDTH: usize = 28;
pub(crate) const MIN_GRAPH_CANVAS_WIDTH: u16 = 32;
pub(crate) const MIN_GRAPH_CANVAS_HEIGHT: u16 = 8;
pub(crate) const HORIZONTAL_PAN_STEP: i32 = 12;
pub(crate) const VERTICAL_PAN_STEP: i32 = 4;

const COLUMN_GAP: i32 = 12;
const ROW_GAP: i32 = 4;
const COMPONENT_GAP_Y: i32 = 6;
const LANE_STEP: i32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct VirtualPoint {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct VirtualRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl VirtualRect {
    pub(crate) fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub(crate) fn right(self) -> i32 {
        self.x + self.width - 1
    }

    pub(crate) fn bottom(self) -> i32 {
        self.y + self.height - 1
    }

    pub(crate) fn center_x(self) -> i32 {
        self.x + self.width / 2
    }

    pub(crate) fn center_y(self) -> i32 {
        self.y + self.height / 2
    }

    pub(crate) fn from_points(a: VirtualPoint, b: VirtualPoint) -> Self {
        let min_x = a.x.min(b.x);
        let max_x = a.x.max(b.x);
        let min_y = a.y.min(b.y);
        let max_y = a.y.max(b.y);
        Self::new(min_x, min_y, max_x - min_x + 1, max_y - min_y + 1)
    }

    pub(crate) fn include_point(&mut self, point: VirtualPoint) {
        let other = Self::from_points(point, point);
        self.expand_to_include(other);
    }

    pub(crate) fn expand_to_include(&mut self, other: Self) {
        if self.width <= 0 || self.height <= 0 {
            *self = other;
            return;
        }

        let min_x = self.x.min(other.x);
        let min_y = self.y.min(other.y);
        let max_x = self.right().max(other.right());
        let max_y = self.bottom().max(other.bottom());
        self.x = min_x;
        self.y = min_y;
        self.width = max_x - min_x + 1;
        self.height = max_y - min_y + 1;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphCanvasEdgeKind {
    Executable,
    VisualLoopArrow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnchorSide {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EdgeOrientation {
    Forward,
    Backward,
    Vertical,
}

#[derive(Debug, Clone)]
pub(crate) struct GraphNodeLayout {
    pub node_index: usize,
    pub rect: VirtualRect,
    pub inner_width: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OrthogonalSegment {
    pub start: VirtualPoint,
    pub end: VirtualPoint,
}

impl OrthogonalSegment {
    pub(crate) fn is_horizontal(self) -> bool {
        self.start.y == self.end.y
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ArrowHead {
    pub position: VirtualPoint,
    pub glyph: char,
}

#[derive(Debug, Clone)]
pub(crate) struct GraphEdgeRoute {
    pub kind: GraphCanvasEdgeKind,
    pub label: Option<String>,
    pub segments: Vec<OrthogonalSegment>,
    pub arrow: ArrowHead,
}

#[derive(Debug, Clone)]
pub(crate) struct GraphLayout {
    pub bounds: VirtualRect,
    pub nodes: Vec<GraphNodeLayout>,
    pub edges: Vec<GraphEdgeRoute>,
    pub has_cycle_edges: bool,
}

impl GraphLayout {
    pub(crate) fn node(&self, node_index: usize) -> Option<&GraphNodeLayout> {
        self.nodes.get(node_index)
    }
}

#[derive(Debug, Clone)]
struct StableLayers {
    layers: Vec<Vec<usize>>,
    cycle_broken: bool,
}

#[derive(Debug, Clone)]
struct ResolvedEdge {
    source_index: usize,
    source_port: Option<String>,
    target_index: usize,
    target_port: Option<String>,
    label: Option<String>,
    kind: GraphCanvasEdgeKind,
    declaration_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct EdgeGroupKey {
    source_index: usize,
    source_port: Option<String>,
    target_index: usize,
    target_port: Option<String>,
}

pub(crate) fn layout_workflow_graph(workflow: &WorkflowDefinition) -> GraphLayout {
    let node_count = workflow.nodes.len();
    if node_count == 0 {
        return GraphLayout {
            bounds: VirtualRect::default(),
            nodes: Vec::new(),
            edges: Vec::new(),
            has_cycle_edges: false,
        };
    }

    let node_index_by_id: HashMap<&str, usize> = workflow
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.as_str(), index))
        .collect();

    let executable_edges: Vec<ResolvedEdge> = workflow
        .edges
        .iter()
        .enumerate()
        .filter_map(|(index, edge)| {
            Some(ResolvedEdge {
                source_index: *node_index_by_id.get(edge.source.as_str())?,
                source_port: edge.source_port.clone(),
                target_index: *node_index_by_id.get(edge.target.as_str())?,
                target_port: edge.target_port.clone(),
                label: edge.label.clone(),
                kind: GraphCanvasEdgeKind::Executable,
                declaration_index: index,
            })
        })
        .collect();

    let visual_edges: Vec<ResolvedEdge> = workflow
        .graph_view_metadata()
        .ok()
        .flatten()
        .map(|graph_view| {
            graph_view
                .visual_edges
                .into_iter()
                .enumerate()
                .filter_map(|(index, edge)| {
                    let kind = match edge.kind {
                        GraphViewVisualEdgeKind::LoopArrow => GraphCanvasEdgeKind::VisualLoopArrow,
                    };
                    Some(ResolvedEdge {
                        source_index: *node_index_by_id.get(edge.source.as_str())?,
                        source_port: edge.source_port,
                        target_index: *node_index_by_id.get(edge.target.as_str())?,
                        target_port: edge.target_port,
                        label: edge.label,
                        kind,
                        declaration_index: workflow.edges.len() + index,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let adjacency = weak_adjacency(node_count, &executable_edges, &visual_edges);
    let components = weakly_connected_components(node_count, &adjacency);

    let mut final_nodes: Vec<Option<GraphNodeLayout>> = vec![None; node_count];
    let mut final_edges: Vec<GraphEdgeRoute> = Vec::new();
    let mut overall_bounds: Option<VirtualRect> = None;
    let mut component_min_y = 0;
    let mut has_cycle_edges = false;

    for component in components {
        let component_exec: Vec<&ResolvedEdge> = executable_edges
            .iter()
            .filter(|edge| {
                component.contains(&edge.source_index) && component.contains(&edge.target_index)
            })
            .collect();
        let component_visual: Vec<&ResolvedEdge> = visual_edges
            .iter()
            .filter(|edge| {
                component.contains(&edge.source_index) && component.contains(&edge.target_index)
            })
            .collect();

        let ordering_edges = if component_exec.is_empty() {
            component_visual.clone()
        } else {
            component_exec.clone()
        };
        let layering = stable_layers(&component, &ordering_edges);
        if layering.cycle_broken && !component_exec.is_empty() {
            has_cycle_edges = true;
        }

        let mut component_nodes = layout_component_nodes(workflow, &layering, component_min_y);
        let mut component_edges = route_component_edges(
            workflow,
            &component_nodes,
            &component_exec,
            &component_visual,
        );
        let mut component_bounds = component_bounds(&component_nodes, &component_edges);
        if component_bounds.y < component_min_y {
            let delta = component_min_y - component_bounds.y;
            shift_component(&mut component_nodes, &mut component_edges, delta);
            component_bounds.y += delta;
            component_bounds.height = component_bounds.height.max(1);
        }

        component_min_y = component_bounds.bottom() + 1 + COMPONENT_GAP_Y;
        overall_bounds = Some(match overall_bounds {
            Some(mut bounds) => {
                bounds.expand_to_include(component_bounds);
                bounds
            }
            None => component_bounds,
        });

        for node in component_nodes {
            let node_index = node.node_index;
            final_nodes[node_index] = Some(node);
        }
        final_edges.extend(component_edges);
    }

    GraphLayout {
        bounds: overall_bounds.unwrap_or_default(),
        nodes: final_nodes
            .into_iter()
            .enumerate()
            .map(|(index, node)| {
                node.unwrap_or_else(|| GraphNodeLayout {
                    node_index: index,
                    rect: VirtualRect::default(),
                    inner_width: NODE_LABEL_MIN_WIDTH,
                })
            })
            .collect(),
        edges: final_edges,
        has_cycle_edges,
    }
}

pub(crate) fn viewport_origin(
    layout: &GraphLayout,
    viewport_width: u16,
    viewport_height: u16,
    selected_node: usize,
    camera: GraphCamera,
    viewport: GraphViewport,
) -> VirtualPoint {
    if layout.nodes.is_empty() || viewport_width == 0 || viewport_height == 0 {
        return VirtualPoint::default();
    }

    let follow_origin =
        follow_selection_origin(layout, viewport_width, viewport_height, selected_node);
    let desired = match camera {
        GraphCamera::FollowSelection => follow_origin,
        GraphCamera::Manual => VirtualPoint {
            x: follow_origin.x + viewport.offset_x,
            y: follow_origin.y + viewport.offset_y,
        },
    };
    clamp_origin(layout.bounds, viewport_width, viewport_height, desired)
}

pub(crate) fn find_directional_neighbor(
    layout: &GraphLayout,
    current_node: usize,
    direction: GraphDirection,
) -> Option<usize> {
    let current = layout.node(current_node)?;
    let current_center = VirtualPoint {
        x: current.rect.center_x(),
        y: current.rect.center_y(),
    };

    layout
        .nodes
        .iter()
        .filter(|candidate| candidate.node_index != current_node)
        .filter_map(|candidate| {
            let center = VirtualPoint {
                x: candidate.rect.center_x(),
                y: candidate.rect.center_y(),
            };

            let dx = center.x - current_center.x;
            let dy = center.y - current_center.y;
            let key = match direction {
                GraphDirection::Left if dx < 0 => (dy.abs(), dx.abs(), candidate.node_index),
                GraphDirection::Right if dx > 0 => (dy.abs(), dx.abs(), candidate.node_index),
                GraphDirection::Up if dy < 0 => (dx.abs(), dy.abs(), candidate.node_index),
                GraphDirection::Down if dy > 0 => (dx.abs(), dy.abs(), candidate.node_index),
                _ => return None,
            };

            Some((key, candidate.node_index))
        })
        .min_by_key(|(key, _)| *key)
        .map(|(_, node_index)| node_index)
}

pub(crate) fn node_outer_width(title: &str) -> i32 {
    let title_len = title
        .chars()
        .count()
        .clamp(NODE_LABEL_MIN_WIDTH, NODE_LABEL_MAX_WIDTH);
    (title_len + 2) as i32
}

fn weak_adjacency(
    node_count: usize,
    executable_edges: &[ResolvedEdge],
    visual_edges: &[ResolvedEdge],
) -> Vec<Vec<usize>> {
    let mut adjacency = vec![Vec::new(); node_count];
    for edge in executable_edges.iter().chain(visual_edges.iter()) {
        adjacency[edge.source_index].push(edge.target_index);
        adjacency[edge.target_index].push(edge.source_index);
    }
    for neighbors in &mut adjacency {
        neighbors.sort_unstable();
        neighbors.dedup();
    }
    adjacency
}

fn weakly_connected_components(node_count: usize, adjacency: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut seen = vec![false; node_count];
    let mut components = Vec::new();

    for root in 0..node_count {
        if seen[root] {
            continue;
        }

        let mut queue = VecDeque::from([root]);
        let mut component = Vec::new();
        seen[root] = true;
        while let Some(node) = queue.pop_front() {
            component.push(node);
            for &neighbor in &adjacency[node] {
                if seen[neighbor] {
                    continue;
                }
                seen[neighbor] = true;
                queue.push_back(neighbor);
            }
        }

        component.sort_unstable();
        components.push(component);
    }

    components
}

fn stable_layers(component_nodes: &[usize], edges: &[&ResolvedEdge]) -> StableLayers {
    let mut incoming_counts: HashMap<usize, usize> = component_nodes
        .iter()
        .copied()
        .map(|node_index| (node_index, 0usize))
        .collect();
    let mut outgoing: HashMap<usize, Vec<usize>> = component_nodes
        .iter()
        .copied()
        .map(|node_index| (node_index, Vec::new()))
        .collect();

    for edge in edges {
        *incoming_counts.entry(edge.target_index).or_default() += 1;
        outgoing
            .entry(edge.source_index)
            .or_default()
            .push(edge.target_index);
    }

    for targets in outgoing.values_mut() {
        targets.sort_unstable();
    }

    let mut remaining = component_nodes.to_vec();
    let mut remaining_flags = vec![false; remaining.iter().copied().max().unwrap_or(0) + 1];
    for &node in &remaining {
        remaining_flags[node] = true;
    }

    let mut layers = Vec::new();
    let mut cycle_broken = false;
    while !remaining.is_empty() {
        let mut next_layer: Vec<usize> = remaining
            .iter()
            .copied()
            .filter(|node_index| incoming_counts.get(node_index).copied().unwrap_or(0) == 0)
            .collect();

        if next_layer.is_empty() {
            next_layer.push(remaining[0]);
            cycle_broken = true;
        }

        next_layer.sort_unstable();

        for &node_index in &next_layer {
            remaining_flags[node_index] = false;
        }
        for &node_index in &next_layer {
            if let Some(targets) = outgoing.get(&node_index) {
                for &target_index in targets {
                    if remaining_flags.get(target_index).copied().unwrap_or(false)
                        && let Some(incoming) = incoming_counts.get_mut(&target_index)
                    {
                        *incoming = incoming.saturating_sub(1);
                    }
                }
            }
        }

        remaining.retain(|node_index| !next_layer.contains(node_index));
        layers.push(next_layer);
    }

    StableLayers {
        layers,
        cycle_broken,
    }
}

fn layout_component_nodes(
    workflow: &WorkflowDefinition,
    layering: &StableLayers,
    y_origin: i32,
) -> Vec<GraphNodeLayout> {
    let row_step = NODE_BOX_HEIGHT + ROW_GAP;
    let component_rows = layering.layers.iter().map(Vec::len).max().unwrap_or(1);
    let mut layer_widths: Vec<i32> = layering
        .layers
        .iter()
        .map(|layer| {
            layer
                .iter()
                .map(|&node_index| node_outer_width(workflow.nodes[node_index].name.as_str()))
                .max()
                .unwrap_or(node_outer_width(""))
        })
        .collect();
    if layer_widths.is_empty() {
        layer_widths.push(node_outer_width(""));
    }

    let mut nodes = Vec::new();
    let mut x_cursor = 0;
    for (layer_index, layer_nodes) in layering.layers.iter().enumerate() {
        let layer_width = layer_widths[layer_index];
        let slot_offset = (component_rows.saturating_sub(layer_nodes.len())) / 2;

        for (row_in_layer, &node_index) in layer_nodes.iter().enumerate() {
            let width = node_outer_width(workflow.nodes[node_index].name.as_str());
            let x = x_cursor + (layer_width - width) / 2;
            let y = y_origin + (slot_offset + row_in_layer) as i32 * row_step;
            nodes.push(GraphNodeLayout {
                node_index,
                rect: VirtualRect::new(x, y, width, NODE_BOX_HEIGHT),
                inner_width: (width - 2).max(1) as usize,
            });
        }

        x_cursor += layer_width + COLUMN_GAP;
    }

    nodes.sort_by_key(|node| node.node_index);
    nodes
}

fn route_component_edges(
    workflow: &WorkflowDefinition,
    nodes: &[GraphNodeLayout],
    executable_edges: &[&ResolvedEdge],
    visual_edges: &[&ResolvedEdge],
) -> Vec<GraphEdgeRoute> {
    let node_map: HashMap<usize, &GraphNodeLayout> =
        nodes.iter().map(|node| (node.node_index, node)).collect();
    let mut ordered_edges: Vec<&ResolvedEdge> = executable_edges
        .iter()
        .chain(visual_edges.iter())
        .copied()
        .collect();
    ordered_edges.sort_by_key(|edge| edge.declaration_index);

    let mut lane_counters: BTreeMap<EdgeGroupKey, usize> = BTreeMap::new();
    ordered_edges
        .into_iter()
        .filter_map(|edge| {
            let source = node_map.get(&edge.source_index)?;
            let target = node_map.get(&edge.target_index)?;
            let group_key = EdgeGroupKey {
                source_index: edge.source_index,
                source_port: edge.source_port.clone(),
                target_index: edge.target_index,
                target_port: edge.target_port.clone(),
            };
            let lane_index = lane_counters.entry(group_key).or_default();
            let route = route_edge(workflow, edge, source, target, *lane_index);
            *lane_index += 1;
            Some(route)
        })
        .collect()
}

fn route_edge(
    workflow: &WorkflowDefinition,
    edge: &ResolvedEdge,
    source: &GraphNodeLayout,
    target: &GraphNodeLayout,
    lane_index: usize,
) -> GraphEdgeRoute {
    let orientation = edge_orientation(source, target);
    let source_anchor_side = match orientation {
        EdgeOrientation::Forward | EdgeOrientation::Backward => AnchorSide::Right,
        EdgeOrientation::Vertical => {
            if target.rect.center_y() > source.rect.center_y() {
                AnchorSide::Bottom
            } else {
                AnchorSide::Top
            }
        }
    };
    let target_anchor_side = match orientation {
        EdgeOrientation::Forward | EdgeOrientation::Backward => AnchorSide::Left,
        EdgeOrientation::Vertical => {
            if target.rect.center_y() > source.rect.center_y() {
                AnchorSide::Top
            } else {
                AnchorSide::Bottom
            }
        }
    };

    let source_anchor = anchor_point(
        workflow,
        source,
        true,
        edge.source_port.as_deref(),
        source_anchor_side,
    );
    let target_anchor = anchor_point(
        workflow,
        target,
        false,
        edge.target_port.as_deref(),
        target_anchor_side,
    );

    let points = match orientation {
        EdgeOrientation::Forward => {
            let base_lane_x = source_anchor.x + 2 + lane_index as i32 * LANE_STEP;
            let lane_x = base_lane_x.min(target_anchor.x.saturating_sub(1));
            vec![
                source_anchor,
                VirtualPoint {
                    x: lane_x,
                    y: source_anchor.y,
                },
                VirtualPoint {
                    x: lane_x,
                    y: target_anchor.y,
                },
                target_anchor,
            ]
        }
        EdgeOrientation::Backward => {
            let source_out_x = source.rect.right() + 2 + lane_index as i32 * LANE_STEP;
            let target_in_x = target.rect.x - 2 - lane_index as i32 * LANE_STEP;
            let lane_y = source.rect.y.min(target.rect.y) - 2 - lane_index as i32 * LANE_STEP;
            vec![
                source_anchor,
                VirtualPoint {
                    x: source_out_x,
                    y: source_anchor.y,
                },
                VirtualPoint {
                    x: source_out_x,
                    y: lane_y,
                },
                VirtualPoint {
                    x: target_in_x,
                    y: lane_y,
                },
                VirtualPoint {
                    x: target_in_x,
                    y: target_anchor.y,
                },
                target_anchor,
            ]
        }
        EdgeOrientation::Vertical => {
            let downward = target.rect.center_y() > source.rect.center_y();
            let lane_x =
                source.rect.right().max(target.rect.right()) + 2 + lane_index as i32 * LANE_STEP;
            let source_exit_y = if downward {
                source_anchor.y + 1
            } else {
                source_anchor.y - 1
            };
            let target_entry_y = if downward {
                target_anchor.y - 1
            } else {
                target_anchor.y + 1
            };
            vec![
                source_anchor,
                VirtualPoint {
                    x: source_anchor.x,
                    y: source_exit_y,
                },
                VirtualPoint {
                    x: lane_x,
                    y: source_exit_y,
                },
                VirtualPoint {
                    x: lane_x,
                    y: target_entry_y,
                },
                VirtualPoint {
                    x: target_anchor.x,
                    y: target_entry_y,
                },
                target_anchor,
            ]
        }
    };

    let segments = segments_from_points(&points);
    let final_segment = segments.last().copied().unwrap_or(OrthogonalSegment {
        start: target_anchor,
        end: target_anchor,
    });
    let arrow = ArrowHead {
        position: target_anchor,
        glyph: arrow_glyph(final_segment),
    };

    GraphEdgeRoute {
        kind: edge.kind,
        label: edge.label.clone(),
        segments,
        arrow,
    }
}

fn edge_orientation(source: &GraphNodeLayout, target: &GraphNodeLayout) -> EdgeOrientation {
    match target.rect.center_x().cmp(&source.rect.center_x()) {
        std::cmp::Ordering::Greater => EdgeOrientation::Forward,
        std::cmp::Ordering::Less => EdgeOrientation::Backward,
        std::cmp::Ordering::Equal => EdgeOrientation::Vertical,
    }
}

fn anchor_point(
    workflow: &WorkflowDefinition,
    node_layout: &GraphNodeLayout,
    is_source: bool,
    port_name: Option<&str>,
    side: AnchorSide,
) -> VirtualPoint {
    let node = &workflow.nodes[node_layout.node_index];
    let ports = if is_source {
        &node.outputs
    } else {
        &node.inputs
    };
    let port_index = port_name.and_then(|name| ports.iter().position(|port| port.name == name));

    match side {
        AnchorSide::Left => VirtualPoint {
            x: node_layout.rect.x,
            y: node_layout.rect.y + side_slot(node_layout.rect.height, ports.len(), port_index),
        },
        AnchorSide::Right => VirtualPoint {
            x: node_layout.rect.right(),
            y: node_layout.rect.y + side_slot(node_layout.rect.height, ports.len(), port_index),
        },
        AnchorSide::Top => VirtualPoint {
            x: node_layout.rect.x + side_slot(node_layout.rect.width, ports.len(), port_index),
            y: node_layout.rect.y,
        },
        AnchorSide::Bottom => VirtualPoint {
            x: node_layout.rect.x + side_slot(node_layout.rect.width, ports.len(), port_index),
            y: node_layout.rect.bottom(),
        },
    }
}

fn side_slot(span: i32, port_count: usize, port_index: Option<usize>) -> i32 {
    if span <= 1 || port_count <= 1 || port_index.is_none() {
        return span.saturating_sub(1) / 2;
    }

    let slot_count = (port_count as i32 - 1).max(1);
    let slot_index = port_index.unwrap_or(0) as i32;
    (slot_index * span.saturating_sub(1) + slot_count / 2) / slot_count
}

fn segments_from_points(points: &[VirtualPoint]) -> Vec<OrthogonalSegment> {
    points
        .windows(2)
        .filter_map(|pair| {
            let start = pair[0];
            let end = pair[1];
            if start == end {
                None
            } else {
                Some(OrthogonalSegment { start, end })
            }
        })
        .collect()
}

fn arrow_glyph(segment: OrthogonalSegment) -> char {
    if segment.start.x == segment.end.x {
        if segment.end.y > segment.start.y {
            'v'
        } else {
            '^'
        }
    } else if segment.end.x > segment.start.x {
        '>'
    } else {
        '<'
    }
}

fn component_bounds(nodes: &[GraphNodeLayout], edges: &[GraphEdgeRoute]) -> VirtualRect {
    let mut bounds = nodes
        .first()
        .map(|node| node.rect)
        .unwrap_or_else(|| VirtualRect::new(0, 0, 1, 1));
    for node in nodes.iter().skip(1) {
        bounds.expand_to_include(node.rect);
    }
    for edge in edges {
        for segment in &edge.segments {
            bounds.expand_to_include(VirtualRect::from_points(segment.start, segment.end));
        }
        bounds.include_point(edge.arrow.position);
    }
    bounds
}

fn shift_component(nodes: &mut [GraphNodeLayout], edges: &mut [GraphEdgeRoute], delta_y: i32) {
    if delta_y == 0 {
        return;
    }

    for node in nodes {
        node.rect.y += delta_y;
    }
    for edge in edges {
        for segment in &mut edge.segments {
            segment.start.y += delta_y;
            segment.end.y += delta_y;
        }
        edge.arrow.position.y += delta_y;
    }
}

fn follow_selection_origin(
    layout: &GraphLayout,
    viewport_width: u16,
    viewport_height: u16,
    selected_node: usize,
) -> VirtualPoint {
    let selected = layout
        .node(selected_node)
        .or_else(|| layout.nodes.first())
        .expect("graph layout should contain at least one node");

    let desired = VirtualPoint {
        x: selected.rect.center_x() - viewport_width as i32 / 2,
        y: selected.rect.center_y() - viewport_height as i32 / 2,
    };
    clamp_origin(layout.bounds, viewport_width, viewport_height, desired)
}

fn clamp_origin(
    bounds: VirtualRect,
    viewport_width: u16,
    viewport_height: u16,
    desired: VirtualPoint,
) -> VirtualPoint {
    let x = axis_origin(bounds.x, bounds.width, viewport_width as i32, desired.x);
    let y = axis_origin(bounds.y, bounds.height, viewport_height as i32, desired.y);
    VirtualPoint { x, y }
}

fn axis_origin(min: i32, span: i32, viewport_span: i32, desired: i32) -> i32 {
    if span <= viewport_span {
        min - (viewport_span - span).max(0) / 2
    } else {
        desired.clamp(min, min + span - viewport_span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_graph::format::{EdgeDef, NodeDef, WorkflowDefinition};
    use rsi_graph::generate::templates::build_starter_workflow;

    fn workflow_with_components() -> WorkflowDefinition {
        WorkflowDefinition::new("components")
            .with_node(NodeDef::action("a", "A"))
            .with_node(NodeDef::action("b", "B"))
            .with_node(NodeDef::action("c", "C"))
            .with_node(NodeDef::action("d", "D"))
            .with_edge(EdgeDef::new("a", "b"))
            .with_edge(EdgeDef::new("c", "d"))
    }

    #[test]
    fn pipeline_layout_flows_left_to_right() {
        let workflow = build_starter_workflow("horizontal").expect("template should exist");
        let layout = layout_workflow_graph(&workflow);

        assert!(layout.nodes[0].rect.x < layout.nodes[1].rect.x);
        assert!(layout.nodes[1].rect.x < layout.nodes[2].rect.x);
        assert_eq!(
            find_directional_neighbor(&layout, 0, GraphDirection::Right),
            Some(1)
        );
    }

    #[test]
    fn disconnected_components_stack_vertically() {
        let workflow = workflow_with_components();
        let layout = layout_workflow_graph(&workflow);

        let first_component_bottom = layout.nodes[1].rect.bottom();
        assert!(layout.nodes[2].rect.y > first_component_bottom);
        assert!(layout.nodes[3].rect.y > first_component_bottom);
    }

    #[test]
    fn cycle_layout_sets_cycle_flag_without_panicking() {
        let workflow = WorkflowDefinition::new("cycle")
            .with_node(NodeDef::action("a", "A"))
            .with_node(NodeDef::action("b", "B"))
            .with_edge(EdgeDef::new("a", "b"))
            .with_edge(EdgeDef::new("b", "a"));

        let layout = layout_workflow_graph(&workflow);

        assert!(layout.has_cycle_edges);
        assert_eq!(layout.nodes.len(), 2);
        assert!(!layout.edges.is_empty());
    }

    #[test]
    fn visual_loop_edges_are_routed() {
        let workflow = build_starter_workflow("pingpong").expect("template should exist");
        let layout = layout_workflow_graph(&workflow);

        assert!(
            layout
                .edges
                .iter()
                .any(|edge| matches!(edge.kind, GraphCanvasEdgeKind::VisualLoopArrow))
        );
        assert!(layout.bounds.height > 0);
    }

    #[test]
    fn manual_viewport_offsets_follow_origin() {
        let workflow = build_starter_workflow("vertical").expect("template should exist");
        let layout = layout_workflow_graph(&workflow);

        let follow = viewport_origin(
            &layout,
            32,
            12,
            0,
            GraphCamera::FollowSelection,
            GraphViewport::default(),
        );
        let manual = viewport_origin(
            &layout,
            32,
            12,
            0,
            GraphCamera::Manual,
            GraphViewport {
                offset_x: 8,
                offset_y: 4,
            },
        );

        assert!(manual.x >= follow.x);
        assert!(manual.y >= follow.y);
    }
}
