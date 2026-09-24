//! Sequential chain topology: nodes execute in order, output flows to next input.

use crate::data::NodeData;
use crate::error::GraphError;
use crate::hook::{ConditionExpression, HookAction, HookContext, HookRegistry, HookStage};
use crate::node::{Node, NodeContext};
use crate::state::{ScopeId, StateStore};

use super::Topology;

/// Sequential chain: nodes execute in strict order, each node's output becomes
/// the next node's input. Optional early exit via [`ConditionExpression`].
pub struct HorizontalGraph {
    nodes: Vec<Box<dyn Node>>,
    early_exit: Option<ConditionExpression>,
}

impl HorizontalGraph {
    pub fn new(nodes: Vec<Box<dyn Node>>) -> Self {
        Self {
            nodes,
            early_exit: None,
        }
    }

    /// Set an early-exit condition. After each node executes, if this condition
    /// evaluates to true on the output, execution stops and the current output
    /// is returned immediately.
    pub fn with_early_exit(mut self, condition: ConditionExpression) -> Self {
        self.early_exit = Some(condition);
        self
    }
}

impl Topology for HorizontalGraph {
    fn name(&self) -> &str {
        "horizontal"
    }

    fn description(&self) -> &str {
        "Sequential chain: nodes execute in order"
    }

    fn execute(
        &mut self,
        input: NodeData,
        state_store: &mut StateStore,
        hooks: &HookRegistry,
    ) -> Result<NodeData, GraphError> {
        let mut current_data = input;

        for node in &self.nodes {
            let node_id = node.id().clone();
            let node_tags = node.tags().to_vec();

            // Create per-node scope.
            let scope_id = ScopeId::new(format!("node_{}", node_id));
            let _ = state_store.create_scope(scope_id.clone(), Some(ScopeId::root()), vec![]);

            // --- BeforeExecute hooks ---
            let hook_ctx = HookContext {
                node_id: &node_id,
                node_tags: &node_tags,
                data: &current_data,
                stage: HookStage::BeforeExecute,
            };

            match hooks.dispatch(HookStage::BeforeExecute, &hook_ctx) {
                HookAction::Continue => {}
                HookAction::ModifyData(modified) => {
                    current_data = modified;
                }
                HookAction::Abort(err) => return Err(err),
                HookAction::Skip => continue,
            }

            // --- Execute node ---
            let mut ctx = NodeContext {
                node_id: node_id.clone(),
                tags: node_tags.clone(),
                state_scope: Some(scope_id),
                hooks: None,
                context_registry: None,
            };

            match node.execute(current_data.clone(), &mut ctx) {
                Ok(output) => {
                    // --- AfterExecute hooks ---
                    let hook_ctx = HookContext {
                        node_id: &node_id,
                        node_tags: &node_tags,
                        data: &output,
                        stage: HookStage::AfterExecute,
                    };
                    current_data = match hooks.dispatch(HookStage::AfterExecute, &hook_ctx) {
                        HookAction::ModifyData(modified) => modified,
                        _ => output,
                    };
                }
                Err(err) => {
                    // --- OnError hooks ---
                    let hook_ctx = HookContext {
                        node_id: &node_id,
                        node_tags: &node_tags,
                        data: &current_data,
                        stage: HookStage::OnError,
                    };
                    match hooks.dispatch(HookStage::OnError, &hook_ctx) {
                        HookAction::Skip => {}
                        HookAction::ModifyData(modified) => {
                            current_data = modified;
                        }
                        HookAction::Abort(hook_err) => return Err(hook_err),
                        HookAction::Continue => return Err(err),
                    }
                }
            }

            // --- Early exit check ---
            if let Some(ref condition) = self.early_exit
                && condition.evaluate(&current_data)
            {
                return Ok(current_data);
            }
        }

        Ok(current_data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Value;
    use crate::test_nodes::*;

    #[test]
    fn empty_horizontal_returns_input() {
        let mut topo = HorizontalGraph::new(vec![]);
        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let mut input = NodeData::new();
        input.insert("x", Value::Number(1.0));

        let output = topo.execute(input.clone(), &mut store, &hooks).unwrap();
        assert_eq!(output, input);
    }

    #[test]
    fn single_node_horizontal() {
        let mut topo = HorizontalGraph::new(vec![Box::new(AppendFieldNode::new(
            "a",
            "added",
            Value::Bool(true),
        ))]);
        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();
        assert_eq!(output.get("added"), Some(&Value::Bool(true)));
    }

    #[test]
    fn three_node_chain() {
        let mut topo = HorizontalGraph::new(vec![
            Box::new(AppendFieldNode::new("a", "from_a", Value::Number(1.0))),
            Box::new(AppendFieldNode::new("b", "from_b", Value::Number(2.0))),
            Box::new(AppendFieldNode::new("c", "from_c", Value::Number(3.0))),
        ]);
        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();
        assert_eq!(output.get("from_a"), Some(&Value::Number(1.0)));
        assert_eq!(output.get("from_b"), Some(&Value::Number(2.0)));
        assert_eq!(output.get("from_c"), Some(&Value::Number(3.0)));
    }

    #[test]
    fn early_exit_stops_execution() {
        // Node A adds "done" = true, early exit checks for it, Node B should not run.
        let mut topo = HorizontalGraph::new(vec![
            Box::new(AppendFieldNode::new("a", "done", Value::Bool(true))),
            Box::new(AppendFieldNode::new("b", "from_b", Value::Number(2.0))),
        ])
        .with_early_exit(ConditionExpression::FieldEquals {
            field: "done".into(),
            value: Value::Bool(true),
        });

        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();
        assert_eq!(output.get("done"), Some(&Value::Bool(true)));
        // Node B should NOT have run.
        assert!(output.get("from_b").is_none());
    }

    #[test]
    fn early_exit_does_not_trigger_when_condition_false() {
        let mut topo = HorizontalGraph::new(vec![
            Box::new(AppendFieldNode::new("a", "from_a", Value::Number(1.0))),
            Box::new(AppendFieldNode::new("b", "from_b", Value::Number(2.0))),
        ])
        .with_early_exit(ConditionExpression::FieldExists("nonexistent".into()));

        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();
        // Both nodes should have run.
        assert_eq!(output.get("from_a"), Some(&Value::Number(1.0)));
        assert_eq!(output.get("from_b"), Some(&Value::Number(2.0)));
    }

    #[test]
    fn failing_node_propagates_error() {
        let mut topo = HorizontalGraph::new(vec![Box::new(FailingNode::new("fail"))]);
        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let result = topo.execute(NodeData::new(), &mut store, &hooks);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("intentional failure")
        );
    }

    #[test]
    fn topology_trait_name_and_description() {
        let topo = HorizontalGraph::new(vec![]);
        assert_eq!(topo.name(), "horizontal");
        assert_eq!(
            topo.description(),
            "Sequential chain: nodes execute in order"
        );
    }
}
