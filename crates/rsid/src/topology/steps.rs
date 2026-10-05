//! Typed node steps and edge routing (#635, plan §3).
//!
//! A topology carries each node's [`TopologyStep`] in `params.step` and the
//! routing of its incoming edges in `params.when`. The bridge copies both
//! into the workflow snapshot's metadata (`topology_steps`, `edge_when`), and
//! the executor decodes them strictly from there. The same static validation
//! runs at upsert (over the topology) and at execute (over the snapshot), so
//! an author cannot reach the executor with an unvalidated step.

use std::collections::{BTreeMap, HashMap, HashSet};

use rsi_common::types::{
    EdgeWhen, TOPOLOGY_STEP_PARAM, TOPOLOGY_WHEN_PARAM, TopologyDefinition, TopologyStep,
};
use rsi_graph::data::Value as GraphValue;
use rsi_graph::format::WorkflowDefinition;
use serde::{Deserialize, Serialize};

/// Workflow metadata key: JSON `{node_id: TopologyStep}`.
pub(crate) const STEPS_METADATA_KEY: &str = "topology_steps";
/// Workflow metadata key: JSON `[{from, to, when}]` for non-`success` edges.
pub(crate) const EDGE_WHEN_METADATA_KEY: &str = "edge_when";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct EdgeWhenEntry {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) when: EdgeWhen,
}

/// Decoded steps of one workflow snapshot.
#[derive(Debug, Default, Clone)]
pub(crate) struct WorkflowSteps {
    steps: HashMap<String, TopologyStep>,
    when: HashMap<(String, String), EdgeWhen>,
}

impl WorkflowSteps {
    /// Strictly decode the snapshot's step metadata. A workflow without it
    /// is all legacy session nodes with `success` edges.
    pub(crate) fn from_workflow(workflow: &WorkflowDefinition) -> Result<Self, String> {
        let text = |key: &str| match workflow.metadata.get(key) {
            None => Ok(None),
            Some(GraphValue::String(text)) => Ok(Some(text.clone())),
            Some(_) => Err(format!("workflow metadata {key} must be a JSON string")),
        };
        let steps: HashMap<String, TopologyStep> = match text(STEPS_METADATA_KEY)? {
            None => HashMap::new(),
            Some(text) => serde_json::from_str(&text)
                .map_err(|error| format!("invalid topology step: {error}"))?,
        };
        let entries: Vec<EdgeWhenEntry> = match text(EDGE_WHEN_METADATA_KEY)? {
            None => Vec::new(),
            Some(text) => serde_json::from_str(&text)
                .map_err(|error| format!("invalid edge routing: {error}"))?,
        };
        let mut when = HashMap::new();
        for entry in entries {
            if when
                .insert((entry.from.clone(), entry.to.clone()), entry.when)
                .is_some()
            {
                return Err(format!(
                    "edge {} -> {} has more than one routing condition",
                    entry.from, entry.to
                ));
            }
        }
        Ok(Self { steps, when })
    }

    /// A typed topology: at least one node carries `params.step`. Legacy
    /// shapes (no step anywhere) keep every T2 legacy-parity rule.
    pub(crate) fn is_typed(&self) -> bool {
        !self.steps.is_empty()
    }

    pub(crate) fn step(&self, node: &str) -> Option<&TopologyStep> {
        self.steps.get(node)
    }

    pub(crate) fn when(&self, from: &str, to: &str) -> EdgeWhen {
        self.when
            .get(&(from.to_owned(), to.to_owned()))
            .copied()
            .unwrap_or_default()
    }

    /// Whether any outgoing edge routes a terminal failure of `node`.
    pub(crate) fn routes_failure(&self, node: &str) -> bool {
        self.when.iter().any(|((from, _), when)| {
            from == node && matches!(when, EdgeWhen::Failure | EdgeWhen::Completed)
        })
    }

    pub(crate) fn pass_content(&self, node: &str) -> bool {
        matches!(
            self.step(node),
            Some(TopologyStep::Session {
                pass_content: true,
                ..
            })
        )
    }

    pub(crate) fn expects_commit(&self, node: &str) -> bool {
        matches!(
            self.step(node),
            Some(TopologyStep::Session {
                expects_commit: true,
                ..
            })
        )
    }

    /// Whether any node is not a session node (the legacy runner cannot run
    /// command or gate nodes).
    pub(crate) fn has_typed_effects(&self) -> bool {
        self.steps
            .values()
            .any(|step| !matches!(step, TopologyStep::Session { .. }))
    }

    pub(crate) fn command_crates(&self) -> Vec<(&str, &str)> {
        let mut crates = Vec::new();
        for (node, step) in &self.steps {
            if let TopologyStep::Command { op } = step {
                for krate in op.crates() {
                    crates.push((node.as_str(), krate));
                }
            }
        }
        crates.sort_unstable();
        crates
    }
}

pub(crate) use crate::store_support::topology_validation::{
    StepEdge, StepNode, validate_graph, validate_topology,
};

/// Execute-time validation over a workflow snapshot (bridged or authored
/// directly through `ExecuteWorkflow`).
pub(crate) fn validate_workflow(workflow: &WorkflowDefinition) -> Result<WorkflowSteps, String> {
    let steps = WorkflowSteps::from_workflow(workflow)?;
    let known: HashSet<&str> = workflow.nodes.iter().map(|node| node.id.as_str()).collect();
    if let Some(unknown) = steps.steps.keys().find(|id| !known.contains(id.as_str())) {
        return Err(format!("step names unknown node {unknown}"));
    }
    for (from, to) in steps.when.keys() {
        if !workflow
            .edges
            .iter()
            .any(|edge| &edge.source == from && &edge.target == to)
        {
            return Err(format!("edge routing names missing edge {from} -> {to}"));
        }
    }
    let loop_edges: HashSet<(String, String)> =
        crate::session::graph_runner::parse_loop_edges_from_metadata(&workflow.metadata)
            .into_iter()
            .collect();
    let nodes: Vec<StepNode<'_>> = workflow
        .nodes
        .iter()
        .map(|node| StepNode {
            id: &node.id,
            step: steps.step(&node.id),
            // The bridge turns unknown params into `key=value` tags.
            author_keys: node
                .tags
                .iter()
                .map(|tag| tag.split_once('=').map_or(tag.as_str(), |(key, _)| key))
                .collect(),
        })
        .collect();
    let edges: Vec<StepEdge<'_>> = workflow
        .edges
        .iter()
        .map(|edge| StepEdge {
            from: &edge.source,
            to: &edge.target,
            loop_edge: loop_edges.contains(&(edge.source.clone(), edge.target.clone())),
            when: steps.when(&edge.source, &edge.target),
        })
        .collect();
    validate_graph(&nodes, &edges)?;
    Ok(steps)
}

/// Bridge helper: the metadata entries for a topology's steps and routing.
pub(crate) fn bridge_metadata(
    def: &TopologyDefinition,
) -> Result<BTreeMap<String, GraphValue>, String> {
    let mut steps = BTreeMap::new();
    let mut routing = Vec::new();
    for node in &def.nodes {
        if let Some(step) = node.step()? {
            steps.insert(node.id.clone(), step);
        }
        for (from, when) in node.incoming_when()? {
            if when != EdgeWhen::Success {
                routing.push(EdgeWhenEntry {
                    from,
                    to: node.id.clone(),
                    when,
                });
            }
        }
    }
    let mut metadata = BTreeMap::new();
    if !steps.is_empty() {
        metadata.insert(
            STEPS_METADATA_KEY.to_owned(),
            GraphValue::String(serde_json::to_string(&steps).map_err(|e| e.to_string())?),
        );
    }
    if !routing.is_empty() {
        metadata.insert(
            EDGE_WHEN_METADATA_KEY.to_owned(),
            GraphValue::String(serde_json::to_string(&routing).map_err(|e| e.to_string())?),
        );
    }
    Ok(metadata)
}

/// The params keys the bridge consumes as typed step data (never tags).
pub(crate) fn is_step_param(key: &str) -> bool {
    key == TOPOLOGY_STEP_PARAM || key == TOPOLOGY_WHEN_PARAM
}
