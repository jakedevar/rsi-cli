use std::sync::Arc;

use crate::data::NodeData;
use crate::edge::Edge;
use crate::error::GraphError;
use crate::hook::{HookAction, HookContext, HookRegistry, HookStage};
use crate::node::{Node, NodeContext};
use crate::state::{ScopeId, StateStore};

/// A linear executor that runs an ordered sequence of nodes, threading data
/// through edges and dispatching lifecycle hooks at each stage.
pub struct LinearExecutor {
    nodes: Vec<Box<dyn Node>>,
    edges: Vec<Edge>,
    state_store: StateStore,
    hook_registry: Arc<HookRegistry>,
}

impl LinearExecutor {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            edges: Vec::new(),
            state_store: StateStore::new(),
            hook_registry: Arc::new(HookRegistry::new()),
        }
    }

    pub fn with_node(mut self, node: Box<dyn Node>) -> Self {
        self.nodes.push(node);
        self
    }

    pub fn with_edge(mut self, edge: Edge) -> Self {
        self.edges.push(edge);
        self
    }

    pub fn with_hook(mut self, hook: Box<dyn crate::hook::Hook>) -> Self {
        Arc::get_mut(&mut self.hook_registry)
            .expect("hook_registry has no other references during build")
            .register(hook);
        self
    }

    pub fn state_store(&self) -> &StateStore {
        &self.state_store
    }

    pub fn state_store_mut(&mut self) -> &mut StateStore {
        &mut self.state_store
    }

    /// Execute the pipeline, passing `initial_input` through each node in order.
    /// Returns the final node's output, or the initial input if the pipeline is
    /// empty.
    pub fn execute(&mut self, initial_input: NodeData) -> Result<NodeData, GraphError> {
        let mut current_data = initial_input;

        for i in 0..self.nodes.len() {
            let node_id = self.nodes[i].id().clone();
            let node_tags = self.nodes[i].tags().to_vec();

            // Create a per-node scope parented to root.
            let scope_id = ScopeId::new(format!("node_{}", node_id));
            // Ignore error if scope already exists (idempotent re-runs).
            let _ = self
                .state_store
                .create_scope(scope_id.clone(), Some(ScopeId::root()), vec![]);

            // --- BeforeExecute hooks ---
            let hook_ctx = HookContext {
                node_id: &node_id,
                node_tags: &node_tags,
                data: &current_data,
                stage: HookStage::BeforeExecute,
            };

            match self
                .hook_registry
                .dispatch(HookStage::BeforeExecute, &hook_ctx)
            {
                HookAction::Continue => {}
                HookAction::ModifyData(modified) => {
                    current_data = modified;
                }
                HookAction::Abort(err) => return Err(err),
                HookAction::Skip => continue,
            }

            // --- Apply incoming edge filter ---
            if let Some(edge) = self.edges.iter().find(|e| e.target == node_id)
                && let Some(ref filter) = edge.filter
            {
                let filter_ctx = HookContext {
                    node_id: &node_id,
                    node_tags: &node_tags,
                    data: &current_data,
                    stage: HookStage::BeforeFilter,
                };
                self.hook_registry
                    .dispatch(HookStage::BeforeFilter, &filter_ctx);

                current_data = filter.apply(&current_data);

                let filter_ctx = HookContext {
                    node_id: &node_id,
                    node_tags: &node_tags,
                    data: &current_data,
                    stage: HookStage::AfterFilter,
                };
                self.hook_registry
                    .dispatch(HookStage::AfterFilter, &filter_ctx);
            }

            // --- Execute node ---
            let mut ctx = NodeContext {
                node_id: node_id.clone(),
                tags: node_tags.clone(),
                state_scope: Some(scope_id),
                hooks: Some(self.hook_registry.clone()),
                context_registry: None,
            };

            match self.nodes[i].execute(current_data.clone(), &mut ctx) {
                Ok(output) => {
                    // --- AfterExecute hooks ---
                    let hook_ctx = HookContext {
                        node_id: &node_id,
                        node_tags: &node_tags,
                        data: &output,
                        stage: HookStage::AfterExecute,
                    };
                    match self
                        .hook_registry
                        .dispatch(HookStage::AfterExecute, &hook_ctx)
                    {
                        HookAction::ModifyData(modified) => {
                            current_data = modified;
                        }
                        _ => {
                            current_data = output;
                        }
                    }
                }
                Err(err) => {
                    // --- OnError hooks ---
                    // Semantics for OnError hook actions:
                    //   Continue  → error observed but not handled; propagate
                    //   Skip      → error handled; swallow and continue pipeline
                    //   ModifyData→ error recovered; use replacement data
                    //   Abort     → replace error with hook's error
                    let hook_ctx = HookContext {
                        node_id: &node_id,
                        node_tags: &node_tags,
                        data: &current_data,
                        stage: HookStage::OnError,
                    };
                    match self.hook_registry.dispatch(HookStage::OnError, &hook_ctx) {
                        HookAction::Skip => {
                            // Hook handled the error — swallow and continue.
                        }
                        HookAction::ModifyData(modified) => {
                            current_data = modified;
                        }
                        HookAction::Abort(hook_err) => return Err(hook_err),
                        HookAction::Continue => return Err(err),
                    }
                }
            }
        }

        Ok(current_data)
    }
}

impl Default for LinearExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Value;
    use crate::test_nodes::*;

    #[test]
    fn empty_executor_returns_input() {
        let mut exec = LinearExecutor::new();
        let mut input = NodeData::new();
        input.insert("x", Value::Number(1.0));

        let output = exec.execute(input.clone()).unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn single_passthrough_node() {
        let mut exec = LinearExecutor::new().with_node(Box::new(PassthroughNode::new("p1")));

        let mut input = NodeData::new();
        input.insert("key", Value::String("value".into()));

        let output = exec.execute(input.clone()).unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn append_field_node_adds_field() {
        let mut exec = LinearExecutor::new().with_node(Box::new(AppendFieldNode::new(
            "a1",
            "added",
            Value::Bool(true),
        )));

        let input = NodeData::new();
        let output = exec.execute(input).unwrap();
        assert_eq!(output.get("added"), Some(&Value::Bool(true)));
    }

    #[test]
    fn failing_node_propagates_error() {
        let mut exec = LinearExecutor::new().with_node(Box::new(FailingNode::new("fail")));

        let result = exec.execute(NodeData::new());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("intentional failure"));
    }
}
