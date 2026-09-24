//! Fan-out/aggregate topology: distribute work to N parallel workers, then aggregate results.

use crate::data::{NodeData, Value};
use crate::error::GraphError;
use crate::hook::{HookAction, HookContext, HookRegistry, HookStage};
use crate::node::{Node, NodeContext};
use crate::state::{ScopeId, StateStore};

use super::Topology;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Strategy for aggregating worker outputs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AggregateStrategy {
    /// Concatenate all outputs into a list.
    Concatenate,
    /// Majority vote on a specified field.
    MajorityVote { field: String },
    /// Named reducer (looked up from registry at execution time).
    NamedReducer(String),
}

/// Fan-out to N workers, then aggregate results.
///
/// Each worker receives a clone of the input and executes independently.
/// Results are aggregated according to the configured [`AggregateStrategy`].
/// Workers that fail are skipped with a note in the `_failures` field.
pub struct VerticalGraph {
    workers: Vec<Box<dyn Node>>,
    aggregator: AggregateStrategy,
    concurrency_limit: Option<usize>,
}

impl VerticalGraph {
    pub fn new(workers: Vec<Box<dyn Node>>, aggregator: AggregateStrategy) -> Self {
        Self {
            workers,
            aggregator,
            concurrency_limit: None,
        }
    }

    /// Set a concurrency limit. Currently noted for future async implementation;
    /// the synchronous executor processes workers sequentially regardless.
    pub fn with_concurrency_limit(mut self, limit: usize) -> Self {
        self.concurrency_limit = Some(limit);
        self
    }

    /// Execute a single worker node with hooks.
    fn execute_worker(
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

    /// Apply the aggregation strategy to collected worker outputs.
    fn aggregate(
        strategy: &AggregateStrategy,
        results: Vec<NodeData>,
        failures: Vec<String>,
    ) -> NodeData {
        let mut output = NodeData::new();

        match strategy {
            AggregateStrategy::Concatenate => {
                let result_values: Vec<Value> = results
                    .into_iter()
                    .map(|data| {
                        let map: BTreeMap<String, Value> =
                            data.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                        Value::Map(map)
                    })
                    .collect();
                output.insert("_results", Value::List(result_values));
            }
            AggregateStrategy::MajorityVote { field } => {
                // Count occurrences of each value for the specified field.
                let mut counts: BTreeMap<String, usize> = BTreeMap::new();
                let mut value_map: BTreeMap<String, Value> = BTreeMap::new();

                for data in &results {
                    if let Some(val) = data.get(field) {
                        let key = format!("{}", val);
                        *counts.entry(key.clone()).or_insert(0) += 1;
                        value_map.entry(key).or_insert_with(|| val.clone());
                    }
                }

                // Pick the most common value.
                if let Some((winner_key, _count)) = counts.iter().max_by_key(|(_k, v)| *v)
                    && let Some(winner_value) = value_map.get(winner_key)
                {
                    output.insert(field.clone(), winner_value.clone());
                }

                output.insert(
                    "_vote_counts",
                    Value::Map(
                        counts
                            .into_iter()
                            .map(|(k, v)| (k, Value::Number(v as f64)))
                            .collect(),
                    ),
                );
            }
            AggregateStrategy::NamedReducer(name) => {
                // Store the reducer name and raw results for external processing.
                output.insert("_reducer", Value::String(name.clone()));
                let result_values: Vec<Value> = results
                    .into_iter()
                    .map(|data| {
                        let map: BTreeMap<String, Value> =
                            data.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                        Value::Map(map)
                    })
                    .collect();
                output.insert("_results", Value::List(result_values));
            }
        }

        if !failures.is_empty() {
            output.insert(
                "_failures",
                Value::List(failures.into_iter().map(Value::String).collect()),
            );
        }

        output
    }
}

impl Topology for VerticalGraph {
    fn name(&self) -> &str {
        "vertical"
    }

    fn description(&self) -> &str {
        "Fan-out to parallel workers with aggregation"
    }

    fn execute(
        &mut self,
        input: NodeData,
        state_store: &mut StateStore,
        hooks: &HookRegistry,
    ) -> Result<NodeData, GraphError> {
        let mut results = Vec::new();
        let mut failures = Vec::new();

        for worker in &self.workers {
            let worker_input = input.clone();
            match Self::execute_worker(&**worker, worker_input, state_store, hooks) {
                Ok(output) => results.push(output),
                Err(err) => {
                    failures.push(format!("{}: {}", worker.id(), err));
                }
            }
        }

        // If all workers failed, that is an error.
        if results.is_empty() && !self.workers.is_empty() {
            return Err(GraphError::ExecutionFailed(format!(
                "all {} workers failed: {}",
                self.workers.len(),
                failures.join("; ")
            )));
        }

        Ok(Self::aggregate(&self.aggregator, results, failures))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Value;
    use crate::test_nodes::*;

    #[test]
    fn three_workers_concatenate() {
        let mut topo = VerticalGraph::new(
            vec![
                Box::new(AppendFieldNode::new("w1", "from_w1", Value::Number(1.0))),
                Box::new(AppendFieldNode::new("w2", "from_w2", Value::Number(2.0))),
                Box::new(AppendFieldNode::new("w3", "from_w3", Value::Number(3.0))),
            ],
            AggregateStrategy::Concatenate,
        );
        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();
        let results = output.get("_results").expect("_results should exist");
        if let Value::List(items) = results {
            assert_eq!(items.len(), 3);
        } else {
            panic!("expected _results to be a list");
        }
    }

    #[test]
    fn majority_vote_picks_winner() {
        // 2 workers say "yes", 1 says "no".
        let mut topo = VerticalGraph::new(
            vec![
                Box::new(AppendFieldNode::new(
                    "w1",
                    "answer",
                    Value::String("yes".into()),
                )),
                Box::new(AppendFieldNode::new(
                    "w2",
                    "answer",
                    Value::String("yes".into()),
                )),
                Box::new(AppendFieldNode::new(
                    "w3",
                    "answer",
                    Value::String("no".into()),
                )),
            ],
            AggregateStrategy::MajorityVote {
                field: "answer".into(),
            },
        );
        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();
        assert_eq!(output.get("answer"), Some(&Value::String("yes".into())));

        // Vote counts should be present.
        let counts = output
            .get("_vote_counts")
            .expect("_vote_counts should exist");
        if let Value::Map(map) = counts {
            assert_eq!(map.get("yes"), Some(&Value::Number(2.0)));
            assert_eq!(map.get("no"), Some(&Value::Number(1.0)));
        } else {
            panic!("expected _vote_counts to be a map");
        }
    }

    #[test]
    fn partial_failure_skips_failed_workers() {
        // 2 good workers + 1 failing worker.
        let mut topo = VerticalGraph::new(
            vec![
                Box::new(AppendFieldNode::new("w1", "from_w1", Value::Number(1.0))),
                Box::new(FailingNode::new("w_fail")),
                Box::new(AppendFieldNode::new("w3", "from_w3", Value::Number(3.0))),
            ],
            AggregateStrategy::Concatenate,
        );
        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();

        // Should have 2 results.
        let results = output.get("_results").expect("_results should exist");
        if let Value::List(items) = results {
            assert_eq!(items.len(), 2);
        } else {
            panic!("expected _results to be a list");
        }

        // Failure note should be present.
        let failures = output.get("_failures").expect("_failures should exist");
        if let Value::List(items) = failures {
            assert_eq!(items.len(), 1);
            if let Value::String(msg) = &items[0] {
                assert!(msg.contains("w_fail"));
            }
        } else {
            panic!("expected _failures to be a list");
        }
    }

    #[test]
    fn single_worker_works() {
        let mut topo = VerticalGraph::new(
            vec![Box::new(AppendFieldNode::new(
                "w1",
                "from_w1",
                Value::Number(42.0),
            ))],
            AggregateStrategy::Concatenate,
        );
        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();
        let results = output.get("_results").expect("_results should exist");
        if let Value::List(items) = results {
            assert_eq!(items.len(), 1);
        } else {
            panic!("expected _results to be a list");
        }
    }

    #[test]
    fn topology_trait_name_and_description() {
        let topo = VerticalGraph::new(vec![], AggregateStrategy::Concatenate);
        assert_eq!(topo.name(), "vertical");
        assert_eq!(
            topo.description(),
            "Fan-out to parallel workers with aggregation"
        );
    }

    #[test]
    fn concurrency_limit_builder() {
        let topo =
            VerticalGraph::new(vec![], AggregateStrategy::Concatenate).with_concurrency_limit(4);
        assert_eq!(topo.concurrency_limit, Some(4));
    }

    #[test]
    fn named_reducer_collects_all_results() {
        let mut topo = VerticalGraph::new(
            vec![
                Box::new(AppendFieldNode::new(
                    "w1",
                    "answer",
                    Value::String("alpha".into()),
                )),
                Box::new(AppendFieldNode::new(
                    "w2",
                    "answer",
                    Value::String("beta".into()),
                )),
                Box::new(AppendFieldNode::new(
                    "w3",
                    "answer",
                    Value::String("gamma".into()),
                )),
            ],
            AggregateStrategy::NamedReducer("custom_agg".into()),
        );
        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = topo.execute(NodeData::new(), &mut store, &hooks).unwrap();

        // NamedReducer stores reducer name under "_reducer"
        let reducer_name = output.get("_reducer").expect("_reducer should exist");
        assert_eq!(*reducer_name, Value::String("custom_agg".into()));

        // All results stored as Value::List under "_results"
        let results = output.get("_results").expect("_results should exist");
        match results {
            Value::List(items) => {
                assert_eq!(items.len(), 3, "Should collect all 3 worker results");
            }
            _ => panic!(
                "NamedReducer should produce a Value::List under _results, got {:?}",
                results
            ),
        }
    }
}
