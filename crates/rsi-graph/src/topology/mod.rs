//! Topology patterns and DAG executor for graph-based orchestration.
//!
//! A [`Topology`] is a reusable pattern (sequential chain, ping-pong, etc.)
//! that produces an [`ExecutableGraph`] or executes directly. The [`DagExecutor`]
//! handles arbitrary DAG execution with topological-sort layering.

pub mod brainstorm;
pub mod decision;
pub mod horizontal;
pub mod hub;
pub mod instructor;
pub mod pingpong;
pub mod vertical;

use crate::data::NodeData;
use crate::edge::Edge;
use crate::error::GraphError;
use crate::hook::*;
use crate::node::{Node, NodeContext, NodeId};
use crate::state::{ScopeId, StateStore};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;

/// Message history management policy for iterative topologies.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum HistoryPolicy {
    /// Keep all messages from all rounds.
    KeepAll,
    /// Keep only the last N messages per node.
    SlidingWindow(usize),
    /// Stateless — no message history between rounds.
    KeepNone,
}

/// A graph that can be executed by the [`DagExecutor`].
pub struct ExecutableGraph {
    pub nodes: Vec<Box<dyn Node>>,
    pub edges: Vec<Edge>,
    pub state_store: StateStore,
    pub hook_registry: Arc<HookRegistry>,
}

impl ExecutableGraph {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            state_store: StateStore::new(),
            hook_registry: Arc::new(HookRegistry::new()),
        }
    }

    /// Add a node to the graph.
    pub fn with_node(mut self, node: Box<dyn Node>) -> Self {
        self.nodes.push(node);
        self
    }

    /// Add an edge to the graph.
    pub fn with_edge(mut self, edge: Edge) -> Self {
        self.edges.push(edge);
        self
    }

    /// Register a hook in the graph's hook registry.
    pub fn with_hook(mut self, hook: Box<dyn Hook>) -> Self {
        Arc::get_mut(&mut self.hook_registry)
            .expect("hook_registry has no other references during build")
            .register(hook);
        self
    }
}

impl Default for ExecutableGraph {
    fn default() -> Self {
        Self::new()
    }
}

/// DAG Executor: topological sort into layers, then execute layer by layer.
///
/// Nodes within the same layer have no dependencies between them and could
/// theoretically run in parallel. Currently executes sequentially within each
/// layer (parallel-ready architecture).
pub struct DagExecutor;

impl DagExecutor {
    /// Execute a graph with the given initial input.
    ///
    /// Source nodes (no incoming edges) receive `initial_input`. Nodes with
    /// multiple incoming edges get their inputs merged via last-write-wins.
    /// Returns the merged output of all sink nodes (no outgoing edges).
    pub fn execute(
        graph: &mut ExecutableGraph,
        initial_input: NodeData,
    ) -> Result<NodeData, GraphError> {
        let layers = Self::topological_layers(&graph.nodes, &graph.edges)?;

        if layers.is_empty() {
            return Ok(initial_input);
        }

        // Build a lookup from NodeId -> index in graph.nodes
        let node_index: HashMap<&NodeId, usize> = graph
            .nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id(), i))
            .collect();

        // Track outputs per node for downstream consumption.
        let mut outputs: HashMap<NodeId, NodeData> = HashMap::new();

        // Build set of nodes that have incoming edges.
        let targets: HashSet<&NodeId> = graph.edges.iter().map(|e| &e.target).collect();

        // Build set of nodes that have outgoing edges.
        let sources: HashSet<&NodeId> = graph.edges.iter().map(|e| &e.source).collect();

        for layer in &layers {
            for node_id in layer {
                let idx = *node_index
                    .get(node_id)
                    .ok_or_else(|| GraphError::NodeNotFound(node_id.to_string()))?;

                // Gather input: merge outputs from upstream nodes, or use initial_input
                // for source nodes (those with no incoming edges).
                let mut current_data = if !targets.contains(node_id) {
                    // Source node — use initial input.
                    initial_input.clone()
                } else {
                    // Merge inputs from all incoming edges (last-write-wins).
                    let mut merged = NodeData::new();
                    for edge in &graph.edges {
                        if &edge.target == node_id
                            && let Some(upstream_output) = outputs.get(&edge.source)
                        {
                            let mut filtered = upstream_output.clone();
                            // Apply edge filter if present.
                            if let Some(ref filter) = edge.filter {
                                filtered = filter.apply(&filtered);
                            }
                            merged.merge(filtered);
                        }
                    }
                    merged
                };

                let node_tags = graph.nodes[idx].tags().to_vec();

                // Create per-node scope.
                let scope_id = ScopeId::new(format!("node_{}", node_id));
                let _ =
                    graph
                        .state_store
                        .create_scope(scope_id.clone(), Some(ScopeId::root()), vec![]);

                // --- BeforeExecute hooks ---
                let hook_ctx = HookContext {
                    node_id,
                    node_tags: &node_tags,
                    data: &current_data,
                    stage: HookStage::BeforeExecute,
                };

                match graph
                    .hook_registry
                    .dispatch(HookStage::BeforeExecute, &hook_ctx)
                {
                    HookAction::Continue => {}
                    HookAction::ModifyData(modified) => {
                        current_data = modified;
                    }
                    HookAction::Abort(err) => return Err(err),
                    HookAction::Skip => {
                        // Store current_data as output so downstream can still consume.
                        outputs.insert(node_id.clone(), current_data);
                        continue;
                    }
                }

                // --- Apply incoming edge filters (BeforeFilter/AfterFilter hooks) ---
                if let Some(edge) = graph.edges.iter().find(|e| &e.target == node_id)
                    && let Some(ref filter) = edge.filter
                {
                    let filter_ctx = HookContext {
                        node_id,
                        node_tags: &node_tags,
                        data: &current_data,
                        stage: HookStage::BeforeFilter,
                    };
                    graph
                        .hook_registry
                        .dispatch(HookStage::BeforeFilter, &filter_ctx);

                    current_data = filter.apply(&current_data);

                    let filter_ctx = HookContext {
                        node_id,
                        node_tags: &node_tags,
                        data: &current_data,
                        stage: HookStage::AfterFilter,
                    };
                    graph
                        .hook_registry
                        .dispatch(HookStage::AfterFilter, &filter_ctx);
                }

                // --- Execute node ---
                let mut ctx = NodeContext {
                    node_id: node_id.clone(),
                    tags: node_tags.clone(),
                    state_scope: Some(scope_id),
                    hooks: Some(graph.hook_registry.clone()),
                    context_registry: None,
                };

                match graph.nodes[idx].execute(current_data.clone(), &mut ctx) {
                    Ok(output) => {
                        // --- AfterExecute hooks ---
                        let hook_ctx = HookContext {
                            node_id,
                            node_tags: &node_tags,
                            data: &output,
                            stage: HookStage::AfterExecute,
                        };
                        let final_output = match graph
                            .hook_registry
                            .dispatch(HookStage::AfterExecute, &hook_ctx)
                        {
                            HookAction::ModifyData(modified) => modified,
                            _ => output,
                        };
                        outputs.insert(node_id.clone(), final_output);
                    }
                    Err(err) => {
                        // --- OnError hooks ---
                        let hook_ctx = HookContext {
                            node_id,
                            node_tags: &node_tags,
                            data: &current_data,
                            stage: HookStage::OnError,
                        };
                        match graph.hook_registry.dispatch(HookStage::OnError, &hook_ctx) {
                            HookAction::Skip => {
                                outputs.insert(node_id.clone(), current_data);
                            }
                            HookAction::ModifyData(modified) => {
                                outputs.insert(node_id.clone(), modified);
                            }
                            HookAction::Abort(hook_err) => return Err(hook_err),
                            HookAction::Continue => return Err(err),
                        }
                    }
                }
            }
        }

        // Collect outputs from sink nodes (no outgoing edges) and merge them.
        let mut result = NodeData::new();
        for layer in layers.iter().rev() {
            for node_id in layer {
                if !sources.contains(node_id)
                    && let Some(output) = outputs.remove(node_id)
                {
                    result.merge(output);
                }
            }
            // Only look at the last layer for sinks — but some sinks could be in
            // earlier layers if they have no outgoing edges. Collect all sinks.
        }

        // If no sinks found (shouldn't happen in a valid graph), return last output.
        if result.is_empty() && !outputs.is_empty() {
            // Fallback: return the last computed output.
            if let Some(last_layer) = layers.last()
                && let Some(last_id) = last_layer.last()
                && let Some(output) = outputs.remove(last_id)
            {
                return Ok(output);
            }
        }

        Ok(result)
    }

    /// Topological sort returning layers. Each layer contains nodes whose
    /// dependencies are all satisfied by previous layers.
    ///
    /// Uses Kahn's algorithm with layer tracking. Returns
    /// [`GraphError::CycleDetected`] if the graph contains a cycle.
    fn topological_layers(
        nodes: &[Box<dyn Node>],
        edges: &[Edge],
    ) -> Result<Vec<Vec<NodeId>>, GraphError> {
        let node_ids: HashSet<NodeId> = nodes.iter().map(|n| n.id().clone()).collect();

        // Build adjacency list and in-degree count.
        let mut in_degree: HashMap<NodeId, usize> = HashMap::new();
        let mut adjacency: HashMap<NodeId, Vec<NodeId>> = HashMap::new();

        for id in &node_ids {
            in_degree.insert(id.clone(), 0);
            adjacency.insert(id.clone(), Vec::new());
        }

        for edge in edges {
            if let Some(deg) = in_degree.get_mut(&edge.target) {
                *deg += 1;
            }
            if let Some(adj) = adjacency.get_mut(&edge.source) {
                adj.push(edge.target.clone());
            }
        }

        // Kahn's algorithm with layer tracking.
        let mut queue: VecDeque<NodeId> = VecDeque::new();
        for (id, &deg) in &in_degree {
            if deg == 0 {
                queue.push_back(id.clone());
            }
        }

        let mut layers: Vec<Vec<NodeId>> = Vec::new();
        let mut visited_count = 0;

        while !queue.is_empty() {
            let layer_size = queue.len();
            let mut current_layer = Vec::with_capacity(layer_size);

            for _ in 0..layer_size {
                let node_id = queue.pop_front().unwrap();
                visited_count += 1;

                if let Some(neighbors) = adjacency.get(&node_id) {
                    for neighbor in neighbors {
                        if let Some(deg) = in_degree.get_mut(neighbor) {
                            *deg -= 1;
                            if *deg == 0 {
                                queue.push_back(neighbor.clone());
                            }
                        }
                    }
                }

                current_layer.push(node_id);
            }

            // Sort layer for deterministic ordering.
            current_layer.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            layers.push(current_layer);
        }

        if visited_count != node_ids.len() {
            return Err(GraphError::CycleDetected);
        }

        Ok(layers)
    }
}

/// Trait for topology templates that configure and execute graph patterns.
pub trait Topology {
    /// Name of this topology pattern.
    fn name(&self) -> &str;

    /// Description of the pattern.
    fn description(&self) -> &str;

    /// Execute this topology with the given input.
    fn execute(
        &mut self,
        input: NodeData,
        state_store: &mut StateStore,
        hooks: &HookRegistry,
    ) -> Result<NodeData, GraphError>;
}

/// Registry of available topology patterns.
#[derive(Default)]
pub struct TopologyRegistry {
    factories: BTreeMap<String, TopologyFactory>,
}

/// A factory that can create topology instances from configuration.
pub type TopologyFactory = fn(serde_json::Value) -> Result<Box<dyn Topology>, GraphError>;

impl TopologyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a topology factory under a name.
    pub fn register(&mut self, name: &str, factory: TopologyFactory) {
        self.factories.insert(name.to_string(), factory);
    }

    /// Look up a factory by name.
    pub fn get(&self, name: &str) -> Option<&TopologyFactory> {
        self.factories.get(name)
    }

    /// List all registered topology names.
    pub fn list(&self) -> Vec<&str> {
        self.factories.keys().map(|s| s.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Value;
    use crate::test_nodes::*;

    // --- DagExecutor tests ---

    #[test]
    fn empty_graph_returns_input() {
        let mut graph = ExecutableGraph::new();
        let mut input = NodeData::new();
        input.insert("x", Value::Number(1.0));

        let output = DagExecutor::execute(&mut graph, input.clone()).unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn single_node_passthrough() {
        let mut graph = ExecutableGraph::new().with_node(Box::new(PassthroughNode::new("a")));

        let mut input = NodeData::new();
        input.insert("key", Value::String("val".into()));

        let output = DagExecutor::execute(&mut graph, input.clone()).unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn linear_chain_three_nodes() {
        let mut graph = ExecutableGraph::new()
            .with_node(Box::new(AppendFieldNode::new(
                "a",
                "from_a",
                Value::Number(1.0),
            )))
            .with_node(Box::new(AppendFieldNode::new(
                "b",
                "from_b",
                Value::Number(2.0),
            )))
            .with_node(Box::new(AppendFieldNode::new(
                "c",
                "from_c",
                Value::Number(3.0),
            )))
            .with_edge(Edge::new("e1", "a", "b"))
            .with_edge(Edge::new("e2", "b", "c"));

        let output = DagExecutor::execute(&mut graph, NodeData::new()).unwrap();
        assert_eq!(output.get("from_a"), Some(&Value::Number(1.0)));
        assert_eq!(output.get("from_b"), Some(&Value::Number(2.0)));
        assert_eq!(output.get("from_c"), Some(&Value::Number(3.0)));
    }

    #[test]
    fn diamond_graph_b_and_c_in_same_layer() {
        // A → B, A → C, B → D, C → D
        let mut graph = ExecutableGraph::new()
            .with_node(Box::new(AppendFieldNode::new(
                "a",
                "from_a",
                Value::Number(1.0),
            )))
            .with_node(Box::new(AppendFieldNode::new(
                "b",
                "from_b",
                Value::Number(2.0),
            )))
            .with_node(Box::new(AppendFieldNode::new(
                "c",
                "from_c",
                Value::Number(3.0),
            )))
            .with_node(Box::new(AppendFieldNode::new(
                "d",
                "from_d",
                Value::Number(4.0),
            )))
            .with_edge(Edge::new("e1", "a", "b"))
            .with_edge(Edge::new("e2", "a", "c"))
            .with_edge(Edge::new("e3", "b", "d"))
            .with_edge(Edge::new("e4", "c", "d"));

        let output = DagExecutor::execute(&mut graph, NodeData::new()).unwrap();

        // D is the sink — it should have fields from all upstream nodes.
        assert_eq!(output.get("from_a"), Some(&Value::Number(1.0)));
        assert_eq!(output.get("from_d"), Some(&Value::Number(4.0)));
        // B and C both receive A's output and add their own field.
        // D merges B and C outputs — both from_b and from_c should be present.
        assert_eq!(output.get("from_b"), Some(&Value::Number(2.0)));
        assert_eq!(output.get("from_c"), Some(&Value::Number(3.0)));
    }

    #[test]
    fn diamond_topological_layers() {
        let nodes: Vec<Box<dyn Node>> = vec![
            Box::new(PassthroughNode::new("a")),
            Box::new(PassthroughNode::new("b")),
            Box::new(PassthroughNode::new("c")),
            Box::new(PassthroughNode::new("d")),
        ];
        let edges = vec![
            Edge::new("e1", "a", "b"),
            Edge::new("e2", "a", "c"),
            Edge::new("e3", "b", "d"),
            Edge::new("e4", "c", "d"),
        ];

        let layers = DagExecutor::topological_layers(&nodes, &edges).unwrap();
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0], vec![NodeId::new("a")]);
        // B and C in same layer (sorted alphabetically).
        assert_eq!(layers[1], vec![NodeId::new("b"), NodeId::new("c")]);
        assert_eq!(layers[2], vec![NodeId::new("d")]);
    }

    #[test]
    fn cycle_detection() {
        let nodes: Vec<Box<dyn Node>> = vec![
            Box::new(PassthroughNode::new("a")),
            Box::new(PassthroughNode::new("b")),
        ];
        let edges = vec![Edge::new("e1", "a", "b"), Edge::new("e2", "b", "a")];

        let result = DagExecutor::topological_layers(&nodes, &edges);
        assert!(matches!(result, Err(GraphError::CycleDetected)));
    }

    #[test]
    fn cycle_detection_three_nodes() {
        let nodes: Vec<Box<dyn Node>> = vec![
            Box::new(PassthroughNode::new("a")),
            Box::new(PassthroughNode::new("b")),
            Box::new(PassthroughNode::new("c")),
        ];
        let edges = vec![
            Edge::new("e1", "a", "b"),
            Edge::new("e2", "b", "c"),
            Edge::new("e3", "c", "a"),
        ];

        let result = DagExecutor::topological_layers(&nodes, &edges);
        assert!(matches!(result, Err(GraphError::CycleDetected)));
    }

    #[test]
    fn failing_node_propagates_error() {
        let mut graph = ExecutableGraph::new().with_node(Box::new(FailingNode::new("fail")));

        let result = DagExecutor::execute(&mut graph, NodeData::new());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("intentional failure")
        );
    }

    #[test]
    fn multiple_source_nodes() {
        // Two independent source nodes, no edges between them.
        let mut graph = ExecutableGraph::new()
            .with_node(Box::new(AppendFieldNode::new(
                "a",
                "from_a",
                Value::Number(1.0),
            )))
            .with_node(Box::new(AppendFieldNode::new(
                "b",
                "from_b",
                Value::Number(2.0),
            )));

        let output = DagExecutor::execute(&mut graph, NodeData::new()).unwrap();
        // Both are sinks (no outgoing edges), outputs merged.
        assert_eq!(output.get("from_a"), Some(&Value::Number(1.0)));
        assert_eq!(output.get("from_b"), Some(&Value::Number(2.0)));
    }

    // --- TopologyRegistry tests ---

    fn dummy_factory(_config: serde_json::Value) -> Result<Box<dyn Topology>, GraphError> {
        Err(GraphError::ExecutionFailed("not implemented".into()))
    }

    #[test]
    fn registry_register_and_list() {
        let mut reg = TopologyRegistry::new();
        assert!(reg.list().is_empty());

        reg.register("horizontal", dummy_factory);
        reg.register("pingpong", dummy_factory);

        let names = reg.list();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"horizontal"));
        assert!(names.contains(&"pingpong"));
    }

    #[test]
    fn registry_get() {
        let mut reg = TopologyRegistry::new();
        reg.register("horizontal", dummy_factory);

        assert!(reg.get("horizontal").is_some());
        assert!(reg.get("nonexistent").is_none());
    }
}
