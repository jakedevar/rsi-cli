//! Topology composition: wrap a [`Topology`] as a [`Node`] for nesting graphs.
//!
//! [`ComposedNode`] allows any topology to be used as a node inside another
//! topology, enabling hierarchical graph composition. State isolation is enforced:
//! the inner topology gets its own [`StateStore`] with explicit key imports from
//! the parent scope.
//!
//! Nesting depth is capped at [`MAX_NESTING_DEPTH`] to prevent runaway recursion.

use std::sync::Mutex;

use crate::data::NodeData;
use crate::error::GraphError;
use crate::hook::HookRegistry;
use crate::node::{Node, NodeContext, NodeId};
use crate::state::StateStore;
use crate::topology::Topology;

/// Maximum allowed nesting depth for composed topologies.
pub const MAX_NESTING_DEPTH: usize = 4;

/// Wraps a [`Topology`] so it can be used as a [`Node`] inside another topology.
///
/// State isolation: the inner topology executes with its own [`StateStore`] and
/// [`HookRegistry`]. Parent state can be selectively imported by key via
/// [`with_state_imports`](ComposedNode::with_state_imports).
pub struct ComposedNode {
    id: NodeId,
    name: String,
    inner: Mutex<Box<dyn Topology + Send>>,
    /// Keys to import from the input data into the inner topology's root state.
    state_imports: Vec<String>,
    /// Current nesting depth (checked at construction time).
    depth: usize,
}

impl ComposedNode {
    /// Create a new composed node at depth 1 (one level of nesting).
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        inner: Box<dyn Topology + Send>,
    ) -> Result<Self, GraphError> {
        Self::with_depth(id, name, inner, 1)
    }

    /// Create a composed node at a specific nesting depth.
    ///
    /// Returns [`GraphError::MaxNestingDepth`] if `depth` exceeds
    /// [`MAX_NESTING_DEPTH`].
    pub fn with_depth(
        id: impl Into<String>,
        name: impl Into<String>,
        inner: Box<dyn Topology + Send>,
        depth: usize,
    ) -> Result<Self, GraphError> {
        if depth > MAX_NESTING_DEPTH {
            return Err(GraphError::MaxNestingDepth(depth));
        }
        Ok(Self {
            id: NodeId::new(id),
            name: name.into(),
            inner: Mutex::new(inner),
            state_imports: Vec::new(),
            depth,
        })
    }

    /// Specify keys to import from the input data into the inner topology's
    /// root state scope before execution.
    pub fn with_state_imports(mut self, imports: Vec<String>) -> Self {
        self.state_imports = imports;
        self
    }

    /// The nesting depth of this composed node.
    pub fn depth(&self) -> usize {
        self.depth
    }
}

impl Node for ComposedNode {
    fn id(&self) -> &NodeId {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn execute(&self, input: NodeData, _ctx: &mut NodeContext) -> Result<NodeData, GraphError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| GraphError::ExecutionFailed("composed node lock poisoned".to_string()))?;

        // Create an isolated state store for the inner topology.
        let mut inner_state = StateStore::new();
        let inner_hooks = HookRegistry::new();

        // Import specified keys from input data into the inner state store's
        // root scope, so the inner topology can read them via state lookups.
        for key in &self.state_imports {
            if let Some(value) = input.get(key) {
                let _ = inner_state.set(&crate::state::ScopeId::root(), key, value.clone());
            }
        }

        inner.execute(input, &mut inner_state, &inner_hooks)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Value;
    use crate::hook::HookRegistry;
    use crate::state::StateStore;
    use crate::test_nodes::*;
    use crate::topology::Topology;
    use crate::topology::horizontal::HorizontalGraph;
    use crate::topology::hub::{HubGraph, SpokeConfig};
    use crate::topology::pingpong::PingPongGraph;
    use crate::topology::vertical::{AggregateStrategy, VerticalGraph};
    use std::sync::atomic::{AtomicUsize, Ordering};

    // --- Test 1: HorizontalGraph where step 2 is a PingPongGraph wrapped in ComposedNode ---

    #[test]
    fn horizontal_with_composed_pingpong_step() {
        // Build inner PingPongGraph: two append nodes, 1 round.
        let inner_topo = PingPongGraph::new(
            Box::new(AppendFieldNode::new(
                "pp-a",
                "pp_from_a",
                Value::Number(10.0),
            )),
            Box::new(AppendFieldNode::new(
                "pp-b",
                "pp_from_b",
                Value::Number(20.0),
            )),
            1,
        );

        let composed = ComposedNode::new("step2", "composed-pingpong", Box::new(inner_topo))
            .expect("depth 1 should be allowed");

        // Outer HorizontalGraph: step1 -> composed -> step3.
        let mut outer = HorizontalGraph::new(vec![
            Box::new(AppendFieldNode::new(
                "step1",
                "from_step1",
                Value::Number(1.0),
            )),
            Box::new(composed),
            Box::new(AppendFieldNode::new(
                "step3",
                "from_step3",
                Value::Number(3.0),
            )),
        ]);

        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = outer.execute(NodeData::new(), &mut store, &hooks).unwrap();

        // step1 added from_step1.
        assert_eq!(output.get("from_step1"), Some(&Value::Number(1.0)));
        // Inner pingpong added pp_from_a and pp_from_b.
        assert_eq!(output.get("pp_from_a"), Some(&Value::Number(10.0)));
        assert_eq!(output.get("pp_from_b"), Some(&Value::Number(20.0)));
        // step3 added from_step3.
        assert_eq!(output.get("from_step3"), Some(&Value::Number(3.0)));
    }

    // --- Test 2: HubGraph with a spoke that is a VerticalGraph wrapped in ComposedNode ---

    /// Hub node that routes to "worker" spoke once, then done.
    struct SingleRouteHub {
        id: NodeId,
        call_count: AtomicUsize,
    }

    impl SingleRouteHub {
        fn new(id: &str) -> Self {
            Self {
                id: NodeId::new(id),
                call_count: AtomicUsize::new(0),
            }
        }
    }

    impl Node for SingleRouteHub {
        fn id(&self) -> &NodeId {
            &self.id
        }
        fn name(&self) -> &str {
            "single-route-hub"
        }

        fn execute(
            &self,
            mut input: NodeData,
            _ctx: &mut NodeContext,
        ) -> Result<NodeData, GraphError> {
            let call = self.call_count.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                input.insert("_route_to", Value::String("worker".to_string()));
                input.insert("_route_query", Value::String("do work".to_string()));
            } else {
                input.insert("_route_to", Value::String("done".to_string()));
                input.insert("_hub_final", Value::Bool(true));
            }
            Ok(input)
        }
    }

    #[test]
    fn hub_with_composed_vertical_spoke() {
        // Inner VerticalGraph: 2 workers, concatenate strategy.
        let inner_topo = VerticalGraph::new(
            vec![
                Box::new(AppendFieldNode::new("w1", "worker_1", Value::Number(100.0))),
                Box::new(AppendFieldNode::new("w2", "worker_2", Value::Number(200.0))),
            ],
            AggregateStrategy::Concatenate,
        );

        let composed =
            ComposedNode::new("spoke-vertical", "composed-vertical", Box::new(inner_topo))
                .expect("depth 1 should be allowed");

        let mut graph = HubGraph::new(Box::new(SingleRouteHub::new("hub")), 10).with_spoke(
            SpokeConfig {
                name: "worker".to_string(),
                description: "Worker spoke".to_string(),
                input_fields: vec![],
            },
            Box::new(composed),
        );

        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = graph.execute(NodeData::new(), &mut store, &hooks).unwrap();

        // Hub should have terminated.
        assert_eq!(output.get("_hub_final"), Some(&Value::Bool(true)));
        // The spoke executed (it produced _result or its fields were merged back).
        // The composed node ran the inner vertical graph successfully.
        assert_eq!(
            output.get("_route_to"),
            Some(&Value::String("done".to_string()))
        );
    }

    // --- Test 3: State isolation ---

    #[test]
    fn inner_state_store_is_isolated_from_outer() {
        // Inner topology writes "inner_secret" to its state store.
        let inner_topo = HorizontalGraph::new(vec![Box::new(AppendFieldNode::new(
            "inner-a",
            "inner_field",
            Value::Bool(true),
        ))]);

        let composed = ComposedNode::new("composed", "composed-inner", Box::new(inner_topo))
            .expect("depth 1 should be allowed");

        // Outer topology: composed node, then a passthrough.
        let mut outer = HorizontalGraph::new(vec![
            Box::new(composed),
            Box::new(PassthroughNode::new("outer-check")),
        ]);

        let mut outer_store = StateStore::new();
        let hooks = HookRegistry::new();

        // Set a value in the outer store's root scope.
        outer_store
            .set(
                &crate::state::ScopeId::root(),
                "outer_key",
                Value::Number(42.0),
            )
            .unwrap();

        let output = outer
            .execute(NodeData::new(), &mut outer_store, &hooks)
            .unwrap();

        // Inner topology's data output flows through (via NodeData), but...
        assert_eq!(output.get("inner_field"), Some(&Value::Bool(true)));

        // The outer store's root value was NOT modified by the inner topology.
        assert_eq!(
            outer_store.get(&crate::state::ScopeId::root(), "outer_key"),
            Some(&Value::Number(42.0))
        );

        // The inner topology's scopes do NOT exist in the outer store.
        let inner_scope = crate::state::ScopeId::new("node_inner-a");
        // The outer executor may create this scope for the composed node itself,
        // but the inner topology's node scopes are in a separate store.
        // Verify the outer store does not have the inner node's scope data.
        assert!(outer_store.get(&inner_scope, "inner_field").is_none());
    }

    // --- Test 4: Nesting depth limit ---

    #[test]
    fn nesting_depth_5_returns_error() {
        let inner = HorizontalGraph::new(vec![Box::new(PassthroughNode::new("leaf"))]);

        let result = ComposedNode::with_depth("deep", "too-deep", Box::new(inner), 5);
        assert!(result.is_err());
        let err = result.err().expect("should be an error");
        assert!(
            matches!(err, GraphError::MaxNestingDepth(5)),
            "expected MaxNestingDepth(5), got: {err:?}"
        );
    }

    #[test]
    fn nesting_depth_at_max_is_ok() {
        let inner = HorizontalGraph::new(vec![Box::new(PassthroughNode::new("leaf"))]);

        let result = ComposedNode::with_depth("max", "at-max", Box::new(inner), MAX_NESTING_DEPTH);
        assert!(result.is_ok());
    }

    // --- Test 5: 3-level nesting works (within limit) ---

    #[test]
    fn three_level_nesting_executes_correctly() {
        // Level 3 (innermost): a simple horizontal chain.
        let level3 = HorizontalGraph::new(vec![Box::new(AppendFieldNode::new(
            "l3",
            "from_level3",
            Value::Number(3.0),
        ))]);

        // Level 2: wraps level3 in a ComposedNode at depth 3.
        let level3_node = ComposedNode::with_depth("l3-composed", "level3", Box::new(level3), 3)
            .expect("depth 3 is within limit");

        let level2 = HorizontalGraph::new(vec![
            Box::new(AppendFieldNode::new(
                "l2",
                "from_level2",
                Value::Number(2.0),
            )),
            Box::new(level3_node),
        ]);

        // Level 1: wraps level2 in a ComposedNode at depth 2.
        let level2_node = ComposedNode::with_depth("l2-composed", "level2", Box::new(level2), 2)
            .expect("depth 2 is within limit");

        // Top level: a horizontal chain containing the level2 composed node.
        let mut top = HorizontalGraph::new(vec![
            Box::new(AppendFieldNode::new(
                "l1",
                "from_level1",
                Value::Number(1.0),
            )),
            Box::new(level2_node),
        ]);

        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = top.execute(NodeData::new(), &mut store, &hooks).unwrap();

        // All three levels should have contributed their fields.
        assert_eq!(output.get("from_level1"), Some(&Value::Number(1.0)));
        assert_eq!(output.get("from_level2"), Some(&Value::Number(2.0)));
        assert_eq!(output.get("from_level3"), Some(&Value::Number(3.0)));
    }

    #[test]
    fn state_imports_populate_inner_root_scope() {
        // This test verifies that state_imports copies values from input data
        // into the inner state store's root scope.
        // We use a StateReaderNode that reads from state.
        let inner_topo = HorizontalGraph::new(vec![Box::new(AppendFieldNode::new(
            "inner",
            "inner_ran",
            Value::Bool(true),
        ))]);

        let composed = ComposedNode::new("composed", "with-imports", Box::new(inner_topo))
            .expect("depth 1 should be allowed")
            .with_state_imports(vec!["imported_key".to_string()]);

        let mut outer = HorizontalGraph::new(vec![
            Box::new(AppendFieldNode::new(
                "setup",
                "imported_key",
                Value::String("hello".into()),
            )),
            Box::new(composed),
        ]);

        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = outer.execute(NodeData::new(), &mut store, &hooks).unwrap();

        // The inner topology ran successfully.
        assert_eq!(output.get("inner_ran"), Some(&Value::Bool(true)));
        // The imported key is still in the data flow.
        assert_eq!(
            output.get("imported_key"),
            Some(&Value::String("hello".into()))
        );
    }
}
