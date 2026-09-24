//! Topology preview pane — compact ASCII DAG render for the
//! `CreateEntityForm` overlay (P2.3 Phase 3).
//!
//! Design (per plan Decision 1): client-side, TUI-only translator from
//! `TopologyDefinition` to a render-friendly shape. Does NOT route through
//! the daemon-side `topology_bridge` (which depends on
//! `WorkflowDocument` / `WorkflowStage` and pulls in workflow execution
//! metadata we don't need for the preview).
//!
//! The output is a 6–8 line ASCII DAG. On any failure to layout (e.g.,
//! malformed topology with a dangling edge), returns the single-line
//! placeholder `"[topology preview unavailable]"` — the Phase 4.5 escape
//! hatch from ticket §4.

use crate::ui::theme;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use rsi_common::types::TopologyDefinition;
use std::collections::{HashMap, HashSet};

/// Cap on the number of lines produced by `render_dag_ascii`. Tail rows
/// beyond the cap are replaced with an ellipsis row.
pub(crate) const PREVIEW_MAX_LINES: usize = 8;

/// Render a compact ASCII DAG preview of `def` capped at `max_lines` rows.
///
/// On any structural failure (dangling edge, cycle that the simple
/// topo-sort cannot resolve, etc.), returns a single placeholder line
/// styled in the overlay hint color. NEVER panics.
pub(crate) fn render_dag_ascii(def: &TopologyDefinition, max_lines: usize) -> Vec<Line<'static>> {
    if def.nodes.is_empty() {
        return vec![placeholder_line("[topology preview unavailable — empty]")];
    }
    let cap = max_lines.min(PREVIEW_MAX_LINES);

    // Build node id -> label map for fast lookup.
    let node_by_id: HashMap<&str, &str> = def
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.label.as_str()))
        .collect();

    // Validate edges (dangling endpoint = malformed).
    for edge in &def.edges {
        if !node_by_id.contains_key(edge.from.as_str())
            || !node_by_id.contains_key(edge.to.as_str())
        {
            return vec![placeholder_line(
                "[topology preview unavailable — dangling edge]",
            )];
        }
    }

    // Build adjacency + indegree for a Kahn-style topo sort. Loop edges
    // are skipped for the topo order (they are back-edges by definition).
    let mut indegree: HashMap<&str, usize> = HashMap::new();
    let mut outedges: HashMap<&str, Vec<&str>> = HashMap::new();
    for n in &def.nodes {
        indegree.insert(n.id.as_str(), 0);
        outedges.insert(n.id.as_str(), Vec::new());
    }
    for edge in &def.edges {
        if edge.loop_edge {
            continue;
        }
        if let Some(c) = indegree.get_mut(edge.to.as_str()) {
            *c += 1;
        }
        if let Some(list) = outedges.get_mut(edge.from.as_str()) {
            list.push(edge.to.as_str());
        }
    }

    // Kahn's algorithm.
    let mut queue: Vec<&str> = indegree
        .iter()
        .filter(|&(_, &v)| v == 0)
        .map(|(k, _)| *k)
        .collect();
    queue.sort();
    let mut order: Vec<&str> = Vec::with_capacity(def.nodes.len());
    let mut visited: HashSet<&str> = HashSet::new();
    while let Some(id) = queue.pop() {
        if !visited.insert(id) {
            continue;
        }
        order.push(id);
        if let Some(neighbors) = outedges.get(id) {
            for &n in neighbors {
                if let Some(c) = indegree.get_mut(n) {
                    if *c > 0 {
                        *c -= 1;
                    }
                    if *c == 0 && !visited.contains(n) {
                        queue.push(n);
                    }
                }
            }
        }
    }

    // If we couldn't visit every node, there's a non-loop cycle — fall
    // back to placeholder rather than emit a partial/misleading preview.
    if order.len() != def.nodes.len() {
        return vec![placeholder_line(
            "[topology preview unavailable — unresolved cycle]",
        )];
    }

    // Emit one line per node in topo order. The first node gets `▶`; the
    // rest indent under a connector `└─`.
    let style = Style::default().fg(theme::text());
    let prereq_style = Style::default().fg(theme::overlay_hint());
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(order.len().min(cap));
    for (i, id) in order.iter().enumerate() {
        if lines.len() + 1 >= cap && i + 1 < order.len() {
            lines.push(Line::from(vec![Span::styled(
                "  …".to_string(),
                prereq_style,
            )]));
            break;
        }
        let label = node_by_id.get(id).copied().unwrap_or("?");
        let glyph = if i == 0 {
            "\u{25B6}"
        } else {
            "\u{2514}\u{2500}"
        };
        let mut spans: Vec<Span<'static>> = vec![
            Span::styled(format!("{glyph} "), prereq_style),
            Span::styled(label.to_string(), style.add_modifier(Modifier::BOLD)),
        ];
        // Prereqs hint (count) so the user sees DAG shape at a glance.
        if let Some(node) = def.nodes.iter().find(|n| n.id == *id) {
            if !node.prereqs.is_empty() {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    format!("(after {})", node.prereqs.join(",")),
                    prereq_style,
                ));
            }
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn placeholder_line(text: &str) -> Line<'static> {
    Line::from(vec![Span::styled(
        text.to_string(),
        Style::default().fg(theme::overlay_hint()),
    )])
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsi_common::types::{SessionKind, TopologyDefinition, TopologyEdge, TopologyNode};

    fn node(id: &str, label: &str, prereqs: &[&str]) -> TopologyNode {
        TopologyNode {
            id: id.to_string(),
            kind: SessionKind::Task,
            label: label.to_string(),
            prereqs: prereqs.iter().map(|s| s.to_string()).collect(),
            max_iterations: None,
            on_failure: None,
            params: Default::default(),
        }
    }

    fn edge(from: &str, to: &str) -> TopologyEdge {
        TopologyEdge {
            from: from.to_string(),
            to: to.to_string(),
            loop_edge: false,
        }
    }

    #[test]
    fn empty_topology_returns_placeholder() {
        let def = TopologyDefinition {
            nodes: Vec::new(),
            edges: Vec::new(),
            until: None,
        };
        let lines = render_dag_ascii(&def, 6);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn dangling_edge_returns_placeholder() {
        let def = TopologyDefinition {
            nodes: vec![node("a", "Alpha", &[])],
            edges: vec![edge("a", "b")],
            until: None,
        };
        let lines = render_dag_ascii(&def, 6);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn three_node_linear_chain_renders_in_order() {
        let def = TopologyDefinition {
            nodes: vec![
                node("a", "Alpha", &[]),
                node("b", "Beta", &["a"]),
                node("c", "Gamma", &["b"]),
            ],
            edges: vec![edge("a", "b"), edge("b", "c")],
            until: None,
        };
        let lines = render_dag_ascii(&def, 6);
        assert!(lines.len() <= 3);
        assert!(lines.len() >= 1);
    }

    #[test]
    fn max_lines_truncation_adds_ellipsis() {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        for i in 0..20 {
            nodes.push(node(
                &format!("n{i}"),
                &format!("Node {i}"),
                if i == 0 { &[] } else { &[] },
            ));
            if i > 0 {
                edges.push(edge(&format!("n{}", i - 1), &format!("n{i}")));
            }
        }
        let def = TopologyDefinition {
            nodes,
            edges,
            until: None,
        };
        let lines = render_dag_ascii(&def, 6);
        assert!(lines.len() <= 6);
    }

    #[test]
    fn loop_edges_do_not_break_topo_sort() {
        // a -> b -> c, with a loop edge c -> a. The loop edge is ignored
        // for ordering purposes (it's a back-edge by intent).
        let def = TopologyDefinition {
            nodes: vec![
                node("a", "Alpha", &[]),
                node("b", "Beta", &["a"]),
                node("c", "Gamma", &["b"]),
            ],
            edges: vec![
                edge("a", "b"),
                edge("b", "c"),
                TopologyEdge {
                    from: "c".to_string(),
                    to: "a".to_string(),
                    loop_edge: true,
                },
            ],
            until: None,
        };
        let lines = render_dag_ascii(&def, 6);
        // Should produce a valid render, NOT the unresolved-cycle placeholder.
        assert!(lines.len() >= 1);
        if let Some(line) = lines.first() {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                !text.contains("unavailable"),
                "loop edges must not poison render: {text}"
            );
        }
    }

    #[test]
    fn non_loop_cycle_returns_placeholder() {
        // a -> b -> a (no loop edge bit) — unresolved cycle.
        let def = TopologyDefinition {
            nodes: vec![node("a", "A", &[]), node("b", "B", &[])],
            edges: vec![edge("a", "b"), edge("b", "a")],
            until: None,
        };
        let lines = render_dag_ascii(&def, 6);
        assert_eq!(lines.len(), 1);
        if let Some(line) = lines.first() {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(text.contains("unavailable"));
        }
    }
}
