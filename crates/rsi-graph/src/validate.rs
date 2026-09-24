use std::collections::{HashMap, HashSet, VecDeque};

use rsi_common::types::{
    WorkflowValidationDiagnostic, WorkflowValidationReport, WorkflowValidationSeverity,
    WorkflowValidationSubject,
};

use crate::format::{
    EdgeDef, GRAPH_VIEW_SCHEMA_VERSION, GraphViewVisualEdge, NodeType, WorkflowDefinition,
};

const CODE_UNKNOWN_VERSION: &str = "workflow.version.unknown";
const CODE_EMPTY_NODE_ID: &str = "workflow.node.empty_id";
const CODE_DUPLICATE_NODE_ID: &str = "workflow.node.duplicate_id";
const CODE_UNSUPPORTED_NODE_KIND: &str = "workflow.node.unsupported_kind";
const CODE_NESTED_SUBGRAPH: &str = "workflow.node.nested_subgraph";
const CODE_MISSING_SOURCE: &str = "workflow.edge.missing_source";
const CODE_MISSING_TARGET: &str = "workflow.edge.missing_target";
const CODE_SELF_LOOP: &str = "workflow.edge.self_loop";
const CODE_CYCLE: &str = "workflow.edge.cycle";
const CODE_INVALID_GRAPH_VIEW: &str = "workflow.graph_view.invalid";
const CODE_UNSUPPORTED_GRAPH_VIEW_SCHEMA: &str = "workflow.graph_view.unsupported_schema";
const CODE_GRAPH_VIEW_EMPTY_VISUAL_EDGE_ID: &str = "workflow.graph_view.visual_edge.empty_id";
const CODE_GRAPH_VIEW_MISSING_VISUAL_EDGE_SOURCE: &str =
    "workflow.graph_view.visual_edge.missing_source";
const CODE_GRAPH_VIEW_MISSING_VISUAL_EDGE_TARGET: &str =
    "workflow.graph_view.visual_edge.missing_target";

/// Validate that a workflow definition can execute honestly in the current v1 DAG runtime.
pub fn validate_executable_workflow(workflow: &WorkflowDefinition) -> WorkflowValidationReport {
    let mut report = WorkflowValidationReport::default();

    if workflow.version != "1.0" {
        report.push(diagnostic(
            CODE_UNKNOWN_VERSION,
            WorkflowValidationSeverity::Warning,
            WorkflowValidationSubject::new("/version"),
            format!(
                "unknown workflow version '{}'; treating it as 1.0",
                workflow.version
            ),
        ));
    }

    let node_ids = validate_nodes(workflow, &mut report);
    validate_edges(workflow, &node_ids, &mut report);
    validate_graph_view(workflow, &node_ids, &mut report);

    report.executable = !report.has_errors();
    report
}

fn validate_nodes(
    workflow: &WorkflowDefinition,
    report: &mut WorkflowValidationReport,
) -> HashSet<String> {
    let mut node_ids = HashSet::new();

    for (index, node) in workflow.nodes.iter().enumerate() {
        let node_id_path = format!("/nodes/{index}/id");
        let trimmed_id = node.id.trim();
        if trimmed_id.is_empty() {
            report.push(diagnostic(
                CODE_EMPTY_NODE_ID,
                WorkflowValidationSeverity::Error,
                WorkflowValidationSubject::new(node_id_path),
                "node id cannot be empty",
            ));
        } else if !node_ids.insert(trimmed_id.to_string()) {
            report.push(diagnostic(
                CODE_DUPLICATE_NODE_ID,
                WorkflowValidationSeverity::Error,
                WorkflowValidationSubject::new(node_id_path).with_node_id(trimmed_id),
                format!("duplicate node id '{trimmed_id}'"),
            ));
        }

        match node.node_type {
            NodeType::Action => {}
            NodeType::Topology => {
                report.push(diagnostic(
                    CODE_UNSUPPORTED_NODE_KIND,
                    WorkflowValidationSeverity::Error,
                    WorkflowValidationSubject::new(format!("/nodes/{index}/type"))
                        .with_node_id(node.id.clone()),
                    format!(
                        "node '{}' uses unsupported executable type 'topology'",
                        node.id
                    ),
                ));
            }
            NodeType::Subgraph => {
                report.push(diagnostic(
                    CODE_UNSUPPORTED_NODE_KIND,
                    WorkflowValidationSeverity::Error,
                    WorkflowValidationSubject::new(format!("/nodes/{index}/type"))
                        .with_node_id(node.id.clone()),
                    format!(
                        "node '{}' uses unsupported executable type 'subgraph'",
                        node.id
                    ),
                ));

                if let Some(inner) = node.subgraph.as_deref()
                    && let Some(nested_id) = first_nested_subgraph_id(inner)
                {
                    report.push(diagnostic(
                        CODE_NESTED_SUBGRAPH,
                        WorkflowValidationSeverity::Error,
                        WorkflowValidationSubject::new(format!("/nodes/{index}/subgraph"))
                            .with_node_id(node.id.clone()),
                        format!(
                            "node '{}' contains nested subgraph node '{}'",
                            node.id, nested_id
                        ),
                    ));
                }
            }
        }
    }

    node_ids
}

fn validate_edges(
    workflow: &WorkflowDefinition,
    node_ids: &HashSet<String>,
    report: &mut WorkflowValidationReport,
) {
    let mut cycle_edges: Vec<&EdgeDef> = Vec::new();

    for (index, edge) in workflow.edges.iter().enumerate() {
        let source_path = format!("/edges/{index}/source");
        let target_path = format!("/edges/{index}/target");

        let has_source = node_ids.contains(edge.source.as_str());
        let has_target = node_ids.contains(edge.target.as_str());

        if !has_source {
            report.push(diagnostic(
                CODE_MISSING_SOURCE,
                WorkflowValidationSeverity::Error,
                WorkflowValidationSubject::new(source_path.as_str()).with_edge_index(index),
                format!(
                    "edge source '{}' does not reference an existing node",
                    edge.source
                ),
            ));
        }

        if !has_target {
            report.push(diagnostic(
                CODE_MISSING_TARGET,
                WorkflowValidationSeverity::Error,
                WorkflowValidationSubject::new(target_path.as_str()).with_edge_index(index),
                format!(
                    "edge target '{}' does not reference an existing node",
                    edge.target
                ),
            ));
        }

        if edge.source == edge.target {
            report.push(diagnostic(
                CODE_SELF_LOOP,
                WorkflowValidationSeverity::Error,
                WorkflowValidationSubject::new(source_path.as_str()).with_edge_index(index),
                format!("edge '{}' -> '{}' is a self-loop", edge.source, edge.target),
            ));
            continue;
        }

        if has_source && has_target {
            cycle_edges.push(edge);
        }
    }

    if executable_edges_have_cycle(node_ids, &cycle_edges) {
        report.push(diagnostic(
            CODE_CYCLE,
            WorkflowValidationSeverity::Error,
            WorkflowValidationSubject::new("/edges"),
            "workflow contains a cycle in executable edges",
        ));
    }
}

fn validate_graph_view(
    workflow: &WorkflowDefinition,
    node_ids: &HashSet<String>,
    report: &mut WorkflowValidationReport,
) {
    let graph_view = match workflow.graph_view_metadata() {
        Ok(graph_view) => graph_view,
        Err(error) => {
            report.push(diagnostic(
                CODE_INVALID_GRAPH_VIEW,
                WorkflowValidationSeverity::Error,
                WorkflowValidationSubject::new(error.path),
                error.message,
            ));
            return;
        }
    };

    let Some(graph_view) = graph_view else {
        return;
    };

    if graph_view.schema_version != GRAPH_VIEW_SCHEMA_VERSION {
        report.push(diagnostic(
            CODE_UNSUPPORTED_GRAPH_VIEW_SCHEMA,
            WorkflowValidationSeverity::Error,
            WorkflowValidationSubject::new("/metadata/graph_view/schema_version"),
            format!(
                "graph_view schema_version {} is unsupported; expected {}",
                graph_view.schema_version, GRAPH_VIEW_SCHEMA_VERSION
            ),
        ));
    }

    for (index, visual_edge) in graph_view.visual_edges.iter().enumerate() {
        validate_visual_edge(index, visual_edge, node_ids, report);
    }
}

fn validate_visual_edge(
    index: usize,
    visual_edge: &GraphViewVisualEdge,
    node_ids: &HashSet<String>,
    report: &mut WorkflowValidationReport,
) {
    if visual_edge.id.trim().is_empty() {
        report.push(diagnostic(
            CODE_GRAPH_VIEW_EMPTY_VISUAL_EDGE_ID,
            WorkflowValidationSeverity::Error,
            WorkflowValidationSubject::new(format!("/metadata/graph_view/visual_edges/{index}/id")),
            "graph_view visual edge id cannot be empty",
        ));
    }

    if !node_ids.contains(visual_edge.source.as_str()) {
        report.push(diagnostic(
            CODE_GRAPH_VIEW_MISSING_VISUAL_EDGE_SOURCE,
            WorkflowValidationSeverity::Error,
            WorkflowValidationSubject::new(format!(
                "/metadata/graph_view/visual_edges/{index}/source"
            )),
            format!(
                "graph_view visual edge source '{}' does not reference an existing node",
                visual_edge.source
            ),
        ));
    }

    if !node_ids.contains(visual_edge.target.as_str()) {
        report.push(diagnostic(
            CODE_GRAPH_VIEW_MISSING_VISUAL_EDGE_TARGET,
            WorkflowValidationSeverity::Error,
            WorkflowValidationSubject::new(format!(
                "/metadata/graph_view/visual_edges/{index}/target"
            )),
            format!(
                "graph_view visual edge target '{}' does not reference an existing node",
                visual_edge.target
            ),
        ));
    }
}

fn first_nested_subgraph_id(workflow: &WorkflowDefinition) -> Option<String> {
    for node in &workflow.nodes {
        if matches!(node.node_type, NodeType::Subgraph) {
            return Some(node.id.clone());
        }

        if let Some(inner) = node.subgraph.as_deref()
            && let Some(nested_id) = first_nested_subgraph_id(inner)
        {
            return Some(nested_id);
        }
    }

    None
}

fn executable_edges_have_cycle(node_ids: &HashSet<String>, edges: &[&EdgeDef]) -> bool {
    if node_ids.is_empty() || edges.is_empty() {
        return false;
    }

    let mut in_degree: HashMap<&str, usize> = node_ids
        .iter()
        .map(|node_id| (node_id.as_str(), 0usize))
        .collect();
    let mut adjacency: HashMap<&str, Vec<&str>> = node_ids
        .iter()
        .map(|node_id| (node_id.as_str(), Vec::new()))
        .collect();

    for edge in edges {
        if let Some(neighbors) = adjacency.get_mut(edge.source.as_str()) {
            neighbors.push(edge.target.as_str());
        }
        if let Some(degree) = in_degree.get_mut(edge.target.as_str()) {
            *degree += 1;
        }
    }

    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter_map(|(node_id, degree)| (*degree == 0).then_some(*node_id))
        .collect();
    let mut visited = 0usize;

    while let Some(node_id) = queue.pop_front() {
        visited += 1;
        if let Some(neighbors) = adjacency.get(node_id) {
            for neighbor in neighbors {
                if let Some(degree) = in_degree.get_mut(neighbor) {
                    *degree -= 1;
                    if *degree == 0 {
                        queue.push_back(neighbor);
                    }
                }
            }
        }
    }

    visited != node_ids.len()
}

fn diagnostic(
    code: &str,
    severity: WorkflowValidationSeverity,
    subject: WorkflowValidationSubject,
    message: impl Into<String>,
) -> WorkflowValidationDiagnostic {
    WorkflowValidationDiagnostic {
        code: code.to_string(),
        severity,
        subject,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Value;
    use crate::format::{
        EdgeDef, GRAPH_VIEW_METADATA_KEY, GraphViewMetadata, GraphViewVisualEdge,
        GraphViewVisualEdgeKind, NodeDef,
    };
    use std::collections::BTreeMap;

    #[test]
    fn executable_validator_accepts_simple_dag() {
        let workflow = WorkflowDefinition::new("simple")
            .with_node(NodeDef::action("a", "A"))
            .with_node(NodeDef::action("b", "B"))
            .with_edge(EdgeDef::new("a", "b"));

        let report = validate_executable_workflow(&workflow);

        assert!(report.executable);
        assert!(report.diagnostics.is_empty());
    }

    #[test]
    fn executable_validator_rejects_unsupported_nodes_and_cycles() {
        let workflow = WorkflowDefinition::new("invalid")
            .with_node(NodeDef::topology("router", "Router", "hub"))
            .with_node(NodeDef::action("a", "A"))
            .with_node(NodeDef::action("b", "B"))
            .with_edge(EdgeDef::new("a", "b"))
            .with_edge(EdgeDef::new("b", "a"));

        let report = validate_executable_workflow(&workflow);

        assert!(!report.executable);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diag| diag.code == CODE_UNSUPPORTED_NODE_KIND)
        );
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diag| diag.code == CODE_CYCLE)
        );
    }

    #[test]
    fn executable_validator_rejects_invalid_graph_view_metadata() {
        let mut workflow = WorkflowDefinition::new("graph-view")
            .with_node(NodeDef::action("a", "A"))
            .with_node(NodeDef::action("b", "B"));
        workflow.metadata.insert(
            GRAPH_VIEW_METADATA_KEY.to_string(),
            Value::String("bad".to_string()),
        );

        let report = validate_executable_workflow(&workflow);

        assert!(!report.executable);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diag| diag.code == CODE_INVALID_GRAPH_VIEW)
        );
    }

    #[test]
    fn executable_validator_rejects_visual_edge_missing_node_refs() {
        let mut workflow = WorkflowDefinition::new("graph-view")
            .with_node(NodeDef::action("a", "A"))
            .with_node(NodeDef::action("b", "B"));
        let graph_view = GraphViewMetadata {
            schema_version: GRAPH_VIEW_SCHEMA_VERSION,
            visual_edges: vec![GraphViewVisualEdge {
                id: "loop-1".to_string(),
                kind: GraphViewVisualEdgeKind::LoopArrow,
                source: "a".to_string(),
                target: "missing".to_string(),
                source_port: None,
                target_port: None,
                label: None,
            }],
            extra: BTreeMap::new(),
        };
        workflow.metadata.insert(
            GRAPH_VIEW_METADATA_KEY.to_string(),
            serde_json::from_value(serde_json::to_value(graph_view).unwrap()).unwrap(),
        );

        let report = validate_executable_workflow(&workflow);

        assert!(!report.executable);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diag| diag.code == CODE_GRAPH_VIEW_MISSING_VISUAL_EDGE_TARGET)
        );
    }
}
