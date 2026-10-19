//! Typed node steps and edge routing decoding (#635, plan §3; moved by #2415).
//!
//! A topology carries each node's [`TopologyStep`] in `params.step` and the
//! routing of its incoming edges in `params.when`. The bridge copies both
//! into the workflow snapshot's metadata (`topology_steps`, `edge_when`), and
//! the executor decodes them strictly from there. Static validation over the
//! decoded steps stays in the daemon (`rsid::topology::steps::validate_workflow`).

use std::collections::{BTreeMap, HashMap};

use rsi_common::types::{
    EdgeWhen, TOPOLOGY_STEP_PARAM, TOPOLOGY_WHEN_PARAM, TopologyDefinition, TopologyStep,
};
use serde::{Deserialize, Serialize};

use crate::data::Value as GraphValue;
use crate::format::WorkflowDefinition;

/// Workflow metadata key: JSON `{node_id: TopologyStep}`.
pub const STEPS_METADATA_KEY: &str = "topology_steps";
/// Workflow metadata key: JSON `[{from, to, when}]` for non-`success` edges.
pub const EDGE_WHEN_METADATA_KEY: &str = "edge_when";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EdgeWhenEntry {
    pub from: String,
    pub to: String,
    pub when: EdgeWhen,
}

/// Decoded steps of one workflow snapshot.
#[derive(Debug, Default, Clone)]
pub struct WorkflowSteps {
    steps: HashMap<String, TopologyStep>,
    when: HashMap<(String, String), EdgeWhen>,
}

impl WorkflowSteps {
    /// Strictly decode the snapshot's step metadata. A workflow without it
    /// is all legacy session nodes with `success` edges.
    pub fn from_workflow(workflow: &WorkflowDefinition) -> Result<Self, String> {
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
    pub fn is_typed(&self) -> bool {
        !self.steps.is_empty()
    }

    pub fn step(&self, node: &str) -> Option<&TopologyStep> {
        self.steps.get(node)
    }

    pub fn when(&self, from: &str, to: &str) -> EdgeWhen {
        self.when
            .get(&(from.to_owned(), to.to_owned()))
            .copied()
            .unwrap_or_default()
    }

    /// Whether any outgoing edge routes a terminal failure of `node`.
    pub fn routes_failure(&self, node: &str) -> bool {
        self.when.iter().any(|((from, _), when)| {
            from == node && matches!(when, EdgeWhen::Failure | EdgeWhen::Completed)
        })
    }

    /// #2688: whether `node` (a land node) has an outgoing `review_required`
    /// edge, so a land the daemon refuses with `land_requires_review` routes
    /// to a pre-land review instead of blocking the execution.
    pub fn routes_review_required(&self, node: &str) -> bool {
        self.when
            .iter()
            .any(|((from, _), when)| from == node && *when == EdgeWhen::ReviewRequired)
    }

    pub fn pass_content(&self, node: &str) -> bool {
        matches!(
            self.step(node),
            Some(TopologyStep::Session {
                pass_content: true,
                ..
            })
        )
    }

    pub fn expects_commit(&self, node: &str) -> bool {
        matches!(
            self.step(node),
            Some(TopologyStep::Session {
                expects_commit: true,
                ..
            })
        )
    }

    /// #2249: a session node that settles only on its own verification receipt.
    pub fn require_verified(&self, node: &str) -> bool {
        matches!(
            self.step(node),
            Some(TopologyStep::Session {
                require_verified: true,
                ..
            })
        )
    }

    /// #2759: a session node whose handoff turn asks the harness to validate
    /// the handoff's shape.
    pub fn structured_handoff(&self, node: &str) -> bool {
        matches!(
            self.step(node),
            Some(TopologyStep::Session {
                structured_handoff: true,
                ..
            })
        )
    }

    /// The node a review node reviews; `None` for any other node.
    pub fn review_of(&self, node: &str) -> Option<&str> {
        match self.step(node) {
            Some(TopologyStep::Review { of, .. }) => Some(of.as_str()),
            _ => None,
        }
    }

    pub fn is_review(&self, node: &str) -> bool {
        self.review_of(node).is_some()
    }

    /// Whether any node is not a session node (the legacy runner cannot run
    /// command or gate nodes).
    pub fn has_typed_effects(&self) -> bool {
        self.steps
            .values()
            .any(|step| !matches!(step, TopologyStep::Session { .. }))
    }

    /// Whether any node is a review node (#1641 S1b): its Work lives on an
    /// Epic's manager ledger, so the execution must have an Epic.
    pub fn has_review_nodes(&self) -> bool {
        self.steps
            .values()
            .any(|step| matches!(step, TopologyStep::Review { .. }))
    }

    /// Whether any node is a land node (#1641 S2): it enqueues on behalf of
    /// the execution's owner: its manager or Epic lead, or for an operator
    /// start the Epic's project manager (#1746).
    pub fn has_land_nodes(&self) -> bool {
        self.steps
            .values()
            .any(|step| matches!(step, TopologyStep::Land { .. }))
    }

    /// #2260: whether the land `node` closes the execution's input Issue.
    pub fn close_issue(&self, node: &str) -> bool {
        matches!(
            self.step(node),
            Some(TopologyStep::Land {
                close_issue: true,
                ..
            })
        )
    }

    /// #2260: whether any land node closes the input Issue, so the execution
    /// needs `inputs.issue` resolved to an Issue snapshot.
    pub fn has_close_issue(&self) -> bool {
        self.steps.values().any(|step| {
            matches!(
                step,
                TopologyStep::Land {
                    close_issue: true,
                    ..
                }
            )
        })
    }

    pub fn command_crates(&self) -> Vec<(&str, &str)> {
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

    /// Ids of the nodes that carry a decoded step (read-only; for the daemon's
    /// unknown-node check).
    pub fn step_node_ids(&self) -> impl Iterator<Item = &str> {
        self.steps.keys().map(String::as_str)
    }

    /// `(from, to)` of every edge with a decoded routing condition (read-only;
    /// for the daemon's missing-edge check).
    pub fn routed_edges(&self) -> impl Iterator<Item = (&str, &str)> {
        self.when
            .keys()
            .map(|(from, to)| (from.as_str(), to.as_str()))
    }
}

/// Bridge helper: the metadata entries for a topology's steps and routing.
pub fn bridge_metadata(def: &TopologyDefinition) -> Result<BTreeMap<String, GraphValue>, String> {
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
pub fn is_step_param(key: &str) -> bool {
    key == TOPOLOGY_STEP_PARAM || key == TOPOLOGY_WHEN_PARAM
}
