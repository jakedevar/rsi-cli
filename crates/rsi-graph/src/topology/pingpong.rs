//! Alternating two-node topology: two nodes take turns for N rounds.

use crate::data::{NodeData, Value};
use crate::error::GraphError;
use crate::hook::{ConditionExpression, HookAction, HookContext, HookRegistry, HookStage};
use crate::node::{Node, NodeContext};
use crate::state::{ScopeId, StateStore};

use super::{HistoryPolicy, Topology};

/// Two nodes alternate execution for up to `max_rounds`. Each round consists
/// of node A executing, then node B executing. The output of each node becomes
/// the input to the next.
///
/// Supports optional termination conditions and history management policies.
pub struct PingPongGraph {
    node_a: Box<dyn Node>,
    node_b: Box<dyn Node>,
    max_rounds: usize,
    termination: Option<ConditionExpression>,
    history_policy: HistoryPolicy,
}

impl PingPongGraph {
    pub fn new(a: Box<dyn Node>, b: Box<dyn Node>, max_rounds: usize) -> Self {
        Self {
            node_a: a,
            node_b: b,
            max_rounds,
            termination: None,
            history_policy: HistoryPolicy::KeepAll,
        }
    }

    /// Set a termination condition. After each node B execution, if this
    /// condition evaluates to true, execution stops early.
    pub fn with_termination(mut self, condition: ConditionExpression) -> Self {
        self.termination = Some(condition);
        self
    }

    /// Set the history policy for managing data accumulation across rounds.
    pub fn with_history_policy(mut self, policy: HistoryPolicy) -> Self {
        self.history_policy = policy;
        self
    }

    /// Execute a single node with hooks, returning the output data.
    fn execute_node(
        node: &dyn Node,
        input: NodeData,
        state_store: &mut StateStore,
        hooks: &HookRegistry,
    ) -> Result<NodeData, GraphError> {
        let node_id = node.id().clone();
        let node_tags = node.tags().to_vec();

        let scope_id = ScopeId::new(format!("node_{}", node_id));
        let _ = state_store.create_scope(scope_id.clone(), Some(ScopeId::root()), vec![]);

        let mut current_data = input;

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
            HookAction::Skip => return Ok(current_data),
        }

        // --- Execute ---
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
                Ok(match hooks.dispatch(HookStage::AfterExecute, &hook_ctx) {
                    HookAction::ModifyData(modified) => modified,
                    _ => output,
                })
            }
            Err(err) => {
                let hook_ctx = HookContext {
                    node_id: &node_id,
                    node_tags: &node_tags,
                    data: &current_data,
                    stage: HookStage::OnError,
                };
                match hooks.dispatch(HookStage::OnError, &hook_ctx) {
                    HookAction::Skip => Ok(current_data),
                    HookAction::ModifyData(modified) => Ok(modified),
                    HookAction::Abort(hook_err) => Err(hook_err),
                    HookAction::Continue => Err(err),
                }
            }
        }
    }

    /// Apply history policy to the data flowing between rounds.
    /// Tracks history in a `_history` list field.
    fn apply_history_policy(data: &mut NodeData, policy: &HistoryPolicy) {
        match policy {
            HistoryPolicy::KeepAll => {
                // Accumulate a snapshot into _history.
                let snapshot = Value::Map(
                    data.iter()
                        .filter(|(k, _)| k.as_str() != "_history")
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                );
                let history = data.get("_history").cloned().unwrap_or(Value::List(vec![]));
                if let Value::List(mut items) = history {
                    items.push(snapshot);
                    data.insert("_history", Value::List(items));
                }
            }
            HistoryPolicy::SlidingWindow(n) => {
                let snapshot = Value::Map(
                    data.iter()
                        .filter(|(k, _)| k.as_str() != "_history")
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                );
                let history = data.get("_history").cloned().unwrap_or(Value::List(vec![]));
                if let Value::List(mut items) = history {
                    items.push(snapshot);
                    // Keep only the last N entries.
                    if items.len() > *n {
                        let start = items.len() - n;
                        items = items[start..].to_vec();
                    }
                    data.insert("_history", Value::List(items));
                }
            }
            HistoryPolicy::KeepNone => {
                // Remove any accumulated history.
                data.remove("_history");
            }
        }
    }
}

impl Topology for PingPongGraph {
    fn name(&self) -> &str {
        "pingpong"
    }

    fn description(&self) -> &str {
        "Two nodes alternate for N rounds"
    }

    fn execute(
        &mut self,
        input: NodeData,
        state_store: &mut StateStore,
        hooks: &HookRegistry,
    ) -> Result<NodeData, GraphError> {
        let mut current_data = input;

        // Track round number in the data for condition evaluation.
        current_data.insert("_round", Value::Number(0.0));

        for round in 0..self.max_rounds {
            current_data.insert("_round", Value::Number(round as f64));

            // --- Execute node A ---
            current_data = Self::execute_node(&*self.node_a, current_data, state_store, hooks)?;

            Self::apply_history_policy(&mut current_data, &self.history_policy);

            // --- Execute node B ---
            current_data = Self::execute_node(&*self.node_b, current_data, state_store, hooks)?;

            Self::apply_history_policy(&mut current_data, &self.history_policy);

            // --- Termination check (after B) ---
            if let Some(ref condition) = self.termination
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
    fn single_round_pingpong() {
        let mut topo = PingPongGraph::new(
            Box::new(AppendFieldNode::new("a", "from_a", Value::Number(1.0))),
            Box::new(AppendFieldNode::new("b", "from_b", Value::Number(2.0))),
            1,
        );
        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();
        assert_eq!(output.get("from_a"), Some(&Value::Number(1.0)));
        assert_eq!(output.get("from_b"), Some(&Value::Number(2.0)));
    }

    #[test]
    fn multiple_rounds() {
        // Each round, A adds from_a and B adds from_b. After 3 rounds,
        // the round counter should be 2 (0-indexed last round).
        let mut topo = PingPongGraph::new(
            Box::new(AppendFieldNode::new("a", "from_a", Value::Number(1.0))),
            Box::new(AppendFieldNode::new("b", "from_b", Value::Number(2.0))),
            3,
        );
        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();
        assert_eq!(output.get("from_a"), Some(&Value::Number(1.0)));
        assert_eq!(output.get("from_b"), Some(&Value::Number(2.0)));
        // Round counter present.
        assert_eq!(output.get("_round"), Some(&Value::Number(2.0)));
    }

    #[test]
    fn termination_stops_early() {
        // Node A adds "done" = true on every execution.
        // Termination checks for "done" == true after B.
        // Should stop after round 0.
        let mut topo = PingPongGraph::new(
            Box::new(AppendFieldNode::new("a", "done", Value::Bool(true))),
            Box::new(AppendFieldNode::new("b", "from_b", Value::Number(2.0))),
            10,
        )
        .with_termination(ConditionExpression::FieldEquals {
            field: "done".into(),
            value: Value::Bool(true),
        });

        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();
        assert_eq!(output.get("done"), Some(&Value::Bool(true)));
        // Should have terminated after round 0.
        assert_eq!(output.get("_round"), Some(&Value::Number(0.0)));
    }

    #[test]
    fn history_policy_keep_all() {
        let mut topo = PingPongGraph::new(
            Box::new(AppendFieldNode::new("a", "from_a", Value::Number(1.0))),
            Box::new(AppendFieldNode::new("b", "from_b", Value::Number(2.0))),
            2,
        )
        .with_history_policy(HistoryPolicy::KeepAll);

        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();
        // Should have _history with entries from each node execution.
        let history = output.get("_history");
        assert!(history.is_some());
        if let Some(Value::List(items)) = history {
            // 2 rounds * 2 nodes = 4 history entries.
            assert_eq!(items.len(), 4);
        } else {
            panic!("expected _history to be a list");
        }
    }

    #[test]
    fn history_policy_sliding_window() {
        let mut topo = PingPongGraph::new(
            Box::new(AppendFieldNode::new("a", "from_a", Value::Number(1.0))),
            Box::new(AppendFieldNode::new("b", "from_b", Value::Number(2.0))),
            3,
        )
        .with_history_policy(HistoryPolicy::SlidingWindow(2));

        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();
        let history = output.get("_history");
        assert!(history.is_some());
        if let Some(Value::List(items)) = history {
            // Window size 2 — only last 2 entries kept.
            assert_eq!(items.len(), 2);
        } else {
            panic!("expected _history to be a list");
        }
    }

    #[test]
    fn history_policy_keep_none() {
        let mut topo = PingPongGraph::new(
            Box::new(AppendFieldNode::new("a", "from_a", Value::Number(1.0))),
            Box::new(AppendFieldNode::new("b", "from_b", Value::Number(2.0))),
            2,
        )
        .with_history_policy(HistoryPolicy::KeepNone);

        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();
        // No history should be present.
        assert!(output.get("_history").is_none());
    }

    #[test]
    fn zero_rounds_returns_input() {
        let mut topo = PingPongGraph::new(
            Box::new(AppendFieldNode::new("a", "from_a", Value::Number(1.0))),
            Box::new(AppendFieldNode::new("b", "from_b", Value::Number(2.0))),
            0,
        );
        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let mut input = NodeData::new();
        input.insert("original", Value::Bool(true));

        let output = topo.execute(input, &mut store, &hooks).unwrap();
        assert_eq!(output.get("original"), Some(&Value::Bool(true)));
        // _round is set to 0 before the loop.
        assert_eq!(output.get("_round"), Some(&Value::Number(0.0)));
        // Nodes should not have run.
        assert!(output.get("from_a").is_none());
        assert!(output.get("from_b").is_none());
    }

    #[test]
    fn failing_node_a_propagates_error() {
        let mut topo = PingPongGraph::new(
            Box::new(FailingNode::new("fail-a")),
            Box::new(AppendFieldNode::new("b", "from_b", Value::Number(2.0))),
            1,
        );
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
    fn failing_node_b_propagates_error() {
        let mut topo = PingPongGraph::new(
            Box::new(AppendFieldNode::new("a", "from_a", Value::Number(1.0))),
            Box::new(FailingNode::new("fail-b")),
            1,
        );
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
        let topo = PingPongGraph::new(
            Box::new(PassthroughNode::new("a")),
            Box::new(PassthroughNode::new("b")),
            1,
        );
        assert_eq!(topo.name(), "pingpong");
        assert_eq!(topo.description(), "Two nodes alternate for N rounds");
    }
}
