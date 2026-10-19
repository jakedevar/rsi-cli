//! Typed node steps and edge routing (#635, plan §3).
//!
//! The pure decoding (`WorkflowSteps`, bridge metadata) lives in
//! `rsi_graph::planning::steps` (#2415); this module keeps the daemon-owned
//! execute-time validation, which still runs the store's `validate_graph`
//! before any execution.

use std::collections::HashSet;

use rsi_graph::format::WorkflowDefinition;

#[cfg(test)]
pub(crate) use rsi_graph::planning::steps::{EDGE_WHEN_METADATA_KEY, STEPS_METADATA_KEY};
pub(crate) use rsi_graph::planning::steps::{WorkflowSteps, bridge_metadata, is_step_param};

pub(crate) use crate::store_support::topology_validation::{
    StepEdge, StepNode, validate_graph, validate_topology,
};

/// Execute-time validation over a workflow snapshot (bridged or authored
/// directly through `ExecuteWorkflow`).
pub(crate) fn validate_workflow(workflow: &WorkflowDefinition) -> Result<WorkflowSteps, String> {
    let steps = WorkflowSteps::from_workflow(workflow)?;
    let known: HashSet<&str> = workflow.nodes.iter().map(|node| node.id.as_str()).collect();
    if let Some(unknown) = steps.step_node_ids().find(|id| !known.contains(id)) {
        return Err(format!("step names unknown node {unknown}"));
    }
    for (from, to) in steps.routed_edges() {
        if !workflow
            .edges
            .iter()
            .any(|edge| edge.source == from && edge.target == to)
        {
            return Err(format!("edge routing names missing edge {from} -> {to}"));
        }
    }
    let loop_edges: HashSet<(String, String)> =
        rsi_graph::planning::metadata::parse_loop_edges_from_metadata(&workflow.metadata)
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
