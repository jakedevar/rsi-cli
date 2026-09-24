//! Brainstorming topology: sequential critics each see prior output and feedback, then solver.

use crate::data::{NodeData, Value};
use crate::error::GraphError;
use crate::hook::{HookAction, HookContext, HookRegistry, HookStage};
use crate::node::{Node, NodeContext};
use crate::state::{ScopeId, StateStore};

use super::Topology;

/// Brainstorming graph: sequential critics (each sees prior output + accumulated
/// feedback history) feeding a solver that receives all critic feedback.
pub struct BrainstormingGraph {
    critics: Vec<Box<dyn Node>>,
    solver: Box<dyn Node>,
}

impl BrainstormingGraph {
    pub fn new(critics: Vec<Box<dyn Node>>, solver: Box<dyn Node>) -> Self {
        Self { critics, solver }
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

impl Topology for BrainstormingGraph {
    fn name(&self) -> &str {
        "brainstorm"
    }

    fn description(&self) -> &str {
        "Sequential critics feed solver"
    }

    fn execute(
        &mut self,
        input: NodeData,
        state_store: &mut StateStore,
        hooks: &HookRegistry,
    ) -> Result<NodeData, GraphError> {
        let mut current_data = input;
        let mut feedback_history: Vec<Value> = Vec::new();

        // Each critic sees the current data + all prior critics' feedback.
        for critic in &self.critics {
            current_data.insert("_feedback_history", Value::List(feedback_history.clone()));

            let critic_output =
                Self::execute_node(&**critic, current_data.clone(), state_store, hooks)?;

            // Extract this critic's feedback and add to history.
            let feedback_text = match critic_output.get("_feedback") {
                Some(Value::String(s)) => s.clone(),
                Some(other) => format!("{}", other),
                None => String::new(),
            };

            feedback_history.push(Value::String(format!("{}: {}", critic.id(), feedback_text)));

            // Merge critic output into current data so next critic sees it.
            current_data.merge(critic_output);
        }

        // Solver receives original data + all critic feedback.
        current_data.insert("_feedback_history", Value::List(feedback_history));

        let solver_output = Self::execute_node(&*self.solver, current_data, state_store, hooks)?;

        Ok(solver_output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Value;
    use crate::test_nodes::*;

    /// A critic that adds feedback based on its ID.
    struct FeedbackCritic {
        id: crate::node::NodeId,
        name: String,
        feedback: String,
    }

    impl FeedbackCritic {
        fn new(id: &str, feedback: &str) -> Self {
            Self {
                id: crate::node::NodeId::new(id),
                name: format!("critic-{id}"),
                feedback: feedback.to_string(),
            }
        }
    }

    impl Node for FeedbackCritic {
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
            input.insert("_feedback", Value::String(self.feedback.clone()));
            // Track how many prior feedback items this critic saw.
            let prior_count = match input.get("_feedback_history") {
                Some(Value::List(items)) => items.len(),
                _ => 0,
            };
            input.insert(
                format!("_saw_prior_{}", self.id),
                Value::Number(prior_count as f64),
            );
            Ok(input)
        }
    }

    #[test]
    fn three_sequential_critics_feed_solver() {
        let mut topo = BrainstormingGraph::new(
            vec![
                Box::new(FeedbackCritic::new("c1", "point A")),
                Box::new(FeedbackCritic::new("c2", "point B")),
                Box::new(FeedbackCritic::new("c3", "point C")),
            ],
            Box::new(PassthroughNode::new("solver")),
        );
        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();

        // Solver should receive all 3 feedback items.
        let history = output
            .get("_feedback_history")
            .expect("should have feedback history");
        if let Value::List(items) = history {
            assert_eq!(items.len(), 3);
        } else {
            panic!("expected _feedback_history to be a list");
        }
    }

    #[test]
    fn single_critic_works() {
        let mut topo = BrainstormingGraph::new(
            vec![Box::new(FeedbackCritic::new("c1", "only feedback"))],
            Box::new(PassthroughNode::new("solver")),
        );
        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();
        let history = output
            .get("_feedback_history")
            .expect("should have feedback history");
        if let Value::List(items) = history {
            assert_eq!(items.len(), 1);
        } else {
            panic!("expected _feedback_history to be a list");
        }
    }

    #[test]
    fn feedback_accumulates_correctly() {
        let mut topo = BrainstormingGraph::new(
            vec![
                Box::new(FeedbackCritic::new("c1", "first")),
                Box::new(FeedbackCritic::new("c2", "second")),
                Box::new(FeedbackCritic::new("c3", "third")),
            ],
            Box::new(PassthroughNode::new("solver")),
        );
        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();

        // c1 saw 0 prior feedback items.
        assert_eq!(output.get("_saw_prior_c1"), Some(&Value::Number(0.0)));
        // c2 saw 1 prior feedback item (from c1).
        assert_eq!(output.get("_saw_prior_c2"), Some(&Value::Number(1.0)));
        // c3 saw 2 prior feedback items (from c1 and c2).
        assert_eq!(output.get("_saw_prior_c3"), Some(&Value::Number(2.0)));
    }

    #[test]
    fn topology_trait_name_and_description() {
        let topo = BrainstormingGraph::new(vec![], Box::new(PassthroughNode::new("s")));
        assert_eq!(topo.name(), "brainstorm");
        assert_eq!(topo.description(), "Sequential critics feed solver");
    }
}
