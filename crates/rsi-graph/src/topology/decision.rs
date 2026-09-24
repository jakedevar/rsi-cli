//! Vertical decision topology: critics evaluate, solver improves, loop until consensus.

use crate::data::{NodeData, Value};
use crate::error::GraphError;
use crate::hook::{HookAction, HookContext, HookRegistry, HookStage};
use crate::node::{Node, NodeContext};
use crate::state::{ScopeId, StateStore};

use super::Topology;

/// Vertical decision graph: fan-out to N critics, aggregate verdicts, pass to
/// solver, repeat until consensus threshold is met or max iterations reached.
pub struct VerticalDecisionGraph {
    critics: Vec<Box<dyn Node>>,
    solver: Box<dyn Node>,
    max_iterations: usize,
    consensus_threshold: f64,
}

impl VerticalDecisionGraph {
    pub fn new(critics: Vec<Box<dyn Node>>, solver: Box<dyn Node>, max_iterations: usize) -> Self {
        Self {
            critics,
            solver,
            max_iterations,
            consensus_threshold: 0.8,
        }
    }

    /// Set the consensus threshold (fraction of critics that must approve).
    pub fn with_threshold(mut self, threshold: f64) -> Self {
        self.consensus_threshold = threshold;
        self
    }

    /// Execute a single node with hooks.
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
}

impl Topology for VerticalDecisionGraph {
    fn name(&self) -> &str {
        "vertical_decision"
    }

    fn description(&self) -> &str {
        "Critics evaluate, solver improves, loop until consensus"
    }

    fn execute(
        &mut self,
        input: NodeData,
        state_store: &mut StateStore,
        hooks: &HookRegistry,
    ) -> Result<NodeData, GraphError> {
        let mut current_data = input;
        let mut converged = false;

        for iteration in 0..self.max_iterations {
            current_data.insert("_iteration", Value::Number(iteration as f64));

            // Fan-out to all critics.
            let mut approvals = 0usize;
            let mut feedback_list: Vec<Value> = Vec::new();
            let total = self.critics.len();

            for critic in &self.critics {
                let critic_output =
                    Self::execute_node(&**critic, current_data.clone(), state_store, hooks)?;

                // Check if critic approved.
                let approved = matches!(critic_output.get("_approved"), Some(Value::Bool(true)));
                if approved {
                    approvals += 1;
                }

                // Collect feedback.
                let feedback_text = match critic_output.get("_feedback") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => format!("{}", other),
                    None => String::new(),
                };

                feedback_list.push(Value::String(format!("{}: {}", critic.id(), feedback_text)));
            }

            // Check consensus.
            let approval_ratio = if total > 0 {
                approvals as f64 / total as f64
            } else {
                1.0
            };

            if approval_ratio >= self.consensus_threshold {
                converged = true;
                current_data.insert("_converged", Value::Bool(true));
                current_data.insert("_final_iteration", Value::Number(iteration as f64));
                current_data.insert("_approval_ratio", Value::Number(approval_ratio));
                break;
            }

            // Aggregate feedback and pass to solver.
            current_data.insert("_feedback", Value::List(feedback_list));
            current_data.insert("_approval_ratio", Value::Number(approval_ratio));

            // Solver produces improved output.
            current_data = Self::execute_node(&*self.solver, current_data, state_store, hooks)?;
        }

        if !converged {
            current_data.insert("_converged", Value::Bool(false));
        }

        Ok(current_data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Value;
    use crate::test_nodes::*;

    /// A critic that always approves.
    struct ApprovingCritic {
        id: crate::node::NodeId,
        name: String,
    }

    impl ApprovingCritic {
        fn new(id: &str) -> Self {
            Self {
                id: crate::node::NodeId::new(id),
                name: format!("approving-{id}"),
            }
        }
    }

    impl Node for ApprovingCritic {
        fn id(&self) -> &crate::node::NodeId {
            &self.id
        }
        fn name(&self) -> &str {
            &self.name
        }

        fn execute(
            &self,
            mut input: NodeData,
            _ctx: &mut NodeContext,
        ) -> Result<NodeData, GraphError> {
            input.insert("_approved", Value::Bool(true));
            input.insert("_feedback", Value::String("looks good".into()));
            Ok(input)
        }
    }

    /// A critic that always rejects.
    struct RejectingCritic {
        id: crate::node::NodeId,
        name: String,
    }

    impl RejectingCritic {
        fn new(id: &str) -> Self {
            Self {
                id: crate::node::NodeId::new(id),
                name: format!("rejecting-{id}"),
            }
        }
    }

    impl Node for RejectingCritic {
        fn id(&self) -> &crate::node::NodeId {
            &self.id
        }
        fn name(&self) -> &str {
            &self.name
        }

        fn execute(
            &self,
            mut input: NodeData,
            _ctx: &mut NodeContext,
        ) -> Result<NodeData, GraphError> {
            input.insert("_approved", Value::Bool(false));
            input.insert("_feedback", Value::String("needs improvement".into()));
            Ok(input)
        }
    }

    #[test]
    fn all_critics_approve_early_exit() {
        let mut topo = VerticalDecisionGraph::new(
            vec![
                Box::new(ApprovingCritic::new("c1")),
                Box::new(ApprovingCritic::new("c2")),
                Box::new(ApprovingCritic::new("c3")),
            ],
            Box::new(PassthroughNode::new("solver")),
            5,
        );
        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();
        assert_eq!(output.get("_converged"), Some(&Value::Bool(true)));
        // Should exit at iteration 0.
        assert_eq!(output.get("_final_iteration"), Some(&Value::Number(0.0)));
    }

    #[test]
    fn two_of_three_approve_above_threshold() {
        // 2/3 = 0.667, threshold 0.6 -> should pass.
        let mut topo = VerticalDecisionGraph::new(
            vec![
                Box::new(ApprovingCritic::new("c1")),
                Box::new(ApprovingCritic::new("c2")),
                Box::new(RejectingCritic::new("c3")),
            ],
            Box::new(PassthroughNode::new("solver")),
            5,
        )
        .with_threshold(0.6);

        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();
        assert_eq!(output.get("_converged"), Some(&Value::Bool(true)));
        assert_eq!(output.get("_final_iteration"), Some(&Value::Number(0.0)));
    }

    #[test]
    fn no_consensus_runs_all_iterations() {
        // All critics reject, threshold 0.8 -> never converges.
        let mut topo = VerticalDecisionGraph::new(
            vec![
                Box::new(RejectingCritic::new("c1")),
                Box::new(RejectingCritic::new("c2")),
                Box::new(RejectingCritic::new("c3")),
            ],
            Box::new(PassthroughNode::new("solver")),
            3,
        );
        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();
        assert_eq!(output.get("_converged"), Some(&Value::Bool(false)));
    }

    #[test]
    fn solver_improves_each_round() {
        // Solver adds an "improved" field. After iterations, it should be present.
        let mut topo = VerticalDecisionGraph::new(
            vec![Box::new(RejectingCritic::new("c1"))],
            Box::new(AppendFieldNode::new(
                "solver",
                "improved",
                Value::Bool(true),
            )),
            2,
        );
        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();
        // Solver should have added the "improved" field.
        assert_eq!(output.get("improved"), Some(&Value::Bool(true)));
        assert_eq!(output.get("_converged"), Some(&Value::Bool(false)));
    }

    #[test]
    fn topology_trait_name_and_description() {
        let topo = VerticalDecisionGraph::new(vec![], Box::new(PassthroughNode::new("s")), 1);
        assert_eq!(topo.name(), "vertical_decision");
        assert_eq!(
            topo.description(),
            "Critics evaluate, solver improves, loop until consensus"
        );
    }
}
