//! Hub-spoke topology: a hub node routes work to spoke nodes via synthesized handoff tools.

use crate::data::{NodeData, Value};
use crate::error::GraphError;
use crate::hook::{HookAction, HookContext, HookRegistry, HookStage};
use crate::node::{Node, NodeContext};
use crate::retrieval::ToolDefinition;
use crate::state::{ScopeId, StateStore};

use super::Topology;

use serde::{Deserialize, Serialize};

/// Metadata about a spoke that the hub can route to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpokeConfig {
    pub name: String,
    pub description: String,
    /// Fields the spoke expects as input.
    pub input_fields: Vec<String>,
}

/// Routing decision recorded in shared state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingDecision {
    pub spoke_name: String,
    pub query: String,
    pub round: usize,
}

/// Hub-spoke topology: hub routes to spokes via tool-like interface.
///
/// The hub node outputs `_route_to` and `_route_query` fields to select a spoke.
/// When `_route_to` is `"done"` or absent, the hub terminates. Spoke results are
/// fed back to the hub in the `_spoke_result` field.
pub struct HubGraph {
    hub: Box<dyn Node>,
    spokes: Vec<(SpokeConfig, Box<dyn Node>)>,
    max_rounds: usize,
}

impl HubGraph {
    pub fn new(hub: Box<dyn Node>, max_rounds: usize) -> Self {
        Self {
            hub,
            spokes: Vec::new(),
            max_rounds,
        }
    }

    pub fn with_spoke(mut self, config: SpokeConfig, node: Box<dyn Node>) -> Self {
        self.spokes.push((config, node));
        self
    }

    /// Generate handoff tool definitions for the hub.
    ///
    /// For each spoke, creates a `delegate_to_{name}(query: String)` tool.
    /// Also creates a `"done"` tool for the hub to signal completion.
    pub fn synthesize_tools(&self) -> Vec<ToolDefinition> {
        use crate::retrieval::{ToolParameters, ToolProperty};
        use std::collections::BTreeMap;

        let mut tools = Vec::with_capacity(self.spokes.len() + 1);

        for (config, _) in &self.spokes {
            let mut properties = BTreeMap::new();
            properties.insert(
                "query".to_string(),
                ToolProperty {
                    property_type: "string".to_string(),
                    description: format!("Query to send to the {} spoke", config.name),
                },
            );

            tools.push(ToolDefinition {
                name: format!("delegate_to_{}", config.name),
                description: config.description.clone(),
                parameters: ToolParameters {
                    required: vec!["query".to_string()],
                    properties,
                },
            });
        }

        // "done" tool for the hub to signal completion.
        let mut done_properties = BTreeMap::new();
        done_properties.insert(
            "result".to_string(),
            ToolProperty {
                property_type: "string".to_string(),
                description: "Final result summary".to_string(),
            },
        );
        tools.push(ToolDefinition {
            name: "done".to_string(),
            description: "Signal that routing is complete".to_string(),
            parameters: ToolParameters {
                required: vec![],
                properties: done_properties,
            },
        });

        tools
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

    /// Find the index of a spoke by name.
    fn find_spoke(&self, name: &str) -> Option<usize> {
        self.spokes.iter().position(|(cfg, _)| cfg.name == name)
    }
}

impl Topology for HubGraph {
    fn name(&self) -> &str {
        "hub"
    }

    fn description(&self) -> &str {
        "Hub routes work to spoke nodes"
    }

    fn execute(
        &mut self,
        input: NodeData,
        state_store: &mut StateStore,
        hooks: &HookRegistry,
    ) -> Result<NodeData, GraphError> {
        // Create the hub scope to store routing decisions.
        let hub_scope = ScopeId::new("hub_routing");
        let _ = state_store.create_scope(hub_scope.clone(), Some(ScopeId::root()), vec![]);

        let mut hub_input = input;
        let mut routing_decisions: Vec<RoutingDecision> = Vec::new();

        for round in 0..self.max_rounds {
            hub_input.insert("_round", Value::Number(round as f64));

            // Execute hub node.
            let hub_output = Self::execute_node(&*self.hub, hub_input, state_store, hooks)?;

            // Check for routing decision.
            let route_to = hub_output.get("_route_to").and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            });

            let route_query = hub_output
                .get("_route_query")
                .and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_default();

            let is_done = match &route_to {
                None => true,
                Some(s) if s == "done" => true,
                _ => false,
            };

            if is_done {
                // Store routing decisions in state.
                let decisions_value = Value::List(
                    routing_decisions
                        .iter()
                        .map(|d| {
                            Value::Map(
                                [
                                    (
                                        "spoke_name".to_string(),
                                        Value::String(d.spoke_name.clone()),
                                    ),
                                    ("query".to_string(), Value::String(d.query.clone())),
                                    ("round".to_string(), Value::Number(d.round as f64)),
                                ]
                                .into_iter()
                                .collect(),
                            )
                        })
                        .collect(),
                );
                let _ = state_store.set(&hub_scope, "routing_decisions", decisions_value);
                return Ok(hub_output);
            }

            // Route to spoke.
            let spoke_name = route_to.expect("route_to is Some when not done");

            // Record the routing decision.
            routing_decisions.push(RoutingDecision {
                spoke_name: spoke_name.clone(),
                query: route_query.clone(),
                round,
            });

            // Find matching spoke.
            let spoke_idx = match self.find_spoke(&spoke_name) {
                Some(idx) => idx,
                None => {
                    return Err(GraphError::NodeNotFound(format!(
                        "spoke '{}' not found",
                        spoke_name
                    )));
                }
            };

            // Prepare spoke input: hub output plus the route query.
            let mut spoke_input = hub_output.clone();
            spoke_input.insert("_route_query", Value::String(route_query));

            // Execute spoke.
            let spoke_output =
                Self::execute_node(&*self.spokes[spoke_idx].1, spoke_input, state_store, hooks)?;

            // Feed spoke result back as hub's next input.
            let spoke_result = spoke_output.get("_result").cloned().unwrap_or_else(|| {
                // Fall back to serializing all non-underscore fields.
                Value::String(
                    spoke_output
                        .iter()
                        .filter(|(k, _)| !k.starts_with('_'))
                        .map(|(k, v)| format!("{k}: {v}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                )
            });

            hub_input = hub_output;
            hub_input.insert("_spoke_result", spoke_result);
            hub_input.insert("_last_spoke", Value::String(spoke_name));
        }

        // Store routing decisions even on max-rounds exit.
        let decisions_value = Value::List(
            routing_decisions
                .iter()
                .map(|d| {
                    Value::Map(
                        [
                            (
                                "spoke_name".to_string(),
                                Value::String(d.spoke_name.clone()),
                            ),
                            ("query".to_string(), Value::String(d.query.clone())),
                            ("round".to_string(), Value::Number(d.round as f64)),
                        ]
                        .into_iter()
                        .collect(),
                    )
                })
                .collect(),
        );
        let _ = state_store.set(&hub_scope, "routing_decisions", decisions_value);

        // Return the last hub input (which includes the last spoke result).
        Ok(hub_input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Value;
    use crate::error::GraphError;
    use crate::node::{Node, NodeContext, NodeId};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Hub node that cycles through spoke names, then returns "done".
    struct RoutingHubNode {
        id: NodeId,
        route_sequence: Vec<String>,
        call_count: AtomicUsize,
    }

    impl RoutingHubNode {
        fn new(id: &str, route_sequence: Vec<&str>) -> Self {
            Self {
                id: NodeId::new(id),
                route_sequence: route_sequence.into_iter().map(String::from).collect(),
                call_count: AtomicUsize::new(0),
            }
        }
    }

    impl Node for RoutingHubNode {
        fn id(&self) -> &NodeId {
            &self.id
        }

        fn name(&self) -> &str {
            "routing-hub"
        }

        fn execute(
            &self,
            mut input: NodeData,
            _ctx: &mut NodeContext,
        ) -> Result<NodeData, GraphError> {
            let call = self.call_count.fetch_add(1, Ordering::SeqCst);

            if call < self.route_sequence.len() {
                let spoke = &self.route_sequence[call];
                input.insert("_route_to", Value::String(spoke.clone()));
                input.insert("_route_query", Value::String(format!("query for {spoke}")));
            } else {
                input.insert("_route_to", Value::String("done".to_string()));
                input.insert("_final", Value::Bool(true));
            }

            Ok(input)
        }
    }

    /// Spoke node that returns input with a marker.
    struct EchoSpokeNode {
        id: NodeId,
        marker: String,
    }

    impl EchoSpokeNode {
        fn new(id: &str, marker: &str) -> Self {
            Self {
                id: NodeId::new(id),
                marker: marker.to_string(),
            }
        }
    }

    impl Node for EchoSpokeNode {
        fn id(&self) -> &NodeId {
            &self.id
        }

        fn name(&self) -> &str {
            "echo-spoke"
        }

        fn execute(
            &self,
            mut input: NodeData,
            _ctx: &mut NodeContext,
        ) -> Result<NodeData, GraphError> {
            input.insert(
                "_result",
                Value::String(format!("spoke_{}_processed", self.marker)),
            );
            input.insert(&self.marker, Value::Bool(true));
            Ok(input)
        }
    }

    #[test]
    fn hub_routes_to_spoke_a_then_b_then_done() {
        let hub = RoutingHubNode::new("hub", vec!["alpha", "beta"]);

        let mut graph = HubGraph::new(Box::new(hub), 10)
            .with_spoke(
                SpokeConfig {
                    name: "alpha".to_string(),
                    description: "Alpha spoke".to_string(),
                    input_fields: vec!["query".to_string()],
                },
                Box::new(EchoSpokeNode::new("spoke-a", "alpha_marker")),
            )
            .with_spoke(
                SpokeConfig {
                    name: "beta".to_string(),
                    description: "Beta spoke".to_string(),
                    input_fields: vec!["query".to_string()],
                },
                Box::new(EchoSpokeNode::new("spoke-b", "beta_marker")),
            );

        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = graph.execute(NodeData::new(), &mut store, &hooks).unwrap();

        // Hub should have terminated with "done".
        assert_eq!(
            output.get("_route_to"),
            Some(&Value::String("done".to_string()))
        );
        assert_eq!(output.get("_final"), Some(&Value::Bool(true)));

        // Should have received spoke results along the way.
        // The last spoke result should be from beta.
        assert_eq!(
            output.get("_last_spoke"),
            Some(&Value::String("beta".to_string()))
        );
    }

    #[test]
    fn hub_with_no_routing_terminates_immediately() {
        // Hub that immediately returns "done".
        let hub = RoutingHubNode::new("hub", vec![]);

        let mut graph = HubGraph::new(Box::new(hub), 10);
        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let output = graph.execute(NodeData::new(), &mut store, &hooks).unwrap();
        assert_eq!(
            output.get("_route_to"),
            Some(&Value::String("done".to_string()))
        );
        assert_eq!(output.get("_final"), Some(&Value::Bool(true)));
    }

    #[test]
    fn unknown_spoke_name_returns_error() {
        // Hub routes to "nonexistent" spoke.
        let hub = RoutingHubNode::new("hub", vec!["nonexistent"]);

        let mut graph = HubGraph::new(Box::new(hub), 10).with_spoke(
            SpokeConfig {
                name: "alpha".to_string(),
                description: "Alpha spoke".to_string(),
                input_fields: vec![],
            },
            Box::new(EchoSpokeNode::new("spoke-a", "alpha_marker")),
        );

        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        let result = graph.execute(NodeData::new(), &mut store, &hooks);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("nonexistent"),
            "error should mention the unknown spoke name, got: {err}"
        );
    }

    #[test]
    fn routing_decisions_visible_in_state_store() {
        let hub = RoutingHubNode::new("hub", vec!["alpha", "beta"]);

        let mut graph = HubGraph::new(Box::new(hub), 10)
            .with_spoke(
                SpokeConfig {
                    name: "alpha".to_string(),
                    description: "Alpha spoke".to_string(),
                    input_fields: vec![],
                },
                Box::new(EchoSpokeNode::new("spoke-a", "alpha_marker")),
            )
            .with_spoke(
                SpokeConfig {
                    name: "beta".to_string(),
                    description: "Beta spoke".to_string(),
                    input_fields: vec![],
                },
                Box::new(EchoSpokeNode::new("spoke-b", "beta_marker")),
            );

        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        graph.execute(NodeData::new(), &mut store, &hooks).unwrap();

        // Check that routing decisions were stored.
        let hub_scope = ScopeId::new("hub_routing");
        let decisions = store.get(&hub_scope, "routing_decisions");
        assert!(
            decisions.is_some(),
            "routing_decisions should be in state store"
        );

        if let Some(Value::List(items)) = decisions {
            assert_eq!(items.len(), 2, "should have 2 routing decisions");

            // First decision: alpha
            if let Value::Map(ref m) = items[0] {
                assert_eq!(
                    m.get("spoke_name"),
                    Some(&Value::String("alpha".to_string()))
                );
                assert_eq!(m.get("round"), Some(&Value::Number(0.0)));
            } else {
                panic!("expected map in routing decisions");
            }

            // Second decision: beta
            if let Value::Map(ref m) = items[1] {
                assert_eq!(
                    m.get("spoke_name"),
                    Some(&Value::String("beta".to_string()))
                );
                assert_eq!(m.get("round"), Some(&Value::Number(1.0)));
            } else {
                panic!("expected map in routing decisions");
            }
        } else {
            panic!("expected routing_decisions to be a list");
        }
    }

    #[test]
    fn synthesize_tools_generates_correct_definitions() {
        let hub = RoutingHubNode::new("hub", vec![]);
        let graph = HubGraph::new(Box::new(hub), 1).with_spoke(
            SpokeConfig {
                name: "search".to_string(),
                description: "Search spoke".to_string(),
                input_fields: vec!["query".to_string()],
            },
            Box::new(EchoSpokeNode::new("spoke-search", "search_marker")),
        );

        let tools = graph.synthesize_tools();
        assert_eq!(tools.len(), 2); // delegate_to_search + done
        assert_eq!(tools[0].name, "delegate_to_search");
        assert_eq!(tools[1].name, "done");
    }

    #[test]
    fn max_rounds_limits_execution() {
        // Hub that always routes to alpha (never returns done).
        struct InfiniteRouterNode {
            id: NodeId,
        }

        impl Node for InfiniteRouterNode {
            fn id(&self) -> &NodeId {
                &self.id
            }
            fn name(&self) -> &str {
                "infinite-router"
            }
            fn execute(
                &self,
                mut input: NodeData,
                _ctx: &mut NodeContext,
            ) -> Result<NodeData, GraphError> {
                input.insert("_route_to", Value::String("alpha".to_string()));
                input.insert("_route_query", Value::String("again".to_string()));
                Ok(input)
            }
        }

        let mut graph = HubGraph::new(
            Box::new(InfiniteRouterNode {
                id: NodeId::new("hub"),
            }),
            3,
        )
        .with_spoke(
            SpokeConfig {
                name: "alpha".to_string(),
                description: "Alpha".to_string(),
                input_fields: vec![],
            },
            Box::new(EchoSpokeNode::new("spoke-a", "alpha_marker")),
        );

        let mut store = StateStore::new();
        let hooks = HookRegistry::new();

        // Should not hang — terminates after max_rounds.
        let output = graph.execute(NodeData::new(), &mut store, &hooks).unwrap();
        assert_eq!(output.get("_round"), Some(&Value::Number(2.0)));
    }
}
