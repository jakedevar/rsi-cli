//! Compile WorkflowDefinition to ExecutableGraph, and decompile back.
//!
//! The compiler validates the definition (node IDs, edge references, etc.)
//! and produces an `ExecutableGraph` ready for `DagExecutor`. The decompiler
//! recovers the original `WorkflowDefinition` from an `ExecutableGraph` using
//! the `Node::metadata()` provenance hook.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::data::{NodeData, Value};
use crate::edge::Edge;
use crate::error::GraphError;
use crate::filter::FieldFilter;
use crate::format::*;
use crate::node::{Node, NodeContext, NodeId};
use crate::topology::ExecutableGraph;

/// Compilation error with source location info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompileError {
    /// JSON pointer or field path (e.g. "/nodes/0/id").
    pub path: String,
    /// Human-readable description of the problem.
    pub message: String,
    /// Whether this is a blocking error or advisory warning.
    pub severity: Severity,
    /// The node id involved, if applicable.
    pub node_id: Option<String>,
    /// The edge index involved, if applicable.
    pub edge_index: Option<usize>,
}

/// Severity level for compilation diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Severity {
    Error,
    Warning,
}

/// Compilation diagnostics collecting errors and warnings.
#[derive(Debug, Default)]
pub struct CompileDiagnostics {
    pub errors: Vec<CompileError>,
    pub warnings: Vec<CompileError>,
}

impl CompileDiagnostics {
    /// Returns true if there are any blocking errors.
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Add a blocking error.
    pub fn add_error(
        &mut self,
        path: &str,
        message: &str,
        node_id: Option<&str>,
        edge_idx: Option<usize>,
    ) {
        self.errors.push(CompileError {
            path: path.to_string(),
            message: message.to_string(),
            severity: Severity::Error,
            node_id: node_id.map(|s| s.to_string()),
            edge_index: edge_idx,
        });
    }

    /// Add an advisory warning.
    pub fn add_warning(
        &mut self,
        path: &str,
        message: &str,
        node_id: Option<&str>,
        edge_idx: Option<usize>,
    ) {
        self.warnings.push(CompileError {
            path: path.to_string(),
            message: message.to_string(),
            severity: Severity::Warning,
            node_id: node_id.map(|s| s.to_string()),
            edge_index: edge_idx,
        });
    }
}

/// A compiled node that executes based on its NodeDef instructions.
/// Carries the original definition for decompilation via `metadata()`.
struct DefinitionNode {
    def: NodeDef,
    node_id: NodeId,
}

impl Node for DefinitionNode {
    fn id(&self) -> &NodeId {
        &self.node_id
    }

    fn name(&self) -> &str {
        &self.def.name
    }

    fn tags(&self) -> &[String] {
        &self.def.tags
    }

    fn execute(&self, input: NodeData, _ctx: &mut NodeContext) -> Result<NodeData, GraphError> {
        // Default behavior: pass through input with metadata from node def.
        // Real LLM execution is handled by the daemon, not the graph engine.
        let mut output = input;
        output.insert("_node_id".to_string(), Value::String(self.def.id.clone()));
        output.insert(
            "_node_name".to_string(),
            Value::String(self.def.name.clone()),
        );
        if !self.def.instructions.is_empty() {
            output.insert(
                "_instructions".to_string(),
                Value::String(self.def.instructions.clone()),
            );
        }
        Ok(output)
    }

    fn metadata(&self) -> Option<serde_json::Value> {
        serde_json::to_value(&self.def).ok()
    }
}

/// Compile a `WorkflowDefinition` into an `ExecutableGraph`.
///
/// Returns the graph and diagnostics on success. If there are blocking errors,
/// returns `Err(diagnostics)` with the error details.
pub fn compile(
    def: &WorkflowDefinition,
) -> Result<(ExecutableGraph, CompileDiagnostics), CompileDiagnostics> {
    let mut diag = CompileDiagnostics::default();

    // Validate version.
    if def.version != "1.0" {
        diag.add_warning(
            "/version",
            &format!("unknown version '{}', treating as 1.0", def.version),
            None,
            None,
        );
    }

    // Validate nodes — check for empty and duplicate IDs.
    let mut seen_ids: HashSet<&str> = HashSet::new();
    for (i, node_def) in def.nodes.iter().enumerate() {
        if node_def.id.is_empty() {
            diag.add_error(&format!("/nodes/{}/id", i), "node id is empty", None, None);
        }
        if !seen_ids.insert(&node_def.id) {
            diag.add_error(
                &format!("/nodes/{}/id", i),
                &format!("duplicate node id '{}'", node_def.id),
                Some(&node_def.id),
                None,
            );
        }
    }

    // Validate edges — check source/target references exist.
    let node_ids: HashSet<&str> = def.nodes.iter().map(|n| n.id.as_str()).collect();
    for (i, edge_def) in def.edges.iter().enumerate() {
        if !node_ids.contains(edge_def.source.as_str()) {
            diag.add_error(
                &format!("/edges/{}/source", i),
                &format!("source node '{}' not found", edge_def.source),
                None,
                Some(i),
            );
        }
        if !node_ids.contains(edge_def.target.as_str()) {
            diag.add_error(
                &format!("/edges/{}/target", i),
                &format!("target node '{}' not found", edge_def.target),
                None,
                Some(i),
            );
        }
    }

    if diag.has_errors() {
        return Err(diag);
    }

    // Build graph.
    let mut graph = ExecutableGraph::new();

    for node_def in &def.nodes {
        let node = DefinitionNode {
            def: node_def.clone(),
            node_id: NodeId::new(&node_def.id),
        };
        graph = graph.with_node(Box::new(node));
    }

    for (i, edge_def) in def.edges.iter().enumerate() {
        let mut edge = Edge::new(format!("edge_{}", i), &edge_def.source, &edge_def.target);
        if let Some(ref filter_def) = edge_def.filter {
            if let Some(ref include) = filter_def.include {
                edge.filter = Some(FieldFilter::Include(include.clone()));
            } else if let Some(ref exclude) = filter_def.exclude {
                edge.filter = Some(FieldFilter::Exclude(exclude.clone()));
            }
        }
        graph = graph.with_edge(edge);
    }

    Ok((graph, diag))
}

/// Decompile an `ExecutableGraph` back to a `WorkflowDefinition`.
///
/// Uses `Node::metadata()` to recover the original `NodeDef` from
/// `DefinitionNode`s. For nodes without metadata, creates a basic action node
/// from trait methods.
pub fn decompile(graph: &ExecutableGraph) -> WorkflowDefinition {
    let mut def = WorkflowDefinition::new("decompiled");

    for node in &graph.nodes {
        let node_def = if let Some(meta) = node.metadata() {
            // Try to deserialize the original NodeDef from metadata.
            serde_json::from_value::<NodeDef>(meta)
                .unwrap_or_else(|_| NodeDef::action(node.id().to_string(), node.name().to_string()))
        } else {
            NodeDef::action(node.id().to_string(), node.name().to_string())
        };
        def = def.with_node(node_def);
    }

    for edge in &graph.edges {
        let mut edge_def = EdgeDef::new(edge.source.to_string(), edge.target.to_string());
        if let Some(ref filter) = edge.filter {
            edge_def.filter = Some(match filter {
                FieldFilter::Include(fields) => FilterDef {
                    include: Some(fields.clone()),
                    exclude: None,
                },
                FieldFilter::Exclude(fields) => FilterDef {
                    include: None,
                    exclude: Some(fields.clone()),
                },
            });
        }
        def = def.with_edge(edge_def);
    }

    def
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a simple 3-node pipeline definition.
    fn three_node_pipeline() -> WorkflowDefinition {
        WorkflowDefinition::new("pipeline")
            .with_node(NodeDef::action("a", "Node A"))
            .with_node(NodeDef::action("b", "Node B"))
            .with_node(NodeDef::action("c", "Node C"))
            .with_edge(EdgeDef::new("a", "b"))
            .with_edge(EdgeDef::new("b", "c"))
    }

    #[test]
    fn compile_simple_three_node_pipeline() {
        let def = three_node_pipeline();
        let (graph, diag) = compile(&def).unwrap();

        assert!(!diag.has_errors());
        assert_eq!(graph.nodes.len(), 3);
        assert_eq!(graph.edges.len(), 2);
        assert_eq!(graph.nodes[0].id().as_str(), "a");
        assert_eq!(graph.nodes[1].id().as_str(), "b");
        assert_eq!(graph.nodes[2].id().as_str(), "c");
    }

    #[test]
    fn compile_invalid_edge_unknown_source() {
        let def = WorkflowDefinition::new("bad")
            .with_node(NodeDef::action("a", "Node A"))
            .with_edge(EdgeDef::new("nonexistent", "a"));

        let diag = compile(&def).err().expect("expected compile error");
        assert!(diag.has_errors());
        assert_eq!(diag.errors.len(), 1);
        assert!(diag.errors[0].message.contains("nonexistent"));
        assert!(diag.errors[0].path.contains("/edges/0/source"));
        assert_eq!(diag.errors[0].edge_index, Some(0));
    }

    #[test]
    fn compile_duplicate_node_id() {
        let def = WorkflowDefinition::new("dup")
            .with_node(NodeDef::action("a", "First A"))
            .with_node(NodeDef::action("a", "Second A"));

        let diag = compile(&def).err().expect("expected compile error");
        assert!(diag.has_errors());
        assert_eq!(diag.errors.len(), 1);
        assert!(diag.errors[0].message.contains("duplicate node id 'a'"));
        assert_eq!(diag.errors[0].node_id, Some("a".to_string()));
    }

    #[test]
    fn compile_decompile_roundtrip_preserves_structure() {
        let original = three_node_pipeline();
        let (graph, _) = compile(&original).unwrap();
        let recovered = decompile(&graph);

        // Same number of nodes and edges.
        assert_eq!(recovered.nodes.len(), original.nodes.len());
        assert_eq!(recovered.edges.len(), original.edges.len());

        // Node IDs match.
        for (orig, recov) in original.nodes.iter().zip(recovered.nodes.iter()) {
            assert_eq!(orig.id, recov.id);
            assert_eq!(orig.name, recov.name);
            assert_eq!(orig.node_type, recov.node_type);
        }

        // Edge source/target match.
        for (orig, recov) in original.edges.iter().zip(recovered.edges.iter()) {
            assert_eq!(orig.source, recov.source);
            assert_eq!(orig.target, recov.target);
        }
    }

    #[test]
    fn compile_with_edge_filter() {
        let def = WorkflowDefinition::new("filtered")
            .with_node(NodeDef::action("a", "A"))
            .with_node(NodeDef::action("b", "B"))
            .with_edge({
                let mut e = EdgeDef::new("a", "b");
                e.filter = Some(FilterDef {
                    include: Some(vec!["field_x".into(), "field_y".into()]),
                    exclude: None,
                });
                e
            });

        let (graph, _) = compile(&def).unwrap();
        assert_eq!(graph.edges.len(), 1);
        let filter = graph.edges[0].filter.as_ref().unwrap();
        assert_eq!(
            *filter,
            FieldFilter::Include(vec!["field_x".into(), "field_y".into()])
        );
    }

    #[test]
    fn compile_error_has_source_path_info() {
        let def = WorkflowDefinition::new("bad")
            .with_node(NodeDef::action("a", "A"))
            .with_node(NodeDef::action("b", "B"))
            .with_edge(EdgeDef::new("a", "missing_target"));

        let diag = compile(&def).err().expect("expected compile error");
        assert!(diag.has_errors());
        let err = &diag.errors[0];
        assert_eq!(err.path, "/edges/0/target");
        assert_eq!(err.severity, Severity::Error);
        assert_eq!(err.edge_index, Some(0));
    }

    #[test]
    fn warning_for_unknown_version() {
        let mut def = three_node_pipeline();
        def.version = "2.5".to_string();

        let (_, diag) = compile(&def).unwrap();
        assert!(!diag.has_errors());
        assert_eq!(diag.warnings.len(), 1);
        assert!(diag.warnings[0].message.contains("2.5"));
        assert_eq!(diag.warnings[0].severity, Severity::Warning);
    }

    #[test]
    fn empty_workflow_compiles_to_empty_graph() {
        let def = WorkflowDefinition::new("empty");
        let (graph, diag) = compile(&def).unwrap();

        assert!(!diag.has_errors());
        assert!(diag.warnings.is_empty());
        assert!(graph.nodes.is_empty());
        assert!(graph.edges.is_empty());
    }

    #[test]
    fn compile_decompile_roundtrip_preserves_instructions() {
        let def = WorkflowDefinition::new("inst")
            .with_node(
                NodeDef::action("a", "Agent").with_instructions("You are a helpful assistant"),
            )
            .with_node(NodeDef::action("b", "Reviewer").with_tag("review"));

        let (graph, _) = compile(&def).unwrap();
        let recovered = decompile(&graph);

        assert_eq!(
            recovered.nodes[0].instructions,
            "You are a helpful assistant"
        );
        assert_eq!(recovered.nodes[1].tags, vec!["review".to_string()]);
    }

    #[test]
    fn compile_decompile_roundtrip_preserves_edge_filters() {
        let def = WorkflowDefinition::new("filt")
            .with_node(NodeDef::action("a", "A"))
            .with_node(NodeDef::action("b", "B"))
            .with_edge({
                let mut e = EdgeDef::new("a", "b");
                e.filter = Some(FilterDef {
                    include: None,
                    exclude: Some(vec!["secret".into()]),
                });
                e
            });

        let (graph, _) = compile(&def).unwrap();
        let recovered = decompile(&graph);

        let filter = recovered.edges[0].filter.as_ref().unwrap();
        assert_eq!(filter.exclude, Some(vec!["secret".into()]));
        assert!(filter.include.is_none());
    }

    #[test]
    fn compile_invalid_edge_unknown_target() {
        let def = WorkflowDefinition::new("bad")
            .with_node(NodeDef::action("a", "A"))
            .with_edge(EdgeDef::new("a", "ghost"));

        let diag = compile(&def).err().expect("expected compile error");
        assert!(diag.errors[0].message.contains("ghost"));
    }

    #[test]
    fn compile_empty_node_id_is_error() {
        let def = WorkflowDefinition::new("bad").with_node(NodeDef::action("", "Empty ID"));

        let diag = compile(&def).err().expect("expected compile error");
        assert!(diag.errors[0].message.contains("node id is empty"));
    }

    #[test]
    fn compile_multiple_errors_reported() {
        let def = WorkflowDefinition::new("multi-err")
            .with_node(NodeDef::action("a", "A"))
            .with_edge(EdgeDef::new("missing1", "missing2"));

        let diag = compile(&def).err().expect("expected compile error");
        // Both source and target are invalid.
        assert_eq!(diag.errors.len(), 2);
    }
}
